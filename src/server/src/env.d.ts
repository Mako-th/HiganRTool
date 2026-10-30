// SEAT_KEYはwrangler.jsoncに書かないsecret（`wrangler secret put SEAT_KEY`）なのでwrangler typesの生成物に載らない。
interface Env {
  SEAT_KEY?: string;
}
