// ルームサーバーの入口（Worker）。ルーム名・バージョン・回数を確かめてから、ルームの Durable Object へ渡す。
// Durable Object へ渡す前に断れるものは全部ここで断る（Durable Object の要求は無料枠の数え物なので、
// 文字の悪い名前や回数超えで起こさない）。

import {
  RANDOM_PATH_PREFIX,
  RETRY_AFTER_SECS,
  WATCH_PATH_PREFIX,
  connectLimitKey,
  decodeRoomName,
  normalizeRoomName,
  parseBuildSegment,
  parseWatchPath,
  roomNameFromPath,
} from "./rules";

import { handleUpdate } from "./update";

export { ConnectLimiter } from "./connect_limiter";
export { RandomQueue } from "./random_queue";
export { Room } from "./room";
export { WatchList } from "./watch_list";

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    // 更新のエンドポイント（/update/…）。Durable Object にも回数の数えにも触らない（回すだけのエンドポイント）。ルームのエンドポイントより先に見る。
    const update = handleUpdate(req, env.UPDATE_BASE_URL);
    if (update !== null) return update;
    const url = new URL(req.url);

    // ランダム対戦の待ち列のエンドポイント（/v0/random/<ビルド番号>）。
    if (url.pathname.startsWith(RANDOM_PATH_PREFIX)) {
      if (req.method !== "GET") return Response.json({ error: "method_not_allowed" }, { status: 405 });
      const build = parseBuildSegment(url.pathname.slice(RANDOM_PATH_PREFIX.length));
      if (build === null) return Response.json({ error: "bad_build" }, { status: 400 });
      if (req.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
        return Response.json({ error: "upgrade_required" }, { status: 426 });
      }
      const limited = await overLimit(req, env);
      if (limited !== null) return limited;
      return env.RANDOM_QUEUE.get(env.RANDOM_QUEUE.idFromName(`b${build}`)).fetch(req);
    }

    // 進行中のランダム対戦の一覧のエンドポイント（/v0/watch/<ビルド番号>/post と /view）。回数の数えは待ち列と同じ。
    if (url.pathname.startsWith(WATCH_PATH_PREFIX)) {
      if (req.method !== "GET") return Response.json({ error: "method_not_allowed" }, { status: 405 });
      const { build, kind } = parseWatchPath(url.pathname);
      if (kind === null) return Response.json({ error: "not_found" }, { status: 404 });
      if (build === null) return Response.json({ error: "bad_build" }, { status: 400 });
      if (req.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
        return Response.json({ error: "upgrade_required" }, { status: 426 });
      }
      const limited = await overLimit(req, env);
      if (limited !== null) return limited;
      return env.WATCH_LIST.get(env.WATCH_LIST.idFromName(`b${build}`)).fetch(req);
    }

    const raw = roomNameFromPath(url.pathname);
    if (raw === null) return Response.json({ error: "not_found" }, { status: 404 });
    if (req.method !== "GET") return Response.json({ error: "method_not_allowed" }, { status: 405 });
    // ルーム名は UTF-8 のパーセント符号化で来る。戻せない符号も文字の決まりの外と同じ 400。
    // 名はログに出さない（room.ts の冒頭。利用者の打った文字なので）。
    const decoded = decodeRoomName(raw);
    const name = decoded === null ? null : normalizeRoomName(decoded);
    if (name === null) return Response.json({ error: "bad_room_name" }, { status: 400 });
    if (req.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return Response.json({ error: "upgrade_required" }, { status: 426 });
    }
    const limited = await overLimit(req, env);
    if (limited !== null) return limited;

    const stub = env.ROOMS.get(env.ROOMS.idFromName(name));
    return stub.fetch(req);
  },
} satisfies ExportedHandler<Env>;

/**
 * 1 IP あたりの回数（IPv6 は /64 でまとめる。鍵の作り方は rules.ts の connectLimitKey）。超えていれば 429 を返す。
 * CF-Connecting-IP は Cloudflare の入口が付け直す（利用者が偽れない）。
 * 数えるのは IP ごとの Durable Object。ルーム（待ち列）に入るときの 1 往復だけ増える（マッチ中の P2P には効かない）。
 * 順は「粗い前段（Rate Limiting の束縛。場所ごと・結果整合）→ 通ったものだけ IP ごとの Durable Object で正確に」。
 * 前段を置くのは、Durable Object への問い合わせが断る回も 1 要求に数えられ、荒らしに無料枠（10 万要求/日）を使い切られないため。
 */
async function overLimit(req: Request, env: Env): Promise<Response | null> {
  const key = connectLimitKey(req.headers.get("CF-Connecting-IP"));
  let limited = false;
  try {
    limited = !(await env.CONNECT_LIMIT.limit({ key })).success;
  } catch {
    // 前段が落ちたときは通して、Durable Object の正確な数えに任せる。
  }
  if (!limited) {
    try {
      limited = !(await env.CONNECT_LIMITER.get(env.CONNECT_LIMITER.idFromName(key)).hit());
    } catch {
      // 数える仕組みが落ちたときは通す（歯止めが落ちて全員が繋げなくなるより、歯止めなしで動く方がよい）。
      // 落ちたことは固定の文だけを残す（wrangler tail で数えられる。鍵・IP・ルーム名は出さない）。
      console.warn("connect_limiter_failed");
    }
  }
  if (!limited) return null;
  return Response.json(
    { error: "rate_limited", retry_after: RETRY_AFTER_SECS },
    { status: 429, headers: { "Retry-After": String(RETRY_AFTER_SECS) } },
  );
}
