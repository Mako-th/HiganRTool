// 繋ぐ回数を数える Durable Object。鍵（IPv4 または IPv6 の /64。rules.ts の connectLimitKey）ごとに 1 つ（idFromName）。
// 入口（index.ts）がルームの Durable Object へ渡す前に hit() を 1 回呼ぶ。数え方と理由は rules.ts の slideConnectWindow。
//
// 許した時刻（最大 CONNECT_LIMIT_PER_MINUTE 個）をストレージに 1 つの値として持つ。メモリだけにしないのは、使われていない
// Durable Object は数秒で退去してメモリの数えが消え、「上限回つないで、退去を待って次の上限回」で 60 秒に上限の 2 倍通せてしまうため。
// 書くのは許した回だけ（叩かれても書き込みが増えない）。古い IP の行は、最後の許した回の 60 秒後のアラームで全部消す。
// ログには何も書かない（鍵は IP なので）。

import { DurableObject } from "cloudflare:workers";
import { CONNECT_WINDOW_MS, slideConnectWindow } from "./rules";

const KEY = "t";

export class ConnectLimiter extends DurableObject<Env> {
  private stamps: number[] = [];

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // 起きたときに前の数えを読み直す（読み終わるまで他の要求を入れない）。
    void ctx.blockConcurrencyWhile(async () => {
      this.stamps = (await ctx.storage.get<number[]>(KEY)) ?? [];
    });
  }

  /** この鍵の接続を 1 回数える。許すなら true、期間の中で上限に達していたら false。 */
  async hit(): Promise<boolean> {
    const now = Date.now();
    const r = slideConnectWindow(this.stamps, now);
    this.stamps = r.stamps;
    if (!r.allowed) return false;
    await this.ctx.storage.put(KEY, r.stamps);
    // アラームの有無は記憶から直に見る。「行が空 = アラーム無し」と推測すると、put の成功後に setAlarm が投げた回の次から、
    // 行があるのにアラームが無くなり、行が永久に残る。
    if ((await this.ctx.storage.getAlarm()) === null) await this.ctx.storage.setAlarm(now + CONNECT_WINDOW_MS);
    return true;
  }

  async alarm(): Promise<void> {
    const now = Date.now();
    const live = this.stamps.filter((t) => t > now - CONNECT_WINDOW_MS);
    this.stamps = live;
    if (live.length === 0) {
      await this.ctx.storage.deleteAll();
      return;
    }
    await this.ctx.storage.put(KEY, live);
    await this.ctx.storage.setAlarm(Math.min(...live) + CONNECT_WINDOW_MS);
  }
}
