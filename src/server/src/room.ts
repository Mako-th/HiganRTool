// ルーム 1 つ = Durable Object 1 つ（ルーム名から idFromName）。1 ルーム 2 人（始める側 host・入る側 guest）。
//
// 状態は WebSocket の attachment にだけ置く（ストレージにもインスタンスの変数にも置かない）。Hibernation で眠ると変数は消えるが
// attachment は残り、接続コード（認証の秘密を含む）も WebSocket が閉じれば消える。ストレージに書くのは期限のアラームだけ。
// ログ（console）には何も書かない（コードも、利用者の打ったルーム名も出さない）。

import { DurableObject } from "cloudflare:workers";
import {
  ANSWER_TTL_MS,
  CLIENT_ID_PARAM,
  CLOSE_NAME,
  Close,
  PING_TEXT,
  PONG_TEXT,
  type ServerMessage,
  parseClientId,
  parseClientMessage,
  roomTtlForPath,
} from "./rules";

type Role = "host" | "guest";

/** コードは 1024 文字までなので 1.1 KB ほど。attachment の上限は同梱の workerd（1.20260815）で 16384 B（ローカルで測った。本番は未確認）。 */
interface Att {
  role: Role;
  /** ルームの期限（UNIX ミリ秒）。host と guest で同じ値。 */
  deadline: number;
  /** host だけ: 預かった始める側のコード。 */
  code: string | null;
  /** guest だけ: host のコードを渡したか。 */
  gotPeer: boolean;
  /** こちらから閉じた（または相手の close を受けた）印。二重に後始末しないため。 */
  done: boolean;
  /** クライアントの起動ごとの乱数（問い cid）。付けない古いクライアントは null（古い attachment では未定義）。 */
  cid: string | null;
  /** host だけ: 相手が揃ったことを知らせた（matched を送った）。guest が入れ替わっても 2 度は送らない。 */
  matched: boolean;
}

const OPEN = 1;

export class Room extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // 生存の確かめは Durable Object を起こさずに返す（起きた時間だけ課金されるため）。
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(PING_TEXT, PONG_TEXT));
  }

  async fetch(req: Request): Promise<Response> {
    const now = Date.now();
    // アラームが遅れていても、期限の過ぎたルームに人を入れない（期限切れを先に片付け、新しいルームとして始める）。
    this.expireIfDue(now);

    // 同じ利用者の入り直し: 届かなかった前の入室が残っていても 3 人目にしない（クライアントは届く前に失敗扱いにして入り直す）。
    const cid = parseClientId(new URL(req.url).searchParams.get(CLIENT_ID_PARAM));
    if (cid !== null) await this.dropStale(cid);

    const live = this.live();
    let role: Role;
    let deadline: number;
    if (live.length === 0) {
      role = "host";
      deadline = now + roomTtlForPath(new URL(req.url).pathname);
    } else if (live.length === 1 && att(live[0]).role === "host") {
      role = "guest";
      deadline = att(live[0]).deadline;
    } else {
      // 3 人目。WebSocket を張らずに断る（張ってから断ると、その分だけ起きている時間が延びる）。
      return Response.json({ error: "room_full" }, { status: 409 });
    }

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.ctx.acceptWebSocket(server, [role]);
    const a: Att = { role, deadline, code: null, gotPeer: false, done: false, cid, matched: false };
    server.serializeAttachment(a);
    send(server, { t: "room", role, expires_at: toSecs(deadline) });
    if (role === "guest") this.tryMatch(now);
    await this.scheduleAlarm();
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    const p = parseClientMessage(message);
    if (p.kind === "error") {
      await this.fail(ws, p.close);
      return;
    }
    if (p.kind === "offer") {
      // offer は host が 1 回だけ。guest のコードのタグは host のコードから作る鍵で付くので、先に host のコードが要る。
      if (a.role !== "host" || a.code !== null) {
        await this.fail(ws, Close.BadMessage);
        return;
      }
      a.code = p.code;
      ws.serializeAttachment(a);
      this.tryMatch(Date.now());
      await this.scheduleAlarm();
      return;
    }
    if (a.role !== "guest" || !a.gotPeer) {
      await this.fail(ws, Close.BadMessage);
      return;
    }
    const host = this.live().find((w) => att(w).role === "host");
    if (host === undefined) {
      // host が切れると guest も閉じる（onGone）ので、ここへは来ない筋。来たら相手がいないと答える。
      await this.fail(ws, Close.PeerLeft);
      return;
    }
    send(host, { t: "peer", code: p.code });
    closeWith(host, Close.Done, false);
    closeWith(ws, Close.Done, false);
    await this.ctx.storage.deleteAlarm();
  }

  async webSocketClose(ws: WebSocket, _code: number, _reason: string, _wasClean: boolean): Promise<void> {
    await this.gone(ws);
  }

  async webSocketError(ws: WebSocket, _error: unknown): Promise<void> {
    await this.gone(ws);
  }

  /** ルームの期限が過ぎていれば全員を閉じ、まだなら掛け直す（前のルームの古いアラーム）。 */
  async alarm(): Promise<void> {
    const now = Date.now();
    if (this.expireIfDue(now)) {
      await this.ctx.storage.deleteAlarm();
      return;
    }
    await this.scheduleAlarm();
  }

  private live(): WebSocket[] {
    return this.ctx.getWebSockets().filter((w) => w.readyState === OPEN && !att(w).done);
  }

  private tryMatch(now: number): void {
    const live = this.live();
    const host = live.find((w) => att(w).role === "host");
    const guest = live.find((w) => att(w).role === "guest");
    if (host === undefined || guest === undefined) return;
    const h = att(host);
    const g = att(guest);
    if (h.code === null || g.gotPeer) return;
    // 揃ってからは answer の期限で縛る（ルームの期限より早ければ）。両者に同じ値を持たせる。
    // guest が入れ替わった 2 度目は、host に matched をもう 1 度送ると順番違いで落とされるので、期限も host のまま延ばさない。
    const first = h.matched !== true;
    if (first) {
      h.deadline = Math.min(h.deadline, now + ANSWER_TTL_MS);
      h.matched = true;
      host.serializeAttachment(h);
    }
    g.deadline = h.deadline;
    g.gotPeer = true;
    guest.serializeAttachment(g);
    send(guest, { t: "peer", code: h.code });
    if (first) send(host, { t: "matched", expires_at: toSecs(h.deadline) });
  }

  /**
   * 同じ cid の古い接続を静かに閉じる。guest なら host はそのまま待つ（同じ利用者なので、host のコードを知っても害は無い）。
   * host なら、その host のコードはもう使えないので guest も閉じる。
   */
  private async dropStale(cid: string): Promise<void> {
    for (const w of this.live()) {
      const a = att(w);
      if (a.cid !== cid) continue;
      closeWith(w, Close.Done, false);
      if (a.role === "host") await this.afterLeave(w);
    }
  }

  private expireIfDue(now: number): boolean {
    const live = this.live();
    if (live.length === 0) return false;
    const deadline = Math.min(...live.map((w) => att(w).deadline));
    if (now < deadline) return false;
    for (const w of live) closeWith(w, Close.Expired, true);
    return true;
  }

  private async scheduleAlarm(): Promise<void> {
    const live = this.live();
    if (live.length === 0) {
      await this.ctx.storage.deleteAlarm();
      return;
    }
    await this.ctx.storage.setAlarm(Math.min(...live.map((w) => att(w).deadline)));
  }

  private async fail(ws: WebSocket, code: number): Promise<void> {
    closeWith(ws, code, true);
    await this.afterLeave(ws);
  }

  private async gone(ws: WebSocket): Promise<void> {
    const a = att(ws);
    if (a.done) return; // こちらから閉じたもの（後始末は済み）。
    a.done = true;
    trySerialize(ws, a);
    try {
      // 閉じる返事を返す（互換日付によっては自動で返らないため。二重でも害は無い）。
      ws.close(1000, "bye");
    } catch {
    }
    await this.afterLeave(ws);
  }

  /**
   * 1 人が抜けた後。host が抜けたら guest も閉じる。guest が抜けたら、host のコードを渡した後なら host も閉じる
   * （抜けた人がそのコードの秘密を知っているので、次の guest に同じコードを渡さない）。渡す前なら host は待つ。
   */
  private async afterLeave(ws: WebSocket): Promise<void> {
    const a = att(ws);
    for (const other of this.live()) {
      if (other === ws) continue;
      if (a.role === "host" || a.gotPeer) closeWith(other, Close.PeerLeft, true);
    }
    await this.scheduleAlarm();
  }
}

function att(ws: WebSocket): Att {
  return ws.deserializeAttachment() as Att;
}

function trySerialize(ws: WebSocket, a: Att): void {
  try {
    ws.serializeAttachment(a);
  } catch {
    // 閉じた WebSocket には書けないことがある。done は live() の readyState でも落ちる。
  }
}

function send(ws: WebSocket, m: ServerMessage): void {
  try {
    ws.send(JSON.stringify(m));
  } catch {
  }
}

function closeWith(ws: WebSocket, code: number, withError: boolean): void {
  const a = att(ws);
  a.done = true;
  trySerialize(ws, a);
  const name = CLOSE_NAME[code] ?? "bad_message";
  if (withError && name !== "done") send(ws, { t: "error", code: name });
  try {
    ws.close(code, name);
  } catch {
  }
}

function toSecs(ms: number): number {
  return Math.floor(ms / 1000);
}
