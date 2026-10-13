// 更新のエンドポイント（src/update.ts）の検査。外へは繋がない。
import { exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import {
  FILES_PREFIX,
  MANIFEST_ASSET,
  MANIFEST_PATH,
  MAX_NAME_LEN,
  isUpdatableFileName,
  normalizeBase,
  updateTarget,
} from "../src/update";
import { freshIp } from "./helpers";

const BASE = "https://dist.example/releases/latest/download/";

// ファイル名の規則の標本。src/core/src/update.rs の NAME_RULE_ACCEPT / NAME_RULE_REJECT と同じ表
// （tools/make_dist.py が 2 つの一致を見る。片方だけ直すと止まる）。
const ACCEPT: readonly string[] = [
  "HiganRTool.exe",
  "HiganRTool.dll",
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

describe("更新の口: 実行環境の設定（wrangler.jsonc の vars）で動く本物の入口", () => {
  it("目録の口は配り場所の目録へ 302 で回し、ためさせない", async () => {
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
