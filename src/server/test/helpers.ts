// 検査の道具: ルームへ WebSocket で繋ぐ関数と、届いた文を順に待つ入れ物。送り元の IP は文書用のアドレスだけを使う。
import { env, exports } from "cloudflare:workers";
import { normalizeRoomName } from "../src/rules";

// 送り元は文書用の IPv6（RFC 3849 の 2001:db8::/32）。ファイルごとに乱数の区切りを変え、回数の制限が持ち越されないようにする
// （/64 で数えるので 3・4 グループ目を変える）。
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

  /** 次の 1 通。ms 待っても来なければ落ちる。 */
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

/** ルーム名（打った文字のまま）で繋ぐ。dll と同じく UTF-8 をパーセント符号化してパスに載せる。 */
export async function connect(room: string, ip?: string, cid?: string): Promise<Client> {
  const query = cid === undefined ? "" : `?cid=${cid}`;
  const res = await rawConnect(`/v0/rooms/${encodeURIComponent(room)}${query}`, { ip });
  if (res.status !== 101 || res.webSocket === null) {
    throw new Error(`expected 101, got ${res.status}: ${await res.text()}`);
  }
  return new Client(res.webSocket);
}

let buildSeq = 0;
/** 検査ごとに別のビルド番号（別の待ち列）。ほかの検査の待ち人と組にならないように。 */
export function freshBuild(): number {
  buildSeq += 1;
  return 1_000_000 + parseInt(fileTag, 16) * 1000 + buildSeq;
}

/** ランダム対戦の待ち列へ繋ぐ（パス `/v0/random/<ビルド番号>`）。 */
export async function connectRandom(build: number, ip?: string, cid?: string): Promise<Client> {
  const query = cid === undefined ? "" : `?cid=${cid}`;
  const res = await rawConnect(`/v0/random/${build}${query}`, { ip });
  if (res.status !== 101 || res.webSocket === null) {
    throw new Error(`expected 101, got ${res.status}: ${await res.text()}`);
  }
  return new Client(res.webSocket);
}

/** 進行中のランダム対戦の一覧へ繋ぐ（post = 載せる口・view = 見る口。パス `/v0/watch/<ビルド番号>/<post|view>`）。 */
export async function connectWatch(build: number, kind: "post" | "view", ip?: string): Promise<Client> {
  const res = await rawConnect(`/v0/watch/${build}/${kind}`, { ip });
  if (res.status !== 101 || res.webSocket === null) {
    throw new Error(`expected 101, got ${res.status}: ${await res.text()}`);
  }
  return new Client(res.webSocket);
}

let cidSeq = 0;
/** 起動ごとの乱数の見本（16 進 32 文字）。検査ごとに別の値。 */
export function freshCid(): string {
  cidSeq += 1;
  return `${fileTag.padStart(4, "0")}${cidSeq.toString(16).padStart(4, "0")}`.padEnd(32, "c");
}

export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/** 合成の接続コード（base64url の文字だけ）。 */
export function fakeCode(tag: string, len = 80): string {
  const base = `AAE${tag}`.replace(/[^A-Za-z0-9_-]/g, "x");
  return (base + "Q".repeat(len)).slice(0, len);
}
