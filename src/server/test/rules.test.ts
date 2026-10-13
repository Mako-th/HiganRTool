// 純粋な決まり（src/rules.ts）の検査。通るものと通らないものをガードの境目の両側で並べる。
import { describe, expect, it } from "vitest";
import {
  CODE_MAX_CHARS,
  Close,
  CLIENT_ID_LEN,
  MESSAGE_MAX_BYTES,
  ROOM_NAME_MAX,
  ROOM_NAME_MAX_BYTES,
  decodeRoomName,
  normalizeRoomName,
  parseClientId,
  parseClientMessage,
  roomNameFromPath,
} from "../src/rules";

describe("cid（起動ごとの乱数）", () => {
  it("16 進小文字ちょうど 32 字だけを通す。無い・長さ違い・大文字・16 進でない字は null", () => {
    const ok = "0123456789abcdef".repeat(2);
    expect(ok.length).toBe(CLIENT_ID_LEN);
    expect(parseClientId(ok)).toBe(ok);
    for (const bad of [null, "", ok.slice(1), `${ok}0`, ok.toUpperCase(), `${ok.slice(1)}g`, ` ${ok.slice(1)}`, `${ok.slice(1)}\n`]) {
      expect(parseClientId(bad), String(bad)).toBeNull();
    }
  });
});

describe("部屋名", () => {

  it("揃えて部屋を引く字にする。今までの ASCII の名もそのまま通る", () => {
    expect(normalizeRoomName("abc")).toBe("abc");
    expect(normalizeRoomName("Room_1-A")).toBe("room_1-a");
    expect(normalizeRoomName("ＡＢＣ")).toBe("abc");
    expect(normalizeRoomName("ｶﾞ")).toBe("ガ");
    expect(normalizeRoomName(" 花 ")).toBe("花");
    expect(normalizeRoomName("部屋")).toBe("部屋");
  });

  it("長さの境目: 揃えた後の 32 字と、揃える前の 128 B", () => {
    expect(normalizeRoomName("x".repeat(ROOM_NAME_MAX))).toBe("x".repeat(ROOM_NAME_MAX));
    expect(normalizeRoomName("x".repeat(ROOM_NAME_MAX + 1))).toBeNull();
    const four = "\u{20BB7}".repeat(ROOM_NAME_MAX);
    expect(normalizeRoomName(four)).toBe(four);
    expect(normalizeRoomName(`${four}a`)).toBeNull();
    expect(normalizeRoomName(` ${"y".repeat(ROOM_NAME_MAX)} `)).toBe("y".repeat(ROOM_NAME_MAX));
    // 揃える前の 128 B は、揃えると縮む名にも当てる（「ｶﾞ」22 個 = 132 B → 揃えると 22 文字）。
    expect(normalizeRoomName("ｶﾞ".repeat(21))).toBe("ガ".repeat(21)); // 126 B
    expect(normalizeRoomName("ｶﾞ".repeat(22))).toBeNull(); // 132 B
    expect(normalizeRoomName("㍿".repeat(8))).toBe("株式会社".repeat(8));
    expect(normalizeRoomName("㍿".repeat(9))).toBeNull();
    expect(ROOM_NAME_MAX_BYTES).toBe(128);
  });

  it("否定: 空・空白だけ・制御文字・`/`・`.` と `..`・代理の片割れは断る", () => {
    for (const bad of ["", " ", "　", "﻿", "a\u0000b", "a\u007Fb", "a\u0085b", "a/b", ".", "..", "a\uD800b"]) {
      expect(normalizeRoomName(bad), JSON.stringify(bad)).toBeNull();
    }
    // 隣: 同じ文字でも名の一部なら通る（`.` `..` は名そのものだけを断る。`／` は揃えると `/` になるがパスを割らないので通す）。
    for (const good of ["...", ".a", "a.", "a..b", "Ａ／Ｂ"]) {
      expect(normalizeRoomName(good), JSON.stringify(good)).not.toBeNull();
    }
  });

  it("符号化を戻す: 壊れた符号は null。`+` はそのまま。区切りは 384 字まで", () => {
    expect(decodeRoomName("%E8%8A%B1")).toBe("花");
    expect(decodeRoomName("%e8%8a%b1")).toBe("花");
    expect(decodeRoomName("a+b")).toBe("a+b");
    expect(decodeRoomName("a%20b")).toBe("a b");
    expect(decodeRoomName("a%2Fb")).toBe("a/b"); // 戻した後の normalizeRoomName が断る
    for (const bad of ["%", "%4", "%ZZ", "a%2", "%E8%8A", "%ED%A0%80", "%C0%AF", "%FF", "%80"]) {
      expect(decodeRoomName(bad), bad).toBeNull();
    }
    expect(decodeRoomName("%41".repeat(ROOM_NAME_MAX_BYTES))).toBe("A".repeat(ROOM_NAME_MAX_BYTES));
    expect(decodeRoomName(`${"%41".repeat(ROOM_NAME_MAX_BYTES)}A`)).toBeNull();
  });

  it("URL のパスは /v0/rooms/<名> だけ", () => {
    expect(roomNameFromPath("/v0/rooms/abc")).toBe("abc");
    expect(roomNameFromPath("/v0/rooms/")).toBeNull();
    expect(roomNameFromPath("/v0/rooms/abc/")).toBeNull();
    expect(roomNameFromPath("/v0/rooms/abc/join")).toBeNull();
    expect(roomNameFromPath("/v1/rooms/abc")).toBeNull();
    expect(roomNameFromPath("/rooms/abc")).toBeNull();
    expect(roomNameFromPath("/")).toBeNull();
  });
});

describe("文の形", () => {
  const code = "AAECAwQF-_9z";

  it("offer と answer を読む", () => {
    expect(parseClientMessage(JSON.stringify({ t: "offer", code }))).toEqual({ kind: "offer", code });
    expect(parseClientMessage(JSON.stringify({ t: "answer", code }))).toEqual({ kind: "answer", code });
  });

  it("コードはちょうど上限の字数まで通る（上限の隣）", () => {
    const c = "A".repeat(CODE_MAX_CHARS);
    expect(parseClientMessage(JSON.stringify({ t: "offer", code: c }))).toEqual({ kind: "offer", code: c });
  });

  it("否定: 上限を 1 字越えたコードは too_large", () => {
    const c = "A".repeat(CODE_MAX_CHARS + 1);
    expect(parseClientMessage(JSON.stringify({ t: "offer", code: c }))).toEqual({
      kind: "error",
      close: Close.TooLarge,
    });
  });

  it("否定: 文が MESSAGE_MAX_BYTES を越えたら、中身が正しくても too_large", () => {
    const pad = "p".repeat(MESSAGE_MAX_BYTES);
    expect(parseClientMessage(JSON.stringify({ t: "offer", code, pad }))).toEqual({
      kind: "error",
      close: Close.TooLarge,
    });
    const mb = "あ".repeat(Math.ceil(MESSAGE_MAX_BYTES / 3) + 1);
    expect(mb.length).toBeLessThan(MESSAGE_MAX_BYTES);
    expect(parseClientMessage(JSON.stringify({ t: "offer", code, pad: mb }))).toEqual({
      kind: "error",
      close: Close.TooLarge,
    });
  });

  it("否定: base64url でない字・空のコードは bad_code", () => {
    for (const c of ["", "abc=", "ab+c", "ab/c", " abc", "abc\n", "あいう"]) {
      expect(parseClientMessage(JSON.stringify({ t: "offer", code: c })), JSON.stringify(c)).toEqual({
        kind: "error",
        close: Close.BadCode,
      });
    }
  });

  it("否定: JSON でない・形が違う・知らない t・code が文字列でない・バイナリは bad_message", () => {
    const bad: (string | ArrayBuffer)[] = [
      "not json",
      "",
      "[]",
      "null",
      '"offer"',
      JSON.stringify({ code }),
      JSON.stringify({ t: "join", code }),
      JSON.stringify({ t: "ping" }), // ping は自動応答の完全一致だけ。Room へ届いたら誤り
      JSON.stringify({ t: "offer" }),
      JSON.stringify({ t: "offer", code: 123 }),
      JSON.stringify({ t: "offer", code: null }),
      new TextEncoder().encode(JSON.stringify({ t: "offer", code })).buffer as ArrayBuffer,
    ];
    for (const b of bad) {
      expect(parseClientMessage(b), String(b)).toEqual({ kind: "error", close: Close.BadMessage });
    }
  });
});
