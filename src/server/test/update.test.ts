// 更新のエンドポイント（src/update.ts）の検査。外へは繋がない。
import { exports } from "cloudflare:workers";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  FILES_PREFIX,
  MANIFEST_ASSET,
  MANIFEST_CACHE_MS,
  MANIFEST_FETCH_TIMEOUT_MS,
  MANIFEST_PATH,
  MAX_MANIFEST_BYTES,
  MAX_NAME_LEN,
  clearManifestCache,
  handleUpdate,
  isUpdatableFileName,
  normalizeBase,
  updateTarget,
  type UpdateDeps,
} from "../src/update";
import { freshIp } from "./helpers";

const BASE = "https://dist.example/releases/latest/download/";

// ファイル名の規則の標本。src/core/src/update.rs の NAME_RULE_ACCEPT / NAME_RULE_REJECT と同じ表
// （tools/make_dist.py が 2 つの一致を見る。片方だけ直すと止まる）。
const ACCEPT: readonly string[] = [
  "HiganRTool.exe",
  "HiganRTool.dll",
  "HiganRTool_replay_fix.exe",
  "README.txt",
  "README_en.txt",
  "LICENSE.txt",
  "THIRD_PARTY_LICENSES.txt",
  "NOTICE.txt",
  "a",
  "A-b_c.d",
  "x.y.z",
  "logs.txt",
  "extra_notes.txt",
  "oldfile",
  "x.older",
  "console.txt",
  "COM10.txt",
  "COM.txt",
];
const REJECT: readonly string[] = [
  "",
  ".",
  "..",
  ".hidden",
  "x.",
  "a..b",
  "a/b",
  "a\\b",
  "..\\HiganRTool.exe",
  "../HiganRTool.exe",
  "C:x.dll",
  "x:y",
  "HiganRTool.exe:stream",
  "HiganRTool.exe ",
  "a b",
  "日本語.txt",
  "Ａ.txt",
  "%2e%2e",
  "HiganRTool.ini",
  "HIGANRTOOL.INI",
  "higanrtool.Ini",
  "game_path.txt",
  "update_launched.txt",
  "manifest.txt",
  "Manifest.unsigned.txt",
  "x.old",
  "HiganRTool.exe.old",
  "HiganRTool.dll.new",
  "x.NEW",
  "logs",
  "LOGS",
  "Code",
  "th09.exe",
  "TH09.EXE",
  "NUL",
  "con",
  "Aux.txt",
  "COM1",
  "lpt9.log",
  "logs\\x.csv",
];

// redirect: "manual" —— 既定の fetch は 302 を辿って外へ出ようとするので、回す先を読むだけにする。
function get(path: string, init: RequestInit = {}): Promise<Response> {
  return exports.default.fetch(`http://rooms.test${path}`, {
    redirect: "manual",
    headers: { "CF-Connecting-IP": freshIp() },
    ...init,
  });
}

// 目録を取り寄せる Worker の fetch は、この検査では必ず偽物に差し替える（配り場所へ本当に繋がない）。
// 差し替えを忘れた回が外へ出ないよう、既定は「繋がらない」。
let outbound: string[] = [];
let outboundReply: (url: string) => Promise<Response> = () => Promise.reject(new TypeError("blocked in test"));

beforeEach(() => {
  outbound = [];
  outboundReply = () => Promise.reject(new TypeError("blocked in test"));
  clearManifestCache();
  vi.spyOn(globalThis, "fetch").mockImplementation((input) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : (input as Request).url;
    outbound.push(url);
    return outboundReply(url);
  });
});
afterEach(() => {
  vi.restoreAllMocks();
});

const MANIFEST_TEXT = "version 200\nsig 00\n";

describe("更新の口: 実行環境の設定（wrangler.jsonc の vars）で動く本物の入口", () => {
  it("目録を取り寄せられるなら入口が本文を 200 で返し、2 回目は配り場所へ行かない", async () => {
    outboundReply = () => Promise.resolve(new Response(MANIFEST_TEXT));
    const a = await get(MANIFEST_PATH);
    expect(a.status).toBe(200);
    expect(await a.text()).toBe(MANIFEST_TEXT);
    expect(a.headers.get("Location")).toBeNull();
    expect(a.headers.get("Cache-Control")).toBe("no-store");
    const b = await get(MANIFEST_PATH);
    expect(await b.text()).toBe(MANIFEST_TEXT);
    expect(outbound.length).toBe(1);
    expect(outbound[0].startsWith("https://")).toBe(true);
    expect(outbound[0].endsWith(`/${MANIFEST_ASSET}`)).toBe(true);
  });

  it("目録の口は（取り寄せられないとき）配り場所の目録へ 302 で回し、ためさせない", async () => {
    const res = await get(MANIFEST_PATH);
    expect(res.status).toBe(302);
    const loc = res.headers.get("Location") ?? "";
    expect(loc.startsWith("https://")).toBe(true);
    expect(loc.endsWith(`/${MANIFEST_ASSET}`)).toBe(true);
    expect(res.headers.get("Cache-Control")).toBe("no-store");
  });

  it("規則に合うファイルは（表に無い新しい名前も）配り場所の同じ名前へ 302 で回す", async () => {
    const root = (await get(MANIFEST_PATH)).headers.get("Location")!.slice(0, -MANIFEST_ASSET.length);
    for (const name of ACCEPT) {
      const res = await get(`${FILES_PREFIX}${name}`);
      expect(res.status, name).toBe(302);
      expect(res.headers.get("Location"), name).toBe(root + name);
    }
  });

  it("規則の外の名前・道は 404 で、回す先を持たない", async () => {
    const bad = [
      `${FILES_PREFIX}HiganRTool.ini`,
      `${FILES_PREFIX}game_path.txt`,
      `${FILES_PREFIX}x.old`,
      `${FILES_PREFIX}.hidden`,
      `${FILES_PREFIX}LOGS`,
      `${FILES_PREFIX}logs`,
      `${FILES_PREFIX}`,
      `${FILES_PREFIX}..`,
      `${FILES_PREFIX}../HiganRTool.exe`,
      `${FILES_PREFIX}%2e%2e/HiganRTool.exe`,
      `${FILES_PREFIX}%2e%2e%2fHiganRTool.exe`,
      `${FILES_PREFIX}HiganRTool.exe/`,
      `${FILES_PREFIX}HiganRTool.exe%00`,
      `${FILES_PREFIX}manifest.txt`,
      `${FILES_PREFIX}a/b`,
      "/update",
      "/update/",
      "/update/other",
      `${MANIFEST_PATH}/x`,
    ];
    for (const p of bad) {
      const res = await get(p);
      expect(res.status, p).toBe(404);
      expect(res.headers.get("Location"), p).toBeNull();
    }
  });

  it("GET と HEAD だけ", async () => {
    expect((await get(MANIFEST_PATH, { method: "HEAD" })).status).toBe(302);
    for (const method of ["POST", "PUT", "DELETE"]) {
      const res = await get(MANIFEST_PATH, { method });
      expect(res.status, method).toBe(405);
      expect(res.headers.get("Location"), method).toBeNull();
    }
  });

  it("部屋の口と無関係な道はこれまでどおり", async () => {
    expect((await get("/v0/rooms/abc", { headers: { "CF-Connecting-IP": freshIp() } })).status).toBe(426);
    expect((await get("/nothing")).status).toBe(404);
    expect((await get("/")).status).toBe(404);
  });

  it("更新の口は Cloudflare の送り元の頭が無くても回す（回数の数えに触らない）", async () => {
    const res = await exports.default.fetch(`http://rooms.test${MANIFEST_PATH}`, { redirect: "manual" });
    expect(res.status).toBe(302);
  });
});

describe("更新の口: ファイル名の規則（Rust の is_allowed_name と同じ表）", () => {
  it("規則に合う名前は回し、外れる名前は 404", () => {
    for (const name of ACCEPT) {
      expect(isUpdatableFileName(name), name).toBe(true);
      expect(updateTarget(`${FILES_PREFIX}${name}`, BASE), name).toEqual({
        kind: "redirect",
        location: `${BASE}${name}`,
      });
    }
    for (const name of REJECT) {
      expect(isUpdatableFileName(name), JSON.stringify(name)).toBe(false);
      expect(updateTarget(`${FILES_PREFIX}${name}`, BASE), JSON.stringify(name)).toEqual({
        kind: "error",
        status: 404,
        error: "not_found",
      });
    }
  });

  it("長さの上限は 64 字", () => {
    expect(isUpdatableFileName("a".repeat(MAX_NAME_LEN))).toBe(true);
    expect(isUpdatableFileName("a".repeat(MAX_NAME_LEN + 1))).toBe(false);
  });
});

describe("更新の口: 回す先の決め方（設定の値ごと）", () => {
  it("配り場所の設定が使えないときは 503", () => {
    for (const base of [undefined, "", "http://dist.example/", "ftp://x/", "not a url", "https://x/?a=1", "https://x/#h"]) {
      expect(updateTarget(MANIFEST_PATH, base), String(base)).toEqual({
        kind: "error",
        status: 503,
        error: "update_not_configured",
      });
    }
  });

  it("末尾の / は有っても無くても同じ", () => {
    const a = updateTarget(`${FILES_PREFIX}README.txt`, BASE);
    const b = updateTarget(`${FILES_PREFIX}README.txt`, BASE.slice(0, -1));
    expect(a).toEqual({ kind: "redirect", location: `${BASE}README.txt` });
    expect(b).toEqual(a);
    expect(normalizeBase(BASE)).toBe(BASE);
  });

  it("更新の口でない道は null（部屋の口へ渡す）", () => {
    for (const p of ["/", "/v0/rooms/a", "/updates", "/updatex/manifest", "/up"]) {
      expect(updateTarget(p, BASE), p).toBeNull();
    }
  });
});

describe("更新の口: 目録の取り寄せとため（偽の fetch・偽の時計）", () => {
  const MANIFEST_URL = `${BASE}${MANIFEST_ASSET}`;
  let clock = 0;
  let calls: { url: string; init?: RequestInit }[] = [];
  let reply: (url: string) => Promise<Response> = () => Promise.resolve(new Response(MANIFEST_TEXT));
  const deps: UpdateDeps = {
    fetch: (url, init) => {
      calls.push({ url, init });
      return reply(url);
    },
    now: () => clock,
  };
  const req = (path: string, method = "GET") => new Request(`http://rooms.test${path}`, { method });

  beforeEach(() => {
    clock = 1_000_000;
    calls = [];
    reply = () => Promise.resolve(new Response(MANIFEST_TEXT));
  });

  it("取り寄せた本文を 200 で返す。302 にしない", async () => {
    const res = (await handleUpdate(req(MANIFEST_PATH), BASE, deps))!;
    expect(res.status).toBe(200);
    expect(await res.text()).toBe(MANIFEST_TEXT);
    expect(res.headers.get("Location")).toBeNull();
    expect(res.headers.get("Content-Type")).toContain("text/plain");
    expect(res.headers.get("Cache-Control")).toBe("no-store");
    expect(calls.map((c) => c.url)).toEqual([MANIFEST_URL]);
  });

  it("取り寄せには待ちの上限を付け、Cloudflare 側のためは付けない（反映の遅れが isolate のための時間に収まるように）", async () => {
    await handleUpdate(req(MANIFEST_PATH), BASE, deps);
    const init = calls[0].init as RequestInit & { cf?: unknown };
    expect(init.signal).toBeInstanceOf(AbortSignal);
    expect(init.signal!.aborted).toBe(false);
    expect(init.cf).toBeUndefined();
    expect(MANIFEST_CACHE_MS).toBe(5 * 60 * 1000);
  });

  it("ためが生きている間は配り場所へ行かず、切れたら取り直す", async () => {
    await handleUpdate(req(MANIFEST_PATH), BASE, deps);
    clock += MANIFEST_CACHE_MS - 1;
    const hit = (await handleUpdate(req(MANIFEST_PATH), BASE, deps))!;
    expect(await hit.text()).toBe(MANIFEST_TEXT);
    expect(calls.length).toBe(1);
    clock += 1;
    reply = () => Promise.resolve(new Response("version 300\n"));
    const fresh = (await handleUpdate(req(MANIFEST_PATH), BASE, deps))!;
    expect(await fresh.text()).toBe("version 300\n");
    expect(calls.length).toBe(2);
  });

  it("同じためから返した本文は、前の応答の読み出しで減らない", async () => {
    for (let i = 0; i < 3; i++) {
      const res = (await handleUpdate(req(MANIFEST_PATH), BASE, deps))!;
      expect(await res.text(), String(i)).toBe(MANIFEST_TEXT);
    }
    expect(calls.length).toBe(1);
  });

  const FAILS: [string, () => Promise<Response>][] = [
    ["接続できない", () => Promise.reject(new TypeError("network"))],
    ["待ちの上限で中断", () => Promise.reject(new DOMException("timeout", "TimeoutError"))],
    ["404", () => Promise.resolve(new Response("no", { status: 404 }))],
    ["500", () => Promise.resolve(new Response("no", { status: 500 }))],
    ["中身が空", () => Promise.resolve(new Response(""))],
    ["上限を越える（長さの申告あり）", () => Promise.resolve(new Response("x".repeat(MAX_MANIFEST_BYTES + 1)))],
    [
      "長さの申告だけが上限を越える（本文を読まずに断る）",
      () =>
        Promise.resolve(new Response(MANIFEST_TEXT, { headers: { "Content-Length": String(MAX_MANIFEST_BYTES + 1) } })),
    ],
    [
      "上限を越える（申告なしの流れ）",
      () =>
        Promise.resolve(
          new Response(
            new ReadableStream({
              start(c) {
                c.enqueue(new Uint8Array(MAX_MANIFEST_BYTES));
                c.enqueue(new Uint8Array(1));
                c.close();
              },
            }),
          ),
        ),
    ],
  ];

  it("取り寄せに失敗したら、これまでどおり配り場所へ 302 で回す。失敗はためない", async () => {
    for (const [what, fail] of FAILS) {
      clearManifestCache();
      calls = [];
      reply = fail;
      const res = (await handleUpdate(req(MANIFEST_PATH), BASE, deps))!;
      expect(res.status, what).toBe(302);
      expect(res.headers.get("Location"), what).toBe(MANIFEST_URL);
      expect(res.headers.get("Cache-Control"), what).toBe("no-store");
      reply = () => Promise.resolve(new Response(MANIFEST_TEXT));
      const next = (await handleUpdate(req(MANIFEST_PATH), BASE, deps))!;
      expect(next.status, `${what}: 失敗の次は取り直す`).toBe(200);
      expect(calls.length, what).toBe(2);
    }
  });

  it("上限ちょうどの目録は返す", async () => {
    reply = () => Promise.resolve(new Response("x".repeat(MAX_MANIFEST_BYTES)));
    const res = (await handleUpdate(req(MANIFEST_PATH), BASE, deps))!;
    expect(res.status).toBe(200);
    expect((await res.text()).length).toBe(MAX_MANIFEST_BYTES);
  });

  it("答えない配り場所は待ちの上限で見切って 302 に戻る", async () => {
    const hang: UpdateDeps = {
      now: deps.now,
      fetch: (url, init) =>
        new Promise<Response>((_, reject) => {
          init!.signal!.addEventListener("abort", () => reject(init!.signal!.reason));
        }),
    };
    const t0 = performance.now();
    const res = (await handleUpdate(req(MANIFEST_PATH), BASE, hang))!;
    const took = performance.now() - t0;
    expect(res.status).toBe(302);
    expect(took).toBeGreaterThanOrEqual(MANIFEST_FETCH_TIMEOUT_MS * 0.9);
    expect(took).toBeLessThan(MANIFEST_FETCH_TIMEOUT_MS + 800);
  });

  it("ファイルは取り寄せず、これまでどおり 302 で回す", async () => {
    const res = (await handleUpdate(req(`${FILES_PREFIX}HiganRTool.exe`), BASE, deps))!;
    expect(res.status).toBe(302);
    expect(res.headers.get("Location")).toBe(`${BASE}HiganRTool.exe`);
    expect(calls.length).toBe(0);
  });

  it("HEAD・規則の外・設定なし・許されない動詞は取り寄せない", async () => {
    const head = (await handleUpdate(req(MANIFEST_PATH, "HEAD"), BASE, deps))!;
    expect(head.status).toBe(302);
    expect((await handleUpdate(req(`${FILES_PREFIX}HiganRTool.ini`), BASE, deps))!.status).toBe(404);
    expect((await handleUpdate(req(MANIFEST_PATH), undefined, deps))!.status).toBe(503);
    expect((await handleUpdate(req(MANIFEST_PATH, "POST"), BASE, deps))!.status).toBe(405);
    expect(await handleUpdate(req("/v0/rooms/a"), BASE, deps)).toBeNull();
    expect(calls.length).toBe(0);
  });

  it("配り場所が変われば別のためとして取り直す", async () => {
    await handleUpdate(req(MANIFEST_PATH), BASE, deps);
    await handleUpdate(req(MANIFEST_PATH), "https://other.example/dl/", deps);
    expect(calls.map((c) => c.url)).toEqual([MANIFEST_URL, "https://other.example/dl/manifest.txt"]);
  });
});
