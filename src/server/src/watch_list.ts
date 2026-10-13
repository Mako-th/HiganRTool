// 進行中のランダム対戦の一覧 = Durable Object（ビルド番号ごとに 1 つ。idFromName("b<番号>")）。口は 2 つ。
//
// - post（載せる口）: 対戦のホストが持ち続ける。最初の 1 通 `post` で、割り当てのルーム名 q.…・1P・2P の名前・同期方式・観戦者の数と上限を
//   送る。サーバーは q.… の形を確かめ、観戦用のルーム名 w.… を自分で導く（任意の w.… を載せられない。ルーム対戦の観戦用のルームを
//   一覧へ漏らさない）。q.… は持たず、一覧にも出さない。切れたら一覧から消す。1 IP（IPv6 は /64）から 2 件まで・全体で 100 件まで。
// - view（見る口）: 観戦者が一覧を開いている間だけ持つ。開いたときに一覧の全体、変わるたびに全体を送る（変化は 1 秒に 1 回にまとめる）。
//
// 状態は WebSocket の attachment にだけ置く（Hibernation で起こされても一覧を作り直せる）。ログ（console）には何も書かない。

import { DurableObject } from "cloudflare:workers";
import {
  CLOSE_NAME,
  Close,
  MESSAGE_MAX_BYTES,
  PING_TEXT,
  PONG_TEXT,
  type ServerMessage,
  WATCH_BROADCAST_MIN_MS,
  WATCH_MAX_PER_IP,
  WATCH_MAX_TOTAL,
  WATCH_STALE_MS,
  WATCH_UNPOSTED_MS,
  type WatchItem,
  connectLimitKey,
  parsePost,
  parseWatchPath,
  spectateRoomName,
} from "./rules";

interface PostAtt {
  kind: "post";
  /** 回数の数えと同じ鍵（IPv4 / IPv6 の /64）。1 IP あたりの件数に使う。 */
  key: string;
  created: number;
  /** 最後に文が届いた時刻。 */
  at: number;
  /** 載せた順（生きている接続の最大 + 1）。 */
  seq: number;
  item: WatchItem | null;
  done: boolean;
}

interface ViewAtt {
  kind: "view";
  done: boolean;
}

type Att = PostAtt | ViewAtt;

const OPEN = 1;
const FLUSH_KEY = "flush";

export class WatchList extends DurableObject<Env> {
  /** 最後に観戦者へ全体を送った時刻（起こされ直したら 0 から。すぐ送る側に倒れる）。 */
  private lastSent = 0;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(PING_TEXT, PONG_TEXT));
  }

  async fetch(req: Request): Promise<Response> {
    const now = Date.now();
    const { kind } = parseWatchPath(new URL(req.url).pathname);
    if (kind === null) return Response.json({ error: "not_found" }, { status: 404 });
    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.ctx.acceptWebSocket(server);
    if (kind === "post") {
      const key = connectLimitKey(req.headers.get("CF-Connecting-IP"));
      const a: PostAtt = { kind: "post", key, created: now, at: now, seq: 0, item: null, done: false };
      server.serializeAttachment(a);
      await this.scheduleAlarm();
    } else {
      const a: ViewAtt = { kind: "view", done: false };
      server.serializeAttachment(a);
      send(server, this.listMessage());
    }
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    if (a.kind === "view") {
      // 見る口は ping（自動応答）以外を送らない。
      closeWith(ws, Close.BadMessage, true);
      return;
    }
    const now = Date.now();
    const o = parseObject(message);
    if (o === Close.TooLarge || o === Close.BadMessage) {
      closeWith(ws, o, true);
      await this.changedIfListed(a);
      return;
    }
    if (o.t === "post" && a.item === null) {
      const p = parsePost(o);
      if (!p.ok) {
        closeWith(ws, Close.BadMessage, true);
        return;
      }
      // await は 1 回だけ。数えてから書くまでの間に別の文を入れない。
      const room = await spectateRoomName(p.room);
      const live = this.livePosts().filter((w) => w !== ws);
      const same = live.find((w) => (att(w) as PostAtt).item?.room === room);
      // 同じ IP からの載せ直し（前の接続が切れたのをまだ知らない）は置き換える。別の IP の同じ部屋は断る。
      if (same !== undefined && (att(same) as PostAtt).key !== a.key) {
        closeWith(ws, Close.BadMessage, true);
        return;
      }
      const rest = live.filter((w) => w !== same && (att(w) as PostAtt).item !== null);
      const mine = rest.filter((w) => (att(w) as PostAtt).key === a.key).length;
      if (rest.length >= WATCH_MAX_TOTAL || mine >= WATCH_MAX_PER_IP) {
        closeWith(ws, Close.ListFull, true);
        return;
      }
      if (same !== undefined) closeWith(same, Close.Done, false);
      const seq = live.reduce((m, w) => Math.max(m, (att(w) as PostAtt).seq), -1) + 1;
      a.item = { room, p1: p.p1, p2: p.p2, mode: p.mode, delay: p.delay, watchers: p.watchers, cap: p.cap };
      a.seq = seq;
      a.at = now;
      ws.serializeAttachment(a);
      send(ws, { t: "listed" });
      await this.changed(now);
      return;
    }
    if (o.t === "count" && a.item !== null) {
      const w = o.watchers;
      if (typeof w !== "number" || !Number.isInteger(w) || w < 0 || w > a.item.cap) {
        closeWith(ws, Close.BadMessage, true);
        await this.changed(now);
        return;
      }
      a.at = now;
      const differs = a.item.watchers !== w;
      a.item.watchers = w;
      ws.serializeAttachment(a);
      if (differs) await this.changed(now);
      return;
    }
    closeWith(ws, Close.BadMessage, true);
    await this.changedIfListed(a);
  }

  async webSocketClose(ws: WebSocket, _code: number, _reason: string, _wasClean: boolean): Promise<void> {
    await this.gone(ws);
  }

  async webSocketError(ws: WebSocket, _error: unknown): Promise<void> {
    await this.gone(ws);
  }

  async alarm(): Promise<void> {
    const now = Date.now();
    let changed = false;
    for (const w of this.livePosts()) {
      const a = att(w) as PostAtt;
      if (now >= this.deadline(w, a)) {
        closeWith(w, Close.Expired, true);
        changed ||= a.item !== null;
      }
    }
    const flush = (await this.ctx.storage.get(FLUSH_KEY)) !== undefined;
    if (flush) await this.ctx.storage.delete(FLUSH_KEY);
    if (flush || changed) this.broadcast(now);
    await this.scheduleAlarm();
  }

  private livePosts(): WebSocket[] {
    return this.ctx.getWebSockets().filter((w) => w.readyState === OPEN && att(w).kind === "post" && !att(w).done);
  }

  private liveViews(): WebSocket[] {
    return this.ctx.getWebSockets().filter((w) => w.readyState === OPEN && att(w).kind === "view" && !att(w).done);
  }

  private listMessage(): ServerMessage {
    const items = this.livePosts()
      .map((w) => att(w) as PostAtt)
      .filter((a) => a.item !== null)
      .sort((x, y) => x.seq - y.seq)
      .map((a) => ({ ...(a.item as WatchItem) }));
    return { t: "list", items };
  }

  /** 載せた人が切れる・断られる。一覧に載っていたなら観戦者へ知らせる。 */
  private async gone(ws: WebSocket): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    a.done = true;
    try {
      ws.serializeAttachment(a);
    } catch {
      // 閉じた WebSocket には書けないことがある。livePosts の readyState でも落ちる。
    }
    try {
      ws.close(1000, "bye");
    } catch {
      // 閉じた後の二重の close。
    }
    await this.changedIfListed(a);
  }

  private async changedIfListed(a: Att): Promise<void> {
    if (a.kind === "post" && a.item !== null) await this.changed(Date.now());
    else await this.scheduleAlarm();
  }

  /** 一覧が変わった。前の送りから最短の間隔が空いていればすぐ、空いていなければアラームでまとめて送る。 */
  private async changed(now: number): Promise<void> {
    if (now - this.lastSent >= WATCH_BROADCAST_MIN_MS) {
      this.broadcast(now);
    } else {
      await this.ctx.storage.put(FLUSH_KEY, 1);
    }
    await this.scheduleAlarm();
  }

  private broadcast(now: number): void {
    this.lastSent = now;
    const m = this.listMessage();
    for (const v of this.liveViews()) send(v, m);
  }

  private deadline(ws: WebSocket, a: PostAtt): number {
    if (a.item === null) return a.created + WATCH_UNPOSTED_MS;
    let last = a.at;
    try {
      const auto = this.ctx.getWebSocketAutoResponseTimestamp(ws);
      if (auto !== null) last = Math.max(last, auto.getTime());
    } catch {
      // 取れなければ最後の文の時刻だけで数える。
    }
    return last + WATCH_STALE_MS;
  }

  private async scheduleAlarm(): Promise<void> {
    const times: number[] = this.livePosts().map((w) => this.deadline(w, att(w) as PostAtt));
    if ((await this.ctx.storage.get(FLUSH_KEY)) !== undefined) {
      times.push(this.lastSent + WATCH_BROADCAST_MIN_MS);
    }
    if (times.length === 0) {
      await this.ctx.storage.deleteAlarm();
      return;
    }
    await this.ctx.storage.setAlarm(Math.min(...times));
  }
}

function att(ws: WebSocket): Att {
  return ws.deserializeAttachment() as Att;
}

/** 届いた 1 通を JSON の物に読む。大きさを最初に見る（大きなものを JSON として解かない）。 */
function parseObject(data: string | ArrayBuffer): Record<string, unknown> | typeof Close.TooLarge | typeof Close.BadMessage {
  if (typeof data !== "string") return Close.BadMessage;
  if (new TextEncoder().encode(data).byteLength > MESSAGE_MAX_BYTES) return Close.TooLarge;
  let v: unknown;
  try {
    v = JSON.parse(data);
  } catch {
    return Close.BadMessage;
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) return Close.BadMessage;
  return v as Record<string, unknown>;
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
  try {
    ws.serializeAttachment(a);
  } catch {
    // 閉じた WebSocket には書けないことがある。
  }
  const name = CLOSE_NAME[code] ?? "bad_message";
  if (withError && name !== "done") send(ws, { t: "error", code: name });
  try {
    ws.close(code, name);
  } catch {
    // 二重の close。
  }
}
