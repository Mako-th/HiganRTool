// 検査は Workers の実行環境（ローカルの miniflare / workerd）の中で走る。外へは繋がない。
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  // 数十本の接続を張る検査があり、ファイルを並列に走らせると 5 秒の既定では CPU の取り合いで落ちる。
  test: { testTimeout: 20_000 },
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      // Cloudflare 上の本物の束縛へ繋ぐ仕組みを使わない（全部ローカルの模型）。
      remoteBindings: false,
    }),
  ],
});
