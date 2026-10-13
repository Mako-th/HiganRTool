// ルームサーバー v0 の決まり（文字・長さ・期限・誤りの番号）と、届いた文の読み方。Workers の API を使わない純粋な部分。
// v0（実験）なので値は変えてよい。

/** URL のバージョン。dll の側が話すバージョンと合わなければ 404 になる。 */
export const PROTO_PATH_PREFIX = "/v0/rooms/";

/**
 * ルーム名。日本語を含む好きな文字を使える（パスには UTF-8 をパーセント符号化して載せ、ここで戻す）。
 * ルームを引く文字は ROOM_NAME_FOLD の順（NFKC → 前後の空白を除く → 小文字）に揃える。全角半角・大文字小文字・前後の空白の違いが
 * 同じルームになる。揃えるのはサーバーだけ（dll は NFKC を持たない）。
 *
 * 断るもの（400 bad_room_name）: 制御文字（Cc）・`/`（符号化を戻す途中の機器が %2F を `/` に戻すとパスが割れうる）・`.` と `..`
 * （URL のパスの読み方で消える）・揃えた後が 0 文字か 33 文字以上（HELLO の name の上限 wire::NAME_MAX = 32。コードポイントで数える）・
 * 揃える前の UTF-8 が 128 B 超（揃える手間とパスの長さの上限。dll も揃えずに確かめられる）。
 * 文字と長さのガードは揃える前の文字で見る（dll が NFKC なしで同じ判断をできるように）。揃えて現れる `/`（全角の `／` など）は通す。
 */
export const ROOM_NAME_MIN = 1;
export const ROOM_NAME_MAX = 32;
export const ROOM_NAME_MAX_BYTES = 128;
/** 制御文字の範囲（見本 rules.json の room_name_controls と同じ）。 */
const ROOM_NAME_CONTROL_RE = /\p{Cc}/u;
export const ROOM_NAME_BAD_CHARS = "/";
export const ROOM_NAME_BAD_NAMES: readonly string[] = [".", ".."];
/** 揃える順。検査は見本の同じ名の列とこれを突き合わせる。 */
export const ROOM_NAME_FOLD: readonly string[] = ["nfkc", "trim", "lower"];
/** パスのルーム名の区切り（符号化したまま）の文字の上限。128 B の名は 1 B が最大 3 文字（%XX）なので、これを越えれば戻すまでもなく 128 B を越える。 */
const ROOM_SEGMENT_MAX_CHARS = ROOM_NAME_MAX_BYTES * 3;

/**
 * 接続コードの文字の上限。core の読み手の上限（`MAX_CODE_CHARS` = 512）の 2 倍。いまの最長のコードは 354 文字で、
 * v0 のうちに伸びても出し直さずに済む余裕。サーバーは文字が base64url であることだけを見る。
 */
export const CODE_MAX_CHARS = 1024;
const CODE_RE = /^[A-Za-z0-9_-]+$/;

/** 1 通の文の上限（UTF-8 のバイト）。越えたら中を読まずに断る。 */
export const MESSAGE_MAX_BYTES = 2048;

/**
 * ルームの期限: 最初の人が繋いでから 10 分。接続コードの期限（30 分）より短くし、待ち合わせ中にコードが切れないようにした。
 * 誰も来なければルーム名の打ち間違いか相手の不在と見て閉じる。
 */
export const ROOM_TTL_MS = 10 * 60 * 1000;

/** 相手が揃ってから、入る側が answer を送るまでの期限。コードを読んで返すだけなので 1 分あれば足りる。 */
export const ANSWER_TTL_MS = 60 * 1000;

/**
 * ランダム対戦の待ち列のパス `/v0/random/<ビルド番号>`。ビルド番号ごとに別の待ち列にする（違うビルドどうしはつながらないため）。
 * 番号は 10 進の整数 0〜4294967295 を、先頭に 0 を付けない 1 通りの書き方だけで受ける（同じ番号が 2 つの待ち列にならないように）。
 */
export const RANDOM_PATH_PREFIX = "/v0/random/";
const RANDOM_BUILD_RE = /^(0|[1-9][0-9]{0,9})$/;
const BUILD_MAX = 4_294_967_295;

/** 待ち列での期限: ルームと同じ 10 分（切れたらクライアントが自動で入り直す）。 */
export const RANDOM_QUEUE_TTL_MS = ROOM_TTL_MS;

/**
 * 組にした後に割り当てるルームの期限。2 人は数秒で入る前提で 1 分にする。組にした直後に片方が消えたとき、残った側が
 * 10 分待たずに待ち列へ戻れる。
 */
export const RANDOM_ROOM_TTL_MS = 60 * 1000;

/**
 * 割り当てるルーム名: `q.` ＋ 乱数 120 bit の 16 進 30 文字（計 32 文字 = ROOM_NAME_MAX）。クライアントは `.` を打てないので人が打つ
 * ルーム名とぶつからず、推測もできない長さ。観戦用のルームの名の先頭 `w.`（クライアントの room.rs）とも別の先頭にした。
 */
export const RANDOM_ROOM_PREFIX = "q.";
export const RANDOM_ROOM_HEX_LEN = 30;
const RANDOM_ROOM_RE = /^q\.[0-9a-f]{30}$/;

/**
 * 進行中のランダム対戦の一覧（`/v0/watch/<ビルド番号>/post` と `/v0/watch/<ビルド番号>/view`）。
 * post は対戦のホストが持ち続けて載せるエンドポイント、view は観戦者が一覧を開いている間だけ持つ口。
 * ビルド番号ごとに別の一覧にする（違うビルドは観戦できないため）。
 */
export const WATCH_PATH_PREFIX = "/v0/watch/";
export const WATCH_POST_SEGMENT = "post";
export const WATCH_VIEW_SEGMENT = "view";

/** 一覧に載せられる数: 全体で 100 件、1 IP（IPv6 は /64）から 2 件まで。 */
export const WATCH_MAX_TOTAL = 100;
export const WATCH_MAX_PER_IP = 2;

/** 観戦用のルーム名: `w.` ＋ SHA-256(ルーム名の ASCII 小文字) の先頭 8 B の 16 進（クライアントの room.rs の `spectate_of` と同じ）。 */
export const WATCH_ROOM_PREFIX = "w.";
export const WATCH_HASH_BYTES = 8;

/** 観戦者の上限として受ける値（クライアントの `spectate::MAX_SPECTATORS`）。 */
export const WATCH_CAP_MAX = 10;
/** 受ける遅延フレーム数の最大（rollback の D も delay 方式の遅れもここに収まる）。 */
export const WATCH_DELAY_MAX = 15;
export const WATCH_MODES: readonly string[] = ["rollback", "delay"];

/**
 * プレイヤー名の文字: クライアントの名前の入力（player_name.rs）が受ける ASCII の英数字・`-`・`_`・`@` と、
 * 受けない文字を置き換えた `?`。長さは HELLO の name の上限（32）。
 */
export const WATCH_NAME_MAX = 32;
const WATCH_NAME_RE = /^[A-Za-z0-9_@?-]{1,32}$/;

/**
 * 載せた人が生きているかの目安: 30 秒ごとの ping（Durable Object を起こさない自動応答）の最後の時刻から数える。
 * ping 3 回分に余裕を足した値を越えて何も無ければ、切れたものとして一覧から消す。
 */
export const WATCH_STALE_MS = 100 * 1000;
/** 繋いでから最初の post が来るまでの上限。 */
export const WATCH_UNPOSTED_MS = 15 * 1000;
/** 一覧の変化を観戦者へ送る最短の間隔（変化が続いても 1 秒に 1 回にまとめる。載せた人が連打して全員へ増幅されないため）。 */
export const WATCH_BROADCAST_MIN_MS = 1000;

/** 一覧の 1 通の上限（バイト）。1 件 150 B 前後 × 100 件に余裕を足した値。 */
export const WATCH_LIST_MAX_BYTES = 32 * 1024;

/** 載せるエンドポイントに届く 1 通の上限は MESSAGE_MAX_BYTES（post は 300 B ほど）。 */

export interface WatchItem {
  /** 観戦用のルーム名（w.…）。割り当てのルーム名 q.… は持たない。 */
  room: string;
  p1: string;
  p2: string;
  mode: string;
  delay: number;
  watchers: number;
  cap: number;
}

/**
 * 割り当てのルーム名から観戦用のルーム名を導く。割り当ての名 q.… は 16 進の小文字だけなので小文字にしても変わらない
 * （クライアントはバイトごとに ASCII 小文字にして SHA-256 へ流す）。
 */
export async function spectateRoomName(room: string): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(room.toLowerCase())));
  let hex = "";
  for (const b of digest.slice(0, WATCH_HASH_BYTES)) hex += b.toString(16).padStart(2, "0");
  return WATCH_ROOM_PREFIX + hex;
}

export type ParsedPost =
  | { ok: true; room: string; p1: string; p2: string; mode: string; delay: number; watchers: number; cap: number }
  | { ok: false };

function intIn(v: unknown, lo: number, hi: number): v is number {
  return typeof v === "number" && Number.isInteger(v) && v >= lo && v <= hi;
}

/** 載せるエンドポイントの最初の 1 通 `{"t":"post",…}` を読む。割り当ての形の名（isRandomRoomName。揃えずに厳密に）・名前の文字・方式・数の範囲を全部見る。 */
export function parsePost(o: Record<string, unknown>): ParsedPost {
  const { room, p1, p2, mode, delay, watchers, cap } = o;
  if (typeof room !== "string" || !isRandomRoomName(room)) return { ok: false };
  if (typeof p1 !== "string" || !WATCH_NAME_RE.test(p1)) return { ok: false };
  if (typeof p2 !== "string" || !WATCH_NAME_RE.test(p2)) return { ok: false };
  if (typeof mode !== "string" || !WATCH_MODES.includes(mode)) return { ok: false };
  if (!intIn(delay, 0, WATCH_DELAY_MAX)) return { ok: false };
  if (!intIn(cap, 1, WATCH_CAP_MAX)) return { ok: false };
  if (!intIn(watchers, 0, cap)) return { ok: false };
  return { ok: true, room, p1, p2, mode, delay, watchers, cap };
}

/** パス `/v0/watch/<番号>/<post|view>` を読む。決まりの外なら null。 */
export function parseWatchPath(pathname: string): { build: number | null; kind: "post" | "view" | null } {
  const rest = pathname.slice(WATCH_PATH_PREFIX.length);
  const parts = rest.split("/");
  if (parts.length !== 2) return { build: null, kind: null };
  const kind = parts[1] === WATCH_POST_SEGMENT ? "post" : parts[1] === WATCH_VIEW_SEGMENT ? "view" : null;
  return { build: parseBuildSegment(parts[0]), kind };
}

/** パス `/v0/random/<番号>` の番号部分を読む。決まりの外なら null（400 bad_build）。 */
export function parseBuildSegment(seg: string): number | null {
  if (!RANDOM_BUILD_RE.test(seg)) return null;
  const n = Number(seg);
  return n <= BUILD_MAX ? n : null;
}

/** 割り当てるルーム名を引く（OS の乱数。`random` は検査が差し替える箇所）。 */
export function newRandomRoomName(
  random: (buf: Uint8Array) => void = (b) => {
    crypto.getRandomValues(b);
  },
): string {
  const bytes = new Uint8Array(RANDOM_ROOM_HEX_LEN / 2);
  random(bytes);
  let hex = "";
  for (const b of bytes) hex += b.toString(16).padStart(2, "0");
  return RANDOM_ROOM_PREFIX + hex;
}

/** 揃えた後のルーム名が、ランダム対戦の割り当てた形か。 */
export function isRandomRoomName(name: string): boolean {
  return RANDOM_ROOM_RE.test(name);
}

/** ルームのエンドポイントのパス（`/v0/rooms/<名>`）から、そのルームの期限を決める。ランダム対戦の割り当てた名だけ短い。 */
export function roomTtlForPath(pathname: string): number {
  const raw = roomNameFromPath(pathname);
  const decoded = raw === null ? null : decodeRoomName(raw);
  const name = decoded === null ? null : normalizeRoomName(decoded);
  return name !== null && isRandomRoomName(name) ? RANDOM_ROOM_TTL_MS : ROOM_TTL_MS;
}

/**
 * 生存の確かめ: 待つ側が 30 秒ごとに送る文字列。完全一致でだけ Durable Object を起こさずに PONG を返す
 * （setWebSocketAutoResponse）ので、空白や鍵の順を変えない。ルームの期限は延ばさない。
 */
export const PING_TEXT = '{"t":"ping"}';
export const PONG_TEXT = '{"t":"pong"}';

/**
 * 1 IP あたりの繋ぐ回数: 60 秒に 30 回まで（超えたら 429 と retry_after = 60）。普通は 1 マッチに 1〜2 回。
 * 30 は、同じ家の複数台が 1 つの IPv4 を分け合っても足りる値（観戦の入り直しだけで 10 を超える）。
 * Rate Limiting の束縛だけでは場所ごと・結果整合で正確に数えられず、実サーバーで 2 分に 25 回以上通ったため、
 * 数える役は Durable Object に持たせた。
 */
export const CONNECT_LIMIT_PER_MINUTE = 30;
/**
 * 粗い前段（Rate Limiting の束縛 CONNECT_LIMIT）の上限: 60 秒に 120 回。精度ではなく、荒らしが Durable Object の要求
 * （無料枠 10 万/日）を使い切るのを防ぐ。wrangler.jsonc の ratelimits の limit と同じ値にする。
 */
export const CONNECT_PRELIMIT_PER_MINUTE = 120;
export const RETRY_AFTER_SECS = 60;
/** 回数を数える期間の長さ（待たせる秒数と同じ 60 秒）。 */
export const CONNECT_WINDOW_MS = RETRY_AFTER_SECS * 1000;

/**
 * 滑る期間（直近 windowMs の許した時刻を全部覚える）で 1 回数える。固定期間だと区切りをまたいで 60 秒に 2 倍通せてしまう。
 * 断った回は覚えない（叩き続けても待ちが延びない）。期間の端ちょうど（now - t === windowMs）は期間の外。
 */
export function slideConnectWindow(
  stamps: readonly number[],
  now: number,
  limit: number = CONNECT_LIMIT_PER_MINUTE,
  windowMs: number = CONNECT_WINDOW_MS,
): { allowed: boolean; stamps: number[] } {
  const live = stamps.filter((t) => t > now - windowMs);
  if (live.length >= limit) return { allowed: false, stamps: live };
  return { allowed: true, stamps: [...live, now] };
}

const IPV4_RE = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;

function isIpv4(s: string): boolean {
  const m = IPV4_RE.exec(s);
  return m !== null && m.slice(1).every((o) => Number(o) <= 255);
}

/** IPv6 を 16 bit × 8 に分ける。読めなければ null。ゾーン（%eth0）と末尾の IPv4 表記は受ける。 */
function parseIpv6(input: string): number[] | null {
  let t = input.split("%")[0].toLowerCase();
  const tail4 = /(\d{1,3}(?:\.\d{1,3}){3})$/.exec(t);
  if (tail4 !== null) {
    if (!isIpv4(tail4[1])) return null;
    const o = tail4[1].split(".").map(Number);
    t = `${t.slice(0, -tail4[1].length)}${((o[0] << 8) | o[1]).toString(16)}:${((o[2] << 8) | o[3]).toString(16)}`;
  }
  const halves = t.split("::");
  if (halves.length > 2) return null;
  const parse = (h: string): number[] | null => {
    if (h === "") return [];
    const out: number[] = [];
    for (const g of h.split(":")) {
      if (!/^[0-9a-f]{1,4}$/.test(g)) return null;
      out.push(parseInt(g, 16));
    }
    return out;
  };
  const head = parse(halves[0]);
  const rest = halves.length === 2 ? parse(halves[1]) : [];
  if (head === null || rest === null) return null;
  if (halves.length === 1) return head.length === 8 ? head : null;
  const fill = 8 - head.length - rest.length;
  if (fill < 1) return null;
  return [...head, ...new Array<number>(fill).fill(0), ...rest];
}

/**
 * 回数を数える鍵。IPv4 はそのまま、IPv6 は上位 64 bit（/64）だけ（端末は /64 を丸ごと持つので、一時アドレスで数えを逃れられないように）。
 * IPv4 射影は IPv4 として数える。ヘッダが無いのはローカルの試しだけなので 1 つの鍵にまとめる。読めない文字も文字のままの鍵で数える。
 */
export function connectLimitKey(ip: string | null): string {
  if (ip === null) return "no-ip";
  const s = ip.trim();
  if (isIpv4(s)) return `v4:${s}`;
  const g = parseIpv6(s);
  if (g === null) return `raw:${s.toLowerCase().slice(0, 64)}`;
  if (g.slice(0, 5).every((x) => x === 0) && g[5] === 0xffff) {
    return `v4:${g[6] >> 8}.${g[6] & 255}.${g[7] >> 8}.${g[7] & 255}`;
  }
  return `v6:${g.slice(0, 4).map((x) => x.toString(16)).join(":")}`;
}

/**
 * WebSocket を閉じるときの番号（4000〜4999 はアプリの番号）。下 3 桁は意味の近い HTTP の番号に揃えた。
 * 閉じる前に必ず {"t":"error","code":<名>} を 1 通送る（閉じる番号を読めない作りのクライアントのため）。
 */
export const Close = {
  Done: 1000,
  /** JSON でない・知らない t・役割や順番に合わない文・バイナリの文。 */
  BadMessage: 4400,
  Expired: 4408,
  PeerLeft: 4410,
  /** 1 通が MESSAGE_MAX_BYTES を越えた・コードが CODE_MAX_CHARS を越えた。 */
  TooLarge: 4413,
  BadCode: 4422,
  /** 一覧に載せられる数（全体・1 IP あたり）を越えた。 */
  ListFull: 4429,
} as const;

export type CloseName = "done" | "bad_message" | "expired" | "peer_left" | "too_large" | "bad_code" | "list_full";

export const CLOSE_NAME: Record<number, CloseName> = {
  [Close.Done]: "done",
  [Close.BadMessage]: "bad_message",
  [Close.Expired]: "expired",
  [Close.PeerLeft]: "peer_left",
  [Close.TooLarge]: "too_large",
  [Close.BadCode]: "bad_code",
  [Close.ListFull]: "list_full",
};

/**
 * 入室の要求の問い `?cid=<16 進 32 文字>`: クライアントが起動ごとに引く乱数。同じ値の古い接続は同じ利用者の入り直し前の残りなので、
 * 新しい入室が置き換える（届かなかった入室が 3 人目扱いで断られ続けないように）。無い・形が違うものは付けなかったものとして扱う
 * （古いクライアントと同じ振る舞い）。
 */
export const CLIENT_ID_PARAM = "cid";
export const CLIENT_ID_LEN = 32;
const CLIENT_ID_RE = /^[0-9a-f]{32}$/;

export function parseClientId(raw: string | null): string | null {
  return raw !== null && CLIENT_ID_RE.test(raw) ? raw : null;
}

/** パスのルーム名の区切り（符号化したまま）を文字に戻す。戻せない符号と長すぎる区切りは null（400）。`+` は空白に戻さない。 */
export function decodeRoomName(segment: string): string | null {
  if (segment.length > ROOM_SEGMENT_MAX_CHARS) return null;
  try {
    return decodeURIComponent(segment);
  } catch {
    return null;
  }
}

/** ルームを引く形（ROOM_NAME_FOLD の順に揃えた文字）にする。決まりの外なら null（400）。 */
export function normalizeRoomName(raw: string): string | null {
  // 代理の片割れは decodeRoomName からは来ないが、直に呼ぶ入口のために断る。
  if (!raw.isWellFormed()) return null;
  if (utf8Length(raw) > ROOM_NAME_MAX_BYTES) return null;
  if (ROOM_NAME_CONTROL_RE.test(raw)) return null;
  for (const c of ROOM_NAME_BAD_CHARS) {
    if (raw.includes(c)) return null;
  }
  if (ROOM_NAME_BAD_NAMES.includes(raw)) return null;
  const key = raw.normalize("NFKC").trim().toLowerCase();
  const chars = [...key].length;
  if (chars < ROOM_NAME_MIN || chars > ROOM_NAME_MAX) return null;
  return key;
}

export function roomNameFromPath(pathname: string): string | null {
  if (!pathname.startsWith(PROTO_PATH_PREFIX)) return null;
  const rest = pathname.slice(PROTO_PATH_PREFIX.length);
  if (rest.length === 0 || rest.includes("/")) return null;
  return rest;
}

export type Parsed =
  | { kind: "offer"; code: string }
  | { kind: "answer"; code: string }
  | { kind: "error"; close: number };

/** 届いた 1 通を読む。文字・大きさ・形だけを見る（順と役割は Room が見る）。大きさを最初に見るのは、大きなものを JSON として解かないため。 */
export function parseClientMessage(data: string | ArrayBuffer): Parsed {
  if (typeof data !== "string") return { kind: "error", close: Close.BadMessage };
  if (utf8Length(data) > MESSAGE_MAX_BYTES) return { kind: "error", close: Close.TooLarge };
  let v: unknown;
  try {
    v = JSON.parse(data);
  } catch {
    return { kind: "error", close: Close.BadMessage };
  }
  if (typeof v !== "object" || v === null || Array.isArray(v)) {
    return { kind: "error", close: Close.BadMessage };
  }
  const o = v as Record<string, unknown>;
  if (o.t !== "offer" && o.t !== "answer") return { kind: "error", close: Close.BadMessage };
  const c = checkCode(o.code);
  if (c !== null) return { kind: "error", close: c };
  return { kind: o.t, code: o.code as string };
}

export function checkCode(code: unknown): number | null {
  if (typeof code !== "string") return Close.BadMessage;
  if (code.length > CODE_MAX_CHARS) return Close.TooLarge;
  // 前後の空白は受けない（送る側の dll が整える約束で、サーバーは 1 通りの形だけを渡す）。
  if (!CODE_RE.test(code)) return Close.BadCode;
  return null;
}

function utf8Length(s: string): number {
  return new TextEncoder().encode(s).byteLength;
}

export type ServerMessage =
  | { t: "room"; role: "host" | "guest"; expires_at: number }
  | { t: "queue"; expires_at: number }
  | { t: "paired"; room: string; order: "first" | "second" }
  | { t: "matched"; expires_at: number }
  | { t: "peer"; code: string }
  | { t: "listed" }
  | { t: "list"; items: WatchItem[] }
  | { t: "error"; code: CloseName };
