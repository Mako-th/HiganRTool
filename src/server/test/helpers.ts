// 検査のツール: ルームへWebSocketで接続する関数と届いた文を順に待つバッファ。送り元のIPは文書用のアドレスだけを使う。
import { env, exports } from "cloudflare:workers";
import { expect } from "vitest";
import { TICKET_MAC_CHARS, normalizeRoomName } from "../src/rules";
import { mintTicket, seatKey } from "../src/ticket";

// 送り元は文書用のIPv6（RFC 3849の2001:db8::/32）。
// ファイルごとに乱数の区切りを変えて回数の制限が持ち越されないようにする（/64で数えるので3・4グループ目を変える）。
const fileTag = Math.floor(Math.random() * 0xffff).toString(16);
let ipSeq = 0;
/** 検査ごとに別の送り元（別の /64）にする。 */
export function freshIp(): string {
  ipSeq += 1;
  return `2001:db8:${fileTag}:${ipSeq.toString(16)}::1`;
}

export function sameSlash64(ip: string, host: number): string {
  return ip.replace(/::1$/, `::${host.toString(16)}`);
}

let roomSeq = 0;
export function freshRoom(prefix = "r"): string {
  roomSeq += 1;
  return `${prefix}-${fileTag}-${roomSeq}`;
}

export function stubFor(name: string): DurableObjectStub {
  const key = normalizeRoomName(name);
  if (key === null) throw new Error(`部屋名の決まりの外: ${JSON.stringify(name)}`);
  return env.ROOMS.get(env.ROOMS.idFromName(key));
}

export function rawConnect(
  path: string,
  init: { ip?: string; upgrade?: boolean; method?: string } = {},
): Promise<Response> {
  const headers: Record<string, string> = { "CF-Connecting-IP": init.ip ?? freshIp() };
  if (init.upgrade !== false) headers.Upgrade = "websocket";
  return exports.default.fetch(`http://rooms.test${path}`, {
    method: init.method ?? "GET",
    headers,
  });
}

export interface Closed {
  code: number;
  reason: string;
}

export class Client {
  private queue: unknown[] = [];
  private waiters: ((m: unknown) => void)[] = [];
  readonly closed: Promise<Closed>;
  private closedFlag = false;

  constructor(readonly ws: WebSocket) {
    let resolveClosed!: (c: Closed) => void;
    this.closed = new Promise((r) => (resolveClosed = r));
    ws.accept();
    ws.addEventListener("message", (e: MessageEvent) => {
      const m = typeof e.data === "string" ? JSON.parse(e.data) : e.data;
      const w = this.waiters.shift();
      if (w) w(m);
      else this.queue.push(m);
    });
    ws.addEventListener("close", (e: CloseEvent) => {
      this.closedFlag = true;
      resolveClosed({ code: e.code, reason: e.reason });
    });
  }

  get isClosed(): boolean {
    return this.closedFlag;
  }

  /** 次の1通。ms待っても来なければ失敗する。 */
  next(ms = 2000): Promise<any> {
    const m = this.queue.shift();
    if (m !== undefined) return Promise.resolve(m);
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error(`no message within ${ms} ms`)), ms);
      this.waiters.push((v) => {
        clearTimeout(t);
        resolve(v);
      });
    });
  }

  async silentFor(ms = 200): Promise<unknown | null> {
    await sleep(ms);
    return this.queue.length > 0 ? this.queue[0] : null;
  }

  sendJson(v: unknown): void {
    this.ws.send(JSON.stringify(v));
  }

  send(v: string | ArrayBuffer): void {
    this.ws.send(v);
  }

  close(): void {
    this.ws.close(1000, "client bye");
  }
}

/** ルームのパス。ticketは観戦用のルームのhostが出す席のチケット（符号化して載せる）。 */
export function roomPath(room: string, cid?: string, ticket?: string): string {
  const q: string[] = [];
  if (cid !== undefined) q.push(`cid=${cid}`);
  if (ticket !== undefined) q.push(`ticket=${encodeURIComponent(ticket)}`);
  return `/v0/rooms/${encodeURIComponent(room)}${q.length === 0 ? "" : `?${q.join("&")}`}`;
}

/** ルーム名（打った文字のまま）で接続する。dllと同じくUTF-8をパーセント符号化してパスに載せる。 */
export async function connect(room: string, ip?: string, cid?: string, ticket?: string): Promise<Client> {
  const res = await rawConnect(roomPath(room, cid, ticket), { ip });
  if (res.status !== 101 || res.webSocket === null) {
    throw new Error(`expected 101, got ${res.status}: ${await res.text()}`);
  }
  return new Client(res.webSocket);
}

let buildSeq = 0;
/** 検査ごとに別のビルド番号（別の待ち列）。ほかの検査の待ち人とペアにならないように。 */
export function freshBuild(): number {
  buildSeq += 1;
  return 1_000_000 + parseInt(fileTag, 16) * 1000 + buildSeq;
}

/** ランダム対戦の待ち列へ接続する（パス`/v0/random/<ビルド番号>`）。 */
export async function connectRandom(build: number, ip?: string, cid?: string): Promise<Client> {
  const query = cid === undefined ? "" : `?cid=${cid}`;
  const res = await rawConnect(`/v0/random/${build}${query}`, { ip });
  if (res.status !== 101 || res.webSocket === null) {
    throw new Error(`expected 101, got ${res.status}: ${await res.text()}`);
  }
  return new Client(res.webSocket);
}

/** 進行中のランダム対戦の一覧へ接続する（post = 載せる口・view = 見る口。パス`/v0/watch/<ビルド番号>/<post|view>`）。 */
export async function connectWatch(build: number, kind: "post" | "view", ip?: string): Promise<Client> {
  const res = await rawConnect(`/v0/watch/${build}/${kind}`, { ip });
  if (res.status !== 101 || res.webSocket === null) {
    throw new Error(`expected 101, got ${res.status}: ${await res.text()}`);
  }
  return new Client(res.webSocket);
}

let cidSeq = 0;
/** 起動ごとの乱数の見本（16進32文字）。検査ごとに別の値。 */
export function freshCid(): string {
  cidSeq += 1;
  return `${fileTag.padStart(4, "0")}${cidSeq.toString(16).padStart(4, "0")}`.padEnd(32, "c");
}

export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/** チケットの形（発行.期限.MAC）。 */
export const TICKET_RE = new RegExp(`^\\d+\\.\\d+\\.[A-Za-z0-9_-]{${TICKET_MAC_CHARS}}$`);

/** guestが受けるpeer（対戦のルームではチケットが載る）。 */
export function peerWithTicket(code: string): unknown {
  return { t: "peer", code, ticket: expect.stringMatching(TICKET_RE) };
}

/** 検査のSEAT_KEYで観戦用のルーム名wについてチケットを作る（既定はいま発行の24時間）。 */
export async function ticketFor(w: string, issued = Math.floor(Date.now() / 1000), expires?: number): Promise<string> {
  const key = await seatKey(env.SEAT_KEY);
  if (key === null) throw new Error("検査の SEAT_KEY が無い");
  return mintTicket(key, w, issued, expires);
}

/** 合成の接続コード（base64urlの文字だけ）。 */
export function fakeCode(tag: string, len = 80): string {
  const base = `AAE${tag}`.replace(/[^A-Za-z0-9_-]/g, "x");
  return (base + "Q".repeat(len)).slice(0, len);
}
