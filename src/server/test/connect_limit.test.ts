// 繋ぐ回数の制限（src/connect_limiter.ts）の検査。純粋な部分は境目の両側を並べ、組み上がりは本物の Durable Object（miniflare）で見る。
import { env } from "cloudflare:workers";
import { evictDurableObject, runDurableObjectAlarm, runInDurableObject } from "cloudflare:test";
import { afterEach, describe, expect, it, vi } from "vitest";
import worker from "../src/index";
import {
  CONNECT_LIMIT_PER_MINUTE,
  CONNECT_PRELIMIT_PER_MINUTE,
  CONNECT_WINDOW_MS,
  RETRY_AFTER_SECS,
  connectLimitKey,
  slideConnectWindow,
} from "../src/rules";
import { connect, freshIp, freshRoom, rawConnect, sameSlash64 } from "./helpers";
// 前段の上限を wrangler.jsonc の値と読み比べるため、設定ファイルを文字のまま読む（vite の ?raw）。
// @ts-expect-error 型の宣言は無い（検査は esbuild で型を剥がして走らせる）
import wranglerText from "../wrangler.jsonc?raw";

describe("滑る窓（slideConnectWindow）", () => {
  function hitN(n: number, t0: number, stepMs: number) {
    let stamps: number[] = [];
    const results: boolean[] = [];
    for (let i = 0; i < n; i++) {
      const r = slideConnectWindow(stamps, t0 + i * stepMs);
      stamps = r.stamps;
      results.push(r.allowed);
    }
    return { stamps, results };
  }

  it(`${CONNECT_LIMIT_PER_MINUTE} 回目まで許し、${CONNECT_LIMIT_PER_MINUTE + 1} 回目を断る`, () => {
    const { results } = hitN(CONNECT_LIMIT_PER_MINUTE + 1, 1_000_000, 10);
    expect(results.slice(0, CONNECT_LIMIT_PER_MINUTE).every(Boolean)).toBe(true);
    expect(results[CONNECT_LIMIT_PER_MINUTE]).toBe(false);
  });

  it("窓の端: 最初の回から 59999 ms 後はまだ断り、60000 ms 後に通る", () => {
    const t0 = 5_000_000;
    const full = hitN(CONNECT_LIMIT_PER_MINUTE, t0, 0).stamps;
    expect(slideConnectWindow(full, t0 + CONNECT_WINDOW_MS - 1).allowed).toBe(false);
    expect(slideConnectWindow(full, t0 + CONNECT_WINDOW_MS).allowed).toBe(true);
  });

  it("どの 60 秒の区間でも上限まで: 窓の終わり際に上限回通したら、区切りをまたいでも通らない", () => {
    const t0 = 7_000_000;
    const late = hitN(CONNECT_LIMIT_PER_MINUTE, t0 + 50_000, 0).stamps;
    expect(slideConnectWindow(late, t0 + 61_000).allowed).toBe(false);
    expect(slideConnectWindow(late, t0 + 50_000 + CONNECT_WINDOW_MS - 1).allowed).toBe(false);
    expect(slideConnectWindow(late, t0 + 50_000 + CONNECT_WINDOW_MS).allowed).toBe(true);
  });

  it("断った回は覚えない（叩き続けても、許した古い回が窓を出れば通る）", () => {
    const t0 = 9_000_000;
    let stamps = hitN(CONNECT_LIMIT_PER_MINUTE, t0, 0).stamps;
    for (let i = 1; i <= 50; i++) {
      const r = slideConnectWindow(stamps, t0 + i * 1000);
      expect(r.allowed).toBe(false);
      stamps = r.stamps;
    }
    expect(stamps.length).toBe(CONNECT_LIMIT_PER_MINUTE);
    expect(slideConnectWindow(stamps, t0 + CONNECT_WINDOW_MS).allowed).toBe(true);
  });
});

describe("回数の鍵（connectLimitKey）", () => {
  it("IPv4 はそのまま。違うアドレスは違う鍵", () => {
    expect(connectLimitKey("192.0.2.1")).toBe("v4:192.0.2.1");
    expect(connectLimitKey("192.0.2.2")).not.toBe(connectLimitKey("192.0.2.1"));
  });

  it("IPv6 は上位 64 bit（/64）でまとめる。省略の書き方・大文字・ゾーンの違いは同じ鍵", () => {
    const k = connectLimitKey("2001:db8:1:2::1");
    expect(k).toBe("v6:2001:db8:1:2");
    expect(connectLimitKey("2001:db8:1:2::ffff")).toBe(k);
    expect(connectLimitKey("2001:0DB8:0001:0002:aaaa:bbbb:cccc:dddd")).toBe(k);
    expect(connectLimitKey("2001:db8:1:2:0:0:0:1%eth0")).toBe(k);
    expect(connectLimitKey("::1:2:3")).toBe("v6:0:0:0:0");
  });

  it("別の /64 は別の鍵（64 bit 目と 65 bit 目の境目）", () => {
    const base = connectLimitKey("2001:db8:1:2::1");
    expect(connectLimitKey("2001:db8:1:3::1")).not.toBe(base);
    expect(connectLimitKey("2001:db8:1:2:8000::1")).toBe(base);
  });

  it("IPv4 射影（::ffff:a.b.c.d）は IPv4 と同じ鍵。末尾の IPv4 表記も読む", () => {
    expect(connectLimitKey("::ffff:192.0.2.1")).toBe(connectLimitKey("192.0.2.1"));
    expect(connectLimitKey("::ffff:c000:201")).toBe(connectLimitKey("192.0.2.1"));
    expect(connectLimitKey("64:ff9b::192.0.2.1")).toBe("v6:64:ff9b:0:0");
  });

  it("ヘッダ無しは 1 つの鍵。読めない字も字のままの鍵で数える（通り抜けにしない）", () => {
    expect(connectLimitKey(null)).toBe("no-ip");
    for (const bad of ["", "abc", "1.2.3.256", "2001:db8::1::2", "1:2:3:4:5:6:7", "12345::1", "::ffff:1.2.3.999"]) {
      expect(connectLimitKey(bad), bad).toMatch(/^raw:/);
    }
    expect(connectLimitKey("abc")).not.toBe(connectLimitKey("abd"));
  });
});

function limiterFor(ip: string): DurableObjectStub {
  return env.CONNECT_LIMITER.get(env.CONNECT_LIMITER.idFromName(connectLimitKey(ip)));
}

/** ルームを毎回分けて（409 を避け）n 回つなぎ、状態コードを返す。 */
async function connectTimes(n: number, ip: string): Promise<number[]> {
  const codes: number[] = [];
  for (let i = 0; i < n; i++) {
    const res = await rawConnect(`/v0/rooms/${freshRoom("cl")}`, { ip });
    codes.push(res.status);
    if (res.webSocket !== null) {
      res.webSocket.accept();
      res.webSocket.close(1000, "bye");
    }
  }
  return codes;
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("組み上がり（入口 → 数え役 → 部屋）", () => {
  it(`同じ IP の ${CONNECT_LIMIT_PER_MINUTE} 回目まで 101、${CONNECT_LIMIT_PER_MINUTE + 1} 回目が 429（形は retry_after = 60 と Retry-After）`, async () => {
    const ip = freshIp();
    const codes = await connectTimes(CONNECT_LIMIT_PER_MINUTE, ip);
    expect(codes.every((c) => c === 101)).toBe(true);
    const res = await rawConnect(`/v0/rooms/${freshRoom("cl")}`, { ip });
    expect(res.status).toBe(429);
    expect(res.headers.get("Retry-After")).toBe(String(RETRY_AFTER_SECS));
    expect(await res.json()).toEqual({ error: "rate_limited", retry_after: RETRY_AFTER_SECS });
  });

  it("同時到着: 上限 + 1 本を同時に出すと、101 が上限ちょうど・429 が 1 本", async () => {
    const ip = freshIp();
    const total = CONNECT_LIMIT_PER_MINUTE + 1;
    const results = await Promise.all(
      Array.from({ length: total }, () => rawConnect(`/v0/rooms/${freshRoom("cl")}`, { ip })),
    );
    for (const r of results) {
      if (r.webSocket !== null) {
        r.webSocket.accept();
        r.webSocket.close(1000, "bye");
      }
    }
    const codes = results.map((r) => r.status);
    expect(codes.filter((c) => c === 101)).toHaveLength(CONNECT_LIMIT_PER_MINUTE);
    expect(codes.filter((c) => c === 429)).toHaveLength(1);
  });

  it("setAlarm が 1 回投げた後の次の許可でアラームが付く（行が永久に残らない）", async () => {
    const ip = freshIp();
    const out = await runInDurableObject(limiterFor(ip), async (inst: any, s) => {
      vi.spyOn(s.storage, "setAlarm").mockRejectedValueOnce(new Error("alarm down"));
      const first = await inst.hit().then(() => "ok", () => "threw");
      const alarmAfterFail = await s.storage.getAlarm();
      const second = await inst.hit();
      return { first, alarmAfterFail, second, alarmAfterNext: await s.storage.getAlarm() };
    });
    expect(out.first).toBe("threw");
    expect(out.alarmAfterFail).toBeNull();
    expect(out.second).toBe(true);
    expect(out.alarmAfterNext).not.toBeNull();
  });

  it("別の IP は、上限に達した IP の隣でも通る", async () => {
    const a = freshIp();
    await connectTimes(CONNECT_LIMIT_PER_MINUTE, a);
    expect((await connectTimes(1, a))[0]).toBe(429);
    expect((await connectTimes(1, freshIp()))[0]).toBe(101);
  });

  it("同じ /64 の別の IPv6 は同じ数えに入り、別の /64 は別の数え", async () => {
    const a = freshIp();
    await connectTimes(CONNECT_LIMIT_PER_MINUTE, a);
    expect((await connectTimes(1, sameSlash64(a, 0xabcd)))[0]).toBe(429);
    expect((await connectTimes(1, sameSlash64(a, 2)))[0]).toBe(429);
    expect((await connectTimes(1, freshIp()))[0]).toBe(101);
  });

  it("窓（60 秒）が過ぎたらまた通る。1 ミリ秒手前ではまだ通らない", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const t0 = Date.now();
    const ip = freshIp();
    expect((await connectTimes(CONNECT_LIMIT_PER_MINUTE, ip)).every((c) => c === 101)).toBe(true);
    expect((await connectTimes(1, ip))[0]).toBe(429);
    vi.setSystemTime(t0 + CONNECT_WINDOW_MS - 1);
    expect((await connectTimes(1, ip))[0]).toBe(429);
    vi.setSystemTime(t0 + CONNECT_WINDOW_MS + 1);
    expect(await connectTimes(CONNECT_LIMIT_PER_MINUTE, ip)).toEqual(Array(CONNECT_LIMIT_PER_MINUTE).fill(101));
    expect((await connectTimes(1, ip))[0]).toBe(429);
  });

  it("Durable Object が退去しても数えは残る（ストレージから読み直す）", async () => {
    const ip = freshIp();
    await connectTimes(CONNECT_LIMIT_PER_MINUTE, ip);
    await evictDurableObject(limiterFor(ip));
    expect((await connectTimes(1, ip))[0]).toBe(429);
  });

  it("断った回は何も書かない。覚えるのは許した回だけ（最大で上限の個数）", async () => {
    const ip = freshIp();
    await connectTimes(CONNECT_LIMIT_PER_MINUTE, ip);
    const before = await runInDurableObject(limiterFor(ip), (_i, s) => s.storage.get<number[]>("t"));
    expect(before).toHaveLength(CONNECT_LIMIT_PER_MINUTE);
    await connectTimes(5, ip);
    const after = await runInDurableObject(limiterFor(ip), (_i, s) => s.storage.get<number[]>("t"));
    expect(after).toEqual(before);
    // 同じ値の書き直しは中身では見分けられないので、書く呼び出しの回数でも見る。
    const writes = await runInDurableObject(limiterFor(ip), async (inst: any, s) => {
      const put = vi.spyOn(s.storage, "put");
      const alarm = vi.spyOn(s.storage, "setAlarm");
      const results = [await inst.hit(), await inst.hit(), await inst.hit()];
      return { results, put: put.mock.calls.length, alarm: alarm.mock.calls.length };
    });
    expect(writes).toEqual({ results: [false, false, false], put: 0, alarm: 0 });
  });

  it("アラームは窓が過ぎてから行を全部消す。窓の中で走っても数えは消さない", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const t0 = Date.now();
    const ip = freshIp();
    await connectTimes(CONNECT_LIMIT_PER_MINUTE, ip);
    expect(await runInDurableObject(limiterFor(ip), (_i, s) => s.storage.getAlarm())).not.toBeNull();

    vi.setSystemTime(t0 + CONNECT_WINDOW_MS / 2);
    expect(await runDurableObjectAlarm(limiterFor(ip))).toBe(true);
    expect((await connectTimes(1, ip))[0]).toBe(429);
    expect(await runInDurableObject(limiterFor(ip), (_i, s) => s.storage.getAlarm())).not.toBeNull();

    vi.setSystemTime(t0 + CONNECT_WINDOW_MS + 1000);
    expect(await runDurableObjectAlarm(limiterFor(ip))).toBe(true);
    const left = await runInDurableObject(limiterFor(ip), async (_i, s) => ({
      keys: [...(await s.storage.list()).keys()],
      alarm: await s.storage.getAlarm(),
    }));
    expect(left).toEqual({ keys: [], alarm: null });
    expect((await connectTimes(1, ip))[0]).toBe(101);
    expect(await runInDurableObject(limiterFor(ip), (_i, s) => s.storage.getAlarm())).not.toBeNull();
  });

  it("入口で断る要求（WebSocket でない・部屋名が不正）は回数に数えない", async () => {
    const ip = freshIp();
    for (let i = 0; i < CONNECT_LIMIT_PER_MINUTE + 3; i++) {
      expect((await rawConnect(`/v0/rooms/${freshRoom("cl")}`, { ip, upgrade: false })).status).toBe(426);
      expect((await rawConnect(`/v0/rooms/${"x".repeat(40)}`, { ip })).status).toBe(400);
    }
    expect((await connectTimes(1, ip))[0]).toBe(101);
  });
});

const passing = { limit: () => Promise.resolve({ success: true }) };

describe("粗い前段（Rate Limiting の束縛）→ 通ったものだけ数え役", () => {
  function request(ip: string): Request {
    return new Request(`http://rooms.test/v0/rooms/${freshRoom("pl")}`, {
      headers: { Upgrade: "websocket", "CF-Connecting-IP": ip },
    });
  }
  function countingLimiter(answer: boolean) {
    const calls = { n: 0 };
    return { calls, ns: { idFromName: () => ({}), get: () => ({ hit: () => { calls.n++; return Promise.resolve(answer); } }) } };
  }

  it("前段が断ったら数え役を呼ばず、同じ 429 と Retry-After を返す", async () => {
    const ip = freshIp();
    const limit = vi.fn(() => Promise.resolve({ success: false }));
    const { calls, ns } = countingLimiter(true);
    const res = await worker.fetch(request(ip), { ROOMS: env.ROOMS, CONNECT_LIMIT: { limit }, CONNECT_LIMITER: ns } as unknown as Env);
    expect(res.status).toBe(429);
    expect(res.headers.get("Retry-After")).toBe(String(RETRY_AFTER_SECS));
    expect(await res.json()).toEqual({ error: "rate_limited", retry_after: RETRY_AFTER_SECS });
    expect(calls.n).toBe(0);
    // 前段の鍵は数え役と同じ（/64 でまとめた鍵）。IP のまま渡すと IPv6 の逃げ道になる。
    expect(limit).toHaveBeenCalledWith({ key: connectLimitKey(ip) });
  });

  it("前段が例外を出したら、通して数え役で数える（数え役が断れば 429）", async () => {
    const boom = { limit: () => Promise.reject(new Error("ratelimit down")) };
    const ok = countingLimiter(true);
    const res = await worker.fetch(request(freshIp()), { ROOMS: env.ROOMS, CONNECT_LIMIT: boom, CONNECT_LIMITER: ok.ns } as unknown as Env);
    expect(res.status).toBe(101);
    res.webSocket!.accept();
    res.webSocket!.close(1000, "bye");
    expect(ok.calls.n).toBe(1);
    const no = countingLimiter(false);
    const res2 = await worker.fetch(request(freshIp()), { ROOMS: env.ROOMS, CONNECT_LIMIT: boom, CONNECT_LIMITER: no.ns } as unknown as Env);
    expect(res2.status).toBe(429);
    expect(no.calls.n).toBe(1);
  });

  it("前段が通したら数え役を 1 回呼ぶ", async () => {
    const ok = countingLimiter(false);
    const res = await worker.fetch(request(freshIp()), { ROOMS: env.ROOMS, CONNECT_LIMIT: passing, CONNECT_LIMITER: ok.ns } as unknown as Env);
    expect(res.status).toBe(429);
    expect(ok.calls.n).toBe(1);
  });

  it("前段の上限は wrangler.jsonc の ratelimits と rules.ts で同じ値", () => {
    const text = String(wranglerText);
    const m = /"name":\s*"CONNECT_LIMIT"[\s\S]*?"limit":\s*(\d+),\s*"period":\s*(\d+)/.exec(text);
    expect(m, "wrangler.jsonc に CONNECT_LIMIT の ratelimits が無い").not.toBeNull();
    expect(Number(m![1])).toBe(CONNECT_PRELIMIT_PER_MINUTE);
    expect(Number(m![2])).toBe(RETRY_AFTER_SECS);
    expect(CONNECT_PRELIMIT_PER_MINUTE).toBeGreaterThan(CONNECT_LIMIT_PER_MINUTE);
  });
});

describe("数え役が落ちたら通す（fail-open）", () => {
  function request(ip: string): Request {
    return new Request(`http://rooms.test/v0/rooms/${freshRoom("fo")}`, {
      headers: { Upgrade: "websocket", "CF-Connecting-IP": ip },
    });
  }

  async function expectOpen(res: Response): Promise<void> {
    expect(res.status).toBe(101);
    res.webSocket!.accept();
    res.webSocket!.close(1000, "bye");
  }

  it("idFromName / hit が例外を出しても部屋へ通し、固定の警告を 1 行だけ残す", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const ip = freshIp();
    const throwsAtId = { idFromName: () => { throw new Error("boom"); }, get: () => { throw new Error("boom"); } };
    const rejectsAtHit = {
      idFromName: () => ({}),
      get: () => ({ hit: () => Promise.reject(new Error("do down")) }),
    };
    for (const limiter of [throwsAtId, rejectsAtHit]) {
      const res = await worker.fetch(request(ip), { ROOMS: env.ROOMS, CONNECT_LIMIT: passing, CONNECT_LIMITER: limiter } as unknown as Env);
      await expectOpen(res);
    }
    expect(warn.mock.calls).toEqual([["connect_limiter_failed"], ["connect_limiter_failed"]]);
  });

  it("否定: 数え役が false を返したら、例外ではないので 429", async () => {
    const denying = { idFromName: () => ({}), get: () => ({ hit: () => Promise.resolve(false) }) };
    const res = await worker.fetch(request(freshIp()), { ROOMS: env.ROOMS, CONNECT_LIMIT: passing, CONNECT_LIMITER: denying } as unknown as Env);
    expect(res.status).toBe(429);
  });

  it("本物の束縛では 1 回ごとに数えが 1 つ増える", async () => {
    const ip = freshIp();
    await connect(freshRoom("fo"), ip).then((c) => c.close());
    const stamps = await runInDurableObject(limiterFor(ip), (_i, s) => s.storage.get<number[]>("t"));
    expect(stamps).toHaveLength(1);
  });
});
