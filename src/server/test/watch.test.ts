// 進行中のランダム対戦の一覧の検査: 入口（index.ts の /v0/watch/<ビルド番号>/post と view）→ 一覧（watch_list.ts）を、
// ローカルの workerd の上で本物の WebSocket で叩く。
import { env } from "cloudflare:workers";
import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import spectateNamesJson from "../../../tests/fixtures/room/spectate_names.json";
import {
  CONNECT_LIMIT_PER_MINUTE,
  Close,
  MESSAGE_MAX_BYTES,
  PING_TEXT,
  WATCH_CAP_MAX,
  WATCH_MAX_PER_IP,
  WATCH_MAX_TOTAL,
  WATCH_STALE_MS,
  newRandomRoomName,
  spectateRoomName,
} from "../src/rules";
import { Client, connectWatch, freshBuild, freshIp, rawConnect, sameSlash64, sleep } from "./helpers";

const W_RE = /^w\.[0-9a-f]{16}$/;

function listStub(build: number): DurableObjectStub {
  return env.WATCH_LIST.get(env.WATCH_LIST.idFromName(`b${build}`));
}

function post(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    t: "post",
    room: newRandomRoomName(),
    p1: "Alice",
    p2: "Bob",
    mode: "rollback",
    delay: 1,
    watchers: 0,
    cap: WATCH_CAP_MAX,
    ...over,
  };
}

/** 載せる口に繋いで post を送り、`listed` を受けるところまで。 */
async function listed(build: number, over: Record<string, unknown> = {}, ip?: string): Promise<{ c: Client; room: string }> {
  const c = await connectWatch(build, "post", ip);
  const m = post(over);
  c.sendJson(m);
  expect(await c.next(), JSON.stringify(over)).toEqual({ t: "listed" });
  return { c, room: m.room as string };
}

/** 見る口に繋ぎ、最初の `list` を返す。 */
async function view(build: number, ip?: string): Promise<{ v: Client; items: any[] }> {
  const v = await connectWatch(build, "view", ip);
  const m = await v.next();
  expect(m.t).toBe("list");
  return { v, items: m.items };
}

/** 載せる口へ送った文が断られる（error ＋ 閉じる番号）こと。 */
async function refused(build: number, msg: unknown, code: number, name: string, ip?: string): Promise<void> {
  const c = await connectWatch(build, "post", ip);
  c.sendJson(msg);
  expect(await c.next(), JSON.stringify(msg)).toEqual({ t: "error", code: name });
  expect((await c.closed).code, JSON.stringify(msg)).toBe(code);
}

describe("載せる", () => {
  it("post が通ると listed が返り、見る口の一覧には観戦用の名 w.… で載る（q.… は載らない）", async () => {
    const build = freshBuild();
    const { c, room } = await listed(build, { p1: "Alice", p2: "Bob", mode: "delay", delay: 2, watchers: 3, cap: 10 });
    const { v, items } = await view(build);
    expect(items).toHaveLength(1);
    expect(items[0]).toEqual({
      room: await spectateRoomName(room),
      p1: "Alice",
      p2: "Bob",
      mode: "delay",
      delay: 2,
      watchers: 3,
      cap: 10,
    });
    expect(items[0].room).toMatch(W_RE);
    expect(JSON.stringify(items)).not.toContain(room);
    expect(JSON.stringify(items)).not.toContain("q.");
    c.close();
    v.close();
  });

  it("観戦用の名は割り当ての名から導く: 見本（Python の hashlib で作ったもの）と全部一致する", async () => {
    const build = freshBuild();
    const v = spectateNamesJson as Record<string, string>;
    const rooms: string[] = [];
    for (let i = 0; v[`q${i}`] !== undefined; i++) {
      await listed(build, { room: v[`q${i}`] }, freshIp());
      rooms.push(v[`w${i}`]);
    }
    const { items } = await view(build);
    expect(items.map((x) => x.room)).toEqual(rooms);
  });

  it("載った順に並ぶ（入れ違いに載せても、載せた順）", async () => {
    const build = freshBuild();
    const names = ["Zed", "Amy", "Max"];
    const clients: Client[] = [];
    for (const n of names) clients.push((await listed(build, { p1: n }, freshIp())).c);
    const { items } = await view(build);
    expect(items.map((x) => x.p1)).toEqual(names);
  });

  it("2 回目の post・載る前の count・載せる口が view の文を送る・上限を越えた cap・知らない t は 4400", async () => {
    const build = freshBuild();
    const { c } = await listed(build);
    c.sendJson(post());
    expect(await c.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await c.closed).code).toBe(Close.BadMessage);
    await refused(build, { t: "count", watchers: 1 }, Close.BadMessage, "bad_message");
    await refused(build, { t: "list" }, Close.BadMessage, "bad_message");
    await refused(build, { t: "offer", code: "AAAA" }, Close.BadMessage, "bad_message");
  });

  it("バイナリの文は 4400・大きすぎる文は 4413", async () => {
    const build = freshBuild();
    const a = await connectWatch(build, "post");
    a.send(new Uint8Array([1, 2, 3]).buffer);
    expect((await a.closed).code).toBe(Close.BadMessage);
    const b = await connectWatch(build, "post");
    b.send(JSON.stringify(post({ pad: "x".repeat(MESSAGE_MAX_BYTES) })));
    expect(await b.next()).toEqual({ t: "error", code: "too_large" });
    expect((await b.closed).code).toBe(Close.TooLarge);
    const c = await connectWatch(build, "post");
    c.send("not json");
    expect((await c.closed).code).toBe(Close.BadMessage);
  });
});

describe("割り当ての名でないものは載せない", () => {
  it("q.＋小文字の 16 進 30 字だけを通す。任意の w.… ・人が打つ名・大文字・前後の空白・長さ違いは 4400 で、一覧に載らない", async () => {
    const build = freshBuild();
    const w = await spectateRoomName(newRandomRoomName());
    const hex = newRandomRoomName().slice(2);
    const bad: unknown[] = [
      w,
      "w." + "0".repeat(16),
      "room-1",
      "q-" + hex,
      "q." + hex.slice(1),
      "q." + hex + "0",
      "Q." + hex,
      "q." + hex.toUpperCase(),
      " q." + hex,
      "q." + hex + "\n",
      "q." + "g".repeat(30),
      "",
      null,
      42,
      ["q." + hex],
    ];
    for (const room of bad) await refused(build, post({ room }), Close.BadMessage, "bad_message");
    const { items } = await view(build);
    expect(items, "断った名が一覧に載った").toEqual([]);
    // 対照: 形が合えば載る（断るガードが全部を断っているだけではない）。
    await listed(build, { room: "q." + hex });
    expect((await view(build)).items).toHaveLength(1);
  });

  it("名前（p1・p2）の文字: 英数字・- _ @ ・置き換えの ? は通り、空・33 字・空白・日本語・記号・文字列でないものは 4400", async () => {
    const build = freshBuild();
    for (const ok of ["a", "Alice-1_@", "?", "?abc?", "A".repeat(32)]) {
      const { c } = await listed(build, { p1: ok, p2: ok }, freshIp());
      c.close();
    }
    for (const name of ["", "A".repeat(33), "a b", " a", "アリス", "a/b", "a.b", "<b>", "a\n", "a\u0000", 1, null]) {
      await refused(build, post({ p1: name }), Close.BadMessage, "bad_message");
      await refused(build, post({ p2: name }), Close.BadMessage, "bad_message");
    }
  });

  it("方式・遅延・観戦者の数と上限: 範囲の外・整数でない・文字列は 4400", async () => {
    const build = freshBuild();
    for (const over of [
      { mode: "auto" },
      { mode: "Rollback" },
      { mode: 1 },
      { delay: -1 },
      { delay: 16 },
      { delay: 1.5 },
      { delay: "1" },
      { cap: 0 },
      { cap: WATCH_CAP_MAX + 1 },
      { cap: 2.5 },
      { watchers: -1 },
      { watchers: 11 },
      { watchers: 3, cap: 2 },
      { watchers: "0" },
    ]) {
      await refused(build, post(over), Close.BadMessage, "bad_message");
    }
    // 端の値は通る。
    const a = await listed(build, { delay: 0, cap: 1, watchers: 1, mode: "delay" }, freshIp());
    const b = await listed(build, { delay: 15, cap: WATCH_CAP_MAX, watchers: WATCH_CAP_MAX }, freshIp());
    expect((await view(build)).items).toHaveLength(2);
    a.c.close();
    b.c.close();
  });
});

describe("数の上限", () => {
  it("1 IP から載せられるのは 2 件まで: 3 件目は list_full（4429）で断り、一覧にも載らない", async () => {
    const build = freshBuild();
    const ip = freshIp();
    await listed(build, {}, ip);
    await listed(build, {}, ip);
    const third = await connectWatch(build, "post", ip);
    third.sendJson(post());
    expect(await third.next()).toEqual({ t: "error", code: "list_full" });
    expect((await third.closed).code).toBe(Close.ListFull);
    expect((await view(build)).items).toHaveLength(WATCH_MAX_PER_IP);
    // 別の IP からは載る。
    await listed(build, {}, freshIp());
    expect((await view(build)).items).toHaveLength(WATCH_MAX_PER_IP + 1);
  });

  it("IPv6 は /64 で数える（同じ /64 の別の端末も同じ 1 IP）", async () => {
    const build = freshBuild();
    const ip = freshIp();
    await listed(build, {}, ip);
    await listed(build, {}, sameSlash64(ip, 2));
    await refused(build, post(), Close.ListFull, "list_full", sameSlash64(ip, 3));
  });

  it("載せていた 1 件が切れれば、同じ IP からまた載せられる", async () => {
    const build = freshBuild();
    const ip = freshIp();
    const a = await listed(build, {}, ip);
    await listed(build, {}, ip);
    a.c.close();
    await a.c.closed;
    await sleep(50);
    await listed(build, {}, ip);
  });

  it("全体は 100 件まで: 101 件目は list_full。ビルド番号が違う一覧とは数えない", async () => {
    const build = freshBuild();
    const open: Client[] = [];
    for (let i = 0; i < WATCH_MAX_TOTAL; i++) open.push((await listed(build, {}, freshIp())).c);
    await refused(build, post(), Close.ListFull, "list_full", freshIp());
    const { items } = await view(build);
    expect(items).toHaveLength(WATCH_MAX_TOTAL);
    await listed(freshBuild(), {}, freshIp());
    for (const c of open) c.close();
  }, 30_000);

  it("載せる前に繋ぎっぱなしの口は件数に入らない（載せていない接続を 3 つ開いても、2 件は載る）", async () => {
    const build = freshBuild();
    const ip = freshIp();
    const idle = [await connectWatch(build, "post", ip), await connectWatch(build, "post", ip), await connectWatch(build, "post", ip)];
    await listed(build, {}, ip);
    await listed(build, {}, ip);
    for (const c of idle) c.close();
  });

  it("同じ部屋: 別の IP からの同じ名は断り、同じ IP からの載せ直しは前の接続を 1000 で閉じて置き換える（一覧は 1 件のまま）", async () => {
    const build = freshBuild();
    const room = newRandomRoomName();
    const ip = freshIp();
    const first = await listed(build, { room, p1: "Old" }, ip);
    await refused(build, post({ room, p1: "Other" }), Close.BadMessage, "bad_message", freshIp());
    expect((await view(build)).items.map((x) => x.p1)).toEqual(["Old"]);
    const again = await listed(build, { room, p1: "New" }, ip);
    expect((await first.c.closed).code).toBe(Close.Done);
    const { items } = await view(build);
    expect(items.map((x) => x.p1)).toEqual(["New"]);
    again.c.close();
  });
});

describe("見る口", () => {
  it("開いたときに一覧の全体が届き、載る・数が変わる・切れるたびに全体が届く", async () => {
    const build = freshBuild();
    const { v, items } = await view(build);
    expect(items).toEqual([]);
    const host = await listed(build, { watchers: 0 });
    const afterPost = await v.next();
    expect(afterPost.t).toBe("list");
    expect(afterPost.items).toHaveLength(1);
    expect(afterPost.items[0].watchers).toBe(0);
    // 数の変化: 前の送りから 1 秒以内なので、まとめて遅れて届く（待たずにアラームを回す）。
    host.c.sendJson({ t: "count", watchers: 4 });
    await sleep(100);
    expect(await v.silentFor(0), "1 秒以内の変化がすぐ届いた").toBeNull();
    expect(await runDurableObjectAlarm(listStub(build))).toBe(true);
    expect((await v.next()).items[0].watchers).toBe(4);
    // 切れる: 一覧から消える。
    host.c.close();
    await host.c.closed;
    const gone = await v.next();
    expect(gone).toEqual({ t: "list", items: [] });
  });

  it("まとめて遅れて届く変化は、アラームを回さなくても 1 秒ほどで自然に届く", async () => {
    const build = freshBuild();
    const { v } = await view(build);
    const host = await listed(build);
    await v.next();
    host.c.sendJson({ t: "count", watchers: 2 });
    const m = await v.next(3000);
    expect(m.items[0].watchers).toBe(2);
  });

  it("同じ数の count では何も送らない・範囲外の count は 4400 で閉じて一覧から消える", async () => {
    const build = freshBuild();
    const { v } = await view(build);
    const host = await listed(build, { watchers: 2, cap: 5 });
    await v.next();
    host.c.sendJson({ t: "count", watchers: 2 });
    expect(await v.silentFor(150)).toBeNull();
    host.c.sendJson({ t: "count", watchers: 6 });
    expect(await host.c.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await host.c.closed).code).toBe(Close.BadMessage);
    expect(await v.next()).toEqual({ t: "list", items: [] });
  });

  it("ビルド番号が違う一覧は混ざらない", async () => {
    const x = freshBuild();
    const y = freshBuild();
    const a = await listed(x, { p1: "OnX" });
    const b = await listed(y, { p1: "OnY" });
    const vx = await view(x);
    const vy = await view(y);
    expect(vx.items.map((i) => i.p1)).toEqual(["OnX"]);
    expect(vy.items.map((i) => i.p1)).toEqual(["OnY"]);
    // x の変化は y の観戦者へ届かない。
    a.c.close();
    expect(await vx.v.next()).toEqual({ t: "list", items: [] });
    expect(await vy.v.silentFor(150)).toBeNull();
    b.c.close();
  });

  it("見る口が ping 以外を送ると 4400 で閉じる。PING には PONG が返る", async () => {
    const build = freshBuild();
    const { v } = await view(build);
    v.send(PING_TEXT);
    expect(await v.next()).toEqual({ t: "pong" });
    v.sendJson({ t: "post" });
    expect(await v.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await v.closed).code).toBe(Close.BadMessage);
  });

  it("観戦者が何人いても一覧は同じ全体で、観戦者が抜けても載せた人には影響しない", async () => {
    const build = freshBuild();
    const host = await listed(build);
    const vs = [await view(build), await view(build), await view(build)];
    for (const x of vs) expect(x.items).toHaveLength(1);
    vs[0].v.close();
    await vs[0].v.closed;
    await sleep(50);
    expect((await view(build)).items).toHaveLength(1);
    expect(host.c.isClosed).toBe(false);
  });
});

describe("期限（載せた人が生きているか）", () => {
  async function forceAtPast(build: number, field: "at" | "created", ms: number): Promise<void> {
    await runInDurableObject(listStub(build), (_inst, state) => {
      for (const ws of state.getWebSockets()) {
        const a = ws.deserializeAttachment() as Record<string, number>;
        a[field] = Date.now() - ms;
        ws.serializeAttachment(a);
      }
    });
  }

  it("最後の文から WATCH_STALE_MS を越えた載せる口は、アラームで 4408 になり一覧から消える。観戦者へも届く", async () => {
    const build = freshBuild();
    const host = await listed(build);
    const { v } = await view(build);
    await forceAtPast(build, "at", WATCH_STALE_MS + 1000);
    expect(await runDurableObjectAlarm(listStub(build))).toBe(true);
    expect(await host.c.next()).toEqual({ t: "error", code: "expired" });
    expect((await host.c.closed).code).toBe(Close.Expired);
    expect(await v.next()).toEqual({ t: "list", items: [] });
  });

  it("否定: 期限の前にアラームが走っても（古いアラーム）消さない", async () => {
    const build = freshBuild();
    const host = await listed(build);
    const { v, items } = await view(build);
    expect(items).toHaveLength(1);
    await runDurableObjectAlarm(listStub(build));
    expect(await host.c.silentFor(100)).toBeNull();
    expect(host.c.isClosed).toBe(false);
    expect(await v.silentFor(50)).toBeNull();
  });

  it("繋いだまま post を送らない口は、締め切りを過ぎるとアラームで 4408 になる。載せ終えた口は影響を受けない", async () => {
    const build = freshBuild();
    const host = await listed(build);
    const idle = await connectWatch(build, "post");
    await runInDurableObject(listStub(build), (_inst, state) => {
      for (const ws of state.getWebSockets()) {
        const a = ws.deserializeAttachment() as { item: unknown; created: number };
        if (a.item === null) {
          a.created = Date.now() - 60_000;
          ws.serializeAttachment(a);
        }
      }
    });
    expect(await runDurableObjectAlarm(listStub(build))).toBe(true);
    expect(await idle.next()).toEqual({ t: "error", code: "expired" });
    expect((await idle.closed).code).toBe(Close.Expired);
    expect(host.c.isClosed).toBe(false);
  });
});

describe("入口で断るもの", () => {
  it("GET 以外は 405・WebSocket でなければ 426・知らない道は 404・ビルド番号が悪ければ 400", async () => {
    for (const method of ["POST", "PUT", "DELETE"]) {
      const r = await rawConnect("/v0/watch/1/view", { method, upgrade: false });
      expect(r.status, method).toBe(405);
      expect(await r.json(), method).toEqual({ error: "method_not_allowed" });
    }
    const plain = await rawConnect("/v0/watch/1/view", { upgrade: false });
    expect(plain.status).toBe(426);
    expect(await plain.json()).toEqual({ error: "upgrade_required" });
    for (const path of ["/v0/watch/1/other", "/v0/watch/1", "/v0/watch/1/", "/v0/watch/1/view/x", "/v0/watch/"]) {
      const r = await rawConnect(path);
      expect(r.status, path).toBe(404);
    }
    for (const bad of ["01", "-1", "abc", "4294967296", "1.5"]) {
      const r = await rawConnect(`/v0/watch/${bad}/view`);
      expect(r.status, bad).toBe(400);
      expect(await r.json(), bad).toEqual({ error: "bad_build" });
    }
  });

  it("1 IP あたりの回数は待ち列・ルームと同じ数えに入る: 見る口を上限回つなぐと 429、ルームへも 429", async () => {
    const ip = freshIp();
    const opened: Client[] = [];
    for (let i = 0; i < CONNECT_LIMIT_PER_MINUTE; i++) {
      const res = await rawConnect(`/v0/watch/${freshBuild()}/view`, { ip });
      expect(res.status, `${i} 回目`).toBe(101);
      opened.push(new Client(res.webSocket!));
    }
    const over = await rawConnect(`/v0/watch/${freshBuild()}/view`, { ip });
    expect(over.status).toBe(429);
    expect(await over.json()).toMatchObject({ error: "rate_limited" });
    const room = await rawConnect("/v0/rooms/limit-shared-watch", { ip });
    expect(room.status).toBe(429);
    const post429 = await rawConnect(`/v0/watch/${freshBuild()}/post`, { ip });
    expect(post429.status).toBe(429);
    for (const c of opened) c.close();
  });

  it("一覧の Durable Object はビルド番号ごと", () => {
    const build = freshBuild();
    expect(listStub(build).id.equals(listStub(build).id)).toBe(true);
    expect(listStub(build).id.equals(listStub(build + 1).id)).toBe(false);
  });
});

describe("既存の口は変わらない", () => {
  it("ルームと待ち列は今までどおり（ルームの host / guest・待ち列の queue）", async () => {
    const r = await rawConnect("/v0/rooms/watch-regress");
    expect(r.status).toBe(101);
    expect(await new Client(r.webSocket!).next()).toMatchObject({ t: "room", role: "host" });
    const q = await rawConnect(`/v0/random/${freshBuild()}`);
    expect(q.status).toBe(101);
    expect(await new Client(q.webSocket!).next()).toMatchObject({ t: "queue" });
  });
});
