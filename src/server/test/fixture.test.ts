// 共通の見本（tests/fixtures/room/）と rules.ts の突き合わせ。Rust の側も同じ見本を include_str! で読むので、
// rules.ts と Rust のコピーのずれを縛る。
import { describe, expect, it } from "vitest";
import namesJson from "../../../tests/fixtures/room/names.json";
import rulesJson from "../../../tests/fixtures/room/rules.json";
import {
  ANSWER_TTL_MS,
  CLIENT_ID_LEN,
  CLIENT_ID_PARAM,
  CLOSE_NAME,
  CODE_MAX_CHARS,
  Close,
  MESSAGE_MAX_BYTES,
  PING_TEXT,
  PONG_TEXT,
  PROTO_PATH_PREFIX,
  RANDOM_PATH_PREFIX,
  RANDOM_QUEUE_TTL_MS,
  RANDOM_ROOM_HEX_LEN,
  RANDOM_ROOM_PREFIX,
  RANDOM_ROOM_TTL_MS,
  RETRY_AFTER_SECS,
  ROOM_NAME_BAD_CHARS,
  ROOM_NAME_BAD_NAMES,
  ROOM_NAME_FOLD,
  ROOM_NAME_MAX,
  ROOM_NAME_MAX_BYTES,
  ROOM_NAME_MIN,
  ROOM_TTL_MS,
  WATCH_CAP_MAX,
  WATCH_DELAY_MAX,
  WATCH_HASH_BYTES,
  WATCH_LIST_MAX_BYTES,
  WATCH_MAX_PER_IP,
  WATCH_MAX_TOTAL,
  WATCH_MODES,
  WATCH_NAME_MAX,
  WATCH_PATH_PREFIX,
  WATCH_POST_SEGMENT,
  WATCH_ROOM_PREFIX,
  WATCH_STALE_MS,
  WATCH_VIEW_SEGMENT,
  checkCode,
  decodeRoomName,
  normalizeRoomName,
  spectateRoomName,
} from "../src/rules";
import spectateNamesJson from "../../../tests/fixtures/room/spectate_names.json";
import { connect, rawConnect } from "./helpers";

const rules = rulesJson as Record<string, string | number>;
const names = namesJson as Record<string, string>;

/** 見本の鍵を読む（無い・型が違うなら落ちる）。最後に読まなかった鍵が無いことを見る。 */
const used = new Set<string>(["about", "about_lines"]);
function num(k: string): number {
  used.add(k);
  const v = rules[k];
  if (typeof v !== "number") throw new Error(`rules.json の ${k} が整数でない: ${String(v)}`);
  return v;
}
function text(k: string): string {
  used.add(k);
  const v = rules[k];
  if (typeof v !== "string") throw new Error(`rules.json の ${k} が文字列でない: ${String(v)}`);
  return v;
}

function parseRanges(s: string): [number, number][] {
  return s.split(",").map((part) => {
    const m = /^U\+([0-9A-F]{4,6})-U\+([0-9A-F]{4,6})$/.exec(part);
    if (m === null) throw new Error(`範囲の形が違う: ${part}`);
    return [parseInt(m[1], 16), parseInt(m[2], 16)];
  });
}

/** 見本の room_name_fold の順に揃える（rules.ts の実装とは別に組み立てる）。 */
function foldByFixture(raw: string): string {
  let s = raw;
  for (const step of (rules.room_name_fold as string).split(",")) {
    if (step === "nfkc") s = s.normalize("NFKC");
    else if (step === "trim") s = s.trim();
    else if (step === "lower") s = s.toLowerCase();
    else throw new Error(`知らない揃えの手順: ${step}`);
  }
  return s;
}

function classify() {
  const ok: [string, string][] = [];
  const bad: [string, string][] = [];
  const serverBad: [string, string][] = [];
  const pairs = new Map<string, { kind: "same" | "diff"; a?: string; b?: string }>();
  for (const [k, v] of Object.entries(names)) {
    if (k === "about") continue;
    const m = /^(same|diff)_(.+)_(a|b)$/.exec(k);
    if (m !== null) {
      const p = pairs.get(m[2]) ?? { kind: m[1] as "same" | "diff" };
      expect(p.kind, k).toBe(m[1]);
      p[m[3] as "a" | "b"] = v;
      pairs.set(m[2], p);
    } else if (k.startsWith("ok_")) ok.push([k, v]);
    else if (k.startsWith("server_bad_")) serverBad.push([k, v]);
    else if (k.startsWith("bad_")) bad.push([k, v]);
    else throw new Error(`names.json の鍵の頭を知らない: ${k}`);
  }
  for (const [n, p] of pairs) {
    expect(p.a, `${n} の _a`).toBeTypeOf("string");
    expect(p.b, `${n} の _b`).toBeTypeOf("string");
  }
  return { ok, bad, serverBad, pairs: [...pairs].map(([n, p]) => ({ n, kind: p.kind, a: p.a!, b: p.b! })) };
}

/** パスに載せたときに 404 になる名（パスの読み方で区切りが消える）。ほかの bad_ は 400。 */
function expected404(key: string): boolean {
  return key === "bad_empty" || key.startsWith("bad_dot_");
}

describe("rules.json と rules.ts", () => {
  it("値とガードの振る舞いが見本と一致する", () => {
    expect(text("path_prefix")).toBe(PROTO_PATH_PREFIX);

    expect(num("room_name_min")).toBe(ROOM_NAME_MIN);
    expect(num("room_name_max")).toBe(ROOM_NAME_MAX);
    expect(num("room_name_max_bytes")).toBe(ROOM_NAME_MAX_BYTES);
    expect(text("room_name_bad_chars")).toBe(ROOM_NAME_BAD_CHARS);
    expect(text("room_name_bad_names").split(",")).toEqual([...ROOM_NAME_BAD_NAMES]);
    expect(text("room_name_fold").split(",")).toEqual([...ROOM_NAME_FOLD]);

    // 全コードポイント（代理を除く）で、`a<字>b` を断るかが見本の範囲と `/` の和に一致すること。
    const ranges = parseRanges(text("room_name_controls"));
    const bad = new Set(ROOM_NAME_BAD_CHARS);
    let rejected = 0;
    for (let cp = 0; cp <= 0x10ffff; cp++) {
      if (cp >= 0xd800 && cp <= 0xdfff) continue;
      const ch = String.fromCodePoint(cp);
      const want = ranges.some(([lo, hi]) => cp >= lo && cp <= hi) || bad.has(ch);
      const got = normalizeRoomName(`a${ch}b`) === null;
      if (got !== want) throw new Error(`U+${cp.toString(16)}: 断る=${got}、見本=${want}`);
      rejected += got ? 1 : 0;
    }
    expect(rejected).toBe(66);

    expect(num("code_min_chars")).toBe(1);
    expect(checkCode("")).toBe(Close.BadCode);
    expect(checkCode("A")).toBeNull();
    expect(num("code_max_chars")).toBe(CODE_MAX_CHARS);
    const codeChars = text("code_chars");
    for (let cp = 0; cp < 128; cp++) {
      const ch = String.fromCharCode(cp);
      expect(checkCode(ch) === null, `コードの字 ${JSON.stringify(ch)}`).toBe(codeChars.includes(ch));
    }

    expect(num("message_max_bytes")).toBe(MESSAGE_MAX_BYTES);
    expect(num("room_ttl_ms")).toBe(ROOM_TTL_MS);
    expect(num("answer_ttl_ms")).toBe(ANSWER_TTL_MS);
    expect(text("ping_text")).toBe(PING_TEXT);
    expect(text("pong_text")).toBe(PONG_TEXT);
    expect(num("retry_after_secs")).toBe(RETRY_AFTER_SECS);
    expect(text("client_id_param")).toBe(CLIENT_ID_PARAM);
    expect(num("client_id_len")).toBe(CLIENT_ID_LEN);

    expect(text("random_path_prefix")).toBe(RANDOM_PATH_PREFIX);
    expect(text("random_room_prefix")).toBe(RANDOM_ROOM_PREFIX);
    expect(num("random_room_hex_len")).toBe(RANDOM_ROOM_HEX_LEN);
    expect(num("random_queue_ttl_ms")).toBe(RANDOM_QUEUE_TTL_MS);
    expect(num("random_room_ttl_ms")).toBe(RANDOM_ROOM_TTL_MS);

    expect(text("watch_path_prefix")).toBe(WATCH_PATH_PREFIX);
    expect(text("watch_post_segment")).toBe(WATCH_POST_SEGMENT);
    expect(text("watch_view_segment")).toBe(WATCH_VIEW_SEGMENT);
    expect(text("watch_room_prefix")).toBe(WATCH_ROOM_PREFIX);
    expect(num("watch_hash_bytes")).toBe(WATCH_HASH_BYTES);
    expect(num("watch_max_total")).toBe(WATCH_MAX_TOTAL);
    expect(num("watch_max_per_ip")).toBe(WATCH_MAX_PER_IP);
    expect(num("watch_cap_max")).toBe(WATCH_CAP_MAX);
    expect(num("watch_delay_max")).toBe(WATCH_DELAY_MAX);
    expect(num("watch_name_max")).toBe(WATCH_NAME_MAX);
    expect(text("watch_modes").split(",")).toEqual([...WATCH_MODES]);
    expect(num("watch_stale_ms")).toBe(WATCH_STALE_MS);
    expect(num("watch_list_max_bytes")).toBe(WATCH_LIST_MAX_BYTES);

    const closeKeys = Object.keys(rules).filter((k) => k.startsWith("close_"));
    for (const k of closeKeys) {
      const n = num(k);
      expect(CLOSE_NAME[n], k).toBe(k.slice("close_".length));
    }
    expect(closeKeys.length).toBe(Object.keys(CLOSE_NAME).length);
    expect(Object.values(Close).sort()).toEqual(closeKeys.map((k) => rules[k]).sort());

    const unread = Object.keys(rules).filter((k) => !used.has(k));
    expect(unread, "比べていない鍵").toEqual([]);
  });
});

describe("spectate_names.json と spectateRoomName", () => {
  it("割り当ての名から導く観戦用の名が、Python の hashlib で作った見本と全部一致する（Rust の側も同じ見本を読む）", async () => {
    const v = spectateNamesJson as Record<string, string>;
    let n = 0;
    for (let i = 0; v[`q${i}`] !== undefined; i++) {
      expect(await spectateRoomName(v[`q${i}`]), `q${i}`).toBe(v[`w${i}`]);
      n += 1;
    }
    expect(n, "見本の組の数").toBeGreaterThanOrEqual(4);
  });
});

describe("names.json と normalizeRoomName", () => {
  it("通す名は見本の手順で揃えた字になり、断る名は null。same は同じ部屋、diff は別の部屋", () => {
    const { ok, bad, serverBad, pairs } = classify();
    const same = pairs.filter((p) => p.kind === "same");
    const diff = pairs.filter((p) => p.kind === "diff");
    // 母数（どの頭も 0 件で素通りにならないように。件数そのものは固定しない）。
    for (const [what, n] of [["ok", ok.length], ["bad", bad.length], ["server_bad", serverBad.length], ["same", same.length], ["diff", diff.length]] as const) {
      expect(n, what).toBeGreaterThan(0);
    }

    for (const [k, v] of ok) expect(normalizeRoomName(v), k).toBe(foldByFixture(v));
    for (const [k, v] of [...bad, ...serverBad]) expect(normalizeRoomName(v), k).toBeNull();
    const keys = new Set<string>();
    for (const p of pairs) {
      const a = normalizeRoomName(p.a);
      const b = normalizeRoomName(p.b);
      expect(a, `${p.n}_a`).toBe(foldByFixture(p.a));
      expect(b, `${p.n}_b`).toBe(foldByFixture(p.b));
      if (p.kind === "same") {
        expect(a, p.n).toBe(b);
        keys.add(a!);
      } else {
        expect(a, p.n).not.toBe(b);
        keys.add(a!);
        keys.add(b!);
      }
    }
    expect(keys.size).toBe(same.length + diff.length * 2);
    // 効く場合が見本にあること: 揃える前は 33 文字以上でも揃えると 32 文字以下になる名、逆に揃えると 33 文字以上になる名、32 文字以下で 128 B だけを越える名。
    const chars = (s: string) => [...s].length;
    const bytes = (s: string) => new TextEncoder().encode(s).byteLength;
    expect(ok.some(([, v]) => chars(v) > ROOM_NAME_MAX), "揃えると縮む・空白を除く ok").toBe(true);
    expect(serverBad.some(([, v]) => chars(v) <= ROOM_NAME_MAX), "揃えると伸びる server_bad").toBe(true);
    expect(
      bad.some(([, v]) => bytes(v) > ROOM_NAME_MAX_BYTES && chars(foldByFixture(v)) <= ROOM_NAME_MAX),
      "128 B だけを越える bad",
    ).toBe(true);
  });

  it("通す名は encodeURIComponent → decodeRoomName で元の字に戻る", () => {
    const { ok, serverBad, pairs } = classify();
    const all = [...ok.map(([, v]) => v), ...serverBad.map(([, v]) => v), ...pairs.flatMap((p) => [p.a, p.b])];
    for (const v of all) expect(decodeRoomName(encodeURIComponent(v)), v).toBe(v);
  });
});

describe("names.json を本物の入口で", () => {
  it("same の 2 人は同じ部屋（2 人目は guest）、diff の 2 人は別の部屋（両方 host）", async () => {
    const { pairs } = classify();
    for (const p of pairs) {
      const a = await connect(p.a);
      expect(await a.next(), `${p.n}_a`).toMatchObject({ t: "room", role: "host" });
      const b = await connect(p.b);
      expect(await b.next(), `${p.n}_b`).toMatchObject({
        t: "room",
        role: p.kind === "same" ? "guest" : "host",
      });
      a.close();
      b.close();
      await a.closed;
      await b.closed;
    }
  });

  it("ok は繋がり、bad と server_bad は 400 bad_room_name（道から区切りが消える名は 404）", async () => {
    const { ok, bad, serverBad } = classify();
    for (const [k, v] of ok) {
      const c = await connect(v);
      expect((await c.next()).t, k).toBe("room");
      c.close();
      await c.closed;
    }
    let n404 = 0;
    for (const [k, v] of [...bad, ...serverBad]) {
      const res = await rawConnect(`/v0/rooms/${encodeURIComponent(v)}`);
      if (expected404(k)) {
        n404 += 1;
        expect(res.status, k).toBe(404);
        expect(await res.json(), k).toEqual({ error: "not_found" });
      } else {
        expect(res.status, k).toBe(400);
        expect(await res.json(), k).toEqual({ error: "bad_room_name" });
      }
    }
    expect(n404).toBeGreaterThan(0);
  });
});
