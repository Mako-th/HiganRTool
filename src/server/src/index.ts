// ルームサーバーの入口（Worker）。ルーム名・バージョン・回数をチェックしてからルームのDurable Objectへ渡す。
// Durable Objectへ渡す前に断れるものは全部ここで断る。
// Durable Objectへの要求は無料枠に数えられるので、使えない文字を含む名前や回数超過では呼ばない。

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
    // 更新のエンドポイント（/update/…）。Durable Objectにも回数のカウントにも触らない（配布元へ転送するだけのエンドポイント）。ルームのエンドポイントより先に見る。
    const update = await handleUpdate(req, env.UPDATE_BASE_URL);
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

    // 進行中のランダム対戦の一覧のエンドポイント（/v0/watch/<ビルド番号>/postと /view）。回数のカウントは待ち列と同じ。
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
    // ルーム名はUTF-8のパーセントエンコードで来る。
    // デコードできない場合も使えない文字と同じ400を返す。名前はログに出さない（room.tsの冒頭を参照。利用者が打った文字なので）。
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
 * 1 IPあたりの回数（IPv6は /64でまとめる。鍵の作り方はrules.tsのconnectLimitKey）。超えていれば429を返す。
 * CF-Connecting-IPはCloudflareの入口が付け直す（利用者が偽れない）。
 * 数えるのはIPごとのDurable Object。ルーム（待ち列）に入るときの1往復だけ増える（マッチ中のP2Pには効かない）。
 * 順序は「粗い前段（Rate Limitingのバインディング。ロケーションごと・結果整合）→ 通ったものだけIPごとのDurable Objectで正確に」。
 * 前段を設けるのは、Durable Objectへの問い合わせが断る場合も1要求に数えられて、荒らしに無料枠（10万要求/日）を使い切られるのを防ぐため。
 */
async function overLimit(req: Request, env: Env): Promise<Response | null> {
  const key = connectLimitKey(req.headers.get("CF-Connecting-IP"));
  let limited = false;
  try {
    limited = !(await env.CONNECT_LIMIT.limit({ key })).success;
  } catch {
    // 前段が失敗したときは通す。判定はDurable Objectの正確なカウントに任せる。
  }
  if (!limited) {
    try {
      limited = !(await env.CONNECT_LIMITER.get(env.CONNECT_LIMITER.idFromName(key)).hit());
    } catch {
      // 数える仕組みが失敗したときは通す（制限が失敗して全員が接続できなくなるより、制限なしで動く方がよい）。
      // 失敗したことは固定の文だけをログに残す（wrangler tailで件数を数えられる。鍵・IP・ルーム名は出さない）。
      console.warn("connect_limiter_failed");
    }
  }
  if (!limited) return null;
  return Response.json(
    { error: "rate_limited", retry_after: RETRY_AFTER_SECS },
    { status: 429, headers: { "Retry-After": String(RETRY_AFTER_SECS) } },
  );
}
