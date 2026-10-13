// 組み上がりの検査: 入口（index.ts）→ ルーム（room.ts）を、ローカルの workerd の上で本物の WebSocket で叩く。
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { CODE_MAX_CHARS, Close, CONNECT_LIMIT_PER_MINUTE, PING_TEXT, RETRY_AFTER_SECS } from "../src/rules";
import { Client, connect, fakeCode, freshCid, freshIp, freshRoom, rawConnect, sleep, stubFor } from "./helpers";

async function openHost(room: string, code = fakeCode("host")): Promise<Client> {
  const a = await connect(room);
  const r = await a.next();
  expect(r).toMatchObject({ t: "room", role: "host" });
  a.sendJson({ t: "offer", code });
  return a;
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

describe("揃う", () => {
  it("host が預け、guest が入ると、互いのコードが渡り、両者が 1000 で閉じる", async () => {
    const room = freshRoom();
    const hostCode = fakeCode("hostcode");
    const guestCode = fakeCode("guestcode");

    const a = await connect(room);
    const ra = await a.next();
    expect(ra.t).toBe("room");
    expect(ra.role).toBe("host");
    // 期限は UNIX 秒。いまから 10 分先の前後（時計のずれを見て広めに）。
    const nowSecs = Math.floor(Date.now() / 1000);
    expect(ra.expires_at).toBeGreaterThanOrEqual(nowSecs + 590);
    expect(ra.expires_at).toBeLessThanOrEqual(nowSecs + 610);
    a.sendJson({ t: "offer", code: hostCode });
    expect(await a.silentFor(100)).toBeNull(); // 相手が来るまで何も来ない

    const b = await connect(room);
    expect(await b.next()).toMatchObject({ t: "room", role: "guest", expires_at: ra.expires_at });
    expect(await b.next()).toEqual({ t: "peer", code: hostCode });
    const matched = await a.next();
    expect(matched.t).toBe("matched");
    // 揃ってからは answer の期限（1 分）で縛る。
    expect(matched.expires_at).toBeLessThanOrEqual(Math.floor(Date.now() / 1000) + 61);

    b.sendJson({ t: "answer", code: guestCode });
    expect(await a.next()).toEqual({ t: "peer", code: guestCode });
    expect((await a.closed).code).toBe(Close.Done);
    expect((await b.closed).code).toBe(Close.Done);

    const c = await connect(room);
    expect(await c.next()).toMatchObject({ t: "room", role: "host" });
    c.close();
  });

  it("順番が逆: guest が host の offer より先に入っても、offer が来た時点で渡る", async () => {
    const room = freshRoom();
    const a = await connect(room);
    expect(await a.next()).toMatchObject({ role: "host" });
    const b = await connect(room);
    expect(await b.next()).toMatchObject({ role: "guest" });
    expect(await b.silentFor(100)).toBeNull();
    expect(await a.silentFor(0)).toBeNull();

    a.sendJson({ t: "offer", code: fakeCode("late") });
    expect(await b.next()).toEqual({ t: "peer", code: fakeCode("late") });
    expect((await a.next()).t).toBe("matched");
    b.sendJson({ t: "answer", code: fakeCode("ans") });
    expect(await a.next()).toEqual({ t: "peer", code: fakeCode("ans") });
    expect((await a.closed).code).toBe(Close.Done);
    expect((await b.closed).code).toBe(Close.Done);
  });

  it("部屋名の大文字と小文字は同じ部屋", async () => {
    const room = freshRoom("Case");
    const a = await openHost(room.toUpperCase());
    const b = await connect(room.toLowerCase());
    expect(await b.next()).toMatchObject({ role: "guest" });
    expect((await b.next()).t).toBe("peer");
    a.close();
    b.close();
  });

  it("PING には Durable Object を起こさずに PONG が返り、期限は延びない", async () => {
    const room = freshRoom();
    const a = await connect(room);
    const r = await a.next();
    a.send(PING_TEXT);
    expect(await a.next()).toEqual({ t: "pong" });
    const b = await connect(room);
    expect(await b.next()).toMatchObject({ role: "guest", expires_at: r.expires_at });
    a.close();
    b.close();
  });
});

describe("3 人目", () => {
  it("否定: 3 人目は 409 room_full で、WebSocket を張らない。先の 2 人は続けられる", async () => {
    const room = freshRoom();
    const a = await openHost(room);
    const b = await connect(room);
    await b.next(); // room
    await b.next(); // peer
    await a.next(); // matched

    const res = await rawConnect(`/v0/rooms/${room}`);
    expect(res.status).toBe(409);
    expect(res.webSocket).toBeNull();
    expect(await res.json()).toEqual({ error: "room_full" });

    b.sendJson({ t: "answer", code: fakeCode("g") });
    expect(await a.next()).toEqual({ t: "peer", code: fakeCode("g") });
    expect((await a.closed).code).toBe(Close.Done);
  });

  it("否定: guest がまだ host のコードを受けていなくても、3 人目は断る", async () => {
    const room = freshRoom();
    const a = await connect(room);
    await a.next();
    const b = await connect(room);
    await b.next();
    const res = await rawConnect(`/v0/rooms/${room}`);
    expect(res.status).toBe(409);
    a.close();
    b.close();
  });
});

describe("入り直し（同じ cid）", () => {
  /** guest が host のコードを受けた状態（host は matched を受け取り済み）まで進める。guest は閉じずに残す（届かなかった入室のコピー）。 */
  async function matchedWithGhost(room: string, cid: string) {
    const a = await openHost(room, fakeCode("h"));
    const ghost = await connect(room, undefined, cid);
    expect(await ghost.next()).toMatchObject({ t: "room", role: "guest" });
    expect(await ghost.next()).toEqual({ t: "peer", code: fakeCode("h") });
    expect((await a.next()).t).toBe("matched");
    return { a, ghost };
  }

  it("届かなかった guest が残っていても、同じ cid の入り直しは room_full にならず、host と揃う", async () => {
    const room = freshRoom();
    const cid = freshCid();
    const { a, ghost } = await matchedWithGhost(room, cid);

    const b = await connect(room, undefined, cid);
    expect(await b.next()).toMatchObject({ t: "room", role: "guest" });
    expect(await b.next()).toEqual({ t: "peer", code: fakeCode("h") });
    // 古い接続は 1000 で閉じられ、host は閉じられない・2 度目の matched も受けない。
    expect((await ghost.closed).code).toBe(Close.Done);
    expect(await a.silentFor(100)).toBeNull();
    expect(a.isClosed).toBe(false);

    b.sendJson({ t: "answer", code: fakeCode("g") });
    expect(await a.next()).toEqual({ t: "peer", code: fakeCode("g") });
    expect((await a.closed).code).toBe(Close.Done);
    expect((await b.closed).code).toBe(Close.Done);
  });

  it("入れ替えた guest の期限は host の answer の期限のまま（host は知らされないので延ばさない）", async () => {
    const room = freshRoom();
    const cid = freshCid();
    const { a, ghost } = await matchedWithGhost(room, cid);
    await forceDeadlinePast(room);
    const b = await connect(room, undefined, cid);
    // 期限切れのルームは片付いて、入り直した人は新しいルームの host になる（期限が過ぎた入れ替え元に引きずられない）。
    expect(await b.next()).toMatchObject({ t: "room", role: "host" });
    expect((await a.closed).code).toBe(Close.Expired);
    expect((await ghost.closed).code).toBe(Close.Expired);
    b.close();
  });

  it("入れ替えた guest の room の期限は、host が受けた matched の期限と同じ（延びていない）", async () => {
    const room = freshRoom();
    const cid = freshCid();
    const a = await openHost(room, fakeCode("h"));
    const ghost = await connect(room, undefined, cid);
    await ghost.next();
    await ghost.next();
    const matched = await a.next();
    await sleep(1100); // 秒の粒度を越えて、延びていれば値が変わるようにする
    const b = await connect(room, undefined, cid);
    expect(await b.next()).toMatchObject({ role: "guest", expires_at: matched.expires_at });
    a.close();
    b.close();
  });

  it("否定: cid が違う 3 人目・cid を付けない古いクライアントの入り直しは、今までどおり 409 room_full", async () => {
    const room = freshRoom();
    const cid = freshCid();
    const { a, ghost } = await matchedWithGhost(room, cid);

    for (const path of [
      `/v0/rooms/${room}?cid=${freshCid()}`,
      `/v0/rooms/${room}`,
      `/v0/rooms/${room}?cid=${cid.toUpperCase()}`,
      `/v0/rooms/${room}?cid=${cid.slice(1)}`,
      `/v0/rooms/${room}?cid=`,
    ]) {
      const res = await rawConnect(path);
      expect(res.status, path).toBe(409);
      expect(await res.json(), path).toEqual({ error: "room_full" });
    }
    // 断られても、揃っている 2 人は続けられる。
    expect(ghost.isClosed).toBe(false);
    ghost.sendJson({ t: "answer", code: fakeCode("g") });
    expect(await a.next()).toEqual({ t: "peer", code: fakeCode("g") });
  });

  it("host の残りは、同じ cid の入り直しが新しい host として置き換え、待っていた guest は peer_left", async () => {
    const room = freshRoom();
    const cid = freshCid();
    const old = await connect(room, undefined, cid);
    expect(await old.next()).toMatchObject({ role: "host" });
    const guest = await connect(room);
    expect(await guest.next()).toMatchObject({ role: "guest" });

    const again = await connect(room, undefined, cid);
    // 自分の残りの guest にならず、host になる。
    expect(await again.next()).toMatchObject({ t: "room", role: "host" });
    expect((await old.closed).code).toBe(Close.Done);
    expect(await guest.next()).toEqual({ t: "error", code: "peer_left" });
    expect((await guest.closed).code).toBe(Close.PeerLeft);
    again.close();
  });

  it("違う部屋の同じ cid には触らない（観戦用の部屋など、1 つの起動が複数の部屋を使う）", async () => {
    const cid = freshCid();
    const r1 = freshRoom();
    const r2 = freshRoom();
    const a = await connect(r1, undefined, cid);
    expect(await a.next()).toMatchObject({ role: "host" });
    const b = await connect(r2, undefined, cid);
    expect(await b.next()).toMatchObject({ role: "host" });
    expect(await a.silentFor(100)).toBeNull();
    expect(a.isClosed).toBe(false);
    a.close();
    b.close();
  });

  it("cid が来る前に作られた古い attachment（cid・matched が無い）の接続でも落ちず、cid 付きの入室を受ける", async () => {
    const room = freshRoom();
    const a = await openHost(room, fakeCode("old"));
    await sleep(50);
    await runInDurableObject(stubFor(room), (_inst, state) => {
      for (const ws of state.getWebSockets()) {
        const att = ws.deserializeAttachment() as Record<string, unknown>;
        delete att.cid;
        delete att.matched;
        ws.serializeAttachment(att);
      }
    });
    const b = await connect(room, undefined, freshCid());
    expect(await b.next()).toMatchObject({ role: "guest" });
    expect(await b.next()).toEqual({ t: "peer", code: fakeCode("old") });
    expect((await a.next()).t).toBe("matched");
    a.close();
    b.close();
  });
});

describe("期限", () => {
  it("期限が過ぎるとアラームで全員に expired を送り 4408 で閉じる", async () => {
    const room = freshRoom();
    const a = await openHost(room);
    const b = await connect(room);
    await b.next();
    await b.next();
    await a.next();
    await forceDeadlinePast(room);
    expect(await runDurableObjectAlarm(stubFor(room))).toBe(true);
    expect(await a.next()).toEqual({ t: "error", code: "expired" });
    expect(await b.next()).toEqual({ t: "error", code: "expired" });
    expect((await a.closed).code).toBe(Close.Expired);
    expect((await b.closed).code).toBe(Close.Expired);
  });

  it("否定: 期限の前にアラームが走っても（前の部屋の古いアラーム）閉じない", async () => {
    const room = freshRoom();
    const a = await openHost(room);
    expect(await runDurableObjectAlarm(stubFor(room))).toBe(true);
    expect(await a.silentFor(100)).toBeNull();
    expect(a.isClosed).toBe(false);
    const b = await connect(room);
    expect(await b.next()).toMatchObject({ role: "guest" });
    expect((await b.next()).t).toBe("peer");
    a.close();
    b.close();
  });

  it("アラームが遅れても、期限の過ぎた部屋には入れず、新しい部屋として始まる", async () => {
    const room = freshRoom();
    const a = await openHost(room);
    await forceDeadlinePast(room);
    const b = await connect(room);
    // 古い host は期限切れで閉じ、入ってきた人は新しいルームの host になる（古い host のコードは渡らない）。
    expect(await b.next()).toMatchObject({ t: "room", role: "host" });
    expect(await a.next()).toEqual({ t: "error", code: "expired" });
    expect((await a.closed).code).toBe(Close.Expired);
    expect(await b.silentFor(100)).toBeNull();
    b.close();
  });
});

describe("大きすぎる・形の悪い文", () => {
  it("ちょうど上限の字数のコードは相手へ渡る（上限の隣）", async () => {
    const room = freshRoom();
    const big = "B".repeat(CODE_MAX_CHARS);
    const a = await openHost(room, big);
    const b = await connect(room);
    await b.next();
    expect(await b.next()).toEqual({ t: "peer", code: big });
    a.close();
    b.close();
  });

  it("否定: 上限を越えたコードは too_large で 4413。部屋に残らず、次の人は host になる", async () => {
    const room = freshRoom();
    const a = await connect(room);
    await a.next();
    a.sendJson({ t: "offer", code: "B".repeat(CODE_MAX_CHARS + 1) });
    expect(await a.next()).toEqual({ t: "error", code: "too_large" });
    expect((await a.closed).code).toBe(Close.TooLarge);
    const c = await connect(room);
    expect(await c.next()).toMatchObject({ role: "host" });
    c.close();
  });

  it("否定: 字の悪いコードは bad_code で 4422。guest には何も渡らず peer_left で閉じる", async () => {
    const room = freshRoom();
    const a = await connect(room);
    await a.next();
    const b = await connect(room);
    await b.next();
    a.sendJson({ t: "offer", code: "abc=def" });
    expect(await a.next()).toEqual({ t: "error", code: "bad_code" });
    expect((await a.closed).code).toBe(Close.BadCode);
    expect(await b.next()).toEqual({ t: "error", code: "peer_left" });
    expect((await b.closed).code).toBe(Close.PeerLeft);
  });

  it("否定: 役割と順番に合わない文は bad_message で 4400", async () => {
    const cases: { name: string; run: (room: string) => Promise<Client> }[] = [
      {
        name: "guest が offer を送る",
        run: async (room) => {
          const a = await openHost(room);
          const b = await connect(room);
          await b.next();
          await b.next();
          b.sendJson({ t: "offer", code: fakeCode("x") });
          void a;
          return b;
        },
      },
      {
        name: "host が answer を送る",
        run: async (room) => {
          const a = await connect(room);
          await a.next();
          a.sendJson({ t: "answer", code: fakeCode("x") });
          return a;
        },
      },
      {
        name: "host が offer を 2 回送る",
        run: async (room) => {
          const a = await openHost(room);
          a.sendJson({ t: "offer", code: fakeCode("again") });
          return a;
        },
      },
      {
        name: "guest が host のコードを受ける前に answer を送る",
        run: async (room) => {
          const a = await connect(room);
          await a.next();
          const b = await connect(room);
          await b.next();
          b.sendJson({ t: "answer", code: fakeCode("early") });
          void a;
          return b;
        },
      },
      {
        name: "JSON でない",
        run: async (room) => {
          const a = await connect(room);
          await a.next();
          a.send("hello");
          return a;
        },
      },
      {
        name: "バイナリの文",
        run: async (room) => {
          const a = await connect(room);
          await a.next();
          a.send(new Uint8Array([1, 2, 3]).buffer as ArrayBuffer);
          return a;
        },
      },
    ];
    for (const c of cases) {
      const who = await c.run(freshRoom());
      expect(await who.next(), c.name).toEqual({ t: "error", code: "bad_message" });
      expect((await who.closed).code, c.name).toBe(Close.BadMessage);
    }
  });

  it("guest が早すぎる answer で落ちても、host のコードは渡っていないので host は待ち続ける", async () => {
    const room = freshRoom();
    const a = await connect(room);
    await a.next();
    const b = await connect(room);
    await b.next();
    b.sendJson({ t: "answer", code: fakeCode("early") });
    expect((await b.closed).code).toBe(Close.BadMessage);
    expect(await a.silentFor(100)).toBeNull();
    a.sendJson({ t: "offer", code: fakeCode("h") });
    const c = await connect(room);
    expect(await c.next()).toMatchObject({ role: "guest" });
    expect(await c.next()).toEqual({ t: "peer", code: fakeCode("h") });
    a.close();
    c.close();
  });
});

describe("入口で断るもの", () => {
  it("否定: 字の悪い部屋名・戻せない符号・符号化した `/` は 400 bad_room_name", async () => {
    for (const bad of ["x".repeat(33), "%", "%ZZ", "%E8%8A", "%ED%A0%80", "%C0%AF", "a%2Fb", "%2F", "%00", "%20", "%E3%80%80"]) {
      const res = await rawConnect(`/v0/rooms/${bad}`);
      expect(res.status, bad).toBe(400);
      expect(await res.json()).toEqual({ error: "bad_room_name" });
    }
  });

  it("否定: 道の `.` `..`（符号化した %2E も）は道の読み方で消えて 404", async () => {
    for (const p of [".", "..", "%2E", "%2e%2E", ".%2e"]) {
      expect((await rawConnect(`/v0/rooms/${p}`)).status, p).toBe(404);
    }
  });

  it("日本語の部屋名: 符号化した道・符号化しない道・16 進の大小が違う道は同じ部屋", async () => {
    const tag = freshRoom("jp");
    const name = `花映塚の部屋-${tag}`;
    const enc = encodeURIComponent(name);
    const a = await openHost(name);
    const raw = await rawConnect(`/v0/rooms/${name}`);
    expect(raw.status).toBe(101);
    const b = new Client(raw.webSocket!);
    expect(await b.next()).toMatchObject({ role: "guest" });
    expect((await b.next()).t).toBe("peer");
    const lower = enc.replace(/%[0-9A-F]{2}/g, (m) => m.toLowerCase());
    expect(lower).not.toBe(enc);
    expect((await rawConnect(`/v0/rooms/${lower}`)).status).toBe(409);
    a.close();
    b.close();
  });

  it("否定: 知らない道は 404、GET 以外は 405、WebSocket でなければ 426", async () => {
    for (const p of ["/", "/v0/rooms/", "/v0/rooms/a/join", "/v1/rooms/a", "/rooms/a"]) {
      expect((await rawConnect(p)).status, p).toBe(404);
    }
    for (const method of ["POST", "PUT", "DELETE"]) {
      const r = await rawConnect("/v0/rooms/abc", { method, upgrade: false });
      expect(r.status, method).toBe(405);
    }
    const res = await rawConnect("/v0/rooms/abc", { upgrade: false });
    expect(res.status).toBe(426);
    expect(await res.json()).toEqual({ error: "upgrade_required" });
  });

  it("否定: 1 IP から 60 秒に上限（CONNECT_LIMIT_PER_MINUTE）を越えると 429 と retry_after。別の IP は通る", async () => {
    const ip = freshIp();
    const opened: Client[] = [];
    for (let i = 0; i < CONNECT_LIMIT_PER_MINUTE; i++) {
      // ルームを分けて、409 ではなく回数のガードだけを見る。
      opened.push(await connect(freshRoom("rl"), ip));
    }
    const res = await rawConnect(`/v0/rooms/${freshRoom("rl")}`, { ip });
    expect(res.status).toBe(429);
    expect(res.headers.get("Retry-After")).toBe(String(RETRY_AFTER_SECS));
    expect(await res.json()).toEqual({ error: "rate_limited", retry_after: RETRY_AFTER_SECS });
    const other = await connect(freshRoom("rl"), freshIp());
    for (const c of [...opened, other]) c.close();
  });
});

describe("片方が途中で切れる", () => {
  it("host が待つ間に抜けると部屋は空になり、次の人が host になる", async () => {
    const room = freshRoom();
    const a = await openHost(room);
    a.close();
    await a.closed;
    await sleep(50);
    const b = await connect(room);
    expect(await b.next()).toMatchObject({ role: "host" });
    b.close();
  });

  it("host が揃った後に抜けると guest は peer_left で 4410", async () => {
    const room = freshRoom();
    const a = await openHost(room);
    const b = await connect(room);
    await b.next();
    await b.next();
    await a.next();
    a.close();
    expect(await b.next()).toEqual({ t: "error", code: "peer_left" });
    expect((await b.closed).code).toBe(Close.PeerLeft);
  });

  it("guest が host のコードを受けた後に抜けると、host も peer_left で閉じる（同じコードを次へ渡さない）", async () => {
    const room = freshRoom();
    const a = await openHost(room);
    const b = await connect(room);
    await b.next();
    await b.next();
    expect((await a.next()).t).toBe("matched");
    b.close();
    expect(await a.next()).toEqual({ t: "error", code: "peer_left" });
    expect((await a.closed).code).toBe(Close.PeerLeft);
  });

  it("guest が host のコードを受ける前に抜けると、host は待ち続け、次の guest と揃う", async () => {
    const room = freshRoom();
    const a = await connect(room);
    await a.next();
    const b = await connect(room);
    await b.next();
    b.close();
    await b.closed;
    expect(await a.silentFor(100)).toBeNull();
    expect(a.isClosed).toBe(false);
    a.sendJson({ t: "offer", code: fakeCode("stay") });
    const c = await connect(room);
    expect(await c.next()).toMatchObject({ role: "guest" });
    expect(await c.next()).toEqual({ t: "peer", code: fakeCode("stay") });
    a.close();
    c.close();
  });
});

describe("Hibernation", () => {
  it("host が預けた後に Durable Object が眠っても、起きて guest と揃う", async () => {
    const room = freshRoom();
    const stub = stubFor(room);
    const a = await openHost(room, fakeCode("sleepy"));
    await sleep(50); // offer を Durable Object が受け終えるのを待つ

    // 眠らせる前のインスタンスに印を付け、眠った後に印が消えている（本当に作り直された）ことを見る。
    await runInDurableObject(stub, (inst) => {
      (inst as unknown as Record<string, unknown>).__mark = 1;
    });
    await evictDurableObject(stub); // 既定は WebSocket を閉じずに眠らせる
    const marked = await runInDurableObject(stub, (inst, state) => ({
      mark: (inst as unknown as Record<string, unknown>).__mark,
      sockets: state.getWebSockets().length,
    }));
    expect(marked.mark).toBeUndefined();
    expect(marked.sockets).toBe(1);

    a.send(PING_TEXT);
    expect(await a.next()).toEqual({ t: "pong" });

    const b = await connect(room);
    expect(await b.next()).toMatchObject({ role: "guest" });
    expect(await b.next()).toEqual({ t: "peer", code: fakeCode("sleepy") });
    expect((await a.next()).t).toBe("matched");

    await evictDurableObject(stub);
    b.sendJson({ t: "answer", code: fakeCode("woke") });
    expect(await a.next()).toEqual({ t: "peer", code: fakeCode("woke") });
    expect((await a.closed).code).toBe(Close.Done);
    expect((await b.closed).code).toBe(Close.Done);
  });

  it("否定: 眠らせるときに WebSocket を閉じる設定では、host は残らない（検査の道具が本当に眠らせている対照）", async () => {
    const room = freshRoom();
    const stub = stubFor(room);
    const a = await openHost(room);
    await sleep(50);
    await evictDurableObject(stub, { webSockets: "close" });
    await a.closed;
    const b = await connect(room);
    expect(await b.next()).toMatchObject({ role: "host" });
    b.close();
  });
});
