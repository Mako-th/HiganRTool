// ルーム名ごとの一覧 = Durable Object（`b<ビルド番号>:<揃えた名>`ごとに1つ。入口はindex.tsの/v0/lobby/<ビルド番号>/<ルーム名>）。
//
// 接続は最初は見るだけ。開くと一覧の全体が届く。変わるたびに全体が届く（変化は1秒に1回にまとめる）。
// 最初の1通で次のどちらかを送るとその接続は一覧に載る側になる（以後は一覧を受け取らない）。
// - create: 待機中の1件。割り当てのルーム名r.…を返す。作った人はその名の2席のルームで待ち、この接続を持ち続ける。
// - post: 試合中の1件。マッチのホストが成立の後に送る。r.…の形・席のチケット（観戦用のw.…について）を見て、観戦用のルーム名w.…を載せる。
//   同じr.…の待機中の1件があれば外す。載せた後はcountで観戦者の数を送る。
// 載せた接続は切れたら一覧から消える。ping（自動応答）が途絶えれば閉じる。見る接続は作ってから10分で閉じる。
// 割り当てのルーム名・チケットは一覧の試合中の行に出さない。
//
// 状態はWebSocketのattachmentにだけ保存する（Hibernationから復帰しても一覧を作り直せる）。ログ（console）には何も書かない。

import { DurableObject } from "cloudflare:workers";
import {
  Close,
  LOBBY_MAX_PER_KEY,
  LOBBY_MAX_TOTAL,
  LOBBY_VIEW_MAX_PER_KEY,
  LOBBY_VIEW_MAX_TOTAL,
  LOBBY_VIEW_TTL_MS,
  type LobbyItem,
  PING_TEXT,
  PONG_TEXT,
  type ServerMessage,
  WATCH_BROADCAST_MIN_MS,
  WATCH_COUNT_BURST,
  WATCH_COUNT_WINDOW_MS,
  WATCH_STALE_MS,
  connectLimitKey,
  newLobbyRoomName,
  parseCreate,
  parseLobbyPost,
  spectateRoomName,
} from "./rules";
import { judgeTicket, seatKey } from "./ticket";
import { OPEN, closeWith, parseObject, send, sweepDeadline } from "./ws_util";

interface Base {
  done: boolean;
  /** 回数を数える鍵と同じ鍵（IPv4 / IPv6の /64）。 */
  key: string;
  created: number;
}

interface ViewAtt extends Base {
  kind: "view";
}

interface Listed extends Base {
  /** 最後に文が届いた時刻。 */
  at: number;
  /** 載せた順（生きている接続の最大 + 1）。 */
  seq: number;
}

interface WaitAtt extends Listed {
  kind: "wait";
  name: string;
  /** 割り当てのルーム名r.…。 */
  room: string;
}

interface PlayItem {
  /** 観戦用のルーム名w.…。割り当てのr.…は持たない。 */
  room: string;
  p1: string;
  p2: string;
  mode: string;
  delay: number;
  watch: boolean;
  watchers: number;
  cap: number;
}

interface PlayAtt extends Listed {
  kind: "play";
  item: PlayItem;
  /** countを数える期間の始まりとその期間に受けた数。 */
  cw?: number;
  cn?: number;
}

type Att = ViewAtt | WaitAtt | PlayAtt;

export class Lobby extends DurableObject<Env> {
  /** 最後に見る接続へ全体を送った時刻（復帰し直したら0から。すぐ送る側に倒れる）。 */
  private lastSent = 0;
  /** まとめて送る変化が残っているかどうか。ストレージには書かない（失っても復帰後の最初のアラームで全体を送る）。 */
  private flushPending = false;
  /** 掛けてあるアラームの時刻。undefinedは未確認（復帰し直した直後）。nullは無い。 */
  private alarmAt: number | null | undefined;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(PING_TEXT, PONG_TEXT));
  }

  async fetch(req: Request): Promise<Response> {
    const now = Date.now();
    const key = connectLimitKey(req.headers.get("CF-Connecting-IP"));
    const views = this.views();
    const mine = views.filter((w) => att(w).key === key);
    if (mine.length >= LOBBY_VIEW_MAX_PER_KEY) {
      // 同じ鍵の一番古い接続を閉じて受け入れる（他の鍵の接続は閉じない。閉じた分で全体は増えない）。
      const oldest = mine.reduce((o, w) => (att(w).created < att(o).created ? w : o));
      closeWith(oldest, Close.Done, false);
    } else if (views.length >= LOBBY_VIEW_MAX_TOTAL) {
      return Response.json({ error: "room_full" }, { status: 409 });
    }
    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.ctx.acceptWebSocket(server);
    const a: ViewAtt = { kind: "view", done: false, key, created: now };
    server.serializeAttachment(a);
    send(server, this.listMessage());
    await this.scheduleAlarm();
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    const now = Date.now();
    const o = parseObject(message);
    if (o === Close.TooLarge || o === Close.BadMessage) {
      closeWith(ws, o, true);
      await this.changedIfListed(a);
      return;
    }
    if (a.kind === "view" && o.t === "create") {
      await this.onCreate(ws, a, o, now);
      return;
    }
    if (a.kind === "view" && o.t === "post") {
      await this.onPost(ws, a, o, now);
      return;
    }
    if (a.kind === "play" && o.t === "count") {
      await this.onCount(ws, a, o, now);
      return;
    }
    closeWith(ws, Close.BadMessage, true);
    await this.changedIfListed(a);
  }

  private async onCreate(ws: WebSocket, a: ViewAtt, o: Record<string, unknown>, now: number): Promise<void> {
    const c = parseCreate(o);
    if (!c.ok) {
      closeWith(ws, Close.BadMessage, true);
      return;
    }
    const live = this.items();
    if (isFull(live, a.key)) {
      closeWith(ws, Close.ListFull, true);
      return;
    }
    const room = newLobbyRoomName();
    const next: WaitAtt = { kind: "wait", done: false, key: a.key, created: a.created, at: now, seq: nextSeq(live), name: c.name, room };
    ws.serializeAttachment(next);
    send(ws, { t: "created", room });
    await this.changed(now);
  }

  private async onPost(ws: WebSocket, a: ViewAtt, o: Record<string, unknown>, now: number): Promise<void> {
    const p = parseLobbyPost(o);
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
    // 待っている間に同じ接続の別の文が先に済んでいたらその文が決めた形を変えない。
    if (att(ws).kind !== "view" || att(ws).done) {
      closeWith(ws, Close.BadMessage, true);
      return;
    }
    const live = this.items();
    // 同じr.…の待機中の1件は作った人が誰でも外す。
    const wait = live.find((w) => {
      const x = att(w);
      return x.kind === "wait" && x.room === p.room;
    });
    const same = live.find((w) => {
      const x = att(w);
      return x.kind === "play" && x.item.room === room;
    });
    // 同じ鍵からの載せ直し（前の接続が切れたのをまだ知らない）は置き換える。別の鍵の同じ試合は断る。
    if (same !== undefined && att(same).key !== a.key) {
      closeWith(ws, Close.BadMessage, true);
      return;
    }
    const rest = live.filter((w) => w !== wait && w !== same);
    if (isFull(rest, a.key)) {
      closeWith(ws, Close.ListFull, true);
      return;
    }
    if (wait !== undefined) closeWith(wait, Close.Done, false);
    if (same !== undefined) closeWith(same, Close.Done, false);
    const item: PlayItem = { room, p1: p.p1, p2: p.p2, mode: p.mode, delay: p.delay, watch: p.watch, watchers: p.watchers, cap: p.cap };
    const next: PlayAtt = { kind: "play", done: false, key: a.key, created: a.created, at: now, seq: nextSeq(live), item };
    ws.serializeAttachment(next);
    send(ws, { t: "listed" });
    await this.changed(now);
  }

  private async onCount(ws: WebSocket, a: PlayAtt, o: Record<string, unknown>, now: number): Promise<void> {
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
    for (const w of this.items()) {
      if (now >= this.deadline(w, att(w) as WaitAtt | PlayAtt)) {
        closeWith(w, Close.Expired, true);
        changed = true;
      }
    }
    for (const w of this.views()) {
      if (now >= viewDeadline(att(w) as ViewAtt)) closeWith(w, Close.Expired, true);
    }
    // lastSentが0なら復帰し直した後なので残っていたかもしれないフラグを失っている。念のため全体を送る。
    const flush = this.flushPending || this.lastSent === 0;
    this.flushPending = false;
    this.alarmAt = null;
    if (flush || changed) this.broadcast(now);
    await this.scheduleAlarm();
  }

  private live(): WebSocket[] {
    return this.ctx.getWebSockets().filter((w) => w.readyState === OPEN && !att(w).done);
  }

  /** 一覧に載っている接続（待機中と試合中）。 */
  private items(): WebSocket[] {
    return this.live().filter((w) => att(w).kind !== "view");
  }

  private views(): WebSocket[] {
    return this.live().filter((w) => att(w).kind === "view");
  }

  /** 待機中（来た順）→ 試合中（来た順）。 */
  private listMessage(): ServerMessage {
    const all = this.items().map((w) => att(w) as WaitAtt | PlayAtt);
    const bySeq = (x: Listed, y: Listed) => x.seq - y.seq;
    const waits = all.filter((a): a is WaitAtt => a.kind === "wait").sort(bySeq);
    const plays = all.filter((a): a is PlayAtt => a.kind === "play").sort(bySeq);
    const items: LobbyItem[] = [
      ...waits.map((a): LobbyItem => ({ kind: "wait", name: a.name, room: a.room })),
      ...plays.map(
        (a): LobbyItem => ({
          kind: "play",
          p1: a.item.p1,
          p2: a.item.p2,
          room: a.item.room,
          watch: a.item.watch,
          watchers: a.item.watchers,
          cap: a.item.cap,
          mode: a.item.mode,
          delay: a.item.delay,
        }),
      ),
    ];
    return { t: "list", items };
  }

  /** 載せた人の接続が切れる・断られる。一覧に載っていたなら見る接続へ知らせる。 */
  private async gone(ws: WebSocket): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    a.done = true;
    try {
      ws.serializeAttachment(a);
    } catch {
      // 閉じたWebSocketには書き込めないことがある。liveのreadyStateで除いても例外が出る。
    }
    try {
      ws.close(1000, "bye");
    } catch {
      // 閉じた後の二重のclose。
    }
    await this.changedIfListed(a);
  }

  private async changedIfListed(a: Att): Promise<void> {
    if (a.kind !== "view") await this.changed(Date.now());
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
    for (const v of this.views()) send(v, m);
  }

  private deadline(ws: WebSocket, a: WaitAtt | PlayAtt): number {
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
   * 書き込めなくても入室・載せる操作は失敗にしない（期限の掃除は次の操作かアラームで遅れて済む）。
   */
  private async scheduleAlarm(): Promise<void> {
    try {
      const times: number[] = this.items().map((w) => this.deadline(w, att(w) as WaitAtt | PlayAtt));
      for (const w of this.views()) times.push(viewDeadline(att(w) as ViewAtt));
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

/** 一覧の件数の上限（全体と同じ鍵から）に達しているかどうか。restは数える対象の接続。 */
function isFull(rest: WebSocket[], key: string): boolean {
  return rest.length >= LOBBY_MAX_TOTAL || rest.filter((w) => att(w).key === key).length >= LOBBY_MAX_PER_KEY;
}

/** 載せた順の次の番号。liveは一覧に載っている接続だけ。 */
function nextSeq(live: WebSocket[]): number {
  return live.reduce((m, w) => Math.max(m, (att(w) as WaitAtt | PlayAtt).seq), -1) + 1;
}

function viewDeadline(a: ViewAtt): number {
  return sweepDeadline(a.created, LOBBY_VIEW_TTL_MS);
}

function att(ws: WebSocket): Att {
  return ws.deserializeAttachment() as Att;
}
