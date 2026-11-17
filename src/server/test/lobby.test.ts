// ルーム名ごとの一覧の検査: 入口（index.tsの /v0/lobby/<ビルド番号>/<ルーム名>）→ 一覧（lobby.ts）をローカルのworkerdの上で本物のWebSocketで叩く。
import { env } from "cloudflare:workers";
import { runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import {
  CONNECT_LIMIT_PER_MINUTE,
  Close,
  LOBBY_MAX_PER_KEY,
  LOBBY_MAX_TOTAL,
  LOBBY_VIEW_MAX_PER_KEY,
  LOBBY_VIEW_MAX_TOTAL,
  LOBBY_VIEW_TTL_MS,
  MESSAGE_MAX_BYTES,
  PING_TEXT,
  RANDOM_ROOM_TTL_MS,
  ROOM_TTL_MS,
  WATCH_CAP_MAX,
  WATCH_STALE_MS,
  WATCH_VIEW_SWEEP_MS,
  newLobbyRoomName,
  newRandomRoomName,
  parseLobbyPath,
  roomTtlForPath,
  spectateRoomName,
} from "../src/rules";
import {
  Client,
  connect,
  connectLobby,
  fakeCode,
  freshBuild,
  freshIp,
  freshRoom,
  lobbyStub,
  rawConnect,
  sameSlash64,
  sleep,
  ticketFor,
} from "./helpers";

const R_RE = /^r\.[0-9a-f]{30}$/;
const W_RE = /^w\.[0-9a-f]{16}$/;

/** 検査ごとに別の一覧（別のビルド番号・別のルーム名）。 */
function freshLobby(): { build: number; name: string } {
  return { build: freshBuild(), name: freshRoom("lb") };
}
type Lobby = { build: number; name: string };

/** 一覧に接続して最初の`list`まで。 */
async function open(l: Lobby, ip?: string): Promise<{ c: Client; items: any[] }> {
  const c = await connectLobby(l.build, l.name, ip);
  const m = await c.next();
  expect(m.t).toBe("list");
  return { c, items: m.items };
}

/** 接続して`create`を送り、`created`を受けるところまで。 */
async function create(l: Lobby, player = "Alice", ip?: string): Promise<{ c: Client; room: string }> {
  const { c } = await open(l, ip);
  c.sendJson({ t: "create", name: player });
  const m = await c.next();
  expect(m.t, JSON.stringify(m)).toBe("created");
  return { c, room: m.room };
}

/** 試合中の1件の`post`の文。チケットはその割り当ての名から導いた観戦用の名に作る（検査のSEAT_KEY）。 */
async function postMsg(over: Record<string, unknown> = {}): Promise<Record<string, unknown>> {
  const room = "room" in over ? over.room : newLobbyRoomName();
  const ticket = typeof room === "string" ? await ticketFor(await spectateRoomName(room)) : "0.0.x";
  return { t: "post", room, p1: "Alice", p2: "Bob", mode: "rollback", delay: 1, watch: true, watchers: 0, cap: WATCH_CAP_MAX, ticket, ...over };
}

/** 接続して`post`を送り、`listed`を受けるところまで。 */
async function post(l: Lobby, over: Record<string, unknown> = {}, ip?: string): Promise<{ c: Client; room: string }> {
  const { c } = await open(l, ip);
  const m = await postMsg(over);
  c.sendJson(m);
  expect(await c.next(), JSON.stringify(over)).toEqual({ t: "listed" });
  return { c, room: m.room as string };
}

/** 一覧を見る接続の次の`list`（まとめて送る変化は1秒ほどで届く）。 */
async function nextList(v: Client): Promise<any[]> {
  const m = await v.next(3000);
  expect(m.t).toBe("list");
  return m.items;
}

/** 最初の文が断られる（error ＋ 閉じる番号）こと。 */
async function refused(l: Lobby, msg: unknown, code: number, name: string, ip?: string): Promise<void> {
  const { c } = await open(l, ip);
  c.sendJson(msg);
  expect(await c.next(), JSON.stringify(msg)).toEqual({ t: "error", code: name });
  expect((await c.closed).code, JSON.stringify(msg)).toBe(code);
}

describe("見る・待機中を載せる（create）", () => {
  it("開くと一覧の全体が届く。create すると r.＋30 桁の 16 進の名が created で返り、見る接続の一覧に待機中で載る", async () => {
    const l = freshLobby();
    const { c: viewer, items } = await open(l);
    expect(items).toEqual([]);
    const { c, room } = await create(l, "Alice");
    expect(room).toMatch(R_RE);
    expect(await nextList(viewer)).toEqual([{ kind: "wait", name: "Alice", room }]);
    // 後から開いた接続にも最初に同じ一覧が届く。
    expect((await open(l)).items).toEqual([{ kind: "wait", name: "Alice", room }]);
    c.close();
    viewer.close();
  });

  it("create した接続は一覧を受け取らない（載せる側になる）。切れると一覧から消えて見る接続へ全体が届く", async () => {
    const l = freshLobby();
    const { c: viewer } = await open(l);
    const a = await create(l);
    await nextList(viewer);
    const b = await create(l, "Bob", freshIp());
    expect(await viewer.silentFor(0)).toBeNull();
    await runDurableObjectAlarm(lobbyStub(l.build, l.name));
    expect((await nextList(viewer)).map((x) => x.name)).toEqual(["Alice", "Bob"]);
    expect(await a.c.silentFor(100)).toBeNull();
    a.c.close();
    await a.c.closed;
    expect((await nextList(viewer)).map((x) => x.name)).toEqual(["Bob"]);
    b.c.close();
  });

  it("割り当ての名は件ごとに別（推測できない乱数）", async () => {
    const l = freshLobby();
    const a = await create(l, "A", freshIp());
    const b = await create(l, "B", freshIp());
    expect(a.room).not.toBe(b.room);
  });

  it("名前の文字は一覧の名前と同じ決まり: 0x20〜0x7E のうち / ; ` 以外は通り、空・33文字・前後の空白・日本語・文字列でないものは 4400", async () => {
    const l = freshLobby();
    for (const ok of ["a", "A b.c", 'a"b\\c', "A".repeat(32)]) {
      const { c } = await create(l, ok, freshIp());
      c.close();
    }
    for (const name of ["", "A".repeat(33), " a", "a ", "アリス", "a/b", "a;b", "a`b", "a\n", 1, null]) {
      await refused(l, { t: "create", name }, Close.BadMessage, "bad_message", freshIp());
    }
    await refused(l, { t: "create" }, Close.BadMessage, "bad_message", freshIp());
  });

  it("名前の \" と \\ は一覧の JSON でそのまま届き、見る側で元の文字に戻る", async () => {
    const l = freshLobby();
    const { c } = await create(l, 'a"b\\c');
    expect((await open(l)).items[0].name).toBe('a"b\\c');
    c.close();
  });
});

describe("試合中を載せる（post）", () => {
  it("post が通ると listed が返り、一覧には観戦用の名 w.… と watch・観戦者の数・上限・方式・遅延で載る（割り当ての名は載らない）", async () => {
    const l = freshLobby();
    const { c, room } = await post(l, { p1: "Alice", p2: "Bob", mode: "delay", delay: 2, watch: false, watchers: 3, cap: 10 });
    const { items } = await open(l);
    expect(items).toEqual([
      { kind: "play", p1: "Alice", p2: "Bob", room: await spectateRoomName(room), watch: false, watchers: 3, cap: 10, mode: "delay", delay: 2 },
    ]);
    expect(items[0].room).toMatch(W_RE);
    expect(JSON.stringify(items)).not.toContain(room);
    expect(JSON.stringify(items)).not.toContain('"r.');
    c.close();
  });

  it("同じ r.… の待機中の 1 件があれば、その接続を 1000 で閉じて外す（作った人が別の IP でも）。試合中の行は載せた順", async () => {
    const l = freshLobby();
    const { c: viewer } = await open(l);
    const made = await create(l, "Waiter", freshIp());
    await nextList(viewer);
    const other = await create(l, "Other", freshIp());
    const p = await post(l, { room: made.room, p1: "Waiter", p2: "Guest" }, freshIp());
    expect((await made.c.closed).code).toBe(Close.Done);
    expect(other.c.isClosed).toBe(false);
    const { items } = await open(l);
    expect(items.map((x) => x.kind)).toEqual(["wait", "play"]);
    expect(items[0]).toMatchObject({ name: "Other", room: other.room });
    expect(items[1]).toMatchObject({ p1: "Waiter", p2: "Guest" });
    expect(JSON.stringify(items)).not.toContain(made.room);
    p.c.close();
    other.c.close();
  });

  it("並びは待機中（来た順）→ 試合中（来た順）。間に混ざって来ても入れ替わらない", async () => {
    const l = freshLobby();
    const w1 = await create(l, "W1", freshIp());
    const p1 = await post(l, { p1: "P1" }, freshIp());
    const w2 = await create(l, "W2", freshIp());
    const p2 = await post(l, { p1: "P2" }, freshIp());
    const { items } = await open(l);
    expect(items.map((x) => (x.kind === "wait" ? x.name : x.p1))).toEqual(["W1", "W2", "P1", "P2"]);
    for (const x of [w1, p1, w2, p2]) x.c.close();
  });

  it("名前（p1・p2）・方式・遅延・数・watch の決まりの外は 4400 で、一覧に載らない。端の値は通る", async () => {
    const l = freshLobby();
    for (const over of [
      { p1: "" },
      { p2: "a/b" },
      { p1: "アリス" },
      { mode: "auto" },
      { delay: 16 },
      { delay: 1.5 },
      { cap: 0 },
      { cap: WATCH_CAP_MAX + 1 },
      { watchers: 11 },
      { watchers: 3, cap: 2 },
      { watch: "true" },
      { watch: 1 },
      { watch: null },
    ]) {
      await refused(l, await postMsg(over), Close.BadMessage, "bad_message", freshIp());
    }
    const noWatch = await postMsg();
    delete noWatch.watch;
    await refused(l, noWatch, Close.BadMessage, "bad_message", freshIp());
    expect((await open(l)).items, "断った文が一覧に載った").toEqual([]);
    const a = await post(l, { delay: 0, cap: 1, watchers: 1, mode: "delay", watch: false }, freshIp());
    const b = await post(l, { delay: 15, cap: WATCH_CAP_MAX, watchers: WATCH_CAP_MAX }, freshIp());
    expect((await open(l)).items).toHaveLength(2);
    a.c.close();
    b.c.close();
  });

  it("room は r.＋小文字の 16 進 30 字だけ。q.… ・w.… ・人が打つ名・大文字・前後の空白・長さ違いは 4400 で、一覧に載らない", async () => {
    const l = freshLobby();
    const hex = newLobbyRoomName().slice(2);
    const bad: unknown[] = [
      newRandomRoomName(),
      await spectateRoomName(newLobbyRoomName()),
      "room-1",
      "r-" + hex,
      "r." + hex.slice(1),
      "r." + hex + "0",
      "R." + hex,
      "r." + hex.toUpperCase(),
      " r." + hex,
      "r." + hex + "\n",
      "r." + "g".repeat(30),
      "",
      null,
      42,
      ["r." + hex],
    ];
    for (const room of bad) await refused(l, await postMsg({ room }), Close.BadMessage, "bad_message", freshIp());
    expect((await open(l)).items, "断った名が一覧に載った").toEqual([]);
    await post(l, { room: "r." + hex }, freshIp());
    expect((await open(l)).items).toHaveLength(1);
  });

  it("席のチケット: 無い・文字列でない・別の名の札・観戦用の名でなく割り当ての名に作った札・期限切れ・形の悪い札は 4400 で、一覧に載らない", async () => {
    const l = freshLobby();
    const room = newLobbyRoomName();
    const w = await spectateRoomName(room);
    const nowSecs = Math.floor(Date.now() / 1000);
    const valid = await ticketFor(w);
    for (const ticket of [
      undefined,
      null,
      42,
      await ticketFor(await spectateRoomName(newLobbyRoomName())),
      await ticketFor(room),
      await ticketFor(w, nowSecs - 100, nowSecs - 1),
      valid.slice(0, -1),
      `${valid}x`,
      "",
    ]) {
      const m = await postMsg({ room });
      if (ticket === undefined) delete m.ticket;
      else m.ticket = ticket;
      await refused(l, m, Close.BadMessage, "bad_message", freshIp());
    }
    expect((await open(l)).items, "断った札で一覧に載った").toEqual([]);
    await post(l, { room, ticket: valid }, freshIp());
    expect((await open(l)).items).toHaveLength(1);
  });

  it("本物の対戦（r.… の 2 席のルーム）で揃った host が matched で、guest が peer で受けた札のどちらでも載せられる", async () => {
    const l = freshLobby();
    const made = await create(l, "Waiter");
    const ha = await connect(made.room);
    await ha.next();
    ha.sendJson({ t: "offer", code: fakeCode("host") });
    const hb = await connect(made.room);
    await hb.next();
    const peer = await hb.next();
    const matched = await ha.next();
    expect(matched.t).toBe("matched");
    const one = await post(l, { room: made.room, ticket: matched.ticket }, freshIp());
    expect((await made.c.closed).code).toBe(Close.Done);
    one.c.close();
    await one.c.closed;
    await sleep(50);
    const two = await post(l, { room: made.room, ticket: peer.ticket }, freshIp());
    expect((await open(l)).items.map((x) => x.room)).toEqual([await spectateRoomName(made.room)]);
    two.c.close();
    ha.close();
    hb.close();
  });

  it("同じ試合の載せ直し: 同じ鍵からは前の接続を 1000 で閉じて置き換え（1 件のまま）、別の鍵からは 4400 で断る", async () => {
    const l = freshLobby();
    const room = newLobbyRoomName();
    const ip = freshIp();
    const first = await post(l, { room, p1: "Old" }, ip);
    await refused(l, await postMsg({ room, p1: "Other" }), Close.BadMessage, "bad_message", freshIp());
    expect((await open(l)).items.map((x) => x.p1)).toEqual(["Old"]);
    const again = await post(l, { room, p1: "New" }, ip);
    expect((await first.c.closed).code).toBe(Close.Done);
    expect((await open(l)).items.map((x) => x.p1)).toEqual(["New"]);
    again.c.close();
  });
});

describe("文の順番", () => {
  it("1 通目は create か post のどちらか 1 通だけ。2 通目の create / post・待機中の count・載せる前の count・知らない t は 4400", async () => {
    const l = freshLobby();
    const a = await create(l, "A", freshIp());
    a.c.sendJson({ t: "create", name: "B" });
    expect(await a.c.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await a.c.closed).code).toBe(Close.BadMessage);

    const b = await create(l, "B", freshIp());
    b.c.sendJson({ t: "count", watchers: 1 });
    expect((await b.c.closed).code).toBe(Close.BadMessage);

    const c = await create(l, "C", freshIp());
    c.c.sendJson(await postMsg());
    expect((await c.c.closed).code).toBe(Close.BadMessage);

    const d = await post(l, {}, freshIp());
    d.c.sendJson(await postMsg());
    expect(await d.c.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await d.c.closed).code).toBe(Close.BadMessage);
    const d2 = await post(l, {}, freshIp());
    d2.c.sendJson({ t: "create", name: "X" });
    expect((await d2.c.closed).code).toBe(Close.BadMessage);

    await refused(l, { t: "count", watchers: 1 }, Close.BadMessage, "bad_message", freshIp());
    await refused(l, { t: "list" }, Close.BadMessage, "bad_message", freshIp());
    await refused(l, { t: "offer", code: "AAAA" }, Close.BadMessage, "bad_message", freshIp());
    // 断った接続の件は一覧に残らない。
    expect((await open(l)).items).toEqual([]);
  });

  it("バイナリの文は 4400・大きすぎる文は 4413・JSON でない文は 4400。何も送らない接続は見るだけで残る", async () => {
    const l = freshLobby();
    const a = (await open(l, freshIp())).c;
    a.send(new Uint8Array([1, 2, 3]).buffer);
    expect((await a.closed).code).toBe(Close.BadMessage);
    const b = (await open(l, freshIp())).c;
    b.send(JSON.stringify(await postMsg({ pad: "x".repeat(MESSAGE_MAX_BYTES) })));
    expect(await b.next()).toEqual({ t: "error", code: "too_large" });
    expect((await b.closed).code).toBe(Close.TooLarge);
    const c = (await open(l, freshIp())).c;
    c.send("not json");
    expect((await c.closed).code).toBe(Close.BadMessage);
    const idle = (await open(l, freshIp())).c;
    await sleep(200);
    expect(idle.isClosed).toBe(false);
  });

  it("PING には PONG が返る", async () => {
    const l = freshLobby();
    const { c } = await open(l);
    c.send(PING_TEXT);
    expect(await c.next()).toEqual({ t: "pong" });
  });
});

describe("count", () => {
  it("観戦者の数が変わると見る接続へ全体が届く。同じ数では何も送らない。範囲外は 4400 で閉じて一覧から消える", async () => {
    const l = freshLobby();
    const { c: viewer } = await open(l);
    const host = await post(l, { watchers: 2, cap: 5 }, freshIp());
    await nextList(viewer);
    host.c.sendJson({ t: "count", watchers: 2 });
    expect(await viewer.silentFor(150)).toBeNull();
    host.c.sendJson({ t: "count", watchers: 4 });
    await sleep(100);
    expect(await runDurableObjectAlarm(lobbyStub(l.build, l.name))).toBe(true);
    expect((await nextList(viewer))[0].watchers).toBe(4);
    host.c.sendJson({ t: "count", watchers: 6 });
    expect(await host.c.next()).toEqual({ t: "error", code: "bad_message" });
    expect((await host.c.closed).code).toBe(Close.BadMessage);
    expect(await nextList(viewer)).toEqual([]);
  });

  it("荒らしの連打（ウィンドウの上限を越えた count）は 4400 で閉じる", async () => {
    const l = freshLobby();
    const host = await post(l, { watchers: 0, cap: 5 }, freshIp());
    for (let i = 0; i < 100; i++) host.c.sendJson({ t: "count", watchers: (i + 1) % 2 });
    expect((await host.c.closed).code).toBe(Close.BadMessage);
  });
});

describe("件数の上限", () => {
  it("同じ鍵から載せられるのは 2 件（待機中と試合中の合計）。3 件目は list_full（4429）。IPv6 は /64 で数える。別の鍵からは載る", async () => {
    const l = freshLobby();
    const ip = freshIp();
    await create(l, "A", ip);
    await post(l, {}, sameSlash64(ip, 2));
    await refused(l, { t: "create", name: "C" }, Close.ListFull, "list_full", sameSlash64(ip, 3));
    await refused(l, await postMsg(), Close.ListFull, "list_full", ip);
    expect((await open(l)).items).toHaveLength(LOBBY_MAX_PER_KEY);
    await create(l, "D", freshIp());
    expect((await open(l)).items).toHaveLength(LOBBY_MAX_PER_KEY + 1);
  });

  it("載せていた 1 件が切れれば、同じ鍵からまた載せられる", async () => {
    const l = freshLobby();
    const ip = freshIp();
    const a = await create(l, "A", ip);
    await create(l, "B", ip);
    a.c.close();
    await a.c.closed;
    await sleep(50);
    await create(l, "C", ip);
  });

  it("待機中を外して載せる post は同じ鍵が 2 件でも通る（外れた分は数えない）。外れない post は list_full", async () => {
    const l = freshLobby();
    const ip = freshIp();
    const a = await create(l, "A", ip);
    await create(l, "B", ip);
    await post(l, { room: a.room }, ip);
    expect((await a.c.closed).code).toBe(Close.Done);
    expect((await open(l)).items.map((x) => x.kind)).toEqual(["wait", "play"]);
    await refused(l, await postMsg(), Close.ListFull, "list_full", ip);
  });

  it("1 つの一覧は 20 件まで: 21 件目は list_full。待機中を外す post は 20 件でも通る。ほかの一覧とは数えない", async () => {
    const l = freshLobby();
    const open20: { c: Client; room: string }[] = [];
    for (let i = 0; i < LOBBY_MAX_TOTAL; i++) open20.push(await create(l, `P${i}`, freshIp()));
    await refused(l, { t: "create", name: "X" }, Close.ListFull, "list_full", freshIp());
    await refused(l, await postMsg(), Close.ListFull, "list_full", freshIp());
    expect((await open(l)).items).toHaveLength(LOBBY_MAX_TOTAL);
    const swapped = await post(l, { room: open20[0].room }, freshIp());
    expect((await open20[0].c.closed).code).toBe(Close.Done);
    expect((await open(l)).items).toHaveLength(LOBBY_MAX_TOTAL);
    await create({ build: l.build, name: freshRoom("lb") }, "other", freshIp());
    await create({ build: freshBuild(), name: l.name }, "other", freshIp());
    for (const x of open20) x.c.close();
    swapped.c.close();
  }, 30_000);

  it("載せない見る接続は件数に入らない（見る接続だけを 3 本開いても 2 件載る）", async () => {
    const l = freshLobby();
    const ip = freshIp();
    const idle = [await open(l, ip), await open(l, ip), await open(l, ip)];
    await create(l, "A", ip);
    await create(l, "B", ip);
    for (const x of idle) x.c.close();
  });
});

describe("見る接続の数と寿命", () => {
  async function ageViews(l: Lobby, ms: number): Promise<void> {
    await runInDurableObject(lobbyStub(l.build, l.name), (_inst, state) => {
      for (const ws of state.getWebSockets()) {
        const a = ws.deserializeAttachment() as { kind: string; created: number };
        if (a.kind === "view") {
          a.created = Date.now() - ms;
          ws.serializeAttachment(a);
        }
      }
    });
  }

  it("決まりの値: 一覧 20 件・同じ鍵 2 件・見る接続 50 本・同じ鍵 3 本・寿命 10 分", () => {
    expect([LOBBY_MAX_TOTAL, LOBBY_MAX_PER_KEY, LOBBY_VIEW_MAX_TOTAL, LOBBY_VIEW_MAX_PER_KEY, LOBBY_VIEW_TTL_MS]).toEqual([20, 2, 50, 3, 600_000]);
  });

  it("同じ鍵（IPv6 は /64）の 4 本目は、その鍵の一番古い接続を 1000 で閉じて入る。ほかの鍵の接続は閉じない", async () => {
    const l = freshLobby();
    const ip = freshIp();
    const other = (await open(l, freshIp())).c;
    const mine: Client[] = [];
    for (let i = 0; i < LOBBY_VIEW_MAX_PER_KEY; i++) {
      mine.push((await open(l, sameSlash64(ip, i + 1))).c);
      await sleep(5);
    }
    const fourth = (await open(l, sameSlash64(ip, 9))).c;
    expect((await mine[0].closed).code).toBe(Close.Done);
    await sleep(50);
    expect(mine[1].isClosed).toBe(false);
    expect(mine[2].isClosed).toBe(false);
    expect(other.isClosed).toBe(false);
    expect(fourth.isClosed).toBe(false);
    for (const c of [other, mine[1], mine[2], fourth]) c.close();
  });

  it("載せた接続は見る接続の本数に数えない（待機中になった接続のあとも、同じ鍵が 3 本開ける）", async () => {
    const l = freshLobby();
    const ip = freshIp();
    const w = await create(l, "A", ip);
    const views = [(await open(l, ip)).c, (await open(l, ip)).c, (await open(l, ip)).c];
    await sleep(50);
    for (const v of views) expect(v.isClosed).toBe(false);
    expect(w.c.isClosed).toBe(false);
    for (const v of views) v.close();
    w.c.close();
  });

  it("全体は 50 本まで: 51 本目は 409 room_full で WebSocket を張らずに断る。載せた接続は数えない。空きができれば入れる", async () => {
    const l = freshLobby();
    const held = (await open(l, freshIp())).c;
    const fake = LOBBY_VIEW_MAX_TOTAL - 1;
    await runInDurableObject(lobbyStub(l.build, l.name), (_inst, state) => {
      for (let i = 0; i < fake; i++) {
        const server = new WebSocketPair()[1];
        state.acceptWebSocket(server);
        server.serializeAttachment({ kind: "view", done: false, key: `fake:${i}`, created: Date.now() });
      }
    });
    const over = await rawConnect(`/v0/lobby/${l.build}/${encodeURIComponent(l.name)}`, { ip: freshIp() });
    expect(over.status).toBe(409);
    expect(await over.json()).toEqual({ error: "room_full" });
    expect(over.webSocket).toBeNull();
    held.close();
    await held.closed;
    await sleep(50);
    const again = (await open(l, freshIp())).c;
    again.close();
  });

  it("作ってから LOBBY_VIEW_TTL_MS で、アラームが expired（4408）で閉じる。期限の前のアラームは閉じない。載せた接続は閉じない", async () => {
    const l = freshLobby();
    const host = await post(l, {}, freshIp());
    const v = (await open(l, freshIp())).c;
    await ageViews(l, LOBBY_VIEW_TTL_MS - 60_000);
    await runDurableObjectAlarm(lobbyStub(l.build, l.name));
    await sleep(100);
    expect(v.isClosed).toBe(false);
    await ageViews(l, LOBBY_VIEW_TTL_MS + WATCH_VIEW_SWEEP_MS);
    expect(await runDurableObjectAlarm(lobbyStub(l.build, l.name))).toBe(true);
    let m = await v.next();
    while (m.t === "list") m = await v.next();
    expect(m).toEqual({ t: "error", code: "expired" });
    expect((await v.closed).code).toBe(Close.Expired);
    expect(host.c.isClosed).toBe(false);
  });
});

describe("期限（載せた人が生きているか）", () => {
  async function forceAtPast(l: Lobby, ms: number): Promise<void> {
    await runInDurableObject(lobbyStub(l.build, l.name), (_inst, state) => {
      for (const ws of state.getWebSockets()) {
        const a = ws.deserializeAttachment() as { kind: string; at?: number };
        if (a.kind !== "view") {
          a.at = Date.now() - ms;
          ws.serializeAttachment(a);
        }
      }
    });
  }

  it("最後の文から WATCH_STALE_MS を越えた待機中・試合中は、アラームで 4408 になり一覧から消える。見る接続へも届く", async () => {
    const l = freshLobby();
    const wait = await create(l, "A", freshIp());
    const play = await post(l, {}, freshIp());
    const { c: viewer } = await open(l);
    await forceAtPast(l, WATCH_STALE_MS + 1000);
    expect(await runDurableObjectAlarm(lobbyStub(l.build, l.name))).toBe(true);
    for (const x of [wait, play]) {
      expect(await x.c.next()).toEqual({ t: "error", code: "expired" });
      expect((await x.c.closed).code).toBe(Close.Expired);
    }
    expect(await nextList(viewer)).toEqual([]);
  });

  it("否定: 期限の前にアラームが走っても（古いアラーム）消さない", async () => {
    const l = freshLobby();
    const wait = await create(l, "A", freshIp());
    await runDurableObjectAlarm(lobbyStub(l.build, l.name));
    expect(await wait.c.silentFor(100)).toBeNull();
    expect(wait.c.isClosed).toBe(false);
    expect((await open(l)).items).toHaveLength(1);
  });
});

describe("一覧はルーム名とビルド番号ごと", () => {
  it("ビルド番号が違う・名前が違う一覧は混ざらない。揃えた名（全角・大文字・前後の空白）が同じなら同じ一覧", async () => {
    const x = freshLobby();
    const a = await create(x, "OnX");
    const otherBuild = await create({ build: freshBuild(), name: x.name }, "OtherBuild");
    const otherName = await create({ build: x.build, name: freshRoom("lb") }, "OtherName");
    expect((await open(x)).items.map((i) => i.name)).toEqual(["OnX"]);
    const alias = await connectLobby(x.build, ` ${x.name.toUpperCase()} `);
    expect((await alias.next()).items.map((i: any) => i.name)).toEqual(["OnX"]);
    for (const c of [a.c, otherBuild.c, otherName.c, alias]) c.close();
  });

  it("一覧の Durable Object は b<ビルド番号>:<揃えた名> ごと", () => {
    const l = freshLobby();
    expect(lobbyStub(l.build, l.name).id.equals(lobbyStub(l.build, l.name.toUpperCase()).id)).toBe(true);
    expect(lobbyStub(l.build, l.name).id.equals(lobbyStub(l.build + 1, l.name).id)).toBe(false);
    expect(lobbyStub(l.build, l.name).id.equals(lobbyStub(l.build, `${l.name}x`).id)).toBe(false);
    expect(lobbyStub(l.build, l.name).id.equals(env.LOBBY.idFromName(`b${l.build}:${l.name}`))).toBe(true);
  });
});

describe("入口で断るもの", () => {
  const path = (b: string | number, name: string) => `/v0/lobby/${b}/${encodeURIComponent(name)}`;

  it("GET 以外は 405・WebSocket でなければ 426・道の形が違えば 404・ビルド番号が悪ければ 400 bad_build", async () => {
    for (const method of ["POST", "PUT", "DELETE"]) {
      const r = await rawConnect(path(1, "a"), { method, upgrade: false });
      expect(r.status, method).toBe(405);
      expect(await r.json(), method).toEqual({ error: "method_not_allowed" });
    }
    const plain = await rawConnect(path(1, "a"), { upgrade: false });
    expect(plain.status).toBe(426);
    expect(await plain.json()).toEqual({ error: "upgrade_required" });
    for (const p of ["/v0/lobby/", "/v0/lobby/1", "/v0/lobby/1/", "/v0/lobby/1/a/b", "/v0/lobby//a"]) {
      const r = await rawConnect(p);
      expect(r.status, p).toBe(p === "/v0/lobby//a" ? 400 : 404);
    }
    for (const bad of ["01", "-1", "abc", "4294967296", "1.5"]) {
      const r = await rawConnect(path(bad, "a"));
      expect(r.status, bad).toBe(400);
      expect(await r.json(), bad).toEqual({ error: "bad_build" });
    }
  });

  it("ルーム名: 揃えた名が w. q. r. で始まるものは 400 bad_room_name（全角・大文字・前後の空白を含む）。先頭でないだけなら通る", async () => {
    const reject = ["w.abc", "q.abc", "r.abc", "R.ABC", "Ｒ．ａｂｃ", " r.abc", "　q.x", "W."];
    for (const name of reject) {
      const r = await rawConnect(path(1, name));
      expect(r.status, name).toBe(400);
      expect(await r.json(), name).toEqual({ error: "bad_room_name" });
    }
    for (const name of ["r", "rr.abc", "ar.abc", "r-abc", "q", "x.r.y"]) {
      const r = await rawConnect(path(freshBuild(), name));
      expect(r.status, name).toBe(101);
      new Client(r.webSocket!).close();
    }
  });

  it("ルーム名の決まりの外（/・制御文字・長すぎ・符号化の誤り）は 400 bad_room_name。. と .. はURLの読み方で道から消えるので 404", async () => {
    const tooLong = "a".repeat(33);
    for (const seg of [".", ".."]) expect((await rawConnect(`/v0/lobby/1/${seg}`)).status, seg).toBe(404);
    for (const seg of ["a%2Fb", "a%00b", encodeURIComponent(tooLong), "%E3%81", "%"]) {
      const r = await rawConnect(`/v0/lobby/1/${seg}`);
      expect(r.status, seg).toBe(400);
      expect(await r.json(), seg).toEqual({ error: "bad_room_name" });
    }
  });

  it("1 IP あたりの回数は待ち列・ルーム・ほかの一覧と同じ数えに入る: 一覧を上限回つなぐと 429、ルームへも 429", async () => {
    const ip = freshIp();
    const opened: Client[] = [];
    for (let i = 0; i < CONNECT_LIMIT_PER_MINUTE; i++) {
      const res = await rawConnect(path(freshBuild(), freshRoom("lb")), { ip });
      expect(res.status, `${i} 回目`).toBe(101);
      opened.push(new Client(res.webSocket!));
    }
    const over = await rawConnect(path(freshBuild(), freshRoom("lb")), { ip });
    expect(over.status).toBe(429);
    expect(await over.json()).toMatchObject({ error: "rate_limited" });
    expect((await rawConnect("/v0/rooms/limit-shared-lobby", { ip })).status).toBe(429);
    for (const c of opened) c.close();
  });
});

describe("r.… の 2 席のルーム", () => {
  it("人が打つ名と同じ期限（ROOM_TTL_MS）。q.… だけが短い", () => {
    expect(roomTtlForPath(`/v0/rooms/${newLobbyRoomName()}`)).toBe(ROOM_TTL_MS);
    expect(roomTtlForPath(`/v0/rooms/${newRandomRoomName()}`)).toBe(RANDOM_ROOM_TTL_MS);
    expect(RANDOM_ROOM_TTL_MS).toBeLessThan(ROOM_TTL_MS);
  });

  it("作った人が待ち、来た人が guest になる（3 人目は room_in_use）。今までの 2 席の手順のまま", async () => {
    const l = freshLobby();
    const made = await create(l, "A");
    const host = await connect(made.room);
    expect(await host.next()).toMatchObject({ t: "room", role: "host" });
    const guest = await connect(made.room);
    expect(await guest.next()).toMatchObject({ t: "room", role: "guest" });
    const third = await rawConnect(`/v0/rooms/${made.room}`);
    expect(third.status).toBe(409);
    expect(await third.json()).toEqual({ error: "room_in_use" });
    host.close();
    guest.close();
    made.c.close();
  });
});

describe("parseLobbyPath（入口と同じ読み方）", () => {
  it("組み立て・ビルド番号・名前の決まりを分けて返し、名は揃えた形", () => {
    expect(parseLobbyPath("/v0/lobby/403920/Room%20A")).toEqual({ kind: "ok", build: 403920, name: "room a" });
    expect(parseLobbyPath("/v0/lobby/0/%EF%BC%A1")).toEqual({ kind: "ok", build: 0, name: "a" });
    expect(parseLobbyPath("/v0/lobby/403920")).toEqual({ kind: "not_found" });
    expect(parseLobbyPath("/v0/rooms/a")).toEqual({ kind: "not_found" });
    expect(parseLobbyPath("/v0/lobby/x/a")).toEqual({ kind: "bad_build" });
    expect(parseLobbyPath("/v0/lobby/1/r.x")).toEqual({ kind: "bad_room_name" });
    expect(parseLobbyPath("/v0/lobby/1/%FF")).toEqual({ kind: "bad_room_name" });
  });
});

describe("書き込みの回数（無料枠を荒らしに使わせない）", () => {
  it("create・post・count を重ねても put は 0 回（状態は attachment にだけ持つ）", async () => {
    const l = freshLobby();
    const stub = lobbyStub(l.build, l.name);
    await runInDurableObject(stub, (_inst, state) => {
      const s = state.storage as unknown as Record<string, any>;
      const orig = s.put.bind(s);
      (state as any).__puts = 0;
      s.put = (...args: unknown[]) => {
        (state as any).__puts += 1;
        return orig(...args);
      };
    });
    const a = await create(l, "A", freshIp());
    const p = await post(l, { cap: 5 }, freshIp());
    for (let i = 0; i < 10; i++) p.c.sendJson({ t: "count", watchers: i % 2 });
    await sleep(100);
    expect(await runInDurableObject(stub, (_inst, state) => (state as any).__puts)).toBe(0);
    a.c.close();
    p.c.close();
  });

  it("復帰し直して印を失っても次のアラームで全体が届く（lastSent が 0 のとき）", async () => {
    const l = freshLobby();
    const { c: viewer } = await open(l);
    const host = await post(l, { watchers: 0 }, freshIp());
    await nextList(viewer);
    host.c.sendJson({ t: "count", watchers: 3 });
    await sleep(100);
    expect(await viewer.silentFor(0)).toBeNull();
    await runInDurableObject(lobbyStub(l.build, l.name), (inst) => {
      (inst as any).flushPending = false;
      (inst as any).lastSent = 0;
    });
    expect(await runDurableObjectAlarm(lobbyStub(l.build, l.name))).toBe(true);
    expect((await nextList(viewer))[0].watchers).toBe(3);
  });

  it("setAlarm が失敗しても接続も create も post も通る", async () => {
    const l = freshLobby();
    await runInDurableObject(lobbyStub(l.build, l.name), (_inst, state) => {
      (state.storage as unknown as Record<string, any>).setAlarm = async () => {
        throw new Error("storage quota exceeded");
      };
    });
    const a = await create(l, "A", freshIp());
    const p = await post(l, {}, freshIp());
    expect((await open(l)).items).toHaveLength(2);
    a.c.close();
    p.c.close();
  });
});

describe("既存の口は変わらない", () => {
  it("ルーム・待ち列・ランダム対戦の一覧は今までどおり", async () => {
    const r = await rawConnect("/v0/rooms/lobby-regress");
    expect(r.status).toBe(101);
    expect(await new Client(r.webSocket!).next()).toMatchObject({ t: "room", role: "host" });
    const q = await rawConnect(`/v0/random/${freshBuild()}`);
    expect(q.status).toBe(101);
    expect(await new Client(q.webSocket!).next()).toMatchObject({ t: "queue" });
    const w = await rawConnect(`/v0/watch/${freshBuild()}/view`);
    expect(w.status).toBe(101);
    expect((await new Client(w.webSocket!).next()).t).toBe("list");
  });
});
