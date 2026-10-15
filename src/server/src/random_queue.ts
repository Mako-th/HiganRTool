// ランダム対戦の待ち列 = Durable Object（ビルド番号ごとに 1 つ。idFromName("b<番号>")）。
//
// 来た順に 2 人ずつ組にし、組ごとに推測できない新しいルーム名（rules.ts の newRandomRoomName）を 2 人へ返して閉じる。
// 2 人目が来た時点で組にするので、待っている人は同じビルドにつき高々 1 人。組になった 2 人は、返されたルーム名で
// 今までのルーム（room.ts。/v0/rooms/<名>）へ入り直し、そこで接続コードを渡し合う。
//
// 状態は room.ts と同じく WebSocket の attachment にだけ置く。ログ（console）には何も書かない（ルーム名・IP・cid を出さない）。

import { DurableObject } from "cloudflare:workers";
import {
  CLIENT_ID_PARAM,
  CLOSE_NAME,
  Close,
  PING_TEXT,
  PONG_TEXT,
  RANDOM_QUEUE_TTL_MS,
  type ServerMessage,
  newRandomRoomName,
  parseClientId,
} from "./rules";

interface QueueAtt {
  /** 並び（待ち列に入った順。生きている接続の最大 + 1）。先に入った人が first。 */
  seq: number;
  deadline: number;
  cid: string | null;
  done: boolean;
}

const OPEN = 1;

export class RandomQueue extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(PING_TEXT, PONG_TEXT));
  }

  async fetch(req: Request): Promise<Response> {
    const now = Date.now();
    this.expireIfDue(now);

    // 同じ利用者の入り直し: 届かなかった前の入室が残っていても、自分自身と組にしない。
    const cid = parseClientId(new URL(req.url).searchParams.get(CLIENT_ID_PARAM));
    if (cid !== null) {
      for (const w of this.live()) {
        if (att(w).cid === cid) closeWith(w, Close.Done, false);
      }
    }

    const live = this.live();
    const seq = live.reduce((m, w) => Math.max(m, att(w).seq), -1) + 1;
    const deadline = now + RANDOM_QUEUE_TTL_MS;
    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.ctx.acceptWebSocket(server);
    const a: QueueAtt = { seq, deadline, cid, done: false };
    server.serializeAttachment(a);
    send(server, { t: "queue", expires_at: Math.floor(deadline / 1000) });
    this.tryPair();
    await this.scheduleAlarm();
    return new Response(null, { status: 101, webSocket: client });
  }

  /** 待ち列の人は ping 以外を送らない。 */
  async webSocketMessage(ws: WebSocket, _message: string | ArrayBuffer): Promise<void> {
    if (att(ws).done) return;
    closeWith(ws, Close.BadMessage, true);
    await this.scheduleAlarm();
  }

  async webSocketClose(ws: WebSocket, _code: number, _reason: string, _wasClean: boolean): Promise<void> {
    await this.gone(ws);
  }

  async webSocketError(ws: WebSocket, _error: unknown): Promise<void> {
    await this.gone(ws);
  }

  async alarm(): Promise<void> {
    this.expireIfDue(Date.now());
    await this.scheduleAlarm();
  }

  private live(): WebSocket[] {
    return this.ctx.getWebSockets().filter((w) => w.readyState === OPEN && !att(w).done);
  }

  /** 先に入った 2 人から順に組にする。 */
  private tryPair(): void {
    const waiting = this.live().sort((x, y) => att(x).seq - att(y).seq);
    while (waiting.length >= 2) {
      const first = waiting.shift() as WebSocket;
      const second = waiting.shift() as WebSocket;
      const room = newRandomRoomName();
      send(first, { t: "paired", room, order: "first" });
      send(second, { t: "paired", room, order: "second" });
      closeWith(first, Close.Done, false);
      closeWith(second, Close.Done, false);
    }
  }

  private expireIfDue(now: number): void {
    for (const w of this.live()) {
      if (now >= att(w).deadline) closeWith(w, Close.Expired, true);
    }
  }

  /** 書けなくても入室は失敗にしない（無料枠の使い切りなど）。期限は次の入室の expireIfDue か、掛け直されたアラームで片づく。 */
  private async scheduleAlarm(): Promise<void> {
    try {
      const live = this.live();
      if (live.length === 0) {
        await this.ctx.storage.deleteAlarm();
        return;
      }
      await this.ctx.storage.setAlarm(Math.min(...live.map((w) => att(w).deadline)));
    } catch {
      // 受け付けたソケットを占有者のまま 500 で捨てるより、期限が遅れる方を取る。
    }
  }

  private async gone(ws: WebSocket): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    a.done = true;
    trySerialize(ws, a);
    try {
      ws.close(1000, "bye");
    } catch {
      // 閉じた後の二重の close。
    }
    await this.scheduleAlarm();
  }
}

function att(ws: WebSocket): QueueAtt {
  return ws.deserializeAttachment() as QueueAtt;
}

function trySerialize(ws: WebSocket, a: QueueAtt): void {
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
    // 相手が先に切れた。
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
    // 二重の close。
  }
}
