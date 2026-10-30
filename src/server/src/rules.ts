// ルームサーバーv0の決まり（文字・長さ・期限・誤りの番号）と届いた文の読み方。WorkersのAPIを使わない純粋な部分。
// v0（実験）なので値は変えてよい。

/** URLのバージョン。dllの側が話すバージョンと合わなければ404になる。 */
export const PROTO_PATH_PREFIX = "/v0/rooms/";

/**
 * ルーム名。日本語を含む好きな文字を使える（パスにはUTF-8をパーセント符号化して載せる。ここで戻す）。
 * ルームの検索に使う文字はROOM_NAME_FOLDの順（NFKC → 前後の空白を除く → 小文字）に揃える。全角半角・大文字小文字・前後の空白の違いが同じルームになる。揃えるのはサーバーだけ（dllはNFKCを持たない）。
 * 断るもの（400 bad_room_name）: 制御文字（Cc）・`/`（符号化を戻す途中の機器が %2Fを`/`に戻すとパスが割れうる）・`.`と`..`（URLのパスの読み方で消える）・揃えた後が0文字か33文字以上（HELLOのnameの上限wire::NAME_MAX = 32。コードポイントで数える）・揃える前のUTF-8が128 B超（揃える手間とパスの長さの上限）。
 * 文字と長さのガードは揃える前の文字で見る（dllがNFKCなしで同じ判断をできるように）。揃えて現れる`/`（全角の`／`など）は通す。
 */
export const ROOM_NAME_MIN = 1;
export const ROOM_NAME_MAX = 32;
export const ROOM_NAME_MAX_BYTES = 128;
/** 制御文字の範囲（見本rules.jsonのroom_name_controlsと同じ）。 */
const ROOM_NAME_CONTROL_RE = /\p{Cc}/u;
export const ROOM_NAME_BAD_CHARS = "/";
export const ROOM_NAME_BAD_NAMES: readonly string[] = [".", ".."];
/** 揃える順。検査は見本の同じ名の列とこれを突き合わせる。 */
export const ROOM_NAME_FOLD: readonly string[] = ["nfkc", "trim", "lower"];
/** パスのルーム名の区切り（符号化したまま）の文字の上限。128 Bの名は1 Bが最大3文字（%XX）なのでこれを越えれば戻すまでもなく128 Bを越える。 */
const ROOM_SEGMENT_MAX_CHARS = ROOM_NAME_MAX_BYTES * 3;

/**
 * 接続コードの文字の上限。coreの読み手の上限（`MAX_CODE_CHARS` = 512）の2倍。いまの最長のコードは354文字。
 * v0のうちに伸びても出し直さずに済む余裕を持たせた。サーバーは文字がbase64urlであることだけを見る。
 */
export const CODE_MAX_CHARS = 1024;
const CODE_RE = /^[A-Za-z0-9_-]+$/;

/** 1通の文の上限（UTF-8のバイト）。越えたら中を読まずに断る。 */
export const MESSAGE_MAX_BYTES = 2048;

/**
 * ルームの期限: 最初の人が接続してから10分。接続コードの期限（30分）より短くして待ち合わせ中にコードが切れないようにした。
 * 誰も来なければルーム名の打ち間違いか相手の不在と見て閉じる。
 */
export const ROOM_TTL_MS = 10 * 60 * 1000;

/** 相手が揃ってから入る側がanswerを送るまでの期限。コードを読んで返すだけなので1分あれば足りる。 */
export const ANSWER_TTL_MS = 60 * 1000;

/**
 * 対戦のルームは成立（hostへanswerを渡した）の後も2人を部屋に残す。ルーム名は使用中のままにする（ルームの期限では閉じない）。
 * 最後の生存確認（PING_TEXTの自動応答の時刻。無ければ成立の時刻）がこれより古い接続は閉じる。もう1人も閉じて部屋を空ける。
 * クライアントは30秒ごとに送るので3回分（クライアントがpongを待つ上限と同じ）。
 */
export const HELD_STALE_MS = 90 * 1000;
/** 成立の後の部屋のアラームの間隔。生存確認の古さはこの間隔で見る（Durable Objectの起動を数分に1回に抑える）。 */
export const HELD_SWEEP_MS = 5 * 60 * 1000;

/** 入口の409の本文のerror。対戦のルームに2人いるとき（成立の前も後も）はInUse、観戦用のルーム（w.…）に空きが無いときはFull。 */
export const RoomRefusal = {
  InUse: "room_in_use",
  Full: "room_full",
} as const;

/**
 * 観戦用のルーム（w.…）で観戦者がhostのコードを受けてからanswerを送るまでの期限（観戦者ごと）。
 * 観戦者のdllはコードを受けたポーリング（50 msごと）の中で返答を作って送る。そのため回線の往復を足しても1秒ほどで届く（推定。測っていない）。答えずに枠を占める接続を早く外すため短くした。
 * 過ぎた観戦者だけを閉じる。hostは残して次の観戦者へ同じコードを渡す（hostの期限は最初の成立からANSWER_TTL_MSのまま）。
 */
export const WATCH_ANSWER_TTL_MS = 10 * 1000;

/**
 * 席のチケット: 対戦の成立で2人へ渡す。サーバーの鍵SEAT_KEY（Workerのsecret）で署名してある。
 * 観戦用のルームのhostの席（upgradeのURLのクエリ）と一覧への投稿（postのフィールド）の資格にする。形と検証はticket.ts。
 */
export const TICKET_PARAM = "ticket";
/** チケットの期限: 発行から24時間（1回の対戦の間に配信側が観戦者ごとにw. へ入り直すため）。 */
export const TICKET_TTL_SECS = 24 * 60 * 60;
/** チケットの署名に含める用途のタグ。同じ鍵で別の用途の署名を作ったときにチケットとして通らないようにする。 */
export const TICKET_PURPOSE = "higanr-seat-v1";
/** チケットの発行・期限の10進の桁の上限（UNIX秒。先頭に0を付けない）。 */
export const TICKET_NUM_MAX_DIGITS = 12;
/** チケットのMAC（HMAC-SHA256の32 B）をbase64url（パディングなし）で書いた文字数。 */
export const TICKET_MAC_CHARS = 43;
/** SEAT_KEYの文字数の下限。これより短い値は鍵が無いものとして扱う（推測できる短い鍵でチケットを作らせない）。 */
export const SEAT_KEY_MIN_CHARS = 32;

/**
 * ランダム対戦の待ち列のパス`/v0/random/<ビルド番号>`。ビルド番号ごとに別の待ち列にする（違うビルドどうしは接続できないため）。
 * 番号は10進の整数0〜4294967295を先頭に0を付けない1通りの書き方だけで受ける（同じ番号が2つの待ち列にならないように）。
 */
export const RANDOM_PATH_PREFIX = "/v0/random/";
const RANDOM_BUILD_RE = /^(0|[1-9][0-9]{0,9})$/;
const BUILD_MAX = 4_294_967_295;

/** 待ち列での期限: ルームと同じ10分（切れたらクライアントが自動で入り直す）。 */
export const RANDOM_QUEUE_TTL_MS = ROOM_TTL_MS;

/**
 * ペアにした後に割り当てるルームの期限。2人は数秒で入る前提で1分にする。
 * ペアにした直後に片方が消えても残った側は10分待たずに待ち列へ戻れる。
 */
export const RANDOM_ROOM_TTL_MS = 60 * 1000;

/**
 * 割り当てるルーム名: `q.` ＋ 乱数120 bitの16進30文字（計32文字 = ROOM_NAME_MAX）。
 * クライアントは`q.`で始まる名を断る（room.rsの`parse`）ので人が打つルーム名とぶつからない。推測もできない長さにした。観戦用のルームの名の先頭`w.`（クライアントのroom.rs）とも別の先頭にした。
 */
export const RANDOM_ROOM_PREFIX = "q.";
export const RANDOM_ROOM_HEX_LEN = 30;
const RANDOM_ROOM_RE = /^q\.[0-9a-f]{30}$/;

/**
 * 進行中のランダム対戦の一覧（`/v0/watch/<ビルド番号>/post`と`/v0/watch/<ビルド番号>/view`）。
 * postは対戦のホストが持ち続けて載せるエンドポイント。viewは観戦者が一覧を開いている間だけ持つエンドポイント。
 * ビルド番号ごとに別の一覧にする（違うビルドは観戦できないため）。
 */
export const WATCH_PATH_PREFIX = "/v0/watch/";
export const WATCH_POST_SEGMENT = "post";
export const WATCH_VIEW_SEGMENT = "view";

/** 一覧に載せられる数: 全体で100件・1 IP（IPv6は /64）から2件まで。 */
export const WATCH_MAX_TOTAL = 100;
export const WATCH_MAX_PER_IP = 2;

/** 観戦用のルーム名: `w.` ＋ SHA-256(ルーム名のASCII小文字)の先頭8 Bの16進（クライアントのroom.rsの`spectate_of`と同じ）。 */
export const WATCH_ROOM_PREFIX = "w.";
export const WATCH_HASH_BYTES = 8;

/** 観戦者の上限として受ける値（クライアントの`spectate::MAX_SPECTATORS`）。 */
export const WATCH_CAP_MAX = 10;
/** 受ける遅延フレーム数の最大（rollbackのDもdelay方式の遅れもここに収まる）。 */
export const WATCH_DELAY_MAX = 15;
export const WATCH_MODES: readonly string[] = ["rollback", "delay"];

/**
 * プレイヤー名の文字: クライアントの名前の入力（player_name.rs）が受ける0x20〜0x7Eのうち`/` `;` `` ` `` 以外（受けない文字を置き換えた `?` を含む）。
 * 前後の空白は無し（クライアントが取り除いて送る）。長さはHELLOのnameの上限（32）。
 */
export const WATCH_NAME_MAX = 32;
const WATCH_NAME_RE = /^[!-.0-:<-_a-~](?:[ -.0-:<-_a-~]{0,30}[!-.0-:<-_a-~])?$/;

/**
 * 載せた人が生きているかの目安: 30秒ごとのping（Durable Objectを起動しない自動応答）の最後の時刻から数える。
 * ping 3回分に余裕を足した値を越えて何も無ければ切れたものとして一覧から消す。
 */
export const WATCH_STALE_MS = 100 * 1000;
/** 接続してから最初のpostが来るまでの上限。 */
export const WATCH_UNPOSTED_MS = 15 * 1000;
/** 一覧の変化を観戦者へ送る最短の間隔（変化が続いても1秒に1回にまとめる。載せた人が連打して全員へ増幅されないため）。 */
export const WATCH_BROADCAST_MIN_MS = 1000;
/**
 * 載せるエンドポイントがWATCH_COUNT_WINDOW_MSのうちに送ってよいcountの数（サーバーだけの値）。
 * 観戦者10人の出入りが重なっても20通ほどなのでそれを越えるものは荒らしとして閉じる。再送は接続の回数の制限で縛られる。
 */
export const WATCH_COUNT_BURST = 30;
export const WATCH_COUNT_WINDOW_MS = 10 * 1000;

/** 一覧の1通の上限（バイト）。1件150 B前後 × 100件に余裕を足した値。 */
export const WATCH_LIST_MAX_BYTES = 32 * 1024;

/**
 * viewエンドポイントの接続の上限: 全体で500本・同じ鍵（connectLimitKey。IPv4は1アドレス・IPv6は /64）から3本まで・作ってから10分で閉じる。
 * 同じ鍵の4本目は一番古い接続を閉じて受け入れる。全体が500本なら新しい接続を409 room_fullで断る。
 * 期限はWATCH_VIEW_SWEEP_MSの単位に切り上げてアラームの書き込みをまとめる。
 */
export const WATCH_VIEW_MAX_TOTAL = 500;
export const WATCH_VIEW_MAX_PER_KEY = 3;
export const WATCH_VIEW_TTL_MS = 10 * 60 * 1000;
export const WATCH_VIEW_SWEEP_MS = 30 * 1000;

/** 載せるエンドポイントに届く1通の上限はMESSAGE_MAX_BYTES（postは300 Bほど）。 */

export interface WatchItem {
  /** 観戦用のルーム名（w.…）。割り当てのルーム名q.… は持たない。 */
  room: string;
  p1: string;
  p2: string;
  mode: string;
  delay: number;
  watchers: number;
  cap: number;
}

/** 元のルーム名の前後から除く文字（クライアントのroom.rsの `is_trimmed_space` = Unicode のWhite_SpaceとU+FEFF）。 */
const SPECTATE_TRIM_CLASS = "[\\u0009-\\u000D\\u0020\\u0085\\u00A0\\u1680\\u2000-\\u200A\\u2028\\u2029\\u202F\\u205F\\u3000\\uFEFF]";
const SPECTATE_TRIM_RE = new RegExp(`^${SPECTATE_TRIM_CLASS}+|${SPECTATE_TRIM_CLASS}+$`, "g");

/**
 * 元の対戦のルーム名から観戦用のルーム名を導く。クライアントの`RoomName::spectate_of`とバイト単位で同じ:
 * 前後の空白を除いてUTF-8のバイトのうちASCIIの大文字だけを小文字にし、SHA-256に渡す。
 * NFKCやASCII以外の小文字化をする検索用の形（normalizeRoomName）で導くと、ASCII以外の名ではクライアントが導く名と食い違う。
 * 割り当ての名q.… は小文字の16進なので変わらない。
 */
export async function spectateRoomName(room: string): Promise<string> {
  const bytes = new TextEncoder().encode(room.replace(SPECTATE_TRIM_RE, ""));
  for (let i = 0; i < bytes.length; i++) {
    if (bytes[i] >= 0x41 && bytes[i] <= 0x5a) bytes[i] += 0x20;
  }
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  let hex = "";
  for (const b of digest.slice(0, WATCH_HASH_BYTES)) hex += b.toString(16).padStart(2, "0");
  return WATCH_ROOM_PREFIX + hex;
}

export type ParsedPost =
  | { ok: true; room: string; p1: string; p2: string; mode: string; delay: number; watchers: number; cap: number; ticket: string }
  | { ok: false };

function intIn(v: unknown, lo: number, hi: number): v is number {
  return typeof v === "number" && Number.isInteger(v) && v >= lo && v <= hi;
}

/**
 * 載せるエンドポイントの最初の1通`{"t":"post",…}`を読む。割り当ての形の名（isRandomRoomName。揃えずに厳密に）・名前の文字・方式・数の範囲を全部見る。
 * チケット（ticket）は文字列であることだけを見る（形と署名はWatchListがticket.tsで見る）。
 */
export function parsePost(o: Record<string, unknown>): ParsedPost {
  const { room, p1, p2, mode, delay, watchers, cap, ticket } = o;
  if (typeof room !== "string" || !isRandomRoomName(room)) return { ok: false };
  if (typeof ticket !== "string") return { ok: false };
  if (typeof p1 !== "string" || !WATCH_NAME_RE.test(p1)) return { ok: false };
  if (typeof p2 !== "string" || !WATCH_NAME_RE.test(p2)) return { ok: false };
  if (typeof mode !== "string" || !WATCH_MODES.includes(mode)) return { ok: false };
  if (!intIn(delay, 0, WATCH_DELAY_MAX)) return { ok: false };
  if (!intIn(cap, 1, WATCH_CAP_MAX)) return { ok: false };
  if (!intIn(watchers, 0, cap)) return { ok: false };
  return { ok: true, room, p1, p2, mode, delay, watchers, cap, ticket };
}

/** パス`/v0/watch/<番号>/<post|view>`を読む。決まりの外ならnull。 */
export function parseWatchPath(pathname: string): { build: number | null; kind: "post" | "view" | null } {
  const rest = pathname.slice(WATCH_PATH_PREFIX.length);
  const parts = rest.split("/");
  if (parts.length !== 2) return { build: null, kind: null };
  const kind = parts[1] === WATCH_POST_SEGMENT ? "post" : parts[1] === WATCH_VIEW_SEGMENT ? "view" : null;
  return { build: parseBuildSegment(parts[0]), kind };
}

/** パス`/v0/random/<番号>`の番号部分を読む。決まりの外ならnull（400 bad_build）。 */
export function parseBuildSegment(seg: string): number | null {
  if (!RANDOM_BUILD_RE.test(seg)) return null;
  const n = Number(seg);
  return n <= BUILD_MAX ? n : null;
}

/** 割り当てるルーム名を生成する（OSの乱数。`random`は検査が差し替える箇所）。 */
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

/** 揃えた後のルーム名がランダム対戦の割り当てた形かどうか。 */
export function isRandomRoomName(name: string): boolean {
  return RANDOM_ROOM_RE.test(name);
}

/** ルームのエンドポイントのパス（`/v0/rooms/<名>`）の名の符号化だけを戻す（クライアントが打った形にする）。戻せなければnull。 */
export function decodedRoomNameOfPath(pathname: string): string | null {
  const raw = roomNameFromPath(pathname);
  return raw === null ? null : decodeRoomName(raw);
}

/** ルームのエンドポイントのパス（`/v0/rooms/<名>`）の名を入口と同じ決まりで検索用の形にする。決まりの外ならnull。 */
export function normalizedRoomNameOfPath(pathname: string): string | null {
  const decoded = decodedRoomNameOfPath(pathname);
  return decoded === null ? null : normalizeRoomName(decoded);
}

/** ルームのエンドポイントのパス（`/v0/rooms/<名>`）からそのルームの期限を決める。ランダム対戦の割り当てた名だけ短い。 */
export function roomTtlForPath(pathname: string): number {
  const name = normalizedRoomNameOfPath(pathname);
  return name !== null && isRandomRoomName(name) ? RANDOM_ROOM_TTL_MS : ROOM_TTL_MS;
}

/**
 * 観戦用のルーム（名の先頭`w.`）かどうか。検索用の形にした名で判断する（大文字・全角でも同じルームになるので同じ判断にする）。
 * 利用者は`w.`で始まる名を打てない（クライアントのroom.rsの`parse`が断る）ので、対戦のルームとは重ならない。
 */
export function isWatchRoomPath(pathname: string): boolean {
  const name = normalizedRoomNameOfPath(pathname);
  return name !== null && name.startsWith(WATCH_ROOM_PREFIX);
}

/**
 * 生存確認: 部屋にいる間（成立の後も）30秒ごとに送る文字列。完全一致のときだけDurable Objectを起動せずにPONGを返す（setWebSocketAutoResponse）。
 * そのため空白や鍵の順を変えない。ルームの期限は延ばさない。
 */
export const PING_TEXT = '{"t":"ping"}';
export const PONG_TEXT = '{"t":"pong"}';

/**
 * 1 IPあたりの接続の回数: 60秒に30回まで（超えたら429と retry_after = 60）。普通は1マッチに1〜2回。
 * 30は同じ家の複数台が1つのIPv4を分け合っても足りる値（観戦の入り直しだけで10を超える）。
 * Rate Limitingのバインディングだけでは場所ごと・結果整合になり正確に数えられない（実サーバーで上限を越えて通った）。
 * そのため数える役はDurable Objectに持たせた。
 */
export const CONNECT_LIMIT_PER_MINUTE = 30;
/**
 * 粗い前段（Rate LimitingのバインディングCONNECT_LIMIT）の上限: 60秒に120回。精度が目的ではない。
 * 荒らしがDurable Objectの要求（無料枠10万/日）を使い切るのを防ぐ。wrangler.jsoncのratelimitsのlimitと同じ値にする。
 */
export const CONNECT_PRELIMIT_PER_MINUTE = 120;
export const RETRY_AFTER_SECS = 60;
/** 回数を数える期間の長さ（待たせる秒数と同じ60秒）。 */
export const CONNECT_WINDOW_MS = RETRY_AFTER_SECS * 1000;

/**
 * スライディングウィンドウ（直近windowMsの許した時刻を全部覚える）で1回数える。固定期間だと区切りをまたいで60秒に2倍通せてしまう。
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

/** IPv6を 16 bit × 8 に分ける。読めなければnull。ゾーン（%eth0）と末尾のIPv4表記は受ける。 */
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
 * 回数を数える鍵。IPv4はそのままの値。IPv6は上位64 bit（/64）だけ（端末は /64を丸ごと持つので一時アドレスで数えを逃れられないように）。
 * IPv4射影はIPv4として数える。ヘッダが無いのはローカルの試しだけなので1つの鍵にまとめる。読めない文字も文字のままの鍵で数える。
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
 * WebSocketを閉じるときの番号（4000〜4999はアプリの番号）。下3桁は意味の近いHTTPの番号に揃えた。
 * 閉じる前に必ず {"t":"error","code":<名>} を1通送る（閉じる番号を読めない作りのクライアントのため）。
 */
export const Close = {
  Done: 1000,
  /** JSONでない・知らないt・役割や順番に合わない文・バイナリの文。 */
  BadMessage: 4400,
  Expired: 4408,
  PeerLeft: 4410,
  /** 1通がMESSAGE_MAX_BYTESを越えた・コードがCODE_MAX_CHARSを越えた。 */
  TooLarge: 4413,
  /** 観戦を断ったhostの観戦用のルームに来た観戦者（入ったときと待っていたときの両方）。下3桁は403に揃えた。 */
  Refused: 4403,
  BadCode: 4422,
  /** 一覧に載せられる数（全体・1 IPあたり）を越えた。 */
  ListFull: 4429,
} as const;

export type CloseName =
  | "done"
  | "bad_message"
  | "expired"
  | "peer_left"
  | "too_large"
  | "refused"
  | "bad_code"
  | "list_full";

export const CLOSE_NAME: Record<number, CloseName> = {
  [Close.Done]: "done",
  [Close.BadMessage]: "bad_message",
  [Close.Expired]: "expired",
  [Close.PeerLeft]: "peer_left",
  [Close.TooLarge]: "too_large",
  [Close.Refused]: "refused",
  [Close.BadCode]: "bad_code",
  [Close.ListFull]: "list_full",
};

/**
 * 入室の要求のクエリ`?cid=<16 進 32 文字>`: クライアントが起動ごとに生成する乱数。
 * 同じ値の古い接続は同じ利用者の入り直し前の残り。新しい入室が置き換える（届かなかった入室が3人目扱いで断られ続けないように）。
 * 無い・形が違うものは付けなかったものとして扱う（古いクライアントと同じ振る舞い）。
 */
export const CLIENT_ID_PARAM = "cid";
export const CLIENT_ID_LEN = 32;
const CLIENT_ID_RE = /^[0-9a-f]{32}$/;

export function parseClientId(raw: string | null): string | null {
  return raw !== null && CLIENT_ID_RE.test(raw) ? raw : null;
}

/** パスのルーム名の区切り（符号化したまま）を文字に戻す。戻せない符号と長すぎる区切りはnull（400）。`+`は空白に戻さない。 */
export function decodeRoomName(segment: string): string | null {
  if (segment.length > ROOM_SEGMENT_MAX_CHARS) return null;
  try {
    return decodeURIComponent(segment);
  } catch {
    return null;
  }
}

/** ルームの検索用の形（ROOM_NAME_FOLDの順に揃えた文字）にする。決まりの外ならnull（400）。 */
export function normalizeRoomName(raw: string): string | null {
  // サロゲートペアの片方はdecodeRoomNameからは来ない。直接呼ぶ入口のために断る。
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
  /** 観戦を断る（観戦用のルームのhostが1回だけ。本文は無い）。 */
  | { kind: "refuse" }
  | { kind: "error"; close: number };

/** 届いた1通を読む。文字・大きさ・形だけを見る（順と役割はRoomが見る）。大きさを最初に見るのは大きなものをJSONとして解かないため。 */
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
  if (o.t === "refuse") return { kind: "refuse" };
  if (o.t !== "offer" && o.t !== "answer") return { kind: "error", close: Close.BadMessage };
  const c = checkCode(o.code);
  if (c !== null) return { kind: "error", close: c };
  return { kind: o.t, code: o.code as string };
}

export function checkCode(code: unknown): number | null {
  if (typeof code !== "string") return Close.BadMessage;
  if (code.length > CODE_MAX_CHARS) return Close.TooLarge;
  // 前後の空白は受けない（送る側のdllが整える約束なのでサーバーは1通りの形だけを渡す）。
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
  /** ticketは対戦のルームで最初に揃ったとき（SEAT_KEYがあるときだけ）。 */
  | { t: "matched"; expires_at: number; ticket?: string }
  /** ticketは対戦のルームのguestへ渡すときだけ（SEAT_KEYがあるときだけ）。 */
  | { t: "peer"; code: string; ticket?: string }
  | { t: "listed" }
  | { t: "list"; items: WatchItem[] }
  | { t: "error"; code: CloseName };
