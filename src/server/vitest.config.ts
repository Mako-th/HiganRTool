// 検査は Workers の実行環境（ローカルの miniflare / workerd）の中で走る。外へは繋がない。
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

/** 検査だけの SEAT_KEY（本物の鍵は wrangler secret で入れ、ここにもリポジトリにも書かない）。 */
const TEST_SEAT_KEY = "test-only-seat-key-not-for-deploy-0123456789";

/** SEAT_KEY が無いときの振る舞いを見る検査（別の実行環境で走らせる）。 */
const NO_KEY_TESTS = "test/no_seat_key.test.ts";

function workers(bindings: Record<string, string>) {
  return cloudflareTest({
    wrangler: { configPath: "./wrangler.jsonc" },
    // Cloudflare 上の本物の束縛へ繋ぐ仕組みを使わない（全部ローカルの模型）。
    remoteBindings: false,
    miniflare: { bindings },
  });
}

export default defineConfig({
  test: {
    projects: [
      {
        plugins: [workers({ SEAT_KEY: TEST_SEAT_KEY })],
        // 数十本の接続を張る検査があり、ファイルを並列に走らせると 5 秒の既定では処理の取り合いで落ちる。
        test: { name: "seat-key", include: ["test/**/*.test.ts"], exclude: [NO_KEY_TESTS], testTimeout: 20_000 },
      },
      {
        plugins: [workers({})],
        test: { name: "no-seat-key", include: [NO_KEY_TESTS], testTimeout: 20_000 },
      },
    ],
  },
});
