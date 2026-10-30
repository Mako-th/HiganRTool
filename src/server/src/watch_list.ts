// 進行中のランダム対戦の一覧 = Durable Object（ビルド番号ごとに1つ。idFromName("b<番号>")）。エンドポイントは2つ。
//
// - post（載せるエンドポイント）: 対戦のホストが接続を持ち続ける。最初の1通`post`で次を送る。
//   割り当てのルーム名q.…・1P・2Pの名前・同期方式・観戦者の数と上限・そのルームで受けた席のチケット。
//   サーバーはq.… の形をチェックし、観戦用のルーム名w.… を自分で導く。任意のw.… は載せられず、ルーム対戦の観戦用のルームを一覧に載せない。
//   チケットがそのw.… について正しく期限内でなければ断る（対戦なしの投稿で枠を埋めさせない）。
//   q.… は保持せず一覧にも出さない。切れたら一覧から消す。1 IP（IPv6は /64）から2件まで・全体で100件まで。
// - view（見るエンドポイント）: 観戦者が一覧を開いている間だけ接続を持つ。開いたときと変わるたびに一覧の全体を送る（変化は1秒に1回にまとめる）。
//   全体で500本・同じ鍵から3本まで（4本目は同じ鍵の一番古いものを閉じる）・作ってから10分でexpiredで閉じる（クライアントは入り直す）。
//
// 状態はWebSocketのattachmentにだけ保存する（Hibernationから復帰しても一覧を作り直せる）。ログ（console）には何も書かない。

import { DurableObject } from "cloudflare:workers";
import {
  CLOSE_NAME,
  Close,
  MESSAGE_MAX_BYTES,
  PING_TEXT,
  PONG_TEXT,
  type ServerMessage,
  WATCH_BROADCAST_MIN_MS,
  WATCH_COUNT_BURST,
  WATCH_COUNT_WINDOW_MS,
  WATCH_MAX_PER_IP,
  WATCH_MAX_TOTAL,
  WATCH_STALE_MS,
  WATCH_UNPOSTED_MS,
  WATCH_VIEW_MAX_PER_KEY,
  WATCH_VIEW_MAX_TOTAL,
  WATCH_VIEW_SWEEP_MS,
  WATCH_VIEW_TTL_MS,
  type WatchItem,
  connectLimitKey,
  parsePost,
  parseWatchPath,
  spectateRoomName,
} from "./rules";
import { judgeTicket, seatKey } from "./ticket";

interface PostAtt {
  kind: "post";
  /** 回数を数える鍵と同じ鍵（IPv4 / IPv6の /64）。1 IPあたりの件数に使う。 */
  key: string;
  created: number;
  /** 最後に文が届いた時刻。 */
  at: number;
  /** 載せた順（生きている接続の最大 + 1）。 */
  seq: number;
  item: WatchItem | null;
  done: boolean;
  /** countを数える期間の始まりとその期間に受けた数。 */
  cw?: number;
  cn?: number;
}

interface ViewAtt {
  kind: "view";
  done: boolean;
  /** 同じ鍵（IPv4 / IPv6の /64）の本数を数えるのと期限（createdから）の判定に使う。古いattachmentでは未定義。 */
  key?: string;
  created?: number;
}

type Att = PostAtt | ViewAtt;

const OPEN = 1;

export class WatchList extends DurableObject<Env> {
  /** 最後に観戦者へ全体を送った時刻（復帰し直したら0から。すぐ送る側に倒れる）。 */
  private lastSent = 0;
  /**
   * まとめて送る変化が残っているかどうかのフラグ。ストレージには書かない（書き込みの枠を荒らしに使わせない）。
   * 追い出されて失っても復帰し直した最初のアラームで必ず全体を送る（alarm）ので、観戦者の表示が古いままになることはない。
   */
  private flushPending = false;
  /** 掛けてあるアラームの時刻。undefinedは未確認（復帰し直した直後）。nullは無い。 */
  private alarmAt: number | null | undefined;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(PING_TEXT, PONG_TEXT));
  }

  async fetch(req: Request): Promise<Response> {
    const now = Date.now();
    const { kind } = parseWatchPath(new URL(req.url).pathname);
    if (kind === null) return Response.json({ error: "not_found" }, { status: 404 });
    const key = connectLimitKey(req.headers.get("CF-Connecting-IP"));
    if (kind === "view") {
      const views = this.liveViews();
      const mine = views.filter((w) => (att(w) as ViewAtt).key === key);
      if (mine.length >= WATCH_VIEW_MAX_PER_KEY) {
        // 同じ鍵の一番古い接続を閉じて受け入れる（他の鍵の接続は閉じない。閉じた分で全体は増えない）。
        const oldest = mine.reduce((o, w) => ((att(w) as ViewAtt).created ?? 0) < ((att(o) as ViewAtt).created ?? 0) ? w : o);
        closeWith(oldest, Close.Done, false);
      } else if (views.length >= WATCH_VIEW_MAX_TOTAL) {
        return Response.json({ error: "room_full" }, { status: 409 });
      }
    }
    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.ctx.acceptWebSocket(server);
    if (kind === "post") {
      const a: PostAtt = { kind: "post", key, created: now, at: now, seq: 0, item: null, done: false };
      server.serializeAttachment(a);
      await this.scheduleAlarm();
    } else {
      const a: ViewAtt = { kind: "view", done: false, key, created: now };
      server.serializeAttachment(a);
      send(server, this.listMessage());
      await this.scheduleAlarm();
    }
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    if (a.kind === "view") {
      // viewエンドポイントはping（自動応答）以外を送らない。
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
      // awaitは数える前に済ませる。数えてから書くまでの間に別の文が割り込まないようにする。
      const room = await spectateRoomName(p.room);
      // その割り当ての名で本当に対戦が成立した証拠（席のチケット）が無ければ載せない。SEAT_KEYが無いときは全部断る。
      const ticket = await judgeTicket(await seatKey(this.env.SEAT_KEY), room, p.ticket, Math.floor(now / 1000));
      if (ticket.kind !== "valid") {
        closeWith(ws, Close.BadMessage, true);
        return;
      }
      const live = this.livePosts().filter((w) => w !== ws);
      const same = live.find((w) => (att(w) as PostAtt).item?.room === room);
      // 同じIPからの載せ直し（前の接続が切れたのをまだ知らない）は置き換える。別のIPの同じ部屋は断る。
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
      if (a.cw === undefined || now - a.cw >= WATCH_COUNT_WINDOW_MS) {
        a.cw = now;
        a.cn = 0;
      }
      a.cn = (a.cn ?? 0) + 1;
      if (a.cn > WATCH_COUNT_BURST) {
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
    for (const w of this.liveViews()) {
      if (now >= viewDeadline(att(w) as ViewAtt)) closeWith(w, Close.Expired, true);
    }
    // lastSentが0なら復帰し直した後なので、残っていたかもしれないフラグを失っている。念のため全体を送る。
    const flush = this.flushPending || this.lastSent === 0;
    this.flushPending = false;
    this.alarmAt = null;
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
      // 閉じたWebSocketには書き込めないことがある。livePostsのreadyStateで除いても例外が出る。
    }
    try {
      ws.close(1000, "bye");
    } catch {
      // 閉じた後の二重のclose。
    }
    await this.changedIfListed(a);
  }

  private async changedIfListed(a: Att): Promise<void> {
    if (a.kind === "post" && a.item !== null) await this.changed(Date.now());
    else await this.scheduleAlarm();
  }

  /** 一覧が変わった。前の送信から最短の間隔が空いていればすぐ送る。空いていなければアラームでまとめて送る。 */
  private async changed(now: number): Promise<void> {
    if (now - this.lastSent >= WATCH_BROADCAST_MIN_MS) {
      this.broadcast(now);
    } else {
      this.flushPending = true;
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
      // 取得できなければ最後の文の時刻だけで数える。
    }
    return last + WATCH_STALE_MS;
  }

  /**
   * アラームは今の予定より早めるときだけ掛ける（setAlarmは書き込みに数えられる）。
   * 予定が早すぎても鳴ったときにalarmが掛け直す。書き込めなくても入室・載せる操作は失敗にしない（期限の掃除は次の操作かアラームで遅れて済む）。
   */
  private async scheduleAlarm(): Promise<void> {
    try {
      const times: number[] = this.livePosts().map((w) => this.deadline(w, att(w) as PostAtt));
      for (const w of this.liveViews()) times.push(viewDeadline(att(w) as ViewAtt));
      if (this.flushPending) times.push(this.lastSent + WATCH_BROADCAST_MIN_MS);
      if (this.alarmAt === undefined) this.alarmAt = await this.ctx.storage.getAlarm();
      if (times.length === 0) {
        if (this.alarmAt !== null) {
          await this.ctx.storage.deleteAlarm();
          this.alarmAt = null;
        }
        return;
      }
      const at = Math.min(...times);
      if (this.alarmAt === null || at < this.alarmAt) {
        await this.ctx.storage.setAlarm(at);
        this.alarmAt = at;
      }
    } catch {
      this.alarmAt = undefined;
    }
  }
}

/** 作ってからWATCH_VIEW_TTL_MS後を掃除の間隔に切り上げた時刻。createdが無い古いattachmentは次のアラームで閉じる。 */
function viewDeadline(a: ViewAtt): number {
  return Math.ceil(((a.created ?? 0) + WATCH_VIEW_TTL_MS) / WATCH_VIEW_SWEEP_MS) * WATCH_VIEW_SWEEP_MS;
}

function att(ws: WebSocket): Att {
  return ws.deserializeAttachment() as Att;
}

/** 届いた1通をJSONとして読む。大きさを最初に見る（大きなものをJSONとして解かない）。 */
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
    // 閉じたWebSocketには書き込めないことがある。
  }
  const name = CLOSE_NAME[code] ?? "bad_message";
  if (withError && name !== "done") send(ws, { t: "error", code: name });
  try {
    ws.close(code, name);
  } catch {
    // 二重のclose。
  }
}
