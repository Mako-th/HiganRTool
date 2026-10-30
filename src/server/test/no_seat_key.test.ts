// SEAT_KEYが無いWorkerの検査（vitest.config.tsのno-seat-keyの実行環境で走る）。
// 対戦は今までどおり成立させてチケットは載せない。観戦用のルームのhostの席と一覧への投稿だけを断る。
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { Close, SEAT_KEY_MIN_CHARS, WATCH_CAP_MAX, newRandomRoomName, spectateRoomName } from "../src/rules";
import { mintTicket, seatKey } from "../src/ticket";
import { connect, connectWatch, fakeCode, freshBuild, freshRoom, rawConnect, roomPath } from "./helpers";

/** 別の鍵で作った形の正しいチケット。鍵の無いWorkerでは検証できない。 */
async function someTicket(w: string): Promise<string> {
  const key = await seatKey("k".repeat(SEAT_KEY_MIN_CHARS));
  return mintTicket(key!, w, Math.floor(Date.now() / 1000));
}

describe("SEAT_KEY が無いとき", () => {
  it("前提: この実行環境には SEAT_KEY が無い", () => {
    expect(env.SEAT_KEY).toBeUndefined();
  });

  it("対戦は成立し、matched にも peer にも札が載らない", async () => {
    const room = freshRoom("nokey");
    const h = await connect(room);
    expect(await h.next()).toMatchObject({ role: "host" });
    h.sendJson({ t: "offer", code: fakeCode("h") });
    const g = await connect(room);
    expect(await g.next()).toMatchObject({ role: "guest" });
    expect(await g.next()).toEqual({ t: "peer", code: fakeCode("h") });
    expect(await h.next()).toEqual({ t: "matched", expires_at: expect.any(Number) });
    g.sendJson({ t: "answer", code: fakeCode("g") });
    expect(await h.next()).toEqual({ t: "peer", code: fakeCode("g") });
    // 鍵が無くても成立の後の2人は残る。
    expect(await h.silentFor(100)).toBeNull();
    expect(h.isClosed || g.isClosed).toBe(false);
    h.close();
    expect((await g.closed).code).toBe(Close.PeerLeft);
  });

  it("観戦用のルームの host の席は、札が無くても形の正しい札があっても 409 room_full。形の悪い札は 400 bad_ticket", async () => {
    const w = await spectateRoomName(newRandomRoomName());
    for (const [ticket, status, error] of [
      [undefined, 409, "room_full"],
      [await someTicket(w), 409, "room_full"],
      ["garbage", 400, "bad_ticket"],
    ] as const) {
      const res = await rawConnect(roomPath(w, undefined, ticket));
      expect(res.status, String(ticket)).toBe(status);
      expect(await res.json()).toEqual({ error });
      expect(res.webSocket).toBeNull();
    }
  });

  it("一覧への投稿は、形の正しい札があっても 4400 で断る", async () => {
    const room = newRandomRoomName();
    const c = await connectWatch(freshBuild(), "post");
    c.sendJson({
      t: "post",
      room,
      p1: "Alice",
      p2: "Bob",
      mode: "rollback",
      delay: 1,
      watchers: 0,
      cap: WATCH_CAP_MAX,
      ticket: await someTicket(await spectateRoomName(room)),
    });
    expect(await c.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await c.closed).code).toBe(Close.BadMessage);
  });
});
