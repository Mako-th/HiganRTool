// 更新のエンドポイント。配布バージョンのローダー（HiganRTool.exe）は花映塚を起動する前にここへ来て署名つきの目録とファイルを取得する。
// このサーバーは中身を持たない。ファイルはいまの配布場所（GitHubのReleasesの最新）へ302でリダイレクトする。
// 配布場所はwrangler.jsoncのvars.UPDATE_BASE_URLの1値で、変えても配布したexeを作り直さずに済む。
// 目録だけはWorkerが配布場所から取得して本文を返す。理由は、ローダーが目録の応答を待つ時間が約2秒で、302を辿るとホストごとに名前解決とTLSが要るため。
// 取得した目録はMANIFEST_CACHE_MSだけisolateの中にキャッシュする。配布場所への取得が起動の回数でなく時間あたりになる。
// 取得に失敗したらこれまでどおり302でリダイレクトする。
// 目録は署名つきで、ファイルは目録のSHA-256と照らされる。そのためリダイレクト先やキャッシュが荒らされても署名のない物は使われない。
// パス・ファイル名の規則の原本はsrc/core/src/update.rs（MANIFEST_PATH・FILES_PATH・is_allowed_name）。

/** 目録のエンドポイント（RustのMANIFEST_PATH）。 */
export const MANIFEST_PATH = "/update/manifest";
/** ファイルのエンドポイントの先頭（RustのFILES_PATH）。後ろにファイル名が付く。 */
export const FILES_PREFIX = "/update/files/";
/** 配布場所に用意する目録のファイル名。 */
export const MANIFEST_ASSET = "manifest.txt";
/** 目録の大きさの上限（RustのMAX_MANIFEST_BYTES）。越える応答はキャッシュも返答もしない。 */
export const MAX_MANIFEST_BYTES = 8 * 1024;
/** 取得した目録をキャッシュする時間。新しいバージョンが利用者に見えるまでの遅れの上限でもある。 */
export const MANIFEST_CACHE_MS = 5 * 60 * 1000;
/** 目録を取得するときのタイムアウト。ローダーの待ち（Rustの CHECK_DEADLINE_MS = 2000）の半分で打ち切って302に戻る余地を残す。 */
export const MANIFEST_FETCH_TIMEOUT_MS = 1000;
/** 名前の長さの上限（RustのMAX_NAME_LEN）。 */
export const MAX_NAME_LEN = 64;
/** 更新が置き換えない名前（RustのRESERVED_NAMES）。小文字にして大文字小文字を区別せずに比べる。 */
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
 * リダイレクトしてよいファイル名かどうか（Rustのis_allowed_nameと同じ規則）。次が全部揃ったときだけ許す:
 * ASCIIの英数字と . _ - だけ・1〜64文字・. で始まらず終わらない・.. を含まない・.old / .newで終わらない・利用者のものでない・Windowsのデバイス名（CON・NUL・COM1など）でない。
 * 目録は署名で守られているので名前の表は持たない。この規則に合えばリダイレクトする。規則の外は404。
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
 * パス`pathname`が更新のエンドポイントならリダイレクト先（配布場所`base` ＋ 名前）か断りを返す。更新のエンドポイントでなければnull（ルームのエンドポイントへ渡す）。
 * 名前の比較はパーセント符号化を解かずにそのまま行う（`%2e%2e`などは規則の外なので404）。
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

/** 配布場所のURLを「httpsで末尾が / 」に揃える。設定が無い・httpsでない・URLでないものはnull。 */
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
  // globalThis.fetchは呼ぶ時に参照する（検査が差し替えられるように）。
  fetch: (input, init) => globalThis.fetch(input, init),
  now: () => Date.now(),
};

/** isolateごとの目録のキャッシュ。URLごとに1つ。失敗は追加しない。 */
const manifestCache = new Map<string, { body: Uint8Array; expires: number }>();

export function clearManifestCache(): void {
  manifestCache.clear();
}

/** 本文が上限を越えたらnull。 */
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
 * 配布場所の目録の本文。キャッシュが有効ならそれを返し、無ければ取得する。取得（redirectを辿った先の200）に失敗したらnull。
 * キャッシュはisolateの中だけ（MANIFEST_CACHE_MS）。Cloudflareのキャッシュ層には任せない。
 * 任せると新しいバージョンが利用者に見えるまでの遅れがMANIFEST_CACHE_MSに収まらなくなる。
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

/** 更新のエンドポイントのResponse。GETとHEADだけ。 */
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
  // HEADは取得せずにリダイレクトする（本文が要らないので配布場所にリクエストしない）。
  if (pathname === MANIFEST_PATH && req.method === "GET") {
    const body = await manifestBody(t.location, deps);
    if (body !== null) {
      // 古い目録を中継のキャッシュ層に残させない。キャッシュ（MANIFEST_CACHE_MS）はこのWorkerの中だけ。
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
