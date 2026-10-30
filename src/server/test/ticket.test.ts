// チケット（src/ticket.ts）の部品の検査: 作ったチケットが同じ名・同じ鍵・期限内でだけvalidになること。
import { describe, expect, it } from "vitest";
import { SEAT_KEY_MIN_CHARS, TICKET_TTL_SECS, newRandomRoomName, spectateRoomName } from "../src/rules";
import { judgeTicket, mintTicket, seatKey } from "../src/ticket";
import { TICKET_RE } from "./helpers";

const KEY_A = "a".repeat(SEAT_KEY_MIN_CHARS);
const KEY_B = "b".repeat(SEAT_KEY_MIN_CHARS);

async function key(raw: string): Promise<CryptoKey> {
  const k = await seatKey(raw);
  if (k === null) throw new Error("鍵が作れない");
  return k;
}

describe("席の札", () => {
  it("作った札は、同じ名・同じ鍵・期限内で valid（発行の秒が返る）。形は 発行.期限.MAC で、期限は発行から 24 時間", async () => {
    const w = await spectateRoomName(newRandomRoomName());
    const k = await key(KEY_A);
    const t = await mintTicket(k, w, 1_790_000_000);
    expect(t).toMatch(TICKET_RE);
    expect(t.startsWith(`1790000000.${1_790_000_000 + TICKET_TTL_SECS}.`)).toBe(true);
    expect(await judgeTicket(k, w, t, 1_790_000_000)).toEqual({ kind: "valid", issued: 1_790_000_000 });
    expect(await judgeTicket(k, w, t, 1_790_000_000 + TICKET_TTL_SECS - 1)).toEqual({ kind: "valid", issued: 1_790_000_000 });
  });

  it("否定: 期限ちょうどから後は expired（署名は正しい）", async () => {
    const w = await spectateRoomName(newRandomRoomName());
    const k = await key(KEY_A);
    const t = await mintTicket(k, w, 1000, 2000);
    expect(await judgeTicket(k, w, t, 1999)).toEqual({ kind: "valid", issued: 1000 });
    expect(await judgeTicket(k, w, t, 2000)).toEqual({ kind: "expired" });
    expect(await judgeTicket(k, w, t, 99_999)).toEqual({ kind: "expired" });
  });

  it("否定: 別の w. の名・別の鍵で作った札・発行や期限を書き換えた札・MAC を 1 文字変えた札は bad", async () => {
    const w = await spectateRoomName(newRandomRoomName());
    const other = await spectateRoomName(newRandomRoomName());
    const k = await key(KEY_A);
    const t = await mintTicket(k, w, 1000, 2000);
    const [issued, expires, mac] = t.split(".");
    expect(await judgeTicket(k, other, t, 1500)).toEqual({ kind: "bad" });
    expect(await judgeTicket(await key(KEY_B), w, t, 1500)).toEqual({ kind: "bad" });
    expect(await judgeTicket(k, w, `${issued}.9999.${mac}`, 1500)).toEqual({ kind: "bad" });
    expect(await judgeTicket(k, w, `1001.${expires}.${mac}`, 1500)).toEqual({ kind: "bad" });
    const flipped = mac.slice(0, 5) + (mac[5] === "A" ? "B" : "A") + mac.slice(6);
    expect(await judgeTicket(k, w, `${issued}.${expires}.${flipped}`, 1500)).toEqual({ kind: "bad" });
  });

  it("否定: 形の悪い札（区切り・桁・先頭の 0・MAC の長さと文字・最後の文字の使わないビット）は bad", async () => {
    const w = await spectateRoomName(newRandomRoomName());
    const k = await key(KEY_A);
    const t = await mintTicket(k, w, 1000, 2000);
    const [, , mac] = t.split(".");
    // 43文字目の下2ビットは使わない。立てた書き方は同じMACに戻るが1通りの文字だけを通す。
    const last = mac[42];
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    const noisy = mac.slice(0, 42) + alphabet[alphabet.indexOf(last) | 1];
    for (const bad of [
      "",
      "x",
      `1000.2000`,
      `1000.2000.${mac}.x`,
      `01000.2000.${mac}`,
      `1000.2000.${mac.slice(1)}`,
      `1000.2000.${mac}A`,
      `1000.2000.${mac.slice(0, 42)}=`,
      `1000.2000.${mac.slice(0, 42)}+`,
      `-1.2000.${mac}`,
      `1000.2000.${noisy}`,
      `1000000000000.2000.${mac}`,
    ]) {
      expect(await judgeTicket(k, w, bad, 1500), bad).toEqual({ kind: "bad" });
    }
  });

  it("札が無ければ none。鍵が無い（未設定・短すぎる）ときは、形の正しい札も none（検証できない）、形の悪い札は bad", async () => {
    const w = await spectateRoomName(newRandomRoomName());
    const t = await mintTicket(await key(KEY_A), w, 1000, 2000);
    expect(await judgeTicket(await key(KEY_A), w, null, 1500)).toEqual({ kind: "none" });
    expect(seatKey(undefined)).toBeNull();
    expect(seatKey("")).toBeNull();
    expect(seatKey("a".repeat(SEAT_KEY_MIN_CHARS - 1))).toBeNull();
    expect(await judgeTicket(null, w, t, 1500)).toEqual({ kind: "none" });
    expect(await judgeTicket(null, w, "garbage", 1500)).toEqual({ kind: "bad" });
  });
});
