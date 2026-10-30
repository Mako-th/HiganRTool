// 接続の回数を数えるDurable Object。鍵（IPv4またはIPv6の /64。rules.tsのconnectLimitKey）ごとに1つ（idFromName）。
// 入口（index.ts）がルームのDurable Objectへ渡す前にhit()を1回呼ぶ。数え方と理由はrules.tsのslideConnectWindow。
//
// 許した時刻（最大CONNECT_LIMIT_PER_MINUTE個）をストレージに1つの値として持つ。
// メモリだけにしないのは、使われていないDurable Objectが数秒で退去してメモリ上の回数が消えるため。
// そうなると「上限回接続して退去を待ち、また上限回接続する」ことで、60秒に上限の2倍を通せてしまう。
// 書くのは許した回だけ（叩かれても書き込みが増えない）。古いIPの行は最後の許した回の60秒後のアラームで全部消す。
// ログには何も書かない（鍵はIPなので）。

import { DurableObject } from "cloudflare:workers";
import { CONNECT_WINDOW_MS, slideConnectWindow } from "./rules";

const KEY = "t";

export class ConnectLimiter extends DurableObject<Env> {
  private stamps: number[] = [];

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // 起動時に前の回数を読み直す（読み終わるまで他の要求を受け付けない）。
    void ctx.blockConcurrencyWhile(async () => {
      this.stamps = (await ctx.storage.get<number[]>(KEY)) ?? [];
    });
  }

  /** この鍵の接続を1回数える。許すならtrue、期間の中で上限に達していたらfalse。 */
  async hit(): Promise<boolean> {
    const now = Date.now();
    const r = slideConnectWindow(this.stamps, now);
    this.stamps = r.stamps;
    if (!r.allowed) return false;
    await this.ctx.storage.put(KEY, r.stamps);
    // アラームの有無はストレージから直接調べる。
    // 「行が空 = アラーム無し」と推測すると、putが成功した後にsetAlarmが例外を投げたとき、次回から行があるのにアラームが無い状態になり、行が永久に残る。
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
