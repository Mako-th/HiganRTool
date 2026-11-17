// 一覧のDurable Object（watch_list.ts・lobby.ts）が共有するWebSocketまわりの小さな部品。
// どちらも状態をattachmentにだけ持ち、その`done`が真なら閉じ済みとして扱う。

import { CLOSE_NAME, Close, MESSAGE_MAX_BYTES, type ServerMessage, WATCH_VIEW_SWEEP_MS } from "./rules";

export const OPEN = 1;

/** 作ってからttlMs後を掃除の間隔（WATCH_VIEW_SWEEP_MS）に切り上げた時刻。アラームの書き込みをまとめる。 */
export function sweepDeadline(created: number, ttlMs: number): number {
  return Math.ceil((created + ttlMs) / WATCH_VIEW_SWEEP_MS) * WATCH_VIEW_SWEEP_MS;
}

/** 届いた1通をJSONとして読む。大きさを最初に見る（大きなものをJSONとして解かない）。 */
export function parseObject(data: string | ArrayBuffer): Record<string, unknown> | typeof Close.TooLarge | typeof Close.BadMessage {
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

export function send(ws: WebSocket, m: ServerMessage): void {
  try {
    ws.send(JSON.stringify(m));
  } catch {
    // 相手が先に切れた。
  }
}

/** attachmentのdoneを立ててから閉じる。withErrorなら閉じる前にerrorを1通送る（doneの閉じ方は送らない）。 */
export function closeWith(ws: WebSocket, code: number, withError: boolean): void {
  const a = ws.deserializeAttachment() as { done: boolean };
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
