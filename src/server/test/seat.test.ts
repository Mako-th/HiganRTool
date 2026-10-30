// 観戦用のルーム（名の先頭`w.`）でhost・guestになる条件の検査。hostになれるのはそのルーム名について正しく期限内のチケットを出した接続だけ。
// チケットは対戦のルームで2人が揃ったときに渡る。guestには誰でもなれる。観戦者が答えずに抜けても居座ってもhostは閉じない。
// 入口（index.ts）→ ルーム（room.ts）をローカルのworkerdの上で本物のWebSocketで叩く。
import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { ANSWER_TTL_MS, Close, WATCH_ANSWER_TTL_MS, newRandomRoomName, spectateRoomName } from "../src/rules";
import {
  Client,
  connect,
  fakeCode,
  freshCid,
  freshRoom,
  peerWithTicket,
  rawConnect,
  roomPath,
  sleep,
  stubFor,
  ticketFor,
} from "./helpers";

/** 観戦用のルームの名とその名のチケット（いま発行）。 */
async function seat(): Promise<{ w: string; ticket: string }> {
  const w = await spectateRoomName(newRandomRoomName());
  return { w, ticket: await ticketFor(w) };
}

/** 入室できる（101）ことと最初のroomの役。 */
async function enter(room: string, ticket?: string, cid?: string): Promise<{ c: Client; role: string }> {
  const res = await rawConnect(roomPath(room, cid, ticket));
  expect(res.status, room).toBe(101);
  const c = new Client(res.webSocket!);
  const m = await c.next();
  expect(m.t).toBe("room");
  return { c, role: m.role };
}

/** 入室できずに断られる（WebSocketは張られない）こと。 */
async function turnedAway(room: string, ticket: string | undefined, status: number, error: string): Promise<void> {
  const res = await rawConnect(roomPath(room, undefined, ticket));
  expect(res.status, `${room} ticket=${String(ticket)}`).toBe(status);
  expect(await res.json()).toEqual({ error });
  expect(res.webSocket).toBeNull();
}

/** 観戦用のルームのhostがofferを登録し終えるまで。 */
async function caster(w: string, ticket: string, code = fakeCode("cast")): Promise<Client> {
  const h = await enter(w, ticket);
  expect(h.role).toBe("host");
  h.c.sendJson({ t: "offer", code });
  await sleep(50);
  return h.c;
}

const nowSecs = (): number => Math.floor(Date.now() / 1000);

describe("観戦用のルームの host の席（札）", () => {
  it("札を出した接続が host になる。札なし・期限切れは 409 room_full、形や MAC が悪い・別の w. の名の札は 400 bad_ticket で、席は空のまま", async () => {
    const { w, ticket } = await seat();
    const other = await spectateRoomName(newRandomRoomName());
    await turnedAway(w, undefined, 409, "room_full");
    await turnedAway(w, await ticketFor(w, nowSecs() - 100, nowSecs() - 1), 409, "room_full");
    await turnedAway(w, await ticketFor(other), 400, "bad_ticket");
    await turnedAway(w, ticket.slice(0, -2) + (ticket.endsWith("A") ? "BA" : "AA"), 400, "bad_ticket");
    await turnedAway(w, "garbage", 400, "bad_ticket");
    await turnedAway(w, "", 400, "bad_ticket");
    // 断られてもhostの枠は空いたまま。正しいチケットならhostになれる。
    const real = await enter(w, ticket);
    expect(real.role).toBe("host");
    real.c.close();
  });

  it("大文字のパスでも同じルームとして札を見る（札は引く形の w. の名に効く）", async () => {
    const { w, ticket } = await seat();
    const h = await enter(w.toUpperCase(), ticket);
    expect(h.role).toBe("host");
    h.c.close();
  });

  it("host がいるルームへ来た接続は、札が無い・同じ発行・古い発行なら guest。形の悪い札は 400", async () => {
    const { w } = await seat();
    const issued = nowSecs();
    const host = await enter(w, await ticketFor(w, issued));
    for (const t of [undefined, await ticketFor(w, issued), await ticketFor(w, issued - 100)]) {
      const g = await enter(w, t);
      expect(g.role, String(t)).toBe("guest");
      g.c.close();
      await g.c.closed;
      await sleep(50);
    }
    await turnedAway(w, "garbage", 400, "bad_ticket");
    expect(host.c.isClosed).toBe(false);
    host.c.close();
  });

  it("より新しい発行の札の host は、前の host（とそのコードを受けた観戦者）を peer_left で閉じて席を取る", async () => {
    const { w } = await seat();
    const issued = nowSecs();
    const squatter = await caster(w, await ticketFor(w, issued - 60), fakeCode("squat"));
    const victim = await enter(w);
    expect(victim.role).toBe("guest");
    expect(await victim.c.next()).toEqual({ t: "peer", code: fakeCode("squat") });
    const real = await enter(w, await ticketFor(w, issued));
    expect(real.role).toBe("host");
    expect(await squatter.next()).toMatchObject({ t: "matched" });
    expect(await squatter.next()).toEqual({ t: "error", code: "peer_left" });
    expect((await squatter.closed).code).toBe(Close.PeerLeft);
    expect(await victim.c.next()).toEqual({ t: "error", code: "peer_left" });
    expect((await victim.c.closed).code).toBe(Close.PeerLeft);
    // 次の観戦者は新しいhostのコードを受ける。
    real.c.sendJson({ t: "offer", code: fakeCode("real") });
    await sleep(50);
    const next = await enter(w);
    expect(next.role).toBe("guest");
    expect(await next.c.next()).toEqual({ t: "peer", code: fakeCode("real") });
    real.c.close();
    next.c.close();
  });

  it("否定: 古い札・同じ札の host は前の host を追い出さない（観戦者が居れば 409）", async () => {
    const { w } = await seat();
    const issued = nowSecs();
    const host = await caster(w, await ticketFor(w, issued));
    const g = await enter(w);
    expect(g.role).toBe("guest");
    await g.c.next();
    for (const t of [await ticketFor(w, issued), await ticketFor(w, issued - 1)]) await turnedAway(w, t, 409, "room_full");
    expect(await host.next()).toMatchObject({ t: "matched" });
    expect(await host.silentFor(100)).toBeNull();
    expect(host.isClosed).toBe(false);
    host.close();
  });

  it("同じ cid の入り直し: 前の host が残っていても、札を出せば新しい host になる。札なしの入り直しは、空いた席を取れない", async () => {
    const { w, ticket } = await seat();
    const cid = freshCid();
    const first = await enter(w, ticket, cid);
    expect(first.role).toBe("host");
    const again = await enter(w, ticket, cid);
    expect(again.role).toBe("host");
    expect((await first.c.closed).code).toBe(Close.Done);
    again.c.close();
    await again.c.closed;
    await sleep(50);
    const res = await rawConnect(roomPath(w, cid));
    expect(res.status).toBe(409);
  });

  it("対戦のルーム（w. でない名・割り当ての名 q.）は札を見ない: 無くても、あっても（でたらめでも）host になれる", async () => {
    const { ticket } = await seat();
    for (const room of [freshRoom("plain"), newRandomRoomName()]) {
      for (const t of [undefined, ticket, "garbage", ""]) {
        const h = await enter(room, t);
        expect(h.role, `${room} ${String(t)}`).toBe("host");
        h.c.close();
        await h.c.closed;
        await sleep(50);
      }
    }
  });
});

describe("対戦のルームで渡る札", () => {
  it("揃うと host は matched、guest は peer で札を受け、それぞれが打った名の観戦用のルームの host になれる（全角と半角は別の名）", async () => {
    const tag = freshRoom("t");
    const hostName = `Ｒｏｏｍ-${tag}`;
    const guestName = `room-${tag}`;
    const h = await connect(hostName);
    expect(await h.next()).toMatchObject({ role: "host" });
    h.sendJson({ t: "offer", code: fakeCode("h") });
    const g = await connect(guestName);
    expect(await g.next()).toMatchObject({ role: "guest" });
    const peer = await g.next();
    expect(peer).toEqual(peerWithTicket(fakeCode("h")));
    const matched = await h.next();
    expect(matched.t).toBe("matched");
    expect(typeof matched.ticket).toBe("string");
    // 2人のチケットは同じ時刻の発行。
    expect(matched.ticket.split(".")[0]).toBe(peer.ticket.split(".")[0]);
    const wHost = await spectateRoomName(hostName);
    const wGuest = await spectateRoomName(guestName);
    expect(wHost).not.toBe(wGuest);
    // 相手の名の観戦用のルームには効かない。
    await turnedAway(wGuest, matched.ticket, 400, "bad_ticket");
    await turnedAway(wHost, peer.ticket, 400, "bad_ticket");
    const ch = await enter(wHost, matched.ticket);
    expect(ch.role).toBe("host");
    const cg = await enter(wGuest, peer.ticket);
    expect(cg.role).toBe("host");
    for (const c of [h, g, ch.c, cg.c]) c.close();
  });

  it("入れ替わった guest（同じ cid の入り直し）も、同じ発行の札を受ける。host へ matched は 2 度送らない", async () => {
    const room = freshRoom();
    const cid = freshCid();
    const h = await connect(room);
    await h.next();
    h.sendJson({ t: "offer", code: fakeCode("h") });
    const ghost = await connect(room, undefined, cid);
    await ghost.next();
    const p1 = await ghost.next();
    const matched = await h.next();
    await sleep(1100); // 秒の粒度を越えるまで待つ（発行し直していれば値が変わる）
    const again = await connect(room, undefined, cid);
    await again.next();
    const p2 = await again.next();
    expect(p2).toEqual(peerWithTicket(fakeCode("h")));
    expect(p2.ticket).toBe(p1.ticket);
    expect(matched.ticket.split(".")[0]).toBe(p2.ticket.split(".")[0]);
    expect(await h.silentFor(100)).toBeNull();
    h.close();
    again.close();
  });

  it("観戦用のルームの成立では札を渡さない（観戦者は札を持たない）", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const g = await enter(w);
    expect(await g.c.next()).toEqual({ t: "peer", code: fakeCode("cast") });
    expect(await host.next()).toEqual({ t: "matched", expires_at: expect.any(Number) });
    host.close();
    g.c.close();
  });
});

/** 観戦用のルームの観戦者（guest）のattachmentの期限だけを過去へずらす。 */
async function ageGuests(room: string): Promise<void> {
  await runInDurableObject(stubFor(room), (_inst, state) => {
    for (const ws of state.getWebSockets()) {
      const a = ws.deserializeAttachment() as { role: string; deadline: number };
      if (a.role !== "guest") continue;
      a.deadline = Date.now() - 1;
      ws.serializeAttachment(a);
    }
  });
}

describe("観戦用のルームの guest の席（答えずに抜ける・居座る観戦者）", () => {
  it("観戦者がコードを受けて答えずに抜けても host は閉じず、次の観戦者が同じコードを受けて答えられる（matched は 2 度送らない）", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const thief = await enter(w);
    expect(await thief.c.next()).toEqual({ t: "peer", code: fakeCode("cast") });
    expect((await host.next()).t).toBe("matched");
    thief.c.close();
    await thief.c.closed;
    await sleep(50);
    expect(await host.silentFor(100)).toBeNull();
    expect(host.isClosed).toBe(false);
    const real = await enter(w);
    expect(real.role).toBe("guest");
    expect(await real.c.next()).toEqual({ t: "peer", code: fakeCode("cast") });
    real.c.sendJson({ t: "answer", code: fakeCode("reply") });
    expect(await host.next()).toEqual({ t: "peer", code: fakeCode("reply") });
    // 観戦用のルームは対戦のルームと違ってanswerの後に2人を1000で閉じる（配信する側は次の観戦者のために入り直す）。
    expect((await host.closed).code).toBe(Close.Done);
    expect((await real.c.closed).code).toBe(Close.Done);
  });

  it("否定: 観戦用のルームに host と観戦者がいるときの 3 人目は、room_in_use でなく今までどおり 409 room_full", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const viewer = await enter(w);
    expect(viewer.role).toBe("guest");
    await turnedAway(w, undefined, 409, "room_full");
    host.close();
    viewer.c.close();
  });

  it("観戦者の誤りの文で観戦者が閉じられても、host は閉じない", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const bad = await enter(w);
    await bad.c.next();
    bad.c.sendJson({ t: "offer", code: fakeCode("x") });
    expect((await bad.c.closed).code).toBe(Close.BadMessage);
    await sleep(50);
    expect((await host.next()).t).toBe("matched");
    expect(await host.silentFor(100)).toBeNull();
    expect(host.isClosed).toBe(false);
    host.close();
  });

  it("観戦者の answer の期限は受けてから WATCH_ANSWER_TTL_MS、host の期限は最初の成立から ANSWER_TTL_MS（matched の expires_at と同じ）", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const before = Date.now();
    const g = await enter(w);
    await g.c.next();
    const matched = await host.next();
    const atts = await runInDurableObject(stubFor(w), (_inst, state) =>
      state.getWebSockets().map((ws) => ws.deserializeAttachment() as { role: string; deadline: number; done: boolean }),
    );
    const gd = atts.find((a) => a.role === "guest" && !a.done)!.deadline;
    const hd = atts.find((a) => a.role === "host")!.deadline;
    expect(gd).toBeGreaterThanOrEqual(before + WATCH_ANSWER_TTL_MS);
    expect(gd).toBeLessThanOrEqual(Date.now() + WATCH_ANSWER_TTL_MS);
    expect(hd).toBeGreaterThanOrEqual(before + ANSWER_TTL_MS);
    expect(hd).toBeLessThanOrEqual(Date.now() + ANSWER_TTL_MS);
    expect(matched.expires_at).toBe(Math.floor(hd / 1000));
    expect(WATCH_ANSWER_TTL_MS).toBeLessThan(ANSWER_TTL_MS);
    host.close();
    g.c.close();
  });

  it("答えずに居座る観戦者は、その期限で expired（4408）になり、host は残る。次の観戦者が同じコードを受ける", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const squatter = await enter(w);
    await squatter.c.next();
    expect((await host.next()).t).toBe("matched");
    // 居座っている間は次の観戦者は409。
    await turnedAway(w, undefined, 409, "room_full");
    await ageGuests(w);
    expect(await runDurableObjectAlarm(stubFor(w))).toBe(true);
    expect(await squatter.c.next()).toEqual({ t: "error", code: "expired" });
    expect((await squatter.c.closed).code).toBe(Close.Expired);
    expect(await host.silentFor(100)).toBeNull();
    expect(host.isClosed).toBe(false);
    const next = await enter(w);
    expect(next.role).toBe("guest");
    expect(await next.c.next()).toEqual({ t: "peer", code: fakeCode("cast") });
    host.close();
    next.c.close();
  });

  it("アラームが遅れても、期限の過ぎた観戦者は次の入室で外れ、来た人は guest としてコードを受ける", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const squatter = await enter(w);
    await squatter.c.next();
    await ageGuests(w);
    const next = await enter(w);
    expect(next.role).toBe("guest");
    expect((await squatter.c.closed).code).toBe(Close.Expired);
    expect(await next.c.next()).toEqual({ t: "peer", code: fakeCode("cast") });
    expect(host.isClosed).toBe(false);
    host.close();
    next.c.close();
  });

  it("host の期限が来れば、観戦者ごと全員 expired（4408）で閉じる", async () => {
    const { w, ticket } = await seat();
    const host = await caster(w, ticket);
    const g = await enter(w);
    await g.c.next();
    expect((await host.next()).t).toBe("matched");
    await runInDurableObject(stubFor(w), (_inst, state) => {
      for (const ws of state.getWebSockets()) {
        const a = ws.deserializeAttachment() as { deadline: number };
        a.deadline = Date.now() - 1;
        ws.serializeAttachment(a);
      }
    });
    expect(await runDurableObjectAlarm(stubFor(w))).toBe(true);
    expect(await host.next()).toEqual({ t: "error", code: "expired" });
    expect((await host.closed).code).toBe(Close.Expired);
    expect((await g.c.closed).code).toBe(Close.Expired);
  });

  it("対照: 対戦のルームでは、コードを受けた guest が抜けると今までどおり host も peer_left で閉じる", async () => {
    const room = freshRoom();
    const h = await connect(room);
    await h.next();
    h.sendJson({ t: "offer", code: fakeCode("m") });
    const g = await connect(room);
    await g.next();
    await g.next();
    expect((await h.next()).t).toBe("matched");
    g.close();
    expect(await h.next()).toEqual({ t: "error", code: "peer_left" });
    expect((await h.closed).code).toBe(Close.PeerLeft);
  });
});
