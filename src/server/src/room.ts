// ルーム1つ = Durable Object 1つ（ルーム名からidFromName）。1ルーム2人（始める側host・入る側guest）。
//
// 状態はWebSocketのattachmentにだけ保存する（ストレージにもインスタンスの変数にも保存しない）。
// Hibernationで眠ると変数は消えるがattachmentは残る。
// 接続コード（認証用の秘密の乱数を含む）もWebSocketが閉じれば消える。ストレージに書くのは期限のアラームだけ。
// ログ（console）には何も書かない（コードも利用者の打ったルーム名もチケットも出さない）。
// 対戦のルームで最初に2人が揃ったとき、hostへはmatchedに、guestへはpeerにホスト枠のチケット（ticket.ts）を載せる。観戦用のルーム（w.…）のホスト枠はチケットで決める。
// 対戦のルームは成立の後も2人を残してルーム名を使用中にする（3人目は409 room_in_use）。
// 生存確認（ping）が古い接続はアラームで閉じる（HELD_STALE_MS）。

import { DurableObject } from "cloudflare:workers";
import {
  ANSWER_TTL_MS,
  CLIENT_ID_PARAM,
  CLOSE_NAME,
  Close,
  HELD_STALE_MS,
  HELD_SWEEP_MS,
  PING_TEXT,
  PONG_TEXT,
  RoomRefusal,
  type ServerMessage,
  TICKET_PARAM,
  WATCH_ANSWER_TTL_MS,
  decodedRoomNameOfPath,
  isWatchRoomPath,
  normalizedRoomNameOfPath,
  parseClientId,
  parseClientMessage,
  roomTtlForPath,
  spectateRoomName,
} from "./rules";
import { type TicketVerdict, judgeTicket, mintTicket, seatKey } from "./ticket";

type Role = "host" | "guest";

/** コードは1024文字までなので1.1 KBほど。attachmentの上限は同梱のworkerd（1.20260815）で16384 B（ローカルで測った。本番は未確認）。 */
interface Att {
  role: Role;
  /** ルームの期限（UNIXミリ秒）。hostとguestで同じ値。 */
  deadline: number;
  /** hostだけ: 保存している始める側のコード。 */
  code: string | null;
  /** guestだけ: hostのコードを渡したか。 */
  gotPeer: boolean;
  /** こちらから閉じた（または相手のcloseを受けた）印。二重に後始末しないため。 */
  done: boolean;
  /** クライアントの起動ごとの乱数（クエリのcid）。付けない古いクライアントはnull（古いattachmentでは未定義）。 */
  cid: string | null;
  /** hostだけ: 相手が揃ったことを知らせた（matchedを送った）。guestが入れ替わっても2度は送らない。 */
  matched: boolean;
  /** hostだけ: 観戦を断った（refuseを受けた）。断ったhostはルームに残し、来るguestを断り続ける。 */
  refused?: boolean;
  /** 観戦用のルームか（`w.`の名）。refuseは観戦用のルームのhostだけが送れる。 */
  watch?: boolean;
  /** 対戦のルームだけ: この接続が打った名から導いた観戦用のルーム名（チケットに署名する名）。SEAT_KEYが無ければ持たない。 */
  wname?: string;
  /** 対戦のルームのhostだけ: 最初に揃ったときに発行したチケットのUNIX秒（入れ替わったguestにも同じ発行のチケットを渡す）。 */
  issued?: number;
  /** 観戦用のルームのhostだけ: ホスト枠を取ったチケットの発行のUNIX秒（より新しいチケットのhostが来たらホスト枠を渡す）。 */
  seat?: number;
  /** 対戦のルームだけ: 成立した（hostへanswerを渡した）UNIXミリ秒。以後はdeadlineでは閉じず生存確認（ping）で調べる。 */
  held?: number;
}

const OPEN = 1;

export class Room extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // 生存のチェックはDurable Objectを起動せずに返す（起動していた時間だけ課金されるため）。
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(PING_TEXT, PONG_TEXT));
  }

  async fetch(req: Request): Promise<Response> {
    const now = Date.now();
    // アラームが遅れていても期限の過ぎたルームと生存確認（ping）の途絶えた成立後の部屋には人を入れない（先に後始末して新しいルームとして始める）。
    this.expireIfDue(now);
    this.closeStaleHeld(now);

    const url = new URL(req.url);
    const cid = parseClientId(url.searchParams.get(CLIENT_ID_PARAM));
    // 観戦用のルームのホスト枠を取れるのは、そのルーム名について正しく期限内のチケットを持つ接続だけ。
    // ホスト枠の判断（liveの確認から受け入れまで）にawaitを挟まないよう、チケットの検証と観戦用の名の計算は先に済ませる。
    const watch = isWatchRoomPath(url.pathname);
    const key = await seatKey(this.env.SEAT_KEY);
    let ticket: TicketVerdict = { kind: "none" };
    let wname: string | undefined;
    if (watch) {
      // 入口が名を検証した後なのでnullにはならないはず。
      const name = normalizedRoomNameOfPath(url.pathname) ?? "";
      ticket = await judgeTicket(key, name, url.searchParams.get(TICKET_PARAM), toSecs(now));
      if (ticket.kind === "bad") return Response.json({ error: "bad_ticket" }, { status: 400 });
    } else if (key !== null) {
      // チケットはこの接続が打った名（dllのspectate_ofと同じ導き方）の観戦用のルームに効く。同じルームの2人でも全角・半角が違えば別の名になる。
      wname = await spectateRoomName(decodedRoomNameOfPath(url.pathname) ?? "");
    }
    const fresh = ticket.kind === "valid" ? ticket.issued : null;
    // 同じ利用者の入り直し: 届かなかった前の入室が残っていても3人目にしない（クライアントは届く前に失敗扱いにして入り直す）。
    if (cid !== null) await this.dropStale(cid);

    const live = this.live();
    const seated = live.find((w) => att(w).role === "host");
    let role: Role;
    let deadline: number;
    if (watch && seated !== undefined && fresh !== null && fresh > (att(seated).seat ?? -1)) {
      // より新しい対戦のチケット: 前のhost（先回りした古いチケットのホスト枠）を閉じてホスト枠を渡す。前のhostのコードを受けた観戦者も閉じる。
      for (const w of live) closeWith(w, Close.PeerLeft, true);
      role = "host";
      deadline = now + roomTtlForPath(url.pathname);
    } else if (live.length === 0) {
      if (watch && fresh === null) return Response.json({ error: RoomRefusal.Full }, { status: 409 });
      role = "host";
      deadline = now + roomTtlForPath(url.pathname);
    } else if (live.length === 1 && att(live[0]).role === "host" && att(live[0]).held === undefined) {
      role = "guest";
      deadline = att(live[0]).deadline;
    } else {
      // 3人目（成立の前も後も）。WebSocketを張らずに断る（張ってから断るとその分だけ起動している時間が延びる）。
      return Response.json({ error: watch ? RoomRefusal.Full : RoomRefusal.InUse }, { status: 409 });
    }

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.ctx.acceptWebSocket(server, [role]);
    const a: Att = {
      role,
      deadline,
      code: null,
      gotPeer: false,
      done: false,
      cid,
      matched: false,
      watch,
    };
    if (wname !== undefined) a.wname = wname;
    if (watch && role === "host" && fresh !== null) a.seat = fresh;
    server.serializeAttachment(a);
    send(server, { t: "room", role, expires_at: toSecs(deadline) });
    if (role === "guest" && att(live[0]).refused === true) {
      // 観戦を断っているhostのルームへ入った。コードは渡さず断りの印つきで閉じる。hostは残す（failはhostを閉じない）。
      await this.fail(server, Close.Refused);
      return new Response(null, { status: 101, webSocket: client });
    }
    if (role === "guest") await this.tryMatch(now);
    await this.scheduleAlarm();
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    const a = att(ws);
    if (a.done) return;
    if (a.held !== undefined) {
      // 成立の後はping（自動応答なので、ここへは来ない）だけを送る約束。ほかの文は誤りとして閉じる（相手もafterLeaveで閉じる）。
      await this.fail(ws, Close.BadMessage);
      return;
    }
    const p = parseClientMessage(message);
    if (p.kind === "error") {
      await this.fail(ws, p.close);
      return;
    }
    if (p.kind === "offer") {
      // offerはhostが1回だけ。guestのコードのタグはhostのコードから作る鍵で付くので、先にhostのコードが要る。
      // 断ったhostはofferを出さない（断りとofferが両方来るとguestの入り方が決まらないため）。
      if (a.role !== "host" || a.code !== null || a.refused === true) {
        await this.fail(ws, Close.BadMessage);
        return;
      }
      a.code = p.code;
      ws.serializeAttachment(a);
      await this.tryMatch(Date.now());
      await this.scheduleAlarm();
      return;
    }
    if (p.kind === "refuse") {
      // 観戦の断りは観戦用のルームのhostがofferの前に1回だけ。対戦のルーム・offerの後・guestからは誤り。
      if (a.role !== "host" || a.watch !== true || a.code !== null || a.refused === true) {
        await this.fail(ws, Close.BadMessage);
        return;
      }
      a.refused = true;
      ws.serializeAttachment(a);
      // 待っているguestはコードを受けないまま断る。hostは閉じない（ルームの期限まで来るguestを断り続ける）。
      for (const w of this.live()) {
        if (att(w).role === "guest") closeWith(w, Close.Refused, true);
      }
      await this.scheduleAlarm();
      return;
    }
    if (a.role !== "guest" || !a.gotPeer) {
      await this.fail(ws, Close.BadMessage);
      return;
    }
    const host = this.live().find((w) => att(w).role === "host");
    if (host === undefined) {
      // hostが切れるとguestも閉じる（onGone）ので、ここへは来ないはず。来たら相手がいないと答える。
      await this.fail(ws, Close.PeerLeft);
      return;
    }
    send(host, { t: "peer", code: p.code });
    if (a.watch === true) {
      closeWith(host, Close.Done, false);
      closeWith(ws, Close.Done, false);
      await this.ctx.storage.deleteAlarm();
      return;
    }
    // 対戦のルーム: 2人を残してルーム名を使用中にする（同じ名で後から別のペアが成立して、観戦用のルームのホスト枠を取り合わないため）。
    const at = Date.now();
    for (const w of [host, ws]) {
      const x = att(w);
      x.held = at;
      w.serializeAttachment(x);
    }
    await this.scheduleAlarm();
  }

  async webSocketClose(ws: WebSocket, _code: number, _reason: string, _wasClean: boolean): Promise<void> {
    await this.gone(ws);
  }

  async webSocketError(ws: WebSocket, _error: unknown): Promise<void> {
    await this.gone(ws);
  }

  /** ルームの期限が過ぎていれば全員を閉じ、まだなら掛け直す（前のルームの古いアラーム）。成立後の部屋は生存確認（ping）を調べる。 */
  async alarm(): Promise<void> {
    const now = Date.now();
    this.expireIfDue(now);
    this.closeStaleHeld(now);
    // 誰も残っていなければ消し、残っていれば（観戦用のルームのhostなど）次の期限へ掛け直す。
    await this.scheduleAlarm();
  }

  private live(): WebSocket[] {
    return this.ctx.getWebSockets().filter((w) => w.readyState === OPEN && !att(w).done);
  }

  private async tryMatch(now: number): Promise<void> {
    const live = this.live();
    const host = live.find((w) => att(w).role === "host");
    const guest = live.find((w) => att(w).role === "guest");
    if (host === undefined || guest === undefined) return;
    const h = att(host);
    const g = att(guest);
    if (h.code === null || g.gotPeer) return;
    // 揃ってからはanswerの期限で縛る（ルームの期限より早ければ）。対戦のルームは両者に同じ値を持たせる。
    // guestが入れ替わった2度目はhostにmatchedをもう1度送ると順番違いで落とされるので、期限もhostのまま延ばさない。
    const first = h.matched !== true;
    if (first) {
      h.deadline = Math.min(h.deadline, now + ANSWER_TTL_MS);
      h.matched = true;
      if (h.watch !== true) h.issued = toSecs(now);
      host.serializeAttachment(h);
    }
    // 観戦用のルームは観戦者ごとに短い期限で縛る（過ぎた観戦者だけを閉じ、hostは次の観戦者を待つ。expireIfDue）。
    g.deadline = h.watch === true ? Math.min(h.deadline, now + WATCH_ANSWER_TTL_MS) : h.deadline;
    g.gotPeer = true;
    guest.serializeAttachment(g);
    // チケットのMACは状態を書き終えてから作る（作る間に次のイベントが来ても同じ成立を2度数えない）。
    const code = h.code;
    const deadline = h.deadline;
    const [gt, ht] = await Promise.all([this.ticketOf(g, h.issued), first ? this.ticketOf(h, h.issued) : undefined]);
    send(guest, gt === undefined ? { t: "peer", code } : { t: "peer", code, ticket: gt });
    if (first) send(host, ht === undefined ? { t: "matched", expires_at: toSecs(deadline) } : { t: "matched", expires_at: toSecs(deadline), ticket: ht });
  }

  /** aの接続へ渡すチケット（発行issued）。観戦用のルーム・SEAT_KEYが無い・名を持たない古いattachmentでは作らない。 */
  private async ticketOf(a: Att, issued: number | undefined): Promise<string | undefined> {
    if (a.wname === undefined || issued === undefined) return undefined;
    const key = await seatKey(this.env.SEAT_KEY);
    return key === null ? undefined : mintTicket(key, a.wname, issued);
  }

  /**
   * 同じcidの古い接続を静かに閉じる。guestならhostはそのまま待つ（同じ利用者なのでhostのコードを知っても害は無い）。
   * hostならそのhostのコードはもう使えないのでguestも閉じる。
   */
  private async dropStale(cid: string): Promise<void> {
    for (const w of this.live()) {
      const a = att(w);
      if (a.cid !== cid) continue;
      closeWith(w, Close.Done, false);
      // 成立の後の接続なら相手も閉じて部屋を空ける（使用中の部屋を1人で残さない）。
      if (a.role === "host" || a.held !== undefined) await this.afterLeave(w);
    }
  }

  /** 成立の後の接続はルームの期限で閉じない（生存確認（ping）はcloseStaleHeldが調べる）。 */
  private expireIfDue(now: number): void {
    const live = this.live().filter((w) => att(w).held === undefined);
    if (live.length === 0) return;
    const host = live.find((w) => att(w).role === "host");
    if (host !== undefined && att(host).watch === true && now < att(host).deadline) {
      // 観戦用のルーム: answerの期限を過ぎた観戦者だけを閉じ、hostは残す（次の観戦者へ同じコードを渡す）。
      for (const w of live) {
        if (w !== host && now >= att(w).deadline) closeWith(w, Close.Expired, true);
      }
      return;
    }
    const deadline = Math.min(...live.map((w) => att(w).deadline));
    if (now < deadline) return;
    for (const w of live) closeWith(w, Close.Expired, true);
  }

  /**
   * 成立の後の部屋で生存確認（ping）がHELD_STALE_MSより古い接続を閉じる。1人でも閉じたら成立の後のもう1人も閉じて部屋を空ける。
   * 生存確認（ping）の時刻は自動応答の時刻（Durable Objectを起動せずに残る）。まだ1度も無ければ成立の時刻から数える。
   */
  private closeStaleHeld(now: number): void {
    const held = this.live().filter((w) => att(w).held !== undefined);
    const stale = held.filter((w) => now - this.lastSign(w) > HELD_STALE_MS);
    if (stale.length === 0) return;
    for (const w of held) closeWith(w, stale.includes(w) ? Close.Expired : Close.PeerLeft, true);
  }

  private lastSign(ws: WebSocket): number {
    let last = att(ws).held ?? 0;
    try {
      const auto = this.ctx.getWebSocketAutoResponseTimestamp(ws);
      if (auto !== null) last = Math.max(last, auto.getTime());
    } catch {
      // 取得できなければ成立の時刻だけで数える。
    }
    return last;
  }

  /** 書き込めなくても入室は失敗にしない（無料枠の使い切りなど）。期限は次の入室のexpireIfDueか掛け直されたアラームで後始末される。 */
  private async scheduleAlarm(): Promise<void> {
    try {
      const live = this.live();
      if (live.length === 0) {
        await this.ctx.storage.deleteAlarm();
        return;
      }
      // 成立の後の接続は期限を持たないのでHELD_SWEEP_MSごとに生存確認（ping）を調べに起きる。
      const sweep = Date.now() + HELD_SWEEP_MS;
      await this.ctx.storage.setAlarm(Math.min(...live.map((w) => att(w).held === undefined ? att(w).deadline : sweep)));
    } catch {
      // 受け付けたソケットを占有者のまま500で捨てるより、期限が遅れる方を取る。
    }
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
   * 1人が抜けた後。hostが抜けたらguestも閉じる。
   * 対戦のルームでguestが抜けたとき、hostのコードを渡した後ならhostも閉じる（抜けた人がそのコードの秘密の乱数を知っているので、次のguestに同じコードを渡さない）。渡す前ならhostは待つ。
   * 成立の後にどちらかが抜けたらもう1人も閉じて部屋を空ける（ルーム名の使用中を解く）。
   * 観戦用のルームではguestが抜けてもhostを閉じない。
   * 観戦用のコードはどの観戦者にも同じ秘密の乱数を渡すものなので、答えずに抜けた観戦者がいても次の観戦者へ同じコードを渡してよい（配布する側を入り直させない）。
   */
  private async afterLeave(ws: WebSocket): Promise<void> {
    const a = att(ws);
    const closeOthers = a.role === "host" || (a.gotPeer && a.watch !== true) || a.held !== undefined;
    for (const other of this.live()) {
      if (other === ws) continue;
      if (closeOthers) closeWith(other, Close.PeerLeft, true);
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
    // 閉じたWebSocketには書き込めないことがある。doneはlive()のreadyStateでも落ちる。
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
