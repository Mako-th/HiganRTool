// ランダム対戦の検査: 入口（index.ts の /v0/random/<ビルド番号>）→ 待ち列（random_queue.ts）→ 割り当てたルーム（room.ts）を、
// ローカルの workerd の上で本物の WebSocket で叩く。
import { env } from "cloudflare:workers";
import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import {
  CONNECT_LIMIT_PER_MINUTE,
  Close,
  PING_TEXT,
  RANDOM_QUEUE_TTL_MS,
  RANDOM_ROOM_HEX_LEN,
  RANDOM_ROOM_TTL_MS,
  RETRY_AFTER_SECS,
  ROOM_NAME_MAX,
  ROOM_TTL_MS,
  isRandomRoomName,
  newRandomRoomName,
  normalizeRoomName,
  parseBuildSegment,
  roomTtlForPath,
} from "../src/rules";
import {
  Client,
  connect,
  connectRandom,
  fakeCode,
  freshBuild,
  freshCid,
  freshIp,
  rawConnect,
  sleep,
} from "./helpers";

const ROOM_RE = /^q\.[0-9a-f]{30}$/;

function queueStub(build: number): DurableObjectStub {
  return env.RANDOM_QUEUE.get(env.RANDOM_QUEUE.idFromName(`b${build}`));
}

/** 待ち列に入って「queue」を受けるところまで。 */
async function join(build: number, cid?: string, ip?: string): Promise<{ c: Client; queue: any }> {
  const c = await connectRandom(build, ip, cid);
  const queue = await c.next();
  expect(queue.t).toBe("queue");
  return { c, queue };
}

async function forceQueueDeadlinePast(build: number): Promise<void> {
  await runInDurableObject(queueStub(build), (_inst, state) => {
    for (const ws of state.getWebSockets()) {
      const a = ws.deserializeAttachment() as { deadline: number };
      a.deadline = Date.now() - 1;
      ws.serializeAttachment(a);
    }
  });
}

describe("2 人を組にする", () => {
  it("1 人目は待ち、2 人目が来ると同じルーム名と先後が 2 人へ返り、両方が 1000 で閉じる", async () => {
    const build = freshBuild();
    const a = await connectRandom(build);
    const qa = await a.next();
    expect(qa.t).toBe("queue");
    // 期限は UNIX 秒。いまから 10 分先の前後。
    const nowSecs = Math.floor(Date.now() / 1000);
    expect(qa.expires_at).toBeGreaterThanOrEqual(nowSecs + RANDOM_QUEUE_TTL_MS / 1000 - 10);
    expect(qa.expires_at).toBeLessThanOrEqual(nowSecs + RANDOM_QUEUE_TTL_MS / 1000 + 10);
    expect(await a.silentFor(100)).toBeNull();

    const b = await connectRandom(build);
    expect((await b.next()).t).toBe("queue");
    const pb = await b.next();
    const pa = await a.next();
    expect(pa.t).toBe("paired");
    expect(pb.t).toBe("paired");
    expect(pa.room).toBe(pb.room);
    expect(pa.order).toBe("first");
    expect(pb.order).toBe("second");
    expect(pa.room).toMatch(ROOM_RE);
    expect((await a.closed).code).toBe(Close.Done);
    expect((await b.closed).code).toBe(Close.Done);
  });

  it("組になった 2 人は返されたルーム名で今のルームに入り、コードを渡し合える（先に入った方が host）", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    const { c: b } = await join(build);
    const { room } = await a.next();
    expect((await b.next()).room).toBe(room);

    const ha = await connect(room);
    const ra = await ha.next();
    expect(ra).toMatchObject({ t: "room", role: "host" });
    // 割り当てたルームの期限は短い（片方が来なくても 10 分待たせない）。
    const nowSecs = Math.floor(Date.now() / 1000);
    expect(ra.expires_at).toBeLessThanOrEqual(nowSecs + RANDOM_ROOM_TTL_MS / 1000 + 5);
    expect(ra.expires_at).toBeGreaterThanOrEqual(nowSecs + RANDOM_ROOM_TTL_MS / 1000 - 5);
    ha.sendJson({ t: "offer", code: fakeCode("rh") });
    const hb = await connect(room);
    expect(await hb.next()).toMatchObject({ t: "room", role: "guest" });
    expect(await hb.next()).toEqual({ t: "peer", code: fakeCode("rh") });
    expect((await ha.next()).t).toBe("matched");
    hb.sendJson({ t: "answer", code: fakeCode("rg") });
    expect(await ha.next()).toEqual({ t: "peer", code: fakeCode("rg") });
    expect((await ha.closed).code).toBe(Close.Done);
    expect((await hb.closed).code).toBe(Close.Done);
  });

  it("先に入った人が first（3 組続けて、入った順のまま）", async () => {
    const build = freshBuild();
    for (let i = 0; i < 3; i++) {
      const { c: a } = await join(build);
      await sleep(5);
      const { c: b } = await join(build);
      expect((await a.next()).order).toBe("first");
      expect((await b.next()).order).toBe("second");
    }
  });

  it("続けて組にしても、組ごとに別のルーム名", async () => {
    const build = freshBuild();
    const rooms = new Set<string>();
    for (let i = 0; i < 6; i++) {
      const { c: a } = await join(build);
      const { c: b } = await join(build);
      const ra = (await a.next()).room;
      expect((await b.next()).room).toBe(ra);
      rooms.add(ra);
    }
    expect(rooms.size).toBe(6);
  });
});

describe("3 人以上・同時", () => {
  it("5 人が同時に来ると 2 組ができ、残る 1 人は待つ。6 人目が来ればその 1 人と組になる", async () => {
    const build = freshBuild();
    const clients = await Promise.all(Array.from({ length: 5 }, () => connectRandom(build)));
    const logs: any[][] = [];
    for (const c of clients) {
      const msgs: any[] = [await c.next()];
      expect(msgs[0].t).toBe("queue");
      const more = await c.silentFor(200);
      if (more !== null) msgs.push(await c.next());
      logs.push(msgs);
    }
    const paired = logs.filter((m) => m.length === 2 && m[1].t === "paired");
    const waiting = logs.filter((m) => m.length === 1);
    // 組になった人には、組の知らせが 1 通だけ（もう一度組にされない）で、閉じられている。
    for (let i = 0; i < clients.length; i++) {
      if (logs[i].length === 2) {
        expect((await clients[i].closed).code).toBe(Close.Done);
        expect(await clients[i].silentFor(30), `${i} 人目に余分な文`).toBeNull();
      }
    }
    expect(paired.length).toBe(4);
    expect(waiting.length).toBe(1);
    // 組ごとに 2 人が同じルーム名で、先後が 1 人ずつ。
    const byRoom = new Map<string, string[]>();
    for (const m of paired) {
      const list = byRoom.get(m[1].room) ?? [];
      list.push(m[1].order);
      byRoom.set(m[1].room, list);
    }
    expect(byRoom.size).toBe(2);
    for (const orders of byRoom.values()) expect(orders.sort()).toEqual(["first", "second"]);

    const idx = logs.findIndex((m) => m.length === 1);
    const last = await connectRandom(build);
    expect((await last.next()).t).toBe("queue");
    const p = await last.next();
    expect(p.t).toBe("paired");
    expect(p.order).toBe("second");
    expect((await clients[idx].next()).order).toBe("first");
    expect(p.room).not.toBe([...byRoom.keys()][0]);
    expect(p.room).not.toBe([...byRoom.keys()][1]);
  });

  it("奇数の 3 人目は組にならない（待ち続け、ほかの組のルーム名も受けない）", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    const { c: b } = await join(build);
    const { c: c3 } = await join(build);
    const pa = await a.next();
    const pb = await b.next();
    expect(pa.t).toBe("paired");
    expect(pb.room).toBe(pa.room);
    // 組になった 2 人は閉じられ、3 人目の入室でもう一度組にされない。
    expect((await a.closed).code).toBe(Close.Done);
    expect((await b.closed).code).toBe(Close.Done);
    expect(await a.silentFor(100)).toBeNull();
    expect(await b.silentFor(0)).toBeNull();
    expect(await c3.silentFor(50)).toBeNull();
    expect(c3.isClosed).toBe(false);
  });
});

describe("待ち列で抜ける", () => {
  it("待っていた人が抜けた後に来た人は、抜けた人と組にならず待つ。その次の人と組になる", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    a.close();
    await a.closed;
    const { c: b } = await join(build);
    expect(await b.silentFor(150)).toBeNull();
    const { c: c3 } = await join(build);
    const pb = await b.next();
    const pc = await c3.next();
    expect(pb).toMatchObject({ t: "paired", order: "first" });
    expect(pc).toMatchObject({ t: "paired", order: "second", room: pb.room });
  });

  it("抜けた後の待ち列に誰もいなければ、来た人はそのまま待つ（空の待ち列に古い人が残らない）", async () => {
    const build = freshBuild();
    for (let i = 0; i < 3; i++) {
      const { c } = await join(build);
      c.close();
      await c.closed;
    }
    const { c: d } = await join(build);
    expect(await d.silentFor(150)).toBeNull();
  });

  it("同じ cid の入り直し: 古い接続は 1000 で閉じ、自分自身とは組にならない", async () => {
    const build = freshBuild();
    const cid = freshCid();
    const { c: old } = await join(build, cid);
    const { c: again } = await join(build, cid);
    expect((await old.closed).code).toBe(Close.Done);
    expect(await again.silentFor(150)).toBeNull();
    const { c: other } = await join(build, freshCid());
    expect(await again.next()).toMatchObject({ t: "paired", order: "first" });
    expect(await other.next()).toMatchObject({ t: "paired", order: "second" });
  });

  it("cid が違う 2 人は組になる（同じ IP でも）", async () => {
    const build = freshBuild();
    const ip = freshIp();
    const { c: a } = await join(build, freshCid(), ip);
    const { c: b } = await join(build, freshCid(), ip);
    expect((await a.next()).t).toBe("paired");
    expect((await b.next()).t).toBe("paired");
  });
});

describe("組にした直後に片方が消えた", () => {
  it("残った側は割り当てられたルームで host として待ち、期限（短い）で 4408 になる。10 分は待たない", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    const { c: b } = await join(build);
    const { room } = await a.next();
    await b.next();
    // b は消える（ルームに入らない）。a だけがルームへ入る。
    const h = await connect(room);
    const r = await h.next();
    expect(r).toMatchObject({ t: "room", role: "host" });
    h.sendJson({ t: "offer", code: fakeCode("lone") });
    expect(await h.silentFor(150)).toBeNull();
    const stub = env.ROOMS.get(env.ROOMS.idFromName(room));
    await runInDurableObject(stub, (_inst, state) => {
      for (const ws of state.getWebSockets()) {
        const att = ws.deserializeAttachment() as { deadline: number };
        att.deadline = Date.now() - 1;
        ws.serializeAttachment(att);
      }
    });
    expect(await runDurableObjectAlarm(stub)).toBe(true);
    expect(await h.next()).toEqual({ t: "error", code: "expired" });
    expect((await h.closed).code).toBe(Close.Expired);
  });

  it("対照: 人が打つ名のルームの期限は 10 分のまま", async () => {
    const res = await rawConnect("/v0/rooms/q-plain-room");
    expect(res.status).toBe(101);
    const c = new Client(res.webSocket!);
    const r = await c.next();
    const nowSecs = Math.floor(Date.now() / 1000);
    expect(r.expires_at).toBeGreaterThanOrEqual(nowSecs + ROOM_TTL_MS / 1000 - 10);
    c.close();
  });

  it("消えた側が待ち列でまだ OPEN に見えても、ルームの期限で戻れる（ルームの期限は割り当てた名だけ短い）", () => {
    const name = newRandomRoomName();
    expect(roomTtlForPath(`/v0/rooms/${encodeURIComponent(name)}`)).toBe(RANDOM_ROOM_TTL_MS);
    expect(roomTtlForPath(`/v0/rooms/${name.toUpperCase()}`)).toBe(RANDOM_ROOM_TTL_MS);
    for (const plain of ["q.short", "room-1", "w.0123456789abcdef", `q.${"0".repeat(29)}`, `q.${"0".repeat(31)}`]) {
      expect(roomTtlForPath(`/v0/rooms/${plain}`), plain).toBe(ROOM_TTL_MS);
    }
    expect(roomTtlForPath("/v0/rooms/")).toBe(ROOM_TTL_MS);
    expect(roomTtlForPath("/v0/random/1")).toBe(ROOM_TTL_MS);
  });
});

describe("待ち列に残ったまま", () => {
  it("期限が来るとアラームで error expired と 4408 で閉じる。次に来た人はその人と組にならない", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    await forceQueueDeadlinePast(build);
    expect(await runDurableObjectAlarm(queueStub(build))).toBe(true);
    expect(await a.next()).toEqual({ t: "error", code: "expired" });
    expect((await a.closed).code).toBe(Close.Expired);
    const { c: b } = await join(build);
    expect(await b.silentFor(150)).toBeNull();
  });

  it("アラームが遅れても、次に来た人の入室で期限切れを片付け、その人は待つ", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    await forceQueueDeadlinePast(build);
    const { c: b } = await join(build);
    expect((await a.closed).code).toBe(Close.Expired);
    expect(await b.silentFor(150)).toBeNull();
    expect(b.isClosed).toBe(false);
  });

  it("否定: 期限の前にアラームが走っても（古いアラーム）閉じない", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    expect(await runDurableObjectAlarm(queueStub(build))).toBe(true);
    expect(await a.silentFor(100)).toBeNull();
    expect(a.isClosed).toBe(false);
  });
});

describe("ビルド番号", () => {
  it("違うビルド番号の人どうしは組にならない。同じビルド番号の人とは組になる", async () => {
    const x = freshBuild();
    const y = freshBuild();
    const { c: a } = await join(x);
    const { c: b } = await join(y);
    expect(await a.silentFor(150)).toBeNull();
    expect(await b.silentFor(0)).toBeNull();
    const { c: a2 } = await join(x);
    expect(await a.next()).toMatchObject({ t: "paired", order: "first" });
    expect(await a2.next()).toMatchObject({ t: "paired", order: "second" });
    expect(await b.silentFor(100)).toBeNull();
  });

  it("番号の書き方: 0 と最大値は通り、先頭の 0・符号・小数・桁あふれ・文字は 400 bad_build", async () => {
    for (const ok of ["0", "1", "403920", "4294967295"]) {
      const res = await rawConnect(`/v0/random/${ok}`);
      expect(res.status, ok).toBe(101);
      new Client(res.webSocket!).close();
    }
    for (const bad of ["", "00", "01", "-1", "+1", "1.5", "4294967296", "99999999999", "abc", "1e3", "0x10", "%31", " 1"]) {
      const res = await rawConnect(`/v0/random/${bad}`);
      expect(res.status, JSON.stringify(bad)).toBe(400);
      expect(await res.json(), JSON.stringify(bad)).toEqual({ error: "bad_build" });
    }
    expect(parseBuildSegment("0")).toBe(0);
    expect(parseBuildSegment("4294967295")).toBe(4294967295);
    expect(parseBuildSegment("4294967296")).toBeNull();
  });
});

describe("入口で断るもの", () => {
  it("GET 以外は 405、WebSocket でなければ 426、/ を含む道は 400（待ち列の Durable Object を起こさない）", async () => {
    for (const method of ["POST", "PUT", "DELETE"]) {
      const post = await rawConnect("/v0/random/1", { method, upgrade: false });
      expect(post.status, method).toBe(405);
      expect(await post.json(), method).toEqual({ error: "method_not_allowed" });
    }
    const plain = await rawConnect("/v0/random/1", { upgrade: false });
    expect(plain.status).toBe(426);
    expect(await plain.json()).toEqual({ error: "upgrade_required" });
    const deep = await rawConnect("/v0/random/1/2");
    expect(deep.status).toBe(400);
  });

  it("1 IP あたりの回数はルームと同じ数えに入る: 待ち列へ上限回つなぐと、ルームへも 429", async () => {
    const ip = freshIp();
    const opened: Client[] = [];
    for (let i = 0; i < CONNECT_LIMIT_PER_MINUTE; i++) {
      const res = await rawConnect(`/v0/random/${freshBuild()}`, { ip });
      expect(res.status, `${i} 回目`).toBe(101);
      opened.push(new Client(res.webSocket!));
    }
    const over = await rawConnect(`/v0/random/${freshBuild()}`, { ip });
    expect(over.status).toBe(429);
    expect(over.headers.get("Retry-After")).toBe(String(RETRY_AFTER_SECS));
    expect(await over.json()).toEqual({ error: "rate_limited", retry_after: RETRY_AFTER_SECS });
    const room = await rawConnect("/v0/rooms/limit-shared", { ip });
    expect(room.status).toBe(429);
    for (const c of opened) c.close();
  });
});

describe("待ち列の中の文", () => {
  it("PING には PONG が返り、待ち列の期限は延びない", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    const deadlines = () =>
      runInDurableObject(queueStub(build), (_inst, state) =>
        state.getWebSockets().map((w) => (w.deserializeAttachment() as { deadline: number }).deadline),
      );
    const before = await deadlines();
    expect(before.length).toBe(1);
    await sleep(20);
    a.send(PING_TEXT);
    expect(await a.next()).toEqual({ t: "pong" });
    expect(await deadlines()).toEqual(before);
  });

  it("待っている人が ping 以外を送ると 4400 で閉じ、その人は組にならない", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    a.sendJson({ t: "offer", code: fakeCode("x") });
    expect(await a.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await a.closed).code).toBe(Close.BadMessage);
    const { c: b } = await join(build);
    expect(await b.silentFor(150)).toBeNull();
  });

  it("バイナリの文も 4400", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    a.send(new Uint8Array([1, 2, 3]).buffer);
    expect((await a.closed).code).toBe(Close.BadMessage);
  });
});

describe("割り当てるルーム名", () => {
  it("形は q. ＋ 16 進 30 字（計 32 字）で、ルーム名の決まりを通り、揃えても変わらない", () => {
    const name = newRandomRoomName();
    expect(name).toMatch(ROOM_RE);
    expect([...name].length).toBe(ROOM_NAME_MAX);
    expect(normalizeRoomName(name)).toBe(name);
    expect(isRandomRoomName(name)).toBe(true);
    expect(name.startsWith("q.")).toBe(true);
  });

  it("乱数 120 bit 分を OS の乱数から引く（15 バイトを 1 回）。0 でも 0xff でも長さは同じ", () => {
    const asked: number[] = [];
    const zero = newRandomRoomName((b) => {
      asked.push(b.length);
      b.fill(0);
    });
    expect(asked).toEqual([RANDOM_ROOM_HEX_LEN / 2]);
    expect(zero).toBe(`q.${"0".repeat(RANDOM_ROOM_HEX_LEN)}`);
    expect(newRandomRoomName((b) => b.fill(0xff))).toBe(`q.${"f".repeat(RANDOM_ROOM_HEX_LEN)}`);
    expect(RANDOM_ROOM_HEX_LEN * 4).toBeGreaterThanOrEqual(120);
  });

  it("引くたびに違う名（2000 個で重複なし）で、ほぼ全部の位置の 16 進の字が変わる", () => {
    const names = Array.from({ length: 2000 }, () => newRandomRoomName());
    expect(new Set(names).size).toBe(2000);
    for (let pos = 2; pos < 32; pos++) {
      expect(new Set(names.map((n) => n[pos])).size, `位置 ${pos}`).toBeGreaterThanOrEqual(12);
    }
  });

  it("人が打つルーム名では作れない: 打てる字（英数字・- _ @）だけの名は割り当ての形にならない", () => {
    expect(isRandomRoomName("room-1")).toBe(false);
    expect(isRandomRoomName("q-" + "a".repeat(30))).toBe(false);
    expect(isRandomRoomName("q" + "a".repeat(31))).toBe(false);
    expect(isRandomRoomName("q." + "a".repeat(29))).toBe(false);
    expect(isRandomRoomName("q." + "g".repeat(30))).toBe(false);
    expect(newRandomRoomName()).toContain(".");
  });

  it("推測では入れない: 返された名の末尾 1 字を変えた名・別の乱数の名は、その組のルームとは別の（空の）ルーム", async () => {
    const build = freshBuild();
    const { c: a } = await join(build);
    const { c: b } = await join(build);
    const { room } = await a.next();
    await b.next();
    const host = await connect(room);
    expect(await host.next()).toMatchObject({ role: "host" });
    const last = room.at(-1) === "0" ? "1" : "0";
    for (const guess of [room.slice(0, -1) + last, newRandomRoomName()]) {
      const g = await connect(guess);
      expect(await g.next(), guess).toMatchObject({ t: "room", role: "host" });
      g.close();
    }
    // 組のルームは 2 人目が来るまで何も起きない。
    expect(await host.silentFor(100)).toBeNull();
    host.close();
  });

  it("待ち列の Durable Object はビルド番号ごと: 同じ番号の道は同じ待ち列", async () => {
    const build = freshBuild();
    expect(queueStub(build).id.equals(queueStub(build).id)).toBe(true);
    expect(queueStub(build).id.equals(queueStub(build + 1).id)).toBe(false);
  });
});
