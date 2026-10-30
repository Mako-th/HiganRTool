// 席のチケットの作成と検証。形は`<発行の UNIX 秒>.<期限の UNIX 秒>.<MAC>`。
// MACはSEAT_KEYのUTF-8を鍵にしたHMAC-SHA256をbase64url（パディングなし。TICKET_MAC_CHARS文字）で書いたもの。
// 署名する文は用途のタグ・観戦用のルーム名w.…・発行・期限を改行でつないだもの。
// チケットも鍵もログ（console）に出さない。

import { SEAT_KEY_MIN_CHARS, TICKET_MAC_CHARS, TICKET_NUM_MAX_DIGITS, TICKET_PURPOSE, TICKET_TTL_SECS } from "./rules";

const TICKET_NUM = `(0|[1-9][0-9]{0,${TICKET_NUM_MAX_DIGITS - 1}})`;
const TICKET_RE = new RegExp(`^${TICKET_NUM}\\.${TICKET_NUM}\\.([A-Za-z0-9_-]{${TICKET_MAC_CHARS}})$`);

let cached: { raw: string; key: Promise<CryptoKey> } | null = null;

/** SEAT_KEYからHMACの鍵を作る。無い・SEAT_KEY_MIN_CHARSより短いならnull（チケットを作らず席と投稿は断る）。 */
export function seatKey(raw: string | undefined): Promise<CryptoKey> | null {
  if (typeof raw !== "string" || raw.length < SEAT_KEY_MIN_CHARS) return null;
  if (cached === null || cached.raw !== raw) {
    const key = crypto.subtle.importKey("raw", new TextEncoder().encode(raw), { name: "HMAC", hash: "SHA-256" }, false, [
      "sign",
      "verify",
    ]);
    cached = { raw, key };
  }
  return cached.key;
}

function signedText(watchName: string, issued: number, expires: number): Uint8Array {
  return new TextEncoder().encode(`${TICKET_PURPOSE}\n${watchName}\n${issued}\n${expires}`);
}

function toBase64Url(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function fromBase64Url(text: string): Uint8Array | null {
  try {
    const s = atob(text.replace(/-/g, "+").replace(/_/g, "/"));
    const out = new Uint8Array(s.length);
    for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
    return out;
  } catch {
    return null;
  }
}

/** 観戦用のルーム名watchNameについてissuedに発行したチケットを作る。 */
export async function mintTicket(
  key: CryptoKey,
  watchName: string,
  issued: number,
  expires: number = issued + TICKET_TTL_SECS,
): Promise<string> {
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, signedText(watchName, issued, expires)));
  return `${issued}.${expires}.${toBase64Url(mac)}`;
}

/**
 * noneはチケットが無い（鍵が無くて検証できないものも含む）。badは形・MACが合わない（別のw. の名のチケットも）。
 * expiredは署名は正しいが期限を過ぎた。validは正しく期限内（issuedは発行のUNIX秒）。
 */
export type TicketVerdict = { kind: "none" } | { kind: "bad" } | { kind: "expired" } | { kind: "valid"; issued: number };

/** チケットticketが観戦用のルーム名watchNameについて正しく、nowSecsの時点で期限内かどうか。 */
export async function judgeTicket(
  key: CryptoKey | null,
  watchName: string,
  ticket: string | null,
  nowSecs: number,
): Promise<TicketVerdict> {
  if (ticket === null) return { kind: "none" };
  const m = TICKET_RE.exec(ticket);
  if (m === null) return { kind: "bad" };
  if (key === null) return { kind: "none" };
  const issued = Number(m[1]);
  const expires = Number(m[2]);
  const mac = fromBase64Url(m[3]);
  // 最後の文字の使わないビットを変えた別の書き方を通さない（同じチケットが1通りの文字になるように）。
  if (mac === null || toBase64Url(mac) !== m[3]) return { kind: "bad" };
  if (!(await crypto.subtle.verify("HMAC", key, mac, signedText(watchName, issued, expires)))) return { kind: "bad" };
  if (nowSecs >= expires) return { kind: "expired" };
  return { kind: "valid", issued };
}
