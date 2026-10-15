// 更新のエンドポイント。配るバージョンのローダー（HiganRTool.exe）が花映塚を起こす前に、署名つきの目録とファイルを取りに来る。
// このサーバーは中身を持たず、ファイルはいまの配り場所（GitHub の Releases の最新）へ 302 で回す。配り場所は wrangler.jsonc の
// vars.UPDATE_BASE_URL の 1 値で、変えても配った exe を作り直さずに済む。
// 目録だけは Worker が配り場所から取り寄せて本文を返す。ローダーの待ちは約 2 秒で、302 を辿ると宿ごとに名前の解決と TLS が要るため。
// 取り寄せた目録は MANIFEST_CACHE_MS だけ isolate の中にためる（配り場所への取りに行きが、起動の回数でなく時間あたりになる）。
// 取り寄せに失敗したら、これまでどおり 302 で回す。
// 目録は署名つきで、ファイルは目録の SHA-256 と照らされるので、回す先・ためた物が荒らされても署名のない物は使われない。
// パス・ファイル名の規則の原本は src/core/src/update.rs（MANIFEST_PATH・FILES_PATH・is_allowed_name）。

/** 目録のエンドポイント（Rust の MANIFEST_PATH）。 */
export const MANIFEST_PATH = "/update/manifest";
/** ファイルのエンドポイントの先頭（Rust の FILES_PATH）。後ろにファイル名が付く。 */
export const FILES_PREFIX = "/update/files/";
/** 配り場所に置く目録のファイル名。 */
export const MANIFEST_ASSET = "manifest.txt";
/** 目録の大きさの上限（Rust の MAX_MANIFEST_BYTES）。越える応答はためも返しもしない。 */
export const MAX_MANIFEST_BYTES = 8 * 1024;
/** 取り寄せた目録をためる時間。新しいバージョンが利用者に見えるまでの遅れの上限でもある。 */
export const MANIFEST_CACHE_MS = 5 * 60 * 1000;
/** 取り寄せの待ち。ローダーの待ち（Rust の CHECK_DEADLINE_MS = 2000）の半分で見切り、302 に戻る余地を残す。 */
export const MANIFEST_FETCH_TIMEOUT_MS = 1000;
/** 名前の長さの上限（Rust の MAX_NAME_LEN）。 */
export const MAX_NAME_LEN = 64;
/** 更新が置き換えない名前（Rust の RESERVED_NAMES）。小文字にして、大文字小文字を区別せずに比べる。 */
const RESERVED_NAMES: readonly string[] = [
  "HiganRTool.ini",
  "game_path.txt",
  "update_launched.txt",
  "manifest.txt",
  "manifest.unsigned.txt",
  "th09.exe",
  "logs",
  "code",
].map((n) => n.toLowerCase());

/**
 * 回してよいファイル名か（Rust の is_allowed_name と同じ規則）。ASCII の英数字と . _ - だけ・1〜64 文字で、
 * . で始まらず・終わらず・.. を含まず、.old / .new で終わらず、利用者のものでなく、Windows の機器の名前（CON・NUL・COM1 など）でない。
 * 目録は署名で守られているので名前の表は持たず、この規則に合えば回す。規則の外は 404。
 */
export function isUpdatableFileName(name: string): boolean {
  if (name.length < 1 || name.length > MAX_NAME_LEN) return false;
  if (!/^[A-Za-z0-9._-]+$/.test(name)) return false;
  if (name.startsWith(".") || name.endsWith(".") || name.includes("..")) return false;
  const lower = name.toLowerCase();
  if (RESERVED_NAMES.includes(lower)) return false;
  if (lower.endsWith(".old") || lower.endsWith(".new")) return false;
  const stem = lower.split(".")[0];
  if (["con", "prn", "aux", "nul"].includes(stem) || /^(com|lpt)[0-9]$/.test(stem)) return false;
  return true;
}

export type UpdateTarget =
  | { kind: "redirect"; location: string }
  | { kind: "error"; status: number; error: string };

/**
 * パス `pathname` が更新のエンドポイントなら回す先（配り場所 `base` ＋ 名前）か断りを返す。更新のエンドポイントでなければ null（ルームのエンドポイントへ渡す）。
 * 名前の比べは百分率符号化を解かずにそのまま（`%2e%2e` などは規則の外なので 404）。
 */
export function updateTarget(pathname: string, base: string | undefined): UpdateTarget | null {
  if (pathname !== "/update" && !pathname.startsWith("/update/")) return null;
  const name = pathname.startsWith(FILES_PREFIX) ? pathname.slice(FILES_PREFIX.length) : null;
  let asset: string;
  if (pathname === MANIFEST_PATH) {
    asset = MANIFEST_ASSET;
  } else if (name !== null && isUpdatableFileName(name)) {
    asset = name;
  } else {
    return { kind: "error", status: 404, error: "not_found" };
  }
  const root = normalizeBase(base);
  if (root === null) return { kind: "error", status: 503, error: "update_not_configured" };
  return { kind: "redirect", location: root + asset };
}

/** 配り場所の URL を「https で、末尾が / 」に揃える。設定が無い・https でない・URL でないものは null。 */
export function normalizeBase(base: string | undefined): string | null {
  if (!base) return null;
  let url: URL;
  try {
    url = new URL(base);
  } catch {
    return null;
  }
  if (url.protocol !== "https:" || url.search !== "" || url.hash !== "") return null;
  const s = url.toString();
  return s.endsWith("/") ? s : s + "/";
}

export interface UpdateDeps {
  fetch: (input: string, init?: RequestInit) => Promise<Response>;
  now: () => number;
}

const realDeps: UpdateDeps = {
  // globalThis.fetch は呼ぶ時に引く（検査が差し替えられるように）。
  fetch: (input, init) => globalThis.fetch(input, init),
  now: () => Date.now(),
};

/** isolate ごとの目録のため。URL ごとに 1 つ。失敗は入れない。 */
const manifestCache = new Map<string, { body: Uint8Array; expires: number }>();

export function clearManifestCache(): void {
  manifestCache.clear();
}

/** 本文が上限を越えたら null。 */
async function readCapped(res: Response, max: number): Promise<Uint8Array | null> {
  const declared = Number(res.headers.get("Content-Length"));
  if (Number.isFinite(declared) && declared > max) return null;
  if (res.body === null) return null;
  const reader = res.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > max) {
      await reader.cancel();
      return null;
    }
    chunks.push(value);
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.byteLength;
  }
  return out;
}

/**
 * 配り場所の目録の本文。ためが生きていればそれを、無ければ取り寄せる。取り寄せ（redirect を辿った先の 200）に失敗したら null。
 * ためは isolate の中だけ（MANIFEST_CACHE_MS）。Cloudflare の層には任せない（任せると、新しいバージョンが利用者に見えるまでの遅れが
 * MANIFEST_CACHE_MS に収まらなくなる）。
 */
async function manifestBody(url: string, deps: UpdateDeps): Promise<Uint8Array | null> {
  const hit = manifestCache.get(url);
  if (hit !== undefined && deps.now() < hit.expires) return hit.body;
  try {
    const res = await deps.fetch(url, { signal: AbortSignal.timeout(MANIFEST_FETCH_TIMEOUT_MS) });
    if (res.status !== 200) return null;
    const body = await readCapped(res, MAX_MANIFEST_BYTES);
    if (body === null || body.byteLength === 0) return null;
    manifestCache.set(url, { body, expires: deps.now() + MANIFEST_CACHE_MS });
    return body;
  } catch {
    return null;
  }
}

/** 更新のエンドポイントの Response。GET と HEAD だけ。 */
export async function handleUpdate(
  req: Request,
  base: string | undefined,
  deps: UpdateDeps = realDeps,
): Promise<Response | null> {
  const pathname = new URL(req.url).pathname;
  const t = updateTarget(pathname, base);
  if (t === null) return null;
  if (req.method !== "GET" && req.method !== "HEAD") {
    return Response.json({ error: "method_not_allowed" }, { status: 405, headers: { Allow: "GET, HEAD" } });
  }
  if (t.kind === "error") return Response.json({ error: t.error }, { status: t.status });
  // HEAD は取り寄せずに回す（本文が要らないので配り場所を叩かない）。
  if (pathname === MANIFEST_PATH && req.method === "GET") {
    const body = await manifestBody(t.location, deps);
    if (body !== null) {
      // 古い目録を中継の層に残させない。ため（MANIFEST_CACHE_MS）はこの Worker の中だけ。
      return new Response(body, {
        headers: { "Content-Type": "text/plain; charset=utf-8", "Cache-Control": "no-store" },
      });
    }
  }
  return new Response(null, {
    status: 302,
    headers: { Location: t.location, "Cache-Control": "no-store" },
  });
}
