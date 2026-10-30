// 観戦の断りの検査: 入口（index.ts）→ ルーム（room.ts）をローカルのworkerdの上で本物のWebSocketで叩く。
// 断りは観戦用のルーム（名の先頭`w.`）のhostが送り、来る観戦者（guest）を4403で閉じる。hostは残す。
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { Close, newRandomRoomName, spectateRoomName } from "../src/rules";
import { Client, connect, fakeCode, freshRoom, sleep, stubFor, ticketFor } from "./helpers";

/** 観戦用のルームの名 → その名のチケット（hostになるのに要る）。 */
const tickets = new Map<string, string>();

/** 観戦用のルームの名（割り当ての名から導く。実際の観戦用のルームと同じ形）。 */
async function watchRoom(): Promise<string> {
  const w = await spectateRoomName(newRandomRoomName());
  tickets.set(w, await ticketFor(w));
  return w;
}

/** hostとして入る。観戦用のルームならチケットを出す。 */
function connectHost(room: string): Promise<Client> {
  return connect(room, undefined, undefined, tickets.get(room));
}

/** 観戦用のルームへhostとして入り、断りを送るところまで。断りが処理されるのを少し待つ。 */
async function refusingHost(room: string): Promise<Client> {
  const h = await connectHost(room);
  expect(await h.next()).toMatchObject({ t: "room", role: "host" });
  h.sendJson({ t: "refuse" });
  await sleep(50);
  return h;
}

async function forceDeadlinePast(room: string): Promise<void> {
  await runInDurableObject(stubFor(room), (_inst, state) => {
    for (const ws of state.getWebSockets()) {
      const a = ws.deserializeAttachment() as { deadline: number };
      a.deadline = Date.now() - 1;
      ws.serializeAttachment(a);
    }
  });
}

/** 観戦者として入り、断られるまで（room → error refused → 4403）。 */
async function expectRefusedGuest(room: string): Promise<void> {
  const g = await connect(room);
  expect(await g.next()).toMatchObject({ t: "room", role: "guest" });
  expect(await g.next()).toEqual({ t: "error", code: "refused" });
  expect((await g.closed).code).toBe(Close.Refused);
}

describe("観戦の断り（観戦用のルーム）", () => {
  it("host が断ると、入ってきた観戦者は room の次に refused で 4403 で閉じ、host は開いたまま（期限は通常の 10 分）", async () => {
    const w = await watchRoom();
    const h = await connectHost(w);
    const r = await h.next();
    expect(r).toMatchObject({ t: "room", role: "host" });
    const nowSecs = Math.floor(Date.now() / 1000);
    expect(r.expires_at).toBeGreaterThanOrEqual(nowSecs + 590);
    expect(r.expires_at).toBeLessThanOrEqual(nowSecs + 610);
    h.sendJson({ t: "refuse" });
    await sleep(50);

    await expectRefusedGuest(w);
    // 断ったhostには何も送られず、閉じられない。
    expect(await h.silentFor(100)).toBeNull();
    expect(h.isClosed).toBe(false);
    h.close();
  });

  it("断りの host は何人来ても断り続ける。閉じる途中の観戦者を 3 人目と数えず、409 にならない", async () => {
    const w = await watchRoom();
    const h = await refusingHost(w);
    // 1人目のcloseを待たずに2人目・3人目を接続する（閉じる途中の観戦者が居ても断りが続くこと）。
    const gs = [await connect(w), await connect(w), await connect(w)];
    for (const g of gs) {
      expect(await g.next()).toMatchObject({ t: "room", role: "guest" });
      expect(await g.next()).toEqual({ t: "error", code: "refused" });
    }
    for (const g of gs) expect((await g.closed).code).toBe(Close.Refused);
    expect(h.isClosed).toBe(false);
    await expectRefusedGuest(w);
    h.close();
  });

  it("待っていた観戦者は、host が断った時点でその場で refused で閉じられ、コードは渡らない。host は残る", async () => {
    const w = await watchRoom();
    const h = await connectHost(w);
    await h.next();
    const g = await connect(w);
    expect(await g.next()).toMatchObject({ t: "room", role: "guest" });
    expect(await g.silentFor(100)).toBeNull();
    h.sendJson({ t: "refuse" });
    expect(await g.next()).toEqual({ t: "error", code: "refused" });
    expect((await g.closed).code).toBe(Close.Refused);
    expect(await h.silentFor(100)).toBeNull();
    expect(h.isClosed).toBe(false);
    h.close();
  });

  it("断りの印は attachment にあるので、眠らせて起きても断り続ける（検査のツールが本当に眠らせている対照つき）", async () => {
    const w = await watchRoom();
    const h = await refusingHost(w);
    const stub = stubFor(w);
    await runInDurableObject(stub, (inst) => {
      (inst as unknown as Record<string, unknown>).__mark = 1;
    });
    await evictDurableObject(stub); // 既定はWebSocketを閉じずに眠らせる
    const marked = await runInDurableObject(stub, (inst, state) => ({
      mark: (inst as unknown as Record<string, unknown>).__mark,
      sockets: state.getWebSockets().length,
    }));
    expect(marked.mark).toBeUndefined();
    expect(marked.sockets).toBe(1);

    await expectRefusedGuest(w);
    expect(h.isClosed).toBe(false);
    h.close();
  });

  it("断りの host も、ルームの期限が来ればほかと同じく expired で 4408 で閉じる", async () => {
    const w = await watchRoom();
    const h = await refusingHost(w);
    await forceDeadlinePast(w);
    expect(await runDurableObjectAlarm(stubFor(w))).toBe(true);
    expect(await h.next()).toEqual({ t: "error", code: "expired" });
    expect((await h.closed).code).toBe(Close.Expired);
  });

  it("断りの host が抜けると、ルームは空になり次の人が host になる（残った観戦者は居ない）", async () => {
    const w = await watchRoom();
    const h = await refusingHost(w);
    h.close();
    await h.closed;
    await sleep(50);
    const next = await connectHost(w);
    expect(await next.next()).toMatchObject({ t: "room", role: "host" });
    next.close();
  });
});

describe("観戦の断りの誤り（4400）", () => {
  it("否定: 対戦のルーム（名が w. でない・割り当ての名 q. も）では refuse は誤り。送った host は bad_message で閉じ、ルームは空になる", async () => {
    for (const room of [freshRoom(), newRandomRoomName()]) {
      const h = await connect(room);
      await h.next();
      h.sendJson({ t: "refuse" });
      expect(await h.next(), room).toEqual({ t: "error", code: "bad_message" });
      expect((await h.closed).code, room).toBe(Close.BadMessage);
      const again = await connect(room);
      expect(await again.next(), room).toMatchObject({ t: "room", role: "host" });
      again.close();
    }
  });

  it("否定: offer を送った host の refuse は誤り（断りと offer が両方来ない）", async () => {
    const w = await watchRoom();
    const h = await connectHost(w);
    await h.next();
    h.sendJson({ t: "offer", code: fakeCode("offered") });
    await sleep(50);
    h.sendJson({ t: "refuse" });
    expect(await h.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await h.closed).code).toBe(Close.BadMessage);
  });

  it("否定: 2 回目の refuse は誤り", async () => {
    const w = await watchRoom();
    const h = await refusingHost(w);
    h.sendJson({ t: "refuse" });
    expect(await h.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await h.closed).code).toBe(Close.BadMessage);
  });

  it("否定: 断ったあとの offer は誤り（断った host が後から預けない）", async () => {
    const w = await watchRoom();
    const h = await refusingHost(w);
    h.sendJson({ t: "offer", code: fakeCode("late") });
    expect(await h.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await h.closed).code).toBe(Close.BadMessage);
  });

  it("否定: guest が refuse を送ると誤り。guest だけが閉じ、host は残る", async () => {
    const w = await watchRoom();
    const h = await connectHost(w);
    await h.next();
    const g = await connect(w);
    await g.next();
    g.sendJson({ t: "refuse" });
    expect(await g.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await g.closed).code).toBe(Close.BadMessage);
    expect(await h.silentFor(100)).toBeNull();
    expect(h.isClosed).toBe(false);
    h.close();
  });
});
