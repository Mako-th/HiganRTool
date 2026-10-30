//! ルームサーバー（`src/server/`）のクライアントの純粋な部品。
//!
//! ルーム名の検証とパスの組み立て・文の読み書き（平らなJSONの小さな読み手）・閉じる番号・入口のHTTPの誤りの本文・クライアントのステートマシンを持つ。
//! ソケットも時計も持たない。
//! WebSocketの通信（WinHTTP）と受信のスレッドはdllの仕事。
//! サーバーの値は`src/server/src/rules.ts`と同じ（`tests/fixtures/room/rules.json`で突き合わせる）。
//! 順番は`room.ts`。
//! サーバーのJSONの`"host"` / `"guest"`は読んだ境目で[`Role::Offer`] / [`Role::Answer`]へコピーする。
//! マッチのホスト（1P）と紛れないようRustの名前にはhost / guestを使わない。
//! dllは`Offer → CodeSide::Offer`・`Answer → CodeSide::Answer`とコピーする。
//! 順番: 始める側（`host`）は`room` → こちらが`offer` → `matched` → `peer`（返す側のコード）。
//! 返す側（`guest`）は`room` → `peer`（始める側のコード）→ こちらが`answer`（`matched`は来ない）。
//! 対戦のルームでは始める側の`matched`と返す側の`peer`に席のチケット（[`SeatTicket`]。観戦用のルームのhostの席の資格）が載る。
//! 順番違いは失敗にする。
//! 誤りで閉じるときは`{"t":"error","code":<名>}`を1通送ってから閉じる。
//! errorの文と閉じる番号は先に来た方で終える。
//! 対戦のルームは成立（始める側は`peer`を受けた・返す側は`answer`を送った）の後も接続を保ち、pingだけを送る（[`State::Holding`]）。
//! サーバーはその間ルーム名を使用中にし、3人目を409 `room_in_use`で断る。
//! 保っている間にサーバーが閉じても失敗にしない。
//! 観戦用のルームは成立でサーバーが1000で閉じる。
//! 観戦を断る配信側は観戦用のルームで`room`の後に`offer`の代わりに`refuse`を送り、ルームの期限（4408）まで残る。
//! 入ってきた観戦者は`room`の後に4403で閉じられる（[`Failure::Refused`]）。
//! 時刻は呼び出し側が[`Now`]で渡す（`ms`は単調な時計、`unix_secs`は壁時計）。
//! サーバーの`expires_at`はUNIX秒でPCの時計とずれうる。
//! そのためこちらの期限は「サーバーが4408を送ってくるはずの時刻の後ろの予備」として持つ（[`DEADLINE_GRACE_MS`]）。
//! 時計のずれで残りが0以下か上限より長く見えたら上限（ルームの期限10分・answerの期限60秒）で代える。
//! dllの監視のスレッドで回す。ゲームのスレッドでは呼ばない（文字列の確保がある）。
//! ほかの`src/core/`と同じくunsafe禁止。パニックしうる添字とunwrapを使わない。

use std::borrow::Cow;

use sha2::{Digest, Sha256};

// ---------------- サーバーの決まり ----------------

/// URLのパスの先頭（バージョンを含む）。
pub const PATH_PREFIX: &str = "/v0/rooms/";
/// ルーム名の長さの上限（文字 = コードポイントの数）。
/// サーバーはこれを**揃えた後**（NFKC → 前後の空白を除く → 小文字）の文字で数える。
/// こちらはNFKCを持たないので揃えても文字の数が変わらないと分かるASCIIだけの名にしかこの数を当てない（[`RoomName::parse`]）。
pub const ROOM_NAME_MAX: usize = 32;
/// ルーム名の大きさの上限（揃える前のUTF-8のバイト）。揃えずに数えられるのでサーバーと同じ数で検証する。
pub const ROOM_NAME_MAX_BYTES: usize = 128;
/// 名そのものがこれなら断る（パスに載せるとURLの読み方で区切りが消え、サーバーに届かず404になる）。
pub const ROOM_NAME_BAD_NAMES: [&str; 2] = [".", ".."];
/// 接続コードの文字の上限。
/// coreの読み手の上限（`code::MAX_CODE_CHARS` = 512）とは別。
/// サーバーはコードの中身を読まず文字と長さだけを見る。
pub const CODE_MAX_CHARS: usize = 1024;
/// 1通の文の上限（UTF-8のバイト）。
/// サーバーの送る文（いちばん長いのは1024文字のコードを載せた`peer`）もこれに収まる。
/// そのためサーバーから届く文にも同じ上限を当てる（越えたら中を読まない）。
pub const MESSAGE_MAX_BYTES: usize = 2048;
/// ルームの期限（始める側が接続してから）。
pub const ROOM_TTL_MS: u64 = 10 * 60 * 1000;
/// answerの期限（始める側のコードを返す側へ渡してから）。
pub const ANSWER_TTL_MS: u64 = 60 * 1000;
/// 対戦のルームの成立の後にサーバーが生存確認（pingの自動応答の時刻）の古さを許す上限。越えた接続と相手をサーバーが閉じる。
pub const HELD_STALE_MS: u64 = 90 * 1000;
/// 成立の後の部屋でサーバーが生存確認の古さをチェックするために起きる間隔。
pub const HELD_SWEEP_MS: u64 = 5 * 60 * 1000;
/// 生存チェックの文。
/// サーバーは文字の完全一致でしか自動応答しない（`setWebSocketAutoResponse`）。
/// そのため空白もキーの順も変えず、この12文字をそのまま送る。
pub const PING_TEXT: &str = r#"{"t":"ping"}"#;
/// 自動応答の文（読むときはJSONとして読むので、文字の形はこれに限らない）。
pub const PONG_TEXT: &str = r#"{"t":"pong"}"#;
/// 観戦用のルームの配信側が観戦を断るときに`offer`の代わりに送る文（`rules.ts`の`parseClientMessage`の`refuse`）。
pub const REFUSE_TEXT: &str = r#"{"t":"refuse"}"#;
/// サーバーが429の本文に入れる待ち時間（秒）。
/// 読み手は本文の`retry_after`を使い、この値で埋めない（本文に無いなら「サーバーのJSONでない」と見る）。
pub const RETRY_AFTER_SECS: u32 = 60;

/// 入室の要求のクエリ`?cid=<16 進小文字>`の名。同じ値の古い接続はサーバーが入り直しとして置き換える（`rules.ts`の`CLIENT_ID_PARAM`）。
pub const CLIENT_ID_PARAM: &str = "cid";
/// 起動ごとの乱数の長さ（バイト）。
pub const CLIENT_ID_BYTES: usize = 16;
/// 観戦用のルームのhostの席を取る接続が付けるクエリ`ticket=<席の札>`の名（`rules.ts`の`TICKET_PARAM`）。
pub const TICKET_PARAM: &str = "ticket";
/// 席のチケットの期限（発行から。`rules.ts`の`TICKET_TTL_SECS`）。期限はサーバーが見る。こちらはチケットの中身を読まずに受け渡す。
pub const TICKET_TTL_SECS: u64 = 24 * 60 * 60;
/// 観戦用のルームで観戦者が配信側のコードを受けてからanswerを送るまでの期限（`rules.ts`の`WATCH_ANSWER_TTL_MS`）。
/// 過ぎた観戦者だけをサーバーが閉じ、配信側は残る。
pub const WATCH_ANSWER_TTL_MS: u64 = 10 * 1000;

// ---------------- こちらが決めた値（サーバーは使わない） ----------------

/// pingを送る間隔。途中の機器が無言のWebSocketを切らないため。
pub const PING_INTERVAL_MS: u64 = 30_000;
/// pingを送ってから`pong`も他の文も来ないまま待つ長さ。pingの3周（90秒）。
/// これを過ぎたら「サーバーが応答しません」（[`Why::NoPong`]）。
/// 片側だけ切れたTCPは送っても誤りにならず受信も止まったままになる。
/// そのためこれが無いとルームの期限（10分）まで気づけない。
/// 1周でなく3周なのは、ping 1通の行き違いで手順を失敗にしないため。
pub const PONG_TIMEOUT_MS: u64 = 3 * PING_INTERVAL_MS;
// 成立の後に保つ接続では、サーバーが生存確認の古さで閉じる前に次のpingが届く。
// こちらがpongを待つ上限もサーバーの上限を越えない。
const _: () = assert!(PING_INTERVAL_MS < HELD_STALE_MS && PONG_TIMEOUT_MS <= HELD_STALE_MS);
/// 101（Upgrade）の後に`room`が来るのを待つ時間。サーバーは101を返す前に`room`を送るので普通はすぐ来る。
/// 来なければ「サーバーが応答しません」の扱い。
pub const ROOM_GREETING_TIMEOUT_MS: u64 = 10_000;
/// こちらの期限の予備。サーバーの期限（4408）が先に届くようその後ろの時刻にする。
/// 秒の切り捨てで最大2秒ほど前へずれうるのでそれより十分長く。
pub const DEADLINE_GRACE_MS: u64 = 5_000;

/// 接続コードに使える文字（ASCIIの英数字・`-`・`_`）。
fn is_allowed_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// プレイヤー名・ルーム名に使える文字（印字できる半角0x20〜0x7Eのうち`` ` `` `;` `/` 以外）。`;` は ini のコメント、`/` はルーム名が
/// パスの区切りに割れるため。`` ` `` は獣王園の名前の入力に合わせた。パスには英数字と `-` `.` `_` `~` 以外を `%XX` にして載せる。
fn is_room_name_byte(b: u8) -> bool {
    matches!(b, 0x20..=0x7E) && !matches!(b, b'`' | b';' | b'/')
}

// ---------------- ルーム名とパス ----------------
//
// こちらは揃えない（NFKCを持たない。サーバーが揃える）。
// 送る前に見るのは揃えずに決まるガードだけ: 空・大きさ（揃える前のUTF-8のバイト）・制御文字・`/`・名そのものが`.` `..`・空白だけ・ASCIIだけの名の長さ。
// `RoomName::parse`（dllの本番）はこれに加えて、使えない文字（`accepts_char`の外）と、前後の空白を除いた名が`w.` `q.`で始まるものを断る（`NotAscii`・`ReservedPrefix`）。
// `parse_server_rules`が断る名はサーバーも必ず断る（逆は無い）。
// サーバーだけが断るのは、揃えて初めて長さの分かるASCII以外の名（33文字以上の日本語・揃えると伸びる文字）。
// これはサーバーの400 `bad_room_name`（[`Failure::BadRoomName`]）で知る。
// dllの`parse`はASCIIだけを通すのでここは出ない。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomNameError {
    /// 空。
    Empty,
    /// 揃える前のUTF-8が[`ROOM_NAME_MAX_BYTES`]を越える（`bytes`はその数）。
    TooManyBytes { bytes: usize },
    /// 使えない文字（制御文字か`/`。`pos`は0からの文字の位置）。
    BadChar { pos: usize },
    /// 名そのものが`.`か`..`（[`ROOM_NAME_BAD_NAMES`]）。
    DotName,
    /// 空白だけ（サーバーが前後の空白を除くと空になる）。
    Blank,
    /// ASCIIだけの名で、前後の空白を除いて[`ROOM_NAME_MAX`]を越える（`len`はその文字の数）。
    /// ASCII以外を含む名はNFKCで文字の数が変わるので数えない。
    TooLong { len: usize },
    /// [`RoomName::accepts_char`]の外の文字がある（`pos`は0からの文字の位置）。
    /// サーバーは通すがdllの側だけ断る（画面の文字の表に無い漢字は「?」で描かれ、相手と同じ名かを見分けられないため）。
    /// [`RoomName::parse`]だけが返す。
    NotAscii { pos: usize },
    /// 前後の空白を除いて小文字にした名が`w.`か`q.`で始まる（観戦用のルームとランダムマッチの割り当ての名と重なる）。[`RoomName::parse`]だけが返す。
    ReservedPrefix,
}

/// 送る前の検証を通ったルーム名（打った文字のまま。揃えていない）。確保しない。
///
/// `==`は「同じ文字」で「同じルーム」ではない（`ＡＢＣ`と`abc`は別の値だがサーバーで同じルームになる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomName {
    buf: [u8; ROOM_NAME_MAX_BYTES],
    len: usize,
}

/// サーバーが前後から除く空白（JSの`trim`が除く文字のうち制御文字でないもの）。
/// 制御文字は先に断るのでUnicodeのWhite_Space（`char::is_whitespace`）とU+FEFFの和集合で足りる。
/// NFKCの後に除くサーバーと揃える前に見るこちらは同じ答えになる。
fn is_trimmed_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{FEFF}'
}

/// 前後の空白を除いた名が観戦用のルーム（[`SPECTATE_ROOM_PREFIX`]）かランダムマッチの割り当て（`room_queue::RANDOM_ROOM_PREFIX`）の先頭かどうか。
/// 大文字小文字は見ない（サーバーが小文字にして検索する）。
fn has_reserved_prefix(trimmed: &str) -> bool {
    [SPECTATE_ROOM_PREFIX, crate::room_queue::RANDOM_ROOM_PREFIX]
        .iter()
        .any(|p| {
            trimmed
                .get(..p.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(p))
        })
}

/// パスに符号化せずに載せる文字（RFC 3986のunreserved）。ほかは全部`%XX`（UTF-8のバイトごと）。
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn hex_upper(n: u8) -> char {
    char::from_digit(u32::from(n & 0x0F), 16).map_or('0', |c| c.to_ascii_uppercase())
}

pub(crate) fn hex_lower(n: u8) -> u8 {
    char::from_digit(u32::from(n & 0x0F), 16).map_or(b'0', |c| c as u8)
}

/// 観戦用のルームの名の先頭（[`RoomName::spectate_of`]）。利用者がこの先頭の名を打てない（`parse`が断る）ので対戦のルームと重ならない。
pub const SPECTATE_ROOM_PREFIX: &str = "w.";
/// 観戦用のルームの名に使うSHA-256の先頭のバイト数（16進で16文字）。
const SPECTATE_HASH_BYTES: usize = 8;
/// 観戦用のルームの名の長さ（文字）: 先頭2 ＋ 16進16。
pub const SPECTATE_ROOM_NAME_LEN: usize = SPECTATE_ROOM_PREFIX.len() + 2 * SPECTATE_HASH_BYTES;

impl RoomName {
    /// ルーム名を検証する。サーバーの決まり（[`RoomName::parse_server_rules`]）に加えて、[`RoomName::accepts_char`]の文字だけを通す
    /// （画面の文字の表に無い漢字は「?」で描かれ、相手と同じ名かを見分けられないため）。日本語・全角・`` ` `` `;` は
    /// [`RoomNameError::NotAscii`]。前後の空白は除いた名を返す（間の空白は残す）。除いた名が`w.` `q.`で始まれば
    /// [`RoomNameError::ReservedPrefix`]。大文字と小文字は別の文字のまま通す（同じルームにするのはサーバー）。
    pub fn parse(raw: &str) -> Result<RoomName, RoomNameError> {
        RoomName::parse_server_rules(raw)?;
        // 長さ・大きさ・制御文字などの断りは上（サーバーと同じガード）が先に返す。ここは文字の種類だけを見る。
        if let Some(pos) = raw.chars().position(|c| !RoomName::accepts_char(c)) {
            return Err(RoomNameError::NotAscii { pos });
        }
        // ここまで通れば全部ASCIIなので、除く空白は半角の空白だけ。
        let trimmed = raw.trim_matches(' ');
        if has_reserved_prefix(trimmed) {
            return Err(RoomNameError::ReservedPrefix);
        }
        RoomName::parse_server_rules(trimmed)
    }

    /// プレイヤー名・ルーム名に使える文字かどうか。[`RoomName::parse`]・ゲーム内の入力（`room_input`・`player_name`）・一覧の名前（`watch_list`）が同じ式を使う。
    pub fn accepts_char(c: char) -> bool {
        c.is_ascii() && is_room_name_byte(c as u8)
    }

    /// サーバーの決まりだけで検証する（揃えない。前後の空白もそのまま送り、サーバーが除く）。
    /// ガードの順はサーバーと同じ: 大きさ → 制御文字と`/` → `.` `..` → 揃えた後の空と長さ。
    /// 「サーバーは日本語の名も通す」ことをloopbackの検査と見本の突き合わせで見るための入口。
    /// 画面の側の検証には使わない。
    pub fn parse_server_rules(raw: &str) -> Result<RoomName, RoomNameError> {
        if raw.is_empty() {
            return Err(RoomNameError::Empty);
        }
        if raw.len() > ROOM_NAME_MAX_BYTES {
            return Err(RoomNameError::TooManyBytes { bytes: raw.len() });
        }
        if let Some(pos) = raw.chars().position(|c| c.is_control() || c == '/') {
            return Err(RoomNameError::BadChar { pos });
        }
        if ROOM_NAME_BAD_NAMES.contains(&raw) {
            return Err(RoomNameError::DotName);
        }
        let trimmed = raw.trim_matches(is_trimmed_space);
        if trimmed.is_empty() {
            return Err(RoomNameError::Blank);
        }
        // ASCIIはNFKCで変わらず、小文字にしても文字の数は同じ。前後の空白を除いた数がサーバーの数と一致する。
        if trimmed.is_ascii() && trimmed.len() > ROOM_NAME_MAX {
            return Err(RoomNameError::TooLong { len: trimmed.len() });
        }
        let mut buf = [0u8; ROOM_NAME_MAX_BYTES];
        for (d, s) in buf.iter_mut().zip(raw.bytes()) {
            *d = s;
        }
        Ok(RoomName {
            buf,
            len: raw.len(),
        })
    }

    /// 打った文字のまま（揃えていない）。
    pub fn as_str(&self) -> &str {
        // 作るときに &strの中身を丸ごとコピーしているのでfrom_utf8は失敗しない。失敗したら空を返す。
        self.buf
            .get(..self.len)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    /// このルームの観戦用のルームの名（インターネット越しの観戦）: `w.` ＋ SHA-256(小文字にしたルーム名のASCII)の先頭8 Bの16進（小文字16文字）。計18文字（サーバーの32文字の上限の内）。
    /// ルームの手順は今のまま（2人・hostとguest）で、対戦する2人のdllと観戦者のdllがこの名のルームで会う。
    /// - 利用者は`w.`で始まる名を打てない（[`RoomName::parse`]が`ReservedPrefix`で断る）ので対戦のルームの名と重ならない。作るのに`parse`は通さない（[`RoomName::parse_server_rules`]は通る）。
    /// - 呼ぶのは`parse`を通った名。その名の`to_ascii_lowercase`はサーバーの「NFKC → 前後の空白を除く → 小文字」と同じ結果になるので大文字小文字だけが違う名（サーバーでは同じルーム）は同じ観戦用のルームになる。`parse_server_rules`だけを通った名（日本語・全角など）は揃え方が食い違いうる（dllの本番は`parse`だけを使うので出ない）。
    /// - 確保しない（バイトごとに小文字にしてハッシュへ渡す）。
    pub fn spectate_of(&self) -> RoomName {
        let mut h = Sha256::new();
        for b in self.as_str().trim_matches(is_trimmed_space).bytes() {
            h.update([b.to_ascii_lowercase()]);
        }
        let digest = h.finalize();
        let mut buf = [0u8; ROOM_NAME_MAX_BYTES];
        let hex = digest
            .iter()
            .take(SPECTATE_HASH_BYTES)
            .flat_map(|b| [hex_lower(b >> 4), hex_lower(b & 0x0F)]);
        let name = SPECTATE_ROOM_PREFIX.bytes().chain(hex);
        for (d, s) in buf.iter_mut().zip(name) {
            *d = s;
        }
        RoomName {
            buf,
            len: SPECTATE_ROOM_NAME_LEN,
        }
    }

    /// 接続するパス（`/v0/rooms/<ルーム名>`）。
    /// ルーム名はUTF-8をパーセント符号化して載せる（unreservedのほかは全部`%XX`、16進は大文字）。
    /// WinHTTPはホスト名とパスを別に渡すのでパスだけを返す。
    pub fn path(&self) -> String {
        let name = self.as_str();
        let mut out = String::with_capacity(PATH_PREFIX.len() + name.len() * 3);
        out.push_str(PATH_PREFIX);
        self.push_encoded(&mut out);
        out
    }

    /// 名をUTF-8のバイトごとにパーセント符号化して足す。
    fn push_encoded(&self, out: &mut String) {
        push_percent_encoded(out, self.as_str());
    }

    /// [`RoomName::path`]に起動ごとの乱数のクエリ（`?cid=…`）を付けたパス。
    pub fn path_with_client(&self, id: &[u8; CLIENT_ID_BYTES]) -> String {
        self.path_with_query(Some(id), None)
    }

    /// 観戦用のルーム（`w.…`）のhostの席を取りに入るパス。
    /// `ticket=<席の札>`を付ける（サーバーはこのルームの名について正しく期限内のチケットを持つ接続にだけhostの席を渡す）。
    /// `id`があれば`cid`を先に付ける。
    pub fn path_with_ticket(
        &self,
        id: Option<&[u8; CLIENT_ID_BYTES]>,
        ticket: &SeatTicket,
    ) -> String {
        self.path_with_query(id, Some(ticket))
    }

    fn path_with_query(
        &self,
        id: Option<&[u8; CLIENT_ID_BYTES]>,
        ticket: Option<&SeatTicket>,
    ) -> String {
        let mut out = self.path();
        let mut sep = '?';
        if let Some(id) = id {
            out.push(sep);
            sep = '&';
            out.push_str(CLIENT_ID_PARAM);
            out.push('=');
            for b in id {
                out.push(char::from(hex_lower(b >> 4)));
                out.push(char::from(hex_lower(b & 0x0F)));
            }
        }
        if let Some(ticket) = ticket {
            out.push(sep);
            out.push_str(TICKET_PARAM);
            out.push('=');
            push_percent_encoded(&mut out, ticket.as_str());
        }
        out
    }

    /// 接続するURL。`base`はスキームとホスト（例`wss://example.workers.dev`）。
    /// 末尾の`/`は除く（`//v0/...`はサーバーで404になるため）。
    /// `base`の形は見ない。
    pub fn url(&self, base: &str) -> String {
        format!("{}{}", base.trim_end_matches('/'), self.path())
    }
}

/// 文字列をUTF-8のバイトごとにパーセント符号化して足す（パスの末尾にも問いの値にも同じ式を使う。`+`も`%2B`にする）。
fn push_percent_encoded(out: &mut String, s: &str) {
    for b in s.bytes() {
        if is_unreserved(b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0F));
        }
    }
}

// ---------------- 席の札 ----------------

/// チケットの発行・期限の10進の桁の上限（`rules.ts`の`TICKET_NUM_MAX_DIGITS`）。
pub const TICKET_NUM_MAX_DIGITS: usize = 12;
/// チケットのMAC（HMAC-SHA256の32 B）のbase64url（パディングなし）の文字数（`rules.ts`の`TICKET_MAC_CHARS`）。
pub const TICKET_MAC_CHARS: usize = 43;
/// サーバーがチケットの署名に含める用途を示す文字列（`rules.ts`の`TICKET_PURPOSE`）。クライアントはチケットの中身を読まないので使わない（dllの偽サーバーが使う）。
pub const TICKET_PURPOSE: &str = "higanr-seat-v1";
/// チケットの文字数の上限: `<発行>.<期限>.<MAC>`。
pub const TICKET_MAX_CHARS: usize = 2 * TICKET_NUM_MAX_DIGITS + 2 + TICKET_MAC_CHARS;

/// 席のチケット（対戦の成立でサーバーが2人へ渡す、観戦用のルームのhostの席と一覧への投稿の資格）。
/// 中身は読まずに受け渡す。
/// 形はサーバーと同じ厳しさで検証し、外れたものはチケットなしと同じに扱う。
/// ルーム名と同じく本ログ・画面に出さない（`Debug`は中身を書かない）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SeatTicket {
    buf: [u8; TICKET_MAX_CHARS],
    len: usize,
}

impl core::fmt::Debug for SeatTicket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SeatTicket(..)")
    }
}

/// チケットの数のフィールド: `0`か先頭が0でない12桁までの10進。
fn is_ticket_number(s: &str) -> bool {
    s == "0"
        || (!s.is_empty()
            && s.len() <= TICKET_NUM_MAX_DIGITS
            && !s.starts_with('0')
            && s.bytes().all(|b| b.is_ascii_digit()))
}

/// base64urlの1文字の値。
fn base64url_value(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

impl SeatTicket {
    /// サーバーの形（`<発行>.<期限>.<MAC>`。MACは32 Bのbase64urlを1通りの書き方で）だけを通す。
    pub fn parse(s: &str) -> Option<SeatTicket> {
        let mut parts = s.split('.');
        let (Some(issued), Some(expires), Some(mac), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        if !is_ticket_number(issued) || !is_ticket_number(expires) {
            return None;
        }
        let values: Option<Vec<u8>> = mac.bytes().map(base64url_value).collect();
        let values = values?;
        // 43文字は258 bit。最後の文字の下2 bitは使わない。0でないとサーバーは別の書き方として断る。
        if values.len() != TICKET_MAC_CHARS || values.last().is_none_or(|v| v & 0b11 != 0) {
            return None;
        }
        let mut buf = [0u8; TICKET_MAX_CHARS];
        for (d, b) in buf.iter_mut().zip(s.bytes()) {
            *d = b;
        }
        Some(SeatTicket { buf, len: s.len() })
    }

    pub fn as_str(&self) -> &str {
        self.buf
            .get(..self.len)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }
}

// ---------------- 接続コードの文字（送る前に） ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeTextError {
    /// 空（サーバーは4422 bad_codeで閉じる）。
    Empty,
    /// [`CODE_MAX_CHARS`]を越える（サーバーは4413 too_largeで閉じる）。`chars`はJSの`length`と同じUTF-16の数。
    TooLong { chars: usize },
    /// base64urlでない文字（前後の空白を含む。4422 bad_code）。`pos`は0からの文字の位置。
    BadChar { pos: usize },
}

impl CodeTextError {
    pub fn close_code(self) -> CloseCode {
        match self {
            CodeTextError::TooLong { .. } => CloseCode::TooLarge,
            CodeTextError::Empty | CodeTextError::BadChar { .. } => CloseCode::BadCode,
        }
    }
}

/// コードの文字と長さを見る（サーバーと同じ順: 長さ → 文字）。前後の空白は除かず断る（dllが整えてから送る）。
pub fn check_code(code: &str) -> Result<(), CodeTextError> {
    // サーバーはJSの`code.length`（UTF-16の数）で比べる。ASCII以外の文字は結局4422だが、番号までサーバーに揃える。
    let chars: usize = code.chars().map(char::len_utf16).sum();
    if chars > CODE_MAX_CHARS {
        return Err(CodeTextError::TooLong { chars });
    }
    if code.is_empty() {
        return Err(CodeTextError::Empty);
    }
    for (pos, ch) in code.chars().enumerate() {
        if !(ch.is_ascii() && is_allowed_byte(ch as u8)) {
            return Err(CodeTextError::BadChar { pos });
        }
    }
    Ok(())
}

/// 始める側の文`{"t":"offer","code":"..."}`。コードはbase64urlの文字だけなのでJSONのエスケープは要らない（検証してから書く）。
pub fn write_offer(code: &str) -> Result<String, CodeTextError> {
    check_code(code)?;
    Ok(format!(r#"{{"t":"offer","code":"{code}"}}"#))
}

/// 返す側の文`{"t":"answer","code":"..."}`。
pub fn write_answer(code: &str) -> Result<String, CodeTextError> {
    check_code(code)?;
    Ok(format!(r#"{{"t":"answer","code":"{code}"}}"#))
}

// ---------------- 閉じる番号 ----------------

/// WebSocketを閉じる番号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCode {
    /// 手順を終えた（両者）。
    Done,
    /// JSONでない・知らないt・役割や順番に合わない文。
    BadMessage,
    /// ルームの期限・answerの期限が切れた。
    Expired,
    /// 相手が手順の途中で切れた。
    PeerLeft,
    /// 1通が大きすぎる・コードが長すぎる。
    TooLarge,
    /// コードの文字が悪い。
    BadCode,
    /// 進行中のランダム対戦の一覧に載せられる数（全体・1 IPあたり）を越えた。
    ListFull,
    /// 観戦用のルームのホストが観戦を許可していない（観戦者だけが閉じられ、ホストは残る）。
    Refused,
}

impl CloseCode {
    /// 全部（見本との突き合わせと番号 ↔ 名の往復の検査のため）。
    pub const ALL: [CloseCode; 8] = [
        CloseCode::Done,
        CloseCode::BadMessage,
        CloseCode::Expired,
        CloseCode::PeerLeft,
        CloseCode::TooLarge,
        CloseCode::BadCode,
        CloseCode::ListFull,
        CloseCode::Refused,
    ];

    pub fn number(self) -> u16 {
        match self {
            CloseCode::Done => 1000,
            CloseCode::BadMessage => 4400,
            CloseCode::Expired => 4408,
            CloseCode::PeerLeft => 4410,
            CloseCode::TooLarge => 4413,
            CloseCode::BadCode => 4422,
            CloseCode::ListFull => 4429,
            CloseCode::Refused => 4403,
        }
    }

    /// 名（errorの文の`code`とcloseのreason）。
    pub fn name(self) -> &'static str {
        match self {
            CloseCode::Done => "done",
            CloseCode::BadMessage => "bad_message",
            CloseCode::Expired => "expired",
            CloseCode::PeerLeft => "peer_left",
            CloseCode::TooLarge => "too_large",
            CloseCode::BadCode => "bad_code",
            CloseCode::ListFull => "list_full",
            CloseCode::Refused => "refused",
        }
    }

    pub fn from_number(n: u16) -> Option<CloseCode> {
        CloseCode::ALL.into_iter().find(|c| c.number() == n)
    }

    pub fn from_name(s: &str) -> Option<CloseCode> {
        CloseCode::ALL.into_iter().find(|c| c.name() == s)
    }
}

// ---------------- 平らなJSONの小さな読み手 ----------------

/// 値。読み手が中身まで要るのは文字列と整数だけ。ほか（小数・真偽・null・入れ子）は知らないキーの値として読み飛ばす。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsonValue<'a> {
    Str(Cow<'a, str>),
    /// i64に収まる整数（小数点・指数の無いもの）。
    Int(i64),
    /// 上以外の値（形は検証したが中身は捨てた）。
    Other,
}

/// 知らないキーの値を読み飛ばすときの入れ子の深さの上限（細工した深い入れ子で手間をかけさせないため）。
const SKIP_DEPTH_MAX: usize = 16;

struct Reader<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JsonAt(pub(crate) usize);

impl<'a> Reader<'a> {
    fn new(s: &'a str) -> Reader<'a> {
        Reader {
            s,
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn err<T>(&self) -> Result<T, JsonAt> {
        Err(JsonAt(self.i))
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), JsonAt> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err()
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonAt> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = match self.peek() {
                Some(c) => (c as char).to_digit(16),
                None => None,
            };
            match d {
                Some(d) => {
                    v = v * 16 + d;
                    self.i += 1;
                }
                None => return self.err(),
            }
        }
        Ok(v)
    }

    /// 文字列（`"`から）。エスケープの無いものは借りたまま返す。
    fn string(&mut self) -> Result<Cow<'a, str>, JsonAt> {
        self.eat(b'"')?;
        let start = self.i;
        // 速い経路: `"`か`\`まで。止まるのはASCIIの文字なので切る位置は必ず文字の境目。
        loop {
            match self.peek() {
                None => return self.err(),
                Some(b'"') => {
                    let out = self.s.get(start..self.i).map(Cow::Borrowed);
                    self.i += 1;
                    return out.ok_or(JsonAt(start));
                }
                Some(b'\\') => break,
                Some(c) if c < 0x20 => return self.err(),
                Some(_) => self.i += 1,
            }
        }
        let mut out = String::from(self.s.get(start..self.i).unwrap_or(""));
        loop {
            match self.peek() {
                None => return self.err(),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(Cow::Owned(out));
                }
                Some(b'\\') => {
                    self.i += 1;
                    let e = self.peek();
                    self.i += 1;
                    let ch = match e {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                // 上位サロゲート。続けて下位サロゲートが要る（片方だけのサロゲートはRustの文字列に入らない）。
                                self.eat(b'\\')?;
                                self.eat(b'u')?;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return self.err();
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return self.err();
                            } else {
                                hi
                            };
                            match char::from_u32(cp) {
                                Some(c) => c,
                                None => return self.err(),
                            }
                        }
                        _ => return Err(JsonAt(self.i - 1)),
                    };
                    out.push(ch);
                }
                Some(c) if c < 0x20 => return self.err(),
                Some(_) => {
                    // 1文字を丸ごとコピーする（多バイトの文字の途中で切らない）。
                    match self.s.get(self.i..).and_then(|r| r.chars().next()) {
                        Some(ch) => {
                            out.push(ch);
                            self.i += ch.len_utf8();
                        }
                        None => return self.err(),
                    }
                }
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - start
    }

    /// 数（RFC 8259 §6の形）。小数点・指数が無くi64に収まれば`Int`、ほかは`Other`。
    fn number(&mut self) -> Result<JsonValue<'a>, JsonAt> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return self.err(),
        }
        let int_end = self.i;
        let mut is_int = true;
        if self.peek() == Some(b'.') {
            self.i += 1;
            if self.digits() == 0 {
                return self.err();
            }
            is_int = false;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return self.err();
            }
            is_int = false;
        }
        if !is_int {
            return Ok(JsonValue::Other);
        }
        Ok(self
            .s
            .get(start..int_end)
            .and_then(|t| t.parse::<i64>().ok())
            .map_or(JsonValue::Other, JsonValue::Int))
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), JsonAt> {
        for &c in word {
            self.eat(c)?;
        }
        Ok(())
    }

    /// どんな値でも形だけ検証して読み飛ばす（知らないキーの値）。
    fn skip_value(&mut self, depth: usize) -> Result<(), JsonAt> {
        if depth > SKIP_DEPTH_MAX {
            return self.err();
        }
        match self.peek() {
            Some(b'"') => self.string().map(|_| ()),
            Some(b'-' | b'0'..=b'9') => self.number().map(|_| ()),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'{') => {
                self.i += 1;
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    self.string()?;
                    self.ws();
                    self.eat(b':')?;
                    self.ws();
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return self.err(),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(());
                }
                loop {
                    self.ws();
                    self.skip_value(depth + 1)?;
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(());
                        }
                        _ => return self.err(),
                    }
                }
            }
            _ => self.err(),
        }
    }
}

/// 平らなJSONのオブジェクト1つをキーと値の列に読む。
/// 同じキーが2回あれば誤り。
/// JSON.parseは後を採るが、サーバーは同じキーを出さない。
/// そのため2通りの読み方がある文は受けない。
pub(crate) fn parse_flat(text: &str) -> Result<Vec<(Cow<'_, str>, JsonValue<'_>)>, JsonAt> {
    let mut r = Reader::new(text);
    let out = flat_object(&mut r, None)?;
    r.ws();
    if r.i != r.b.len() {
        return r.err();
    }
    Ok(out)
}

pub(crate) type Pairs<'a> = Vec<(Cow<'a, str>, JsonValue<'a>)>;

/// [`parse_flat`]の平らなオブジェクトの中に「平らなオブジェクトの配列」が1つある形（`{"t":"list","items":[{…},{…}]}`）。
/// `list_key`の値が配列なら、その要素のオブジェクトを`items`へ読む（配列の中のオブジェクトは平らなものだけ）。
/// 配列が`max_items`を越えれば誤り。
/// 戻りは配列を除いた本体と要素の列。
pub(crate) fn parse_flat_with_items<'a>(
    text: &'a str,
    list_key: &str,
    max_items: usize,
) -> Result<(Pairs<'a>, Vec<Pairs<'a>>), JsonAt> {
    let mut r = Reader::new(text);
    let mut items = Vec::new();
    let head = flat_object(&mut r, Some((list_key, max_items, &mut items)))?;
    r.ws();
    if r.i != r.b.len() {
        return r.err();
    }
    Ok((head, items))
}

/// 平らなオブジェクト1つを読む（後ろの空白と終わりは見ない）。`list`があればそのキーの配列の要素を読む。
fn flat_object<'a>(
    r: &mut Reader<'a>,
    mut list: Option<(&str, usize, &mut Vec<Pairs<'a>>)>,
) -> Result<Pairs<'a>, JsonAt> {
    let mut out: Pairs<'a> = Vec::new();
    r.ws();
    r.eat(b'{')?;
    r.ws();
    if r.peek() == Some(b'}') {
        r.i += 1;
        return Ok(out);
    }
    loop {
        r.ws();
        let key_at = r.i;
        let key = r.string()?;
        r.ws();
        r.eat(b':')?;
        r.ws();
        let is_list = list.as_ref().is_some_and(|(k, _, _)| *k == key) && r.peek() == Some(b'[');
        let value = match r.peek() {
            _ if is_list => {
                if let Some((_, max, items)) = list.as_mut() {
                    read_items(r, *max, items)?;
                }
                JsonValue::Other
            }
            Some(b'"') => JsonValue::Str(r.string()?),
            Some(b'-' | b'0'..=b'9') => r.number()?,
            _ => {
                r.skip_value(1)?;
                JsonValue::Other
            }
        };
        if out.iter().any(|(k, _)| *k == key) {
            return Err(JsonAt(key_at));
        }
        out.push((key, value));
        r.ws();
        match r.peek() {
            Some(b',') => r.i += 1,
            Some(b'}') => {
                r.i += 1;
                return Ok(out);
            }
            _ => return r.err(),
        }
    }
}

/// `[`から`]`まで: 平らなオブジェクトの配列。
fn read_items<'a>(
    r: &mut Reader<'a>,
    max: usize,
    items: &mut Vec<Pairs<'a>>,
) -> Result<(), JsonAt> {
    r.eat(b'[')?;
    r.ws();
    if r.peek() == Some(b']') {
        r.i += 1;
        return Ok(());
    }
    loop {
        if items.len() >= max {
            return r.err();
        }
        items.push(flat_object(r, None)?);
        r.ws();
        match r.peek() {
            Some(b',') => r.i += 1,
            Some(b']') => {
                r.i += 1;
                return Ok(());
            }
            _ => return r.err(),
        }
    }
}

// ---------------- サーバーから届く文 ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgError {
    /// [`MESSAGE_MAX_BYTES`]を越える（中を読まない）。
    TooLarge { bytes: usize },
    /// JSONのオブジェクトでない（`at`は読めなくなったバイトの位置。同じキーが2回もここ）。
    NotJson { at: usize },
    /// 要るキーが無い。
    Missing(&'static str),
    /// キーの値の型が違う（文字列のところに数など）。
    WrongType(&'static str),
    /// 型は合うが値が決まりの外（知らない役割・負の時刻・文字の悪いコードなど）。
    BadValue(&'static str),
    /// 知らない`t`（pingも含む。サーバーはpingを送らない）。
    UnknownType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Room,
    Peer,
    Matched,
    Pong,
    Error,
}

/// 役割（サーバーの`"host"` / `"guest"`を境目でコピーしたもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 始める側（サーバーの`"host"`）。自分のコードを先に出し、相手の返す側のコードを待つ。
    Offer,
    /// 返す側（サーバーの`"guest"`）。相手の始める側のコードを受けてから自分のコードを返す。
    Answer,
}

impl Role {
    pub fn from_wire(s: &str) -> Option<Role> {
        match s {
            "host" => Some(Role::Offer),
            "guest" => Some(Role::Answer),
            _ => None,
        }
    }
}

/// サーバーから届く文（と自動応答のpong）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    /// 接続した直後に1回。`expires_at`はルームの期限（UNIX秒）。
    Room { role: Role, expires_at: u64 },
    /// 相手のコード（返す側には始める側のコード、始める側には返す側のコード）。`ticket`は対戦のルームの返す側へだけ届く席のチケット。
    Peer {
        code: String,
        ticket: Option<SeatTicket>,
    },
    /// 始める側にだけ。`expires_at`はanswerの期限（UNIX秒）。`ticket`は対戦のルームの始める側へだけ届く席のチケット。
    Matched {
        expires_at: u64,
        ticket: Option<SeatTicket>,
    },
    /// pingへの自動応答。
    Pong,
    /// 閉じる直前の1通。知らない名は`None`（閉じる番号の側で読める）。
    Error { code: Option<CloseCode> },
}

impl ServerMsg {
    pub fn kind(&self) -> MsgKind {
        match self {
            ServerMsg::Room { .. } => MsgKind::Room,
            ServerMsg::Peer { .. } => MsgKind::Peer,
            ServerMsg::Matched { .. } => MsgKind::Matched,
            ServerMsg::Pong => MsgKind::Pong,
            ServerMsg::Error { .. } => MsgKind::Error,
        }
    }
}

pub(crate) struct Fields<'a, 'b>(pub(crate) &'b [(Cow<'a, str>, JsonValue<'a>)]);

impl<'a> Fields<'a, '_> {
    pub(crate) fn get(&self, key: &str) -> Option<&JsonValue<'a>> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub(crate) fn str(&self, key: &'static str) -> Result<&str, MsgError> {
        match self.get(key) {
            None => Err(MsgError::Missing(key)),
            Some(JsonValue::Str(s)) => Ok(s),
            Some(_) => Err(MsgError::WrongType(key)),
        }
    }

    pub(crate) fn int(&self, key: &'static str) -> Result<i64, MsgError> {
        match self.get(key) {
            None => Err(MsgError::Missing(key)),
            Some(JsonValue::Int(n)) => Ok(*n),
            Some(_) => Err(MsgError::WrongType(key)),
        }
    }

    pub(crate) fn unix_secs(&self, key: &'static str) -> Result<u64, MsgError> {
        u64::try_from(self.int(key)?).map_err(|_| MsgError::BadValue(key))
    }

    /// 席のチケットのフィールド。無い・文字列でない・形が外れたものはどれもチケットなし（対戦の手順は止めない）。
    fn ticket(&self) -> Option<SeatTicket> {
        match self.get("ticket") {
            Some(JsonValue::Str(s)) => SeatTicket::parse(s),
            _ => None,
        }
    }
}

/// サーバーから届いた1通を読む。知らないキーは読み飛ばし、知らない`t`は誤り。順番と役割は見ない（[`RoomClient`]が見る）。
pub fn parse_server_message(text: &str) -> Result<ServerMsg, MsgError> {
    if text.len() > MESSAGE_MAX_BYTES {
        return Err(MsgError::TooLarge { bytes: text.len() });
    }
    let list = parse_flat(text).map_err(|JsonAt(at)| MsgError::NotJson { at })?;
    let f = Fields(&list);
    match f.str("t")? {
        "room" => {
            let role = Role::from_wire(f.str("role")?).ok_or(MsgError::BadValue("role"))?;
            Ok(ServerMsg::Room {
                role,
                expires_at: f.unix_secs("expires_at")?,
            })
        }
        "peer" => {
            let code = f.str("code")?;
            // サーバーは文字を検証したコードしか渡さない。崩れていればサーバーか途中が壊れている。
            check_code(code).map_err(|_| MsgError::BadValue("code"))?;
            Ok(ServerMsg::Peer {
                code: code.to_owned(),
                ticket: f.ticket(),
            })
        }
        "matched" => Ok(ServerMsg::Matched {
            expires_at: f.unix_secs("expires_at")?,
            ticket: f.ticket(),
        }),
        "pong" => Ok(ServerMsg::Pong),
        "error" => Ok(ServerMsg::Error {
            code: CloseCode::from_name(f.str("code")?),
        }),
        _ => Err(MsgError::UnknownType),
    }
}

// ---------------- 入口のHTTPの誤り ----------------

/// 入口が断るときの本文の`error`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpErrorName {
    /// 404。パスが`/v0/rooms/<名>`でない（バージョン違いを含む）。
    NotFound,
    /// 405。GETでない。
    MethodNotAllowed,
    /// 400。ルーム名の文字・長さが決まりの外。
    BadRoomName,
    /// 426。WebSocketのUpgradeでない。
    UpgradeRequired,
    /// 429。回数制限（`retry_after`を伴う）。
    RateLimited,
    /// 409。観戦用のルームに既に2人いるか、チケットの無い・期限の切れた接続が空の観戦用のルームに来た（古いサーバーは対戦のルームの3人目にも返す）。
    RoomFull,
    /// 409。対戦のルームに2人いる（成立の前も後も。成立の後も2人が残ってルーム名を使用中にする）。
    RoomInUse,
    /// 400。観戦用のルームのhostの席を取る接続のチケットの形・署名が合わない（別のルームの名のチケットも）。
    BadTicket,
}

impl HttpErrorName {
    fn from_wire(s: &str) -> Option<HttpErrorName> {
        Some(match s {
            "not_found" => HttpErrorName::NotFound,
            "method_not_allowed" => HttpErrorName::MethodNotAllowed,
            "bad_room_name" => HttpErrorName::BadRoomName,
            "upgrade_required" => HttpErrorName::UpgradeRequired,
            "rate_limited" => HttpErrorName::RateLimited,
            "room_full" => HttpErrorName::RoomFull,
            "room_in_use" => HttpErrorName::RoomInUse,
            "bad_ticket" => HttpErrorName::BadTicket,
            _ => return None,
        })
    }
}

/// 入口の誤りの本文`{"error":..., "retry_after":...}`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpErrorBody {
    pub error: HttpErrorName,
    /// 429だけに付く待ち時間（秒）。
    pub retry_after_secs: Option<u32>,
}

/// 入口の誤りの本文を読む。知らない`error`の名は`BadValue`（Cloudflare自身の誤りの頁などと同じ扱い）。
pub fn read_http_error(body: &str) -> Result<HttpErrorBody, MsgError> {
    if body.len() > MESSAGE_MAX_BYTES {
        return Err(MsgError::TooLarge { bytes: body.len() });
    }
    let list = parse_flat(body).map_err(|JsonAt(at)| MsgError::NotJson { at })?;
    let f = Fields(&list);
    let error = HttpErrorName::from_wire(f.str("error")?).ok_or(MsgError::BadValue("error"))?;
    let retry_after_secs = match f.get("retry_after") {
        None => None,
        Some(_) => Some(
            u32::try_from(f.int("retry_after")?).map_err(|_| MsgError::BadValue("retry_after"))?,
        ),
    };
    Ok(HttpErrorBody {
        error,
        retry_after_secs,
    })
}

// ---------------- 上に知らせる失敗 ----------------

/// 「サーバーが応答しません」の中で何が起きたか（記録と調査用。画面の文言は[`FailureKind`]で分ける）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// 接続する要求に応答が無い（名前解決できない・時間切れ・接続できない）。dllの通信の処理が知らせる。
    NoAnswer,
    /// 101以外のHTTPの番号（404バージョン違い・405・426・5xx・その他）。
    HttpStatus(u16),
    /// 400 / 409 / 429なのに本文がサーバーのJSONでない（Cloudflare自身の誤りの頁など）。
    NotOurBody(u16),
    /// 101の後に`room`が来ない（[`ROOM_GREETING_TIMEOUT_MS`]）。
    NoRoom,
    /// サーバーの文が読めない。
    BadMessage(MsgError),
    /// 役割や順番に合わない文。
    OutOfOrder(MsgKind),
    /// サーバーが閉じた番号（早すぎる1000・4400 / 4413 / 4422・知らない番号）。4400系はこちらの送った文が悪かったことを示す。
    Closed(u16),
    /// 期限・相手以外のerrorの文（`None`は知らない名）。
    ErrorMessage(Option<CloseCode>),
    /// answerを送った後に1000で閉じられないままanswerの期限を過ぎた。
    NoClose,
    /// 閉じる番号なしに切れた（受信の誤り）。
    Lost,
    /// pingを送った後[`PONG_TIMEOUT_MS`]の間`pong`も他の文も来ない（片側だけ切れたTCP）。
    NoPong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// 利用者が直せない。「サーバーが応答しません。接続コードで接続してください」の扱い。
    Unresponsive(Why),
    /// 429。`retry_after_secs`秒後に接続し直せる（コピペへは誘導しない）。
    RateLimited { retry_after_secs: u32 },
    /// 409 `room_full`。観戦用のルームの席が無い（古いサーバーは対戦のルームの3人目にも返す）。ルーム名を変える。
    RoomFull,
    /// 409 `room_in_use`。対戦のルームを2人が使っている。ルーム名を変える。
    RoomInUse,
    /// 400。ルーム名が決まりの外（こちらでも検証するので普通は来ない）。ルーム名を変える。
    BadRoomName,
    /// ルームの期限・answerの期限が切れた（4408またはこちらの予備の期限）。
    /// dllが自動で入り直すのは始める側で相手がまだ来ていないときだけ（`room_rejoin::should_rejoin`）。
    Expired,
    /// 相手が手順の途中で抜けた（4410）。
    PeerLeft,
    /// 観戦用のルームの配信側が観戦を断っている（4403。観戦者だけに来る）。入り直しても変わらない。
    Refused,
    /// 400 `bad_ticket`。観戦用のルームのhostの席を取る接続のチケットをサーバーが受けない（チケットを付けた接続だけに来る）。入り直しても変わらない。
    BadTicket,
}

/// 画面の文言の種類（文そのものはdllの表が持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Unresponsive,
    WaitThenRetry,
    ChangeRoomName,
    Expired,
    PeerLeft,
    Refused,
}

impl Failure {
    pub fn kind(self) -> FailureKind {
        match self {
            // チケットは配信側だけが付けるので画面へ出す経路には来ない。
            Failure::Unresponsive(_) | Failure::BadTicket => FailureKind::Unresponsive,
            Failure::RateLimited { .. } => FailureKind::WaitThenRetry,
            Failure::RoomFull | Failure::RoomInUse | Failure::BadRoomName => {
                FailureKind::ChangeRoomName
            }
            Failure::Expired => FailureKind::Expired,
            Failure::PeerLeft => FailureKind::PeerLeft,
            Failure::Refused => FailureKind::Refused,
        }
    }
}

/// 101でない入口の応答をdllへ知らせる失敗にコピーする。
/// 本文が番号に合うサーバーのJSONのときだけ400 / 409 / 429をルーム名・待ちの扱いにする。
/// ほかは全部「サーバーが応答しません」（無料枠を越えたときのCloudflareの頁を含む）。
pub fn classify_http(status: u16, body: &str) -> Failure {
    let parsed = read_http_error(body).ok();
    let name = parsed.map(|b| b.error);
    match status {
        400 if name == Some(HttpErrorName::BadRoomName) => Failure::BadRoomName,
        400 if name == Some(HttpErrorName::BadTicket) => Failure::BadTicket,
        409 if name == Some(HttpErrorName::RoomFull) => Failure::RoomFull,
        409 if name == Some(HttpErrorName::RoomInUse) => Failure::RoomInUse,
        429 => match parsed {
            Some(HttpErrorBody {
                error: HttpErrorName::RateLimited,
                retry_after_secs: Some(retry_after_secs),
            }) => Failure::RateLimited { retry_after_secs },
            _ => Failure::Unresponsive(Why::NotOurBody(status)),
        },
        400 | 409 => Failure::Unresponsive(Why::NotOurBody(status)),
        _ => Failure::Unresponsive(Why::HttpStatus(status)),
    }
}

// ---------------- クライアントのステートマシン ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// 単調な時計（ms）。期限とpingの時刻はこれで持つ。
    pub ms: u64,
    /// 壁時計（UNIX秒）。サーバーの`expires_at`から残りを出すときだけ使う。
    pub unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// ルームに入った。`role`に合うコードを作って[`RoomClient::submit_code`]へ（返す側は[`Event::PeerCode`]の後）。
    /// `expires_at`はルームの期限（UNIX秒。表示の目安）。
    Joined { role: Role, expires_at: u64 },
    /// 始める側だけ: 自分のコードが相手へ渡った。`expires_at`はanswerの期限（UNIX秒）。
    Matched { expires_at: u64 },
    /// 相手のコード。返す側はこれから自分のコードを作って返す。始める側はこの直後に[`Event::Held`]（観戦用のルームは[`Event::Done`]）。
    PeerCode(String),
    /// 対戦のルームで成立した（始める側は相手のコードを受けた・返す側は自分のコードを送った）。以後は接続を保ってpingだけを送る。
    /// dllは対戦を終えるまで解放しない（解放するとサーバーがルーム名の使用中を解除する）。
    Held,
    /// 接続が終わった。観戦用のルームは手順を終えた（サーバーが閉じる）。
    /// 対戦のルームは保っていた接続をサーバーが閉じたか応答が途絶えた（失敗にしない。対戦には関わらない）。
    Done,
    /// 手順が終わらなかった。dllはWebSocketを閉じる。
    Failed(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// サーバーへこの文字をそのままテキストの文で送る。
    Send(String),
    Notify(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 101を待っている（通信はdll）。
    Connecting,
    /// 101の後、`room`を待っている。
    AwaitRoom,
    /// 自分のコードを待っている（始める側は`room`の後、返す側は`peer`の後）。
    NeedOwnCode,
    /// 始める側: `offer`を送った後、`matched`を待っている。
    AwaitMatched,
    /// 相手のコード（`peer`）を待っている（始める側は`matched`の後、返す側は`room`の後）。
    AwaitPeerCode,
    /// 返す側: `answer`を送った後にサーバーの1000を待っている。対戦のルームでは次の`poll`で[`State::Holding`]へ移る。
    AwaitClose,
    /// 対戦のルームの成立の後に接続を保ってpingだけを送っている（ルーム名を使用中にする）。
    Holding,
    /// 始める側: `refuse`を送った後にルームの期限（4408）までルームに残っている。
    Refusing,
    Done,
    Failed(Failure),
}

/// [`RoomClient::submit_code`]が断った理由。断ったときは状態を変えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    /// いまはコードを受ける番でない（そのときの状態）。
    NotNow(State),
    /// コードの文字がサーバーの決まりの外。
    Code(CodeTextError),
}

/// ルームサーバーのクライアントの手順（モジュールのdocの「順番」）。I/Oをせず、送る文とイベントの列を返す。
///
/// 使い方: 接続する前に`new` → 101なら`on_upgraded`、101でなければ`on_http_refused`、接続できなければ`on_transport_failed`。
/// 以後は受けたテキストの文を`on_text`、閉じる番号を`on_close`、受信の誤りを`on_transport_failed`へ渡す。
/// [`RoomClient::next_wake`]の時刻か、それより前に`poll`を呼ぶ（pingと期限）。
#[derive(Debug, Clone)]
pub struct RoomClient {
    state: State,
    role: Option<Role>,
    /// いまの状態の期限（こちらの予備。過ぎたら`poll`が失敗にする）。
    deadline_ms: Option<u64>,
    /// ルームの期限（返す側がanswerの期限と比べるため）。
    room_deadline_ms: u64,
    next_ping_ms: Option<u64>,
    /// 答えの来ていない最初のpingを送った時刻。文が1通でも届けばNoneに戻す。
    /// 続けて送ったpingでは動かさない（最初のpingから[`PONG_TIMEOUT_MS`]で切る）。
    unanswered_ping_ms: Option<u64>,
    /// この入室で受けた席のチケット（始める側は`matched`、返す側は`peer`で届く）。手順を終えた後も残す。
    ticket: Option<SeatTicket>,
    /// 対戦のルーム: 成立の後も接続を保つ（[`State::Holding`]）。観戦用のルームは偽。
    hold: bool,
}

impl Default for RoomClient {
    fn default() -> Self {
        RoomClient::new()
    }
}

/// サーバーの期限（UNIX秒）を、こちらの単調な時計の予備の期限に変換する。
/// 時計のずれで残りが0以下か`bound_ms`より長く見えたら`bound_ms`で代える（モジュールのdocの「時刻」）。
pub(crate) fn backstop(now: Now, expires_at: u64, bound_ms: u64) -> u64 {
    let left_ms = expires_at
        .checked_sub(now.unix_secs)
        .map(|s| s.saturating_mul(1000));
    let left = match left_ms {
        Some(l) if l > 0 && l <= bound_ms => l,
        _ => bound_ms,
    };
    now.ms
        .saturating_add(left)
        .saturating_add(DEADLINE_GRACE_MS)
}

impl RoomClient {
    /// 成立でサーバーが閉じる手順（観戦用のルーム）。
    pub fn new() -> RoomClient {
        RoomClient {
            state: State::Connecting,
            role: None,
            deadline_ms: None,
            room_deadline_ms: u64::MAX,
            next_ping_ms: None,
            unanswered_ping_ms: None,
            ticket: None,
            hold: false,
        }
    }

    /// `room`に入る手順。対戦のルーム（観戦用の`w.`の名でない）なら成立の後も接続を保つ。
    pub fn for_room(room: &RoomName) -> RoomClient {
        RoomClient {
            hold: !room.as_str().starts_with(SPECTATE_ROOM_PREFIX),
            ..RoomClient::new()
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn role(&self) -> Option<Role> {
        self.role
    }

    /// この入室で受けた席のチケット。古いサーバー・鍵の無いサーバーは載せないので無いことがある。
    pub fn ticket(&self) -> Option<SeatTicket> {
        self.ticket
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, State::Done | State::Failed(_))
    }

    pub fn next_ping_at(&self) -> Option<u64> {
        self.next_ping_ms
    }

    /// いまの状態の期限（ms。こちらの予備）。
    pub fn deadline_at(&self) -> Option<u64> {
        self.deadline_ms
    }

    /// 答えの来ていないpingから数えた「応答しません」の時刻（ms）。答えを待っていなければNone。
    pub fn silence_deadline_at(&self) -> Option<u64> {
        self.unanswered_ping_ms
            .map(|p| p.saturating_add(PONG_TIMEOUT_MS))
    }

    /// 次に`poll`を呼ぶべき時刻（ms）。何も待っていなければNone。
    pub fn next_wake(&self) -> Option<u64> {
        [
            self.next_ping_ms,
            self.deadline_ms,
            self.silence_deadline_at(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn stop_clocks(&mut self) {
        self.deadline_ms = None;
        self.next_ping_ms = None;
        self.unanswered_ping_ms = None;
    }

    fn fail(&mut self, f: Failure) -> Vec<Action> {
        self.state = State::Failed(f);
        self.stop_clocks();
        vec![Action::Notify(Event::Failed(f))]
    }

    fn finish(&mut self) -> Action {
        self.state = State::Done;
        self.stop_clocks();
        Action::Notify(Event::Done)
    }

    /// 成立の後: 期限を外し、pingの時計だけを残す。
    fn hold_on(&mut self) -> Action {
        self.state = State::Holding;
        self.deadline_ms = None;
        Action::Notify(Event::Held)
    }

    /// 101を受けた。`room`を待ち始め、pingの時計を回す。
    pub fn on_upgraded(&mut self, now: Now) -> Vec<Action> {
        if self.state != State::Connecting {
            return Vec::new();
        }
        self.state = State::AwaitRoom;
        self.deadline_ms = Some(now.ms.saturating_add(ROOM_GREETING_TIMEOUT_MS));
        self.next_ping_ms = Some(now.ms.saturating_add(PING_INTERVAL_MS));
        Vec::new()
    }

    pub fn on_http_refused(&mut self, status: u16, body: &str) -> Vec<Action> {
        if self.is_finished() {
            return Vec::new();
        }
        self.fail(classify_http(status, body))
    }

    /// 接続できない（名前解決できない・時間切れ）・接続した後に閉じる番号なしに切れた。
    pub fn on_transport_failed(&mut self) -> Vec<Action> {
        match self.state {
            State::Done | State::Failed(_) => Vec::new(),
            State::Holding => vec![self.finish()],
            State::Connecting => self.fail(Failure::Unresponsive(Why::NoAnswer)),
            _ => self.fail(Failure::Unresponsive(Why::Lost)),
        }
    }

    pub fn on_close(&mut self, code: u16) -> Vec<Action> {
        if self.is_finished() {
            return Vec::new();
        }
        if self.state == State::Holding {
            return vec![self.finish()];
        }
        match CloseCode::from_number(code) {
            Some(CloseCode::Done) if self.state == State::AwaitClose => vec![self.finish()],
            Some(CloseCode::Expired) => self.fail(Failure::Expired),
            Some(CloseCode::PeerLeft) => self.fail(Failure::PeerLeft),
            Some(CloseCode::Refused) => self.fail(Failure::Refused),
            _ => self.fail(Failure::Unresponsive(Why::Closed(code))),
        }
    }

    pub fn on_text(&mut self, now: Now, text: &str) -> Vec<Action> {
        if self.is_finished() {
            return Vec::new();
        }
        // 文が届いた = サーバーまでの経路は生きている（pongに限らない）。
        self.unanswered_ping_ms = None;
        let msg = match parse_server_message(text) {
            Ok(m) => m,
            Err(e) => return self.fail(Failure::Unresponsive(Why::BadMessage(e))),
        };
        let kind = msg.kind();
        match (self.state, self.role, msg) {
            (State::Connecting, _, _) => self.fail(Failure::Unresponsive(Why::OutOfOrder(kind))),
            (_, _, ServerMsg::Pong) => Vec::new(),
            // 保っている間に来る文は閉じる前のerrorだけ（サーバーはpingの答えのほかに送らない）。どれでも失敗にせず終える。
            (State::Holding, _, _) => vec![self.finish()],
            // errorの文は閉じる番号と同じに扱う。1000（done）のerrorはサーバーが送らない。
            (_, _, ServerMsg::Error { code }) => match code {
                Some(CloseCode::Expired) => self.fail(Failure::Expired),
                Some(CloseCode::PeerLeft) => self.fail(Failure::PeerLeft),
                Some(CloseCode::Refused) => self.fail(Failure::Refused),
                other => self.fail(Failure::Unresponsive(Why::ErrorMessage(other))),
            },
            (State::AwaitRoom, _, ServerMsg::Room { role, expires_at }) => {
                self.role = Some(role);
                self.room_deadline_ms = backstop(now, expires_at, ROOM_TTL_MS);
                self.deadline_ms = Some(self.room_deadline_ms);
                self.state = match role {
                    Role::Offer => State::NeedOwnCode,
                    Role::Answer => State::AwaitPeerCode,
                };
                vec![Action::Notify(Event::Joined { role, expires_at })]
            }
            (State::AwaitMatched, Some(Role::Offer), ServerMsg::Matched { expires_at, ticket }) => {
                self.ticket = ticket;
                let d = backstop(now, expires_at, ANSWER_TTL_MS).min(self.room_deadline_ms);
                self.deadline_ms = Some(d);
                self.state = State::AwaitPeerCode;
                vec![Action::Notify(Event::Matched { expires_at })]
            }
            (State::AwaitPeerCode, Some(Role::Offer), ServerMsg::Peer { code, .. }) => {
                let end = if self.hold {
                    self.hold_on()
                } else {
                    self.finish()
                };
                vec![Action::Notify(Event::PeerCode(code)), end]
            }
            (State::AwaitPeerCode, Some(Role::Answer), ServerMsg::Peer { code, ticket }) => {
                self.ticket = ticket;
                // 返す側にはmatchedが来ないのでanswerの期限はこちらで数える。
                let d = now
                    .ms
                    .saturating_add(ANSWER_TTL_MS)
                    .saturating_add(DEADLINE_GRACE_MS)
                    .min(self.room_deadline_ms);
                self.deadline_ms = Some(d);
                self.state = State::NeedOwnCode;
                vec![Action::Notify(Event::PeerCode(code))]
            }
            _ => self.fail(Failure::Unresponsive(Why::OutOfOrder(kind))),
        }
    }

    /// 自分のコードをサーバーへ送る（始める側は`Joined`の後、返す側は`PeerCode`の後）。送る文を返す。
    pub fn submit_code(&mut self, code: &str) -> Result<Vec<Action>, SubmitError> {
        let role = match (self.state, self.role) {
            (State::NeedOwnCode, Some(r)) => r,
            (s, _) => return Err(SubmitError::NotNow(s)),
        };
        let (text, next) = match role {
            Role::Offer => (write_offer(code), State::AwaitMatched),
            Role::Answer => (write_answer(code), State::AwaitClose),
        };
        let text = text.map_err(SubmitError::Code)?;
        self.state = next;
        Ok(vec![Action::Send(text)])
    }

    /// 観戦を断る（観戦用のルームの始める側が`Joined`の後に`offer`の代わりに）。
    /// 以後はルームの期限（4408）まで残る。来る観戦者はサーバーが4403で閉じる。
    /// 断ったときは状態を変えない。
    pub fn refuse(&mut self) -> Result<Vec<Action>, SubmitError> {
        match (self.state, self.role) {
            (State::NeedOwnCode, Some(Role::Offer)) => {
                self.state = State::Refusing;
                Ok(vec![Action::Send(REFUSE_TEXT.to_owned())])
            }
            (s, _) => Err(SubmitError::NotNow(s)),
        }
    }

    /// 時計を進める。期限を過ぎていれば失敗、pingの時刻ならping。
    pub fn poll(&mut self, now: Now) -> Vec<Action> {
        if self.is_finished() || self.state == State::Connecting {
            return Vec::new();
        }
        if self.hold && self.state == State::AwaitClose {
            // 対戦のルームの返す側はanswerを渡し終えた時点で成立（サーバーは閉じずに残す）。
            // 送れなかったときは通信の処理がこの前に`on_transport_failed`で失敗にしている。
            return vec![self.hold_on()];
        }
        if let Some(d) = self.deadline_ms
            && now.ms >= d
        {
            let f = match self.state {
                State::AwaitRoom => Failure::Unresponsive(Why::NoRoom),
                State::AwaitClose => Failure::Unresponsive(Why::NoClose),
                _ => Failure::Expired,
            };
            return self.fail(f);
        }
        // pingより先に見る（3周目の後の4通目を送る前に切る）。
        if let Some(s) = self.silence_deadline_at()
            && now.ms >= s
        {
            if self.state == State::Holding {
                return vec![self.finish()];
            }
            return self.fail(Failure::Unresponsive(Why::NoPong));
        }
        if let Some(p) = self.next_ping_ms
            && now.ms >= p
        {
            // 長く呼ばれなかった後に続けて何通も出さないよう次はいまから数える。
            self.next_ping_ms = Some(now.ms.saturating_add(PING_INTERVAL_MS));
            self.unanswered_ping_ms.get_or_insert(now.ms);
            return vec![Action::Send(PING_TEXT.to_owned())];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    //! 期待値の外の基準: サーバーの値は`tests/fixtures/room/rules.json`、文の形はサーバーの`JSON.stringify`の出力を手で書いたもの。
    use super::*;

    const RULES: &str = include_str!("../../../tests/fixtures/room/rules.json");
    const NAMES: &str = include_str!("../../../tests/fixtures/room/names.json");

    /// 見本の`U+0000-U+001F,U+007F-U+009F`の形を範囲の列に変換する（形が違えば落ちる）。
    fn parse_ranges(s: &str) -> Vec<(u32, u32)> {
        s.split(',')
            .map(|part| {
                let (lo, hi) = part.split_once('-').expect("範囲は lo-hi");
                let hex = |x: &str| {
                    u32::from_str_radix(x.strip_prefix("U+").expect("U+ で始まる"), 16)
                        .expect("16 進")
                };
                (hex(lo), hex(hi))
            })
            .collect()
    }

    /// パスの`%XX`をデコードする（検査の側の手書き。サーバーの`decodeURIComponent`の代わり）。`%`の後が16進2桁でなければ落ちる。
    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(h, 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn t(ms: u64) -> Now {
        // 壁時計は単調な時計と別の値にずらしておく（同じ値だと取り違えても通る）。
        Now {
            ms,
            unix_secs: 1_790_000_000 + ms / 1000,
        }
    }

    fn b64(n: usize) -> String {
        "Ab9-_z".chars().cycle().take(n).collect()
    }

    // ---- 見本との突き合わせ ----

    #[test]
    fn constants_match_rules_fixture() {
        let list = parse_flat(RULES).expect("rules.json は平らな JSON");
        let mut used: Vec<&str> = vec!["about", "about_lines"];
        let int = |used: &mut Vec<&'static str>, k: &'static str| -> i64 {
            used.push(k);
            match list.iter().find(|(key, _)| key == k) {
                Some((_, JsonValue::Int(n))) => *n,
                other => panic!("{k} が整数でない: {other:?}"),
            }
        };
        let text = |used: &mut Vec<&'static str>, k: &'static str| -> String {
            used.push(k);
            match list.iter().find(|(key, _)| key == k) {
                Some((_, JsonValue::Str(s))) => s.to_string(),
                other => panic!("{k} が文字列でない: {other:?}"),
            }
        };

        assert_eq!(text(&mut used, "path_prefix"), PATH_PREFIX);

        let min = int(&mut used, "room_name_min") as usize;
        let max = int(&mut used, "room_name_max") as usize;
        assert_eq!(max, ROOM_NAME_MAX);
        assert!(RoomName::parse(&b64(min)).is_ok());
        assert!(RoomName::parse(&b64(min - 1)).is_err());
        assert!(RoomName::parse(&b64(max)).is_ok());
        assert!(RoomName::parse(&b64(max + 1)).is_err());
        let max_bytes = int(&mut used, "room_name_max_bytes") as usize;
        assert_eq!(max_bytes, ROOM_NAME_MAX_BYTES);
        // 4 Bの文字（U+20BB7）で大きさの端を作る（ASCIIだと長さのガードが先に落ちるため）。
        let four = |n: usize| "\u{20BB7}".repeat(n);
        assert!(RoomName::parse_server_rules(&four(max_bytes / 4)).is_ok());
        assert_eq!(
            RoomName::parse_server_rules(&format!("{}a", four(max_bytes / 4))),
            Err(RoomNameError::TooManyBytes {
                bytes: max_bytes + 1
            })
        );
        let ranges = parse_ranges(&text(&mut used, "room_name_controls"));
        let bad_chars = text(&mut used, "room_name_bad_chars");
        let mut n_bad = 0;
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            let want = ranges
                .iter()
                .any(|&(lo, hi)| (lo..=hi).contains(&u32::from(c)))
                || bad_chars.contains(c);
            let got = RoomName::parse_server_rules(&format!("a{c}b"));
            if want {
                assert_eq!(
                    got,
                    Err(RoomNameError::BadChar { pos: 1 }),
                    "U+{:04X}",
                    u32::from(c)
                );
                n_bad += 1;
            } else {
                assert!(got.is_ok(), "U+{:04X}: {got:?}", u32::from(c));
            }
        }
        assert_eq!(n_bad, 66, "Cc の 65 文字（C0 32・DEL 1・C1 32）と `/`");
        let bad_names = text(&mut used, "room_name_bad_names");
        let bad_names: Vec<&str> = bad_names.split(',').collect();
        assert_eq!(bad_names, ROOM_NAME_BAD_NAMES);
        for n in &bad_names {
            assert_eq!(RoomName::parse(n), Err(RoomNameError::DotName), "{n}");
        }
        // 揃えるのはサーバーだけ。こちらは手順の名を知っているだけ。打った文字のまま送る。
        assert_eq!(text(&mut used, "room_name_fold"), "nfkc,trim,lower");
        assert_eq!(RoomName::parse("RoOm_Z-9").unwrap().as_str(), "RoOm_Z-9");

        let cmin = int(&mut used, "code_min_chars") as usize;
        let cmax = int(&mut used, "code_max_chars") as usize;
        assert_eq!(cmax, CODE_MAX_CHARS);
        assert!(check_code(&b64(cmin)).is_ok());
        assert!(check_code(&b64(cmin - 1)).is_err());
        assert!(check_code(&b64(cmax)).is_ok());
        assert!(check_code(&b64(cmax + 1)).is_err());
        let code_chars = text(&mut used, "code_chars");
        for c in (0u8..128).map(char::from) {
            assert_eq!(
                check_code(&c.to_string()).is_ok(),
                code_chars.contains(c),
                "コードの文字 {c:?}"
            );
        }

        assert_eq!(
            int(&mut used, "message_max_bytes") as usize,
            MESSAGE_MAX_BYTES
        );
        assert_eq!(int(&mut used, "room_ttl_ms") as u64, ROOM_TTL_MS);
        assert_eq!(int(&mut used, "answer_ttl_ms") as u64, ANSWER_TTL_MS);
        assert_eq!(int(&mut used, "held_stale_ms") as u64, HELD_STALE_MS);
        assert_eq!(int(&mut used, "held_sweep_ms") as u64, HELD_SWEEP_MS);
        let mut n_http = 0;
        for (k, v) in &list {
            let Some(name) = k.strip_prefix("http_") else {
                continue;
            };
            assert_eq!(v, &JsonValue::Str(name.into()), "{k}");
            assert!(HttpErrorName::from_wire(name).is_some(), "{k}");
            used.push(match name {
                "room_in_use" => "http_room_in_use",
                "room_full" => "http_room_full",
                other => panic!("知らない http_ の鍵 {other}"),
            });
            n_http += 1;
        }
        assert_eq!(n_http, 2);
        assert_eq!(
            classify_http(409, r#"{"error":"room_in_use"}"#),
            Failure::RoomInUse
        );
        assert_eq!(text(&mut used, "ping_text"), PING_TEXT);
        assert_eq!(text(&mut used, "pong_text"), PONG_TEXT);
        assert_eq!(int(&mut used, "retry_after_secs") as u32, RETRY_AFTER_SECS);
        assert_eq!(text(&mut used, "client_id_param"), CLIENT_ID_PARAM);
        assert_eq!(
            int(&mut used, "client_id_len") as usize,
            2 * CLIENT_ID_BYTES
        );
        assert_eq!(text(&mut used, "ticket_param"), TICKET_PARAM);
        assert_eq!(int(&mut used, "ticket_ttl_secs") as u64, TICKET_TTL_SECS);
        assert_eq!(text(&mut used, "ticket_purpose"), TICKET_PURPOSE);
        assert_eq!(
            int(&mut used, "ticket_num_max_digits") as usize,
            TICKET_NUM_MAX_DIGITS
        );
        assert_eq!(
            int(&mut used, "ticket_mac_chars") as usize,
            TICKET_MAC_CHARS
        );

        for c in CloseCode::ALL {
            let key = format!("close_{}", c.name());
            let found = list.iter().find(|(k, _)| *k == key);
            assert_eq!(
                found.map(|(_, v)| v.clone()),
                Some(JsonValue::Int(i64::from(c.number()))),
                "{key}"
            );
        }
        let close_keys = list.iter().filter(|(k, _)| k.starts_with("close_")).count();
        assert_eq!(close_keys, CloseCode::ALL.len(), "見本の閉じる番号の数");

        // 読まなかったキーが無いこと（見本に足した値がここで比べられないまま通らないように）。
        let unread: Vec<&str> = list
            .iter()
            .map(|(k, _)| k.as_ref())
            // `random_`のキーはランダム対戦の値（`room_queue.rs`の検査が同じ見本と突き合わせる）。`watch_`のキーは一覧の値（`watch_list.rs`）。
            .filter(|k| {
                !k.starts_with("close_")
                    && !k.starts_with("random_")
                    && !k.starts_with("watch_")
                    && !used.iter().any(|u| u == k)
            })
            .collect();
        assert!(unread.is_empty(), "比べていない鍵: {unread:?}");
    }

    #[test]
    fn ping_text_is_exact() {
        assert_eq!(PING_TEXT.len(), 12);
        assert_eq!(PING_TEXT.as_bytes(), br#"{"t":"ping"}"#);
        // 1文字違いはJSONとしてはpingでもサーバーの自動応答には当たらない。ここでは読み手が1文字違いのtをpongと取り違えないこと。
        for bad in [
            r#"{"t":"Pong"}"#,
            r#"{"t":"pon"}"#,
            r#"{"t":"pongs"}"#,
            r#"{"t":"ping"}"#,
        ] {
            assert_eq!(
                parse_server_message(bad),
                Err(MsgError::UnknownType),
                "{bad}"
            );
        }
        assert_eq!(parse_server_message(PONG_TEXT), Ok(ServerMsg::Pong));
    }

    // ---- ルーム名とパス ----

    #[test]
    fn room_names() {
        let n32 = "a".repeat(32);
        assert_eq!(RoomName::parse_server_rules(&n32).unwrap().as_str(), n32);
        assert_eq!(
            RoomName::parse_server_rules(&"a".repeat(33)),
            Err(RoomNameError::TooLong { len: 33 })
        );
        let padded = format!("  {n32}  ");
        assert_eq!(
            RoomName::parse_server_rules(&padded).unwrap().as_str(),
            padded
        );
        assert_eq!(
            RoomName::parse_server_rules(&format!(" {}", "a".repeat(33))),
            Err(RoomNameError::TooLong { len: 33 })
        );
        assert_eq!(RoomName::parse_server_rules(""), Err(RoomNameError::Empty));
        for blank in [" ", "   ", "\u{3000}", "\u{FEFF}", " \u{2028}\u{A0} "] {
            assert_eq!(
                RoomName::parse_server_rules(blank),
                Err(RoomNameError::Blank),
                "{blank:?}"
            );
        }
        assert_eq!(
            RoomName::parse_server_rules("room\t"),
            Err(RoomNameError::BadChar { pos: 4 })
        );
        assert_eq!(
            RoomName::parse_server_rules("部屋/1"),
            Err(RoomNameError::BadChar { pos: 2 })
        );
        assert_eq!(
            RoomName::parse_server_rules("a\u{85}"),
            Err(RoomNameError::BadChar { pos: 1 })
        );
        assert_eq!(
            RoomName::parse_server_rules("."),
            Err(RoomNameError::DotName)
        );
        assert_eq!(
            RoomName::parse_server_rules(".."),
            Err(RoomNameError::DotName)
        );
        assert_eq!(
            RoomName::parse_server_rules(&format!("{}/", "あ".repeat(43))),
            Err(RoomNameError::TooManyBytes { bytes: 130 })
        );

        // 日本語・全角・記号は通る。
        // ASCII以外の名の長さはこちらでは数えない（揃えると縮む「ｶﾞ」40文字はサーバーで20文字になって通るので、こちらが断ってはいけない）。
        for ok in [
            "花映塚の部屋",
            "ＡＢＣ",
            "ｶﾞｶﾞ",
            "a b",
            "...",
            "a.b",
            "a%20",
            "Ａ／Ｂ",
            "\u{2028}x",
        ] {
            assert_eq!(
                RoomName::parse_server_rules(ok).unwrap().as_str(),
                ok,
                "{ok:?}"
            );
        }
        assert!(RoomName::parse_server_rules(&"部".repeat(33)).is_ok());
        assert!(RoomName::parse_server_rules(&"ｶﾞ".repeat(20)).is_ok());

        let r = RoomName::parse("Room-1_X").unwrap();
        assert_eq!(r.as_str(), "Room-1_X");
        assert_ne!(r, RoomName::parse("room-1_x").unwrap());
        assert_eq!(r.path(), "/v0/rooms/Room-1_X");
        assert_eq!(
            r.url("wss://example.workers.dev"),
            "wss://example.workers.dev/v0/rooms/Room-1_X"
        );
        assert_eq!(
            r.url("ws://127.0.0.1:8787//"),
            "ws://127.0.0.1:8787/v0/rooms/Room-1_X"
        );
    }

    #[test]
    fn room_path_is_percent_encoded() {
        let p = |s: &str| RoomName::parse_server_rules(s).unwrap().path();
        assert_eq!(p("AZaz09-._~"), "/v0/rooms/AZaz09-._~");
        assert_eq!(p("花"), "/v0/rooms/%E8%8A%B1");
        assert_eq!(p("ｶﾞ"), "/v0/rooms/%EF%BD%B6%EF%BE%9E");
        assert_eq!(p("\u{20BB7}"), "/v0/rooms/%F0%A0%AE%B7");
        assert_eq!(
            p(" a+b%c?d#e\\f&g=h'"),
            "/v0/rooms/%20a%2Bb%25c%3Fd%23e%5Cf%26g%3Dh%27"
        );
        assert_eq!(p("\u{3000}x\u{FEFF}"), "/v0/rooms/%E3%80%80x%EF%BB%BF");
        // パスに生の`/`は決して入らない（`/`はparseが断る。全角の`／`は符号化される）。
        assert_eq!(p("Ａ／Ｂ"), "/v0/rooms/%EF%BC%A1%EF%BC%8F%EF%BC%A2");
        // 128 Bの名のパスは先頭 ＋ 384文字まで（サーバーの区切りの前置きのガードの内）。
        let big = "\u{20BB7}".repeat(32);
        assert_eq!(p(&big).len(), PATH_PREFIX.len() + 128 * 3);
    }

    #[test]
    fn client_id_goes_in_the_query_after_the_encoded_path() {
        let mut id = [0u8; CLIENT_ID_BYTES];
        for (i, b) in id.iter_mut().enumerate() {
            // 上下の4 bitが違う値（16進の桁の入れ違いがあれば検査が落ちる）。
            *b = (i as u8).wrapping_mul(0x1F).wrapping_add(0xA3);
        }
        let name = RoomName::parse_server_rules("花 a").unwrap();
        let p = name.path_with_client(&id);
        let (path, query) = p.split_once('?').expect("問いがある");
        assert_eq!(path, name.path());
        let hex = query.strip_prefix("cid=").expect("cid=");
        assert_eq!(hex.len(), 2 * CLIENT_ID_BYTES);
        assert!(
            hex.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "16 進小文字だけ: {hex}"
        );
        assert_eq!(&hex[..4], "a3c2", "先頭の 2 バイトは 0xA3 0xC2 の順");
        assert_eq!(p.matches('?').count(), 1);
    }

    /// 発行・期限・MACの3つのフィールドのチケット（MACの最後の文字は下2 bitが0の`w`）。
    const TICKET: &str = "1790000000.1790086400.Ab9-_zAb9-_zAb9-_zAb9-_zAb9-_zAb9-_zAb9-_zw";

    #[test]
    fn the_ticket_goes_in_the_query_after_the_client_id_and_no_room_name_is_sent() {
        let origin = RoomName::parse("Room_1").unwrap();
        let w = origin.spectate_of();
        let ticket = SeatTicket::parse(TICKET).unwrap();
        assert_eq!(
            w.path_with_ticket(None, &ticket),
            format!("{}?ticket={TICKET}", w.path())
        );
        let id = [0xA3u8; CLIENT_ID_BYTES];
        let p = w.path_with_ticket(Some(&id), &ticket);
        assert_eq!(
            p,
            format!(
                "{}?cid={}&ticket={TICKET}",
                w.path(),
                "a3".repeat(CLIENT_ID_BYTES)
            )
        );
        assert!(!p.to_ascii_lowercase().contains("room_1"), "{p}");
        assert!(!p.contains("origin"), "{p}");
        // cidだけの形（対戦のルーム・観戦者）はチケットを含まない。
        assert!(!w.path_with_client(&id).contains("ticket"));
        assert!(!w.path().contains('?'));
    }

    #[test]
    fn a_seat_ticket_passes_only_in_the_servers_exact_shape() {
        let t = SeatTicket::parse(TICKET).unwrap();
        assert_eq!(t.as_str(), TICKET);
        assert_eq!(format!("{t:?}"), "SeatTicket(..)", "Debug に札が出た");
        let mac = &TICKET[TICKET.len() - TICKET_MAC_CHARS..];
        let longest = format!("{0}.{0}.{mac}", "9".repeat(TICKET_NUM_MAX_DIGITS));
        assert_eq!(longest.len(), TICKET_MAX_CHARS);
        assert!(SeatTicket::parse(&longest).is_some());
        assert!(SeatTicket::parse(&format!("0.0.{mac}")).is_some());
        // 下2 bitが0の最後の文字は16通り。
        let ok_last: String = (0u8..64)
            .filter_map(|v| {
                let s = format!("1.2.{}{}", &mac[..42], char::from(b64_char(v)));
                SeatTicket::parse(&s).map(|_| char::from(b64_char(v)))
            })
            .collect();
        assert_eq!(ok_last, "AEIMQUYcgkosw048");
        let bad = [
            String::new(),
            "1.2".to_owned(),
            mac.to_owned(),
            format!("1.2.{mac}.3"),
            format!("01.2.{mac}"),
            format!("1.02.{mac}"),
            format!("{}.2.{mac}", "1".repeat(TICKET_NUM_MAX_DIGITS + 1)),
            format!("1.{}.{mac}", "1".repeat(TICKET_NUM_MAX_DIGITS + 1)),
            format!("-1.2.{mac}"),
            format!(".2.{mac}"),
            format!("1..{mac}"),
            format!("1.2.{}", &mac[..42]),
            format!("1.2.{mac}A"),
            format!("1.2.{}+", &mac[..42]),
            format!("1.2.{}=", &mac[..42]),
            format!("1.2.{}x", &mac[..42]),
            format!("1.2.{} ", &mac[..42]),
            format!(" 1.2.{mac}"),
            format!("1.2.{}%41", &mac[..40]),
            format!("1.2.{}あ", &mac[..41]),
        ];
        for b in &bad {
            assert_eq!(SeatTicket::parse(b), None, "{b:?}");
        }
    }

    fn b64_char(v: u8) -> u8 {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"[usize::from(v)]
    }

    #[test]
    fn room_names_follow_the_names_fixture() {
        let list = parse_flat(NAMES).expect("names.json は平らな JSON");
        let (mut n_pass, mut n_bad, mut n_dll_pass, mut n_dll_refuse) = (0, 0, 0, 0);
        for (k, v) in &list {
            let JsonValue::Str(name) = v else {
                panic!("{k} が文字列でない")
            };
            if k == "about" {
                continue;
            }
            let client_passes = if k.starts_with("bad_") {
                false
            } else if k.starts_with("ok_")
                || k.starts_with("server_bad_")
                || k.starts_with("same_")
                || k.starts_with("diff_")
            {
                true
            } else {
                panic!("names.json の鍵の接頭を知らない: {k}")
            };
            let got = RoomName::parse_server_rules(name);
            // dllの本番（`parse`）の期待: サーバーの決まりを通る名のうち使える文字（`accepts_char`）だけの名は前後の空白を除いて通る。
            // ほかは`NotAscii`。サーバーの決まりで断る名は`parse`も断る（別の理由で）。
            let ascii_only = name.chars().all(RoomName::accepts_char);
            if client_passes {
                let r = got.unwrap_or_else(|e| panic!("{k} をサーバーの決まりで断った: {e:?}"));
                assert_eq!(r.as_str(), name.as_ref(), "{k}");
                let path = r.path();
                let seg = path.strip_prefix(PATH_PREFIX).unwrap();
                assert!(
                    seg.bytes().all(|b| is_unreserved(b) || b == b'%'),
                    "{k}: {seg}"
                );
                assert_eq!(percent_decode(seg), name.as_ref(), "{k}");
                n_pass += 1;
                if ascii_only {
                    let trimmed = RoomName::parse_server_rules(name.trim_matches(' '))
                        .unwrap_or_else(|e| {
                            panic!("{k} を除いた名をサーバーの決まりで断った: {e:?}")
                        });
                    assert_eq!(
                        RoomName::parse(name),
                        Ok(trimmed),
                        "{k}: dll が使える文字の名を断った"
                    );
                    n_dll_pass += 1;
                } else {
                    assert!(
                        matches!(RoomName::parse(name), Err(RoomNameError::NotAscii { .. })),
                        "{k}: dll が ASCII でない名を NotAscii で断らなかった"
                    );
                    n_dll_refuse += 1;
                }
            } else {
                assert!(got.is_err(), "{k} を通した");
                assert!(RoomName::parse(name).is_err(), "{k} を dll が通した");
                n_bad += 1;
            }
        }
        // 母数（どれも0件で素通りにならないように）。
        assert!(
            n_pass > 0 && n_bad > 0 && n_dll_pass > 0 && n_dll_refuse > 0,
            "通す {n_pass}・断る {n_bad}・dll が通す {n_dll_pass}・dll だけが断る {n_dll_refuse}"
        );
    }

    #[test]
    fn an_at_sign_in_a_room_name_goes_into_the_path_as_percent_40_and_comes_back() {
        let name = RoomName::parse("Room@x_1").unwrap();
        assert_eq!(name.path(), "/v0/rooms/Room%40x_1");
        let path = name.path();
        let seg = path.strip_prefix(PATH_PREFIX).unwrap();
        assert_eq!(percent_decode(seg), "Room@x_1");
        // 観戦用のルームの名はASCIIの`w.`と16進だけで、`@`を含む名から作っても`@`は出ない。
        assert!(!name.spectate_of().as_str().contains('@'));
        // 接続コードの文字のガードは`@`を通さないまま（base64url）。
        assert_eq!(check_code("ab@cd"), Err(CodeTextError::BadChar { pos: 2 }));
    }

    #[test]
    fn dll_room_names_follow_the_printable_ascii_rule() {
        // 使える文字は0x20〜0x7Eのうち`` ` `` `;` `/` 以外の92文字だけ。
        let accepted: Vec<char> = (0..0x3100u32)
            .filter_map(char::from_u32)
            .filter(|&c| RoomName::accepts_char(c))
            .collect();
        assert_eq!(accepted.len(), 92);
        assert!(accepted.iter().all(|c| (' '..='~').contains(c)));
        for no in ['`', ';', '/', '\u{7F}', '\u{1F}', 'あ', 'Ａ', '\u{3000}'] {
            assert!(!RoomName::accepts_char(no), "{no:?}");
        }
        for yes in ['.', '@', '"', '\\', ' ', '?', '#', '%', '+', '-', '_', '~'] {
            assert!(RoomName::accepts_char(yes), "{yes:?}");
        }
        for ok in [
            "a",
            "Z",
            "0",
            "-",
            "_",
            "Room_1-A",
            "ROOM",
            "room",
            "a@b",
            "@",
            &"x".repeat(32),
            "a b",
            "a.b",
            ".a",
            "...",
            "a+b",
            "a%20",
            "a\"b",
            "a\\b",
            "a?b#c",
            "w",
            "q",
            "w-x",
            "ww.x",
            "w .x",
            "xq.",
        ] {
            assert_eq!(RoomName::parse(ok).unwrap().as_str(), ok, "{ok:?}");
        }
        assert_ne!(
            RoomName::parse("Room-1").unwrap(),
            RoomName::parse("room-1").unwrap()
        );
        // 前後の空白は除き、間の空白は残す。
        for (typed, kept) in [
            ("  abc  ", "abc"),
            (" a b ", "a b"),
            ("a  b", "a  b"),
            (" x", "x"),
            ("x ", "x"),
        ] {
            assert_eq!(RoomName::parse(typed).unwrap().as_str(), kept, "{typed:?}");
        }
        assert_eq!(RoomName::parse(" a b ").unwrap().path(), "/v0/rooms/a%20b");
        // 断る: 文字の位置つき（0からの文字の数。バイトでない: 「花」は3 B）。
        for (bad, pos) in [
            ("花映塚の部屋", 0),
            ("ab花", 2),
            ("ＡＢＣ", 0),
            ("abc１", 3),
            ("ｶﾞ", 0),
            ("a`b", 1),
            ("a;b", 1),
            (";", 0),
            ("abc`", 3),
            ("Ａ／Ｂ", 0),
            ("🌸abc", 0),
            ("\u{3000}x", 0),
            ("x\u{3000}", 1),
        ] {
            assert_eq!(
                RoomName::parse(bad),
                Err(RoomNameError::NotAscii { pos }),
                "{bad:?}"
            );
            assert!(RoomName::parse_server_rules(bad).is_ok(), "{bad:?}");
        }
        // 前後の空白を除いて小文字にした名が`w.` `q.`で始まる名はサーバーの決まりを通るが、dllは断る。
        for bad in [
            "w.abc",
            "W.abc",
            "q.abc",
            "Q.ABC",
            "w.",
            "q.",
            " w.x",
            "  Q.x  ",
            "q.0123456789abcdef0123456789abcd",
        ] {
            assert_eq!(
                RoomName::parse(bad),
                Err(RoomNameError::ReservedPrefix),
                "{bad:?}"
            );
            assert!(RoomName::parse_server_rules(bad).is_ok(), "{bad:?}");
        }
        assert_eq!(RoomName::parse(""), Err(RoomNameError::Empty));
        assert_eq!(RoomName::parse("   "), Err(RoomNameError::Blank));
        assert_eq!(
            RoomName::parse("a\tb"),
            Err(RoomNameError::BadChar { pos: 1 })
        );
        assert_eq!(
            RoomName::parse("a/b"),
            Err(RoomNameError::BadChar { pos: 1 })
        );
        assert_eq!(
            RoomName::parse("花/1"),
            Err(RoomNameError::BadChar { pos: 1 })
        );
        assert_eq!(RoomName::parse(".."), Err(RoomNameError::DotName));
        assert_eq!(
            RoomName::parse(&"a".repeat(33)),
            Err(RoomNameError::TooLong { len: 33 })
        );
        assert_eq!(
            RoomName::parse(&"花".repeat(43)),
            Err(RoomNameError::TooManyBytes { bytes: 129 })
        );
    }

    // ---- コードと送る文 ----

    #[test]
    fn codes_and_outgoing_text() {
        let c1024 = b64(1024);
        assert_eq!(check_code(&c1024), Ok(()));
        assert_eq!(
            check_code(&b64(1025)),
            Err(CodeTextError::TooLong { chars: 1025 })
        );
        // サーバーは長さを先に見る。文字も悪い1025文字はtoo_large。
        let bad_long = format!("{} ", b64(1024));
        assert_eq!(
            check_code(&bad_long).map_err(CodeTextError::close_code),
            Err(CloseCode::TooLarge)
        );
        assert_eq!(check_code(""), Err(CodeTextError::Empty));
        assert_eq!(CodeTextError::Empty.close_code(), CloseCode::BadCode);
        for (bad, pos) in [
            (" AAAA", 0),
            ("AAAA ", 4),
            ("AAAA\n", 4),
            ("AA+A", 2),
            ("AA/A", 2),
            ("AAA=", 3),
            ("AAあ", 2),
            ("AA\"A", 2),
        ] {
            assert_eq!(
                check_code(bad),
                Err(CodeTextError::BadChar { pos }),
                "{bad:?}"
            );
        }

        assert_eq!(
            write_offer("Abc-_9").unwrap(),
            r#"{"t":"offer","code":"Abc-_9"}"#
        );
        assert_eq!(
            write_answer("Abc-_9").unwrap(),
            r#"{"t":"answer","code":"Abc-_9"}"#
        );
        assert_eq!(write_offer("a\"b"), Err(CodeTextError::BadChar { pos: 1 }));
        assert_eq!(write_answer(""), Err(CodeTextError::Empty));
        assert!(write_answer(&c1024).unwrap().len() <= MESSAGE_MAX_BYTES);
    }

    #[test]
    fn close_codes_round_trip() {
        for c in CloseCode::ALL {
            assert_eq!(CloseCode::from_number(c.number()), Some(c));
            assert_eq!(CloseCode::from_name(c.name()), Some(c));
        }
        for n in [0, 1001, 1005, 1006, 1011, 4000, 4401, 4999] {
            assert_eq!(CloseCode::from_number(n), None, "{n}");
        }
        assert_eq!(CloseCode::from_name("Done"), None);
        assert_eq!(CloseCode::from_name("expired "), None);
    }

    // ---- サーバーの文の読み手 ----

    #[test]
    fn server_messages_read() {
        assert_eq!(
            parse_server_message(r#"{"t":"room","role":"host","expires_at":1790000600}"#),
            Ok(ServerMsg::Room {
                role: Role::Offer,
                expires_at: 1_790_000_600
            })
        );
        assert_eq!(
            parse_server_message(r#"{"t":"room","role":"guest","expires_at":1790000599}"#),
            Ok(ServerMsg::Room {
                role: Role::Answer,
                expires_at: 1_790_000_599
            })
        );
        assert_eq!(Role::from_wire("host"), Some(Role::Offer));
        assert_eq!(Role::from_wire("guest"), Some(Role::Answer));
        assert_eq!(
            parse_server_message(r#"{"t":"peer","code":"AbC-_0"}"#),
            Ok(ServerMsg::Peer {
                code: "AbC-_0".into(),
                ticket: None
            })
        );
        assert_eq!(
            parse_server_message(r#"{"t":"matched","expires_at":1790000060}"#),
            Ok(ServerMsg::Matched {
                expires_at: 1_790_000_060,
                ticket: None
            })
        );
        let ticket = SeatTicket::parse(TICKET);
        assert!(ticket.is_some());
        assert_eq!(
            parse_server_message(&format!(
                r#"{{"t":"matched","expires_at":1790000060,"ticket":"{TICKET}"}}"#
            )),
            Ok(ServerMsg::Matched {
                expires_at: 1_790_000_060,
                ticket
            })
        );
        assert_eq!(
            parse_server_message(&format!(
                r#"{{"t":"peer","code":"AbC-_0","ticket":"{TICKET}"}}"#
            )),
            Ok(ServerMsg::Peer {
                code: "AbC-_0".into(),
                ticket
            })
        );
        // 形の外れたチケット・文字列でないチケットはチケットなし（文そのものは読める）。
        for bad in [
            format!(r#""{TICKET}x""#),
            r#""""#.to_owned(),
            "1790000000".to_owned(),
            "null".to_owned(),
            format!(r#"["{TICKET}"]"#),
        ] {
            assert_eq!(
                parse_server_message(&format!(
                    r#"{{"t":"matched","expires_at":1,"ticket":{bad}}}"#
                )),
                Ok(ServerMsg::Matched {
                    expires_at: 1,
                    ticket: None
                }),
                "{bad}"
            );
            assert_eq!(
                parse_server_message(&format!(r#"{{"t":"peer","code":"A","ticket":{bad}}}"#)),
                Ok(ServerMsg::Peer {
                    code: "A".into(),
                    ticket: None
                }),
                "{bad}"
            );
        }
        assert_eq!(
            parse_server_message(r#"{"t":"error","code":"peer_left"}"#),
            Ok(ServerMsg::Error {
                code: Some(CloseCode::PeerLeft)
            })
        );
        assert_eq!(
            parse_server_message(r#"{"t":"error","code":"new_kind"}"#),
            Ok(ServerMsg::Error { code: None })
        );
        let loose = " {\n \"x\" : {\"a\":[1,2.5,{\"b\":null}],\"c\":true} , \"role\":\"gu\\u0065st\",\"y\":-1.5e3,\
                     \"z\":false,\"t\" :\"room\",\"w\":\"\\\"\\\\\\/\\n\\ud83d\\ude00\",\"expires_at\": 0 }\t";
        assert_eq!(
            parse_server_message(loose),
            Ok(ServerMsg::Room {
                role: Role::Answer,
                expires_at: 0
            })
        );
        assert_eq!(
            parse_server_message(r#"{"note":"部屋 ✓","t":"pong"}"#),
            Ok(ServerMsg::Pong)
        );
    }

    #[test]
    fn server_messages_rejected() {
        use MsgError::*;
        let cases: &[(&str, MsgError)] = &[
            ("hello", NotJson { at: 0 }),
            ("", NotJson { at: 0 }),
            ("{", NotJson { at: 1 }),
            (r#"{"t":"room""#, NotJson { at: 11 }),
            (r#"{"t":"pong"} x"#, NotJson { at: 13 }),
            (r#"{"t":"pong"}{}"#, NotJson { at: 12 }),
            ("[]", NotJson { at: 0 }),
            (r#""pong""#, NotJson { at: 0 }),
            (r#"{"t":"pong",}"#, NotJson { at: 12 }),
            (r#"{'t':'pong'}"#, NotJson { at: 1 }),
            // `\n`はJSONのエスケープなので読める（tは "po<改行>g" で知らないt）。生の制御文字は次の行。
            (r#"{"t":"po\ng"}"#, UnknownType),
            ("{\"t\":\"po\u{1}ng\"}", NotJson { at: 8 }),
            (r#"{"t":"\x"}"#, NotJson { at: 7 }),
            (r#"{"t":"\ud800"}"#, NotJson { at: 12 }),
            (r#"{"t":"\udc00"}"#, NotJson { at: 12 }),
            (r#"{"t":"pong","t":"pong"}"#, NotJson { at: 12 }),
            (
                r#"{"t":"room","role":"host","expires_at":01}"#,
                NotJson { at: 40 },
            ),
            (r#"{"t":"pong","x":tru}"#, NotJson { at: 19 }),
            (r#"{"t":"pong","x":[1,]}"#, NotJson { at: 19 }),
            ("{}", Missing("t")),
            (r#"{"type":"pong"}"#, Missing("t")),
            (r#"{"t":1}"#, WrongType("t")),
            (r#"{"t":null}"#, WrongType("t")),
            (r#"{"t":["pong"]}"#, WrongType("t")),
            (r#"{"t":"hello"}"#, UnknownType),
            (r#"{"t":"offer","code":"AAAA"}"#, UnknownType),
            (r#"{"t":"room","expires_at":1}"#, Missing("role")),
            (
                r#"{"t":"room","role":"Host","expires_at":1}"#,
                BadValue("role"),
            ),
            (
                r#"{"t":"room","role":"offer","expires_at":1}"#,
                BadValue("role"),
            ),
            (r#"{"t":"room","role":1,"expires_at":1}"#, WrongType("role")),
            (r#"{"t":"room","role":"host"}"#, Missing("expires_at")),
            (
                r#"{"t":"room","role":"host","expires_at":"1"}"#,
                WrongType("expires_at"),
            ),
            (
                r#"{"t":"room","role":"host","expires_at":1.5}"#,
                WrongType("expires_at"),
            ),
            (
                r#"{"t":"room","role":"host","expires_at":1e3}"#,
                WrongType("expires_at"),
            ),
            (
                r#"{"t":"room","role":"host","expires_at":-1}"#,
                BadValue("expires_at"),
            ),
            (
                r#"{"t":"room","role":"host","expires_at":99999999999999999999}"#,
                WrongType("expires_at"),
            ),
            (
                r#"{"t":"matched","expires_at":true}"#,
                WrongType("expires_at"),
            ),
            (r#"{"t":"peer"}"#, Missing("code")),
            (r#"{"t":"peer","code":5}"#, WrongType("code")),
            (r#"{"t":"peer","code":""}"#, BadValue("code")),
            (r#"{"t":"peer","code":" AAAA"}"#, BadValue("code")),
            (r#"{"t":"error"}"#, Missing("code")),
            (r#"{"t":"error","code":4408}"#, WrongType("code")),
        ];
        for (text, want) in cases {
            assert_eq!(parse_server_message(text), Err(*want), "{text}");
        }
        let pad = |n: usize| {
            let head = r#"{"t":"pong","x":""#;
            format!("{head}{}\"}}", "a".repeat(n - head.len() - 2))
        };
        assert_eq!(pad(MESSAGE_MAX_BYTES).len(), MESSAGE_MAX_BYTES);
        assert_eq!(
            parse_server_message(&pad(MESSAGE_MAX_BYTES)),
            Ok(ServerMsg::Pong)
        );
        assert_eq!(
            parse_server_message(&pad(MESSAGE_MAX_BYTES + 1)),
            Err(TooLarge {
                bytes: MESSAGE_MAX_BYTES + 1
            })
        );
        let deep = format!(
            r#"{{"t":"pong","x":{}1{}}}"#,
            "[".repeat(40),
            "]".repeat(40)
        );
        assert!(matches!(parse_server_message(&deep), Err(NotJson { .. })));
        let p1024 = format!(r#"{{"t":"peer","code":"{}"}}"#, b64(1024));
        assert!(matches!(
            parse_server_message(&p1024),
            Ok(ServerMsg::Peer { .. })
        ));
        let p1025 = format!(r#"{{"t":"peer","code":"{}"}}"#, b64(1025));
        assert_eq!(parse_server_message(&p1025), Err(BadValue("code")));
    }

    // ---- 入口のHTTP ----

    #[test]
    fn http_refusals() {
        use Failure::*;
        assert_eq!(
            read_http_error(r#"{"error":"rate_limited","retry_after":60}"#),
            Ok(HttpErrorBody {
                error: HttpErrorName::RateLimited,
                retry_after_secs: Some(60)
            })
        );
        assert_eq!(
            read_http_error(r#"{"error":"room_full"}"#),
            Ok(HttpErrorBody {
                error: HttpErrorName::RoomFull,
                retry_after_secs: None
            })
        );
        assert_eq!(
            read_http_error(r#"{"error":"nope"}"#),
            Err(MsgError::BadValue("error"))
        );
        assert_eq!(
            read_http_error(r#"{"err":"room_full"}"#),
            Err(MsgError::Missing("error"))
        );
        assert_eq!(
            read_http_error(r#"{"error":"rate_limited","retry_after":-1}"#),
            Err(MsgError::BadValue("retry_after"))
        );
        assert_eq!(
            read_http_error(r#"{"error":"rate_limited","retry_after":"60"}"#),
            Err(MsgError::WrongType("retry_after"))
        );

        let html = "<html><body>error code: 1027</body></html>";
        let cases: &[(u16, &str, Failure)] = &[
            (
                404,
                r#"{"error":"not_found"}"#,
                Unresponsive(Why::HttpStatus(404)),
            ),
            (
                405,
                r#"{"error":"method_not_allowed"}"#,
                Unresponsive(Why::HttpStatus(405)),
            ),
            (
                426,
                r#"{"error":"upgrade_required"}"#,
                Unresponsive(Why::HttpStatus(426)),
            ),
            (400, r#"{"error":"bad_room_name"}"#, BadRoomName),
            (409, r#"{"error":"room_full"}"#, RoomFull),
            (409, r#"{"error":"room_in_use"}"#, RoomInUse),
            (
                400,
                r#"{"error":"room_in_use"}"#,
                Unresponsive(Why::NotOurBody(400)),
            ),
            (
                429,
                r#"{"error":"rate_limited","retry_after":60}"#,
                RateLimited {
                    retry_after_secs: 60,
                },
            ),
            (
                429,
                r#"{"error":"rate_limited","retry_after":7}"#,
                RateLimited {
                    retry_after_secs: 7,
                },
            ),
            (
                429,
                r#"{"error":"rate_limited"}"#,
                Unresponsive(Why::NotOurBody(429)),
            ),
            (429, html, Unresponsive(Why::NotOurBody(429))),
            (409, html, Unresponsive(Why::NotOurBody(409))),
            (400, html, Unresponsive(Why::NotOurBody(400))),
            (
                400,
                r#"{"error":"room_full"}"#,
                Unresponsive(Why::NotOurBody(400)),
            ),
            (400, r#"{"error":"bad_ticket"}"#, BadTicket),
            (
                409,
                r#"{"error":"bad_ticket"}"#,
                Unresponsive(Why::NotOurBody(409)),
            ),
            (
                400,
                r#"{"error":"bad_origin"}"#,
                Unresponsive(Why::NotOurBody(400)),
            ),
            (
                409,
                r#"{"error":"bad_room_name"}"#,
                Unresponsive(Why::NotOurBody(409)),
            ),
            (
                404,
                r#"{"error":"bad_room_name"}"#,
                Unresponsive(Why::HttpStatus(404)),
            ),
            (500, "", Unresponsive(Why::HttpStatus(500))),
            (503, html, Unresponsive(Why::HttpStatus(503))),
            (
                200,
                r#"{"error":"room_full"}"#,
                Unresponsive(Why::HttpStatus(200)),
            ),
        ];
        for (status, body, want) in cases {
            assert_eq!(classify_http(*status, body), *want, "{status} {body}");
        }
        assert_eq!(classify_http(404, "").kind(), FailureKind::Unresponsive);
        assert_eq!(
            RateLimited {
                retry_after_secs: 1
            }
            .kind(),
            FailureKind::WaitThenRetry
        );
        assert_eq!(RoomFull.kind(), FailureKind::ChangeRoomName);
        assert_eq!(RoomInUse.kind(), FailureKind::ChangeRoomName);
        assert_eq!(BadRoomName.kind(), FailureKind::ChangeRoomName);
        assert_eq!(Expired.kind(), FailureKind::Expired);
        assert_eq!(PeerLeft.kind(), FailureKind::PeerLeft);
        assert_eq!(Refused.kind(), FailureKind::Refused);
        assert_eq!(BadTicket.kind(), FailureKind::Unresponsive);

        let mut c = RoomClient::new();
        assert_eq!(
            c.on_http_refused(429, r#"{"error":"rate_limited","retry_after":60}"#),
            vec![Action::Notify(Event::Failed(RateLimited {
                retry_after_secs: 60
            }))]
        );
        assert!(c.is_finished());
    }

    // ---- ステートマシン ----

    const ROOM_OFFER: &str = r#"{"t":"room","role":"host","expires_at":1790000600}"#;
    const ROOM_ANSWER: &str = r#"{"t":"room","role":"guest","expires_at":1790000300}"#;

    fn send(text: &str) -> Action {
        Action::Send(text.to_owned())
    }

    fn joined(c: &mut RoomClient, room: &str, at: u64) -> Vec<Action> {
        assert_eq!(c.on_upgraded(t(at)), vec![]);
        c.on_text(t(at + 20), room)
    }

    #[test]
    fn offer_side_happy_path() {
        let mut c = RoomClient::new();
        assert_eq!(c.state(), State::Connecting);
        assert_eq!(
            joined(&mut c, ROOM_OFFER, 0),
            vec![Action::Notify(Event::Joined {
                role: Role::Offer,
                expires_at: 1_790_000_600
            })]
        );
        assert_eq!(c.role(), Some(Role::Offer));
        assert_eq!(c.state(), State::NeedOwnCode);
        assert_eq!(c.on_text(t(100), PONG_TEXT), vec![]);
        assert_eq!(
            c.submit_code("OfferCode"),
            Ok(vec![send(r#"{"t":"offer","code":"OfferCode"}"#)])
        );
        assert_eq!(c.state(), State::AwaitMatched);
        assert_eq!(
            c.on_text(t(5_000), r#"{"t":"matched","expires_at":1790000065}"#),
            vec![Action::Notify(Event::Matched {
                expires_at: 1_790_000_065
            })]
        );
        assert_eq!(c.state(), State::AwaitPeerCode);
        assert_eq!(
            c.on_text(t(9_000), r#"{"t":"peer","code":"AnswerCode"}"#),
            vec![
                Action::Notify(Event::PeerCode("AnswerCode".into())),
                Action::Notify(Event::Done)
            ]
        );
        assert_eq!(c.state(), State::Done);
        assert_eq!(c.on_close(1000), vec![]);
        assert_eq!(c.poll(t(10_000_000)), vec![]);
        assert_eq!(c.next_wake(), None);
    }

    #[test]
    fn answer_side_happy_path() {
        let mut c = RoomClient::new();
        assert_eq!(
            joined(&mut c, ROOM_ANSWER, 0),
            vec![Action::Notify(Event::Joined {
                role: Role::Answer,
                expires_at: 1_790_000_300
            })]
        );
        assert_eq!(c.role(), Some(Role::Answer));
        assert_eq!(c.state(), State::AwaitPeerCode);
        assert_eq!(
            c.submit_code("TooEarly"),
            Err(SubmitError::NotNow(State::AwaitPeerCode))
        );
        assert_eq!(
            c.on_text(t(30), r#"{"t":"peer","code":"OfferCode"}"#),
            vec![Action::Notify(Event::PeerCode("OfferCode".into()))]
        );
        assert_eq!(c.state(), State::NeedOwnCode);
        assert_eq!(
            c.submit_code("bad code"),
            Err(SubmitError::Code(CodeTextError::BadChar { pos: 3 }))
        );
        assert_eq!(c.state(), State::NeedOwnCode, "断ったときは状態を変えない");
        assert_eq!(
            c.submit_code("AnswerCode"),
            Ok(vec![send(r#"{"t":"answer","code":"AnswerCode"}"#)])
        );
        assert_eq!(c.state(), State::AwaitClose);
        assert_eq!(
            c.submit_code("AnswerCode"),
            Err(SubmitError::NotNow(State::AwaitClose))
        );
        assert_eq!(c.on_close(1000), vec![Action::Notify(Event::Done)]);
        assert_eq!(c.state(), State::Done);
    }

    fn match_room() -> RoomName {
        RoomName::parse("Synth-Room").unwrap()
    }

    /// 対戦のルームで成立して保っている始める側・返す側。
    fn held(role: Role) -> RoomClient {
        let mut c = RoomClient::for_room(&match_room());
        match role {
            Role::Offer => {
                joined(&mut c, ROOM_OFFER, 0);
                c.submit_code("OfferCode").unwrap();
                c.on_text(t(5_000), r#"{"t":"matched","expires_at":1790000065}"#);
                c.on_text(t(9_000), r#"{"t":"peer","code":"AnswerCode"}"#);
            }
            Role::Answer => {
                joined(&mut c, ROOM_ANSWER, 0);
                c.on_text(t(30), r#"{"t":"peer","code":"OfferCode"}"#);
                c.submit_code("AnswerCode").unwrap();
                c.poll(t(40));
            }
        }
        assert_eq!(c.state(), State::Holding, "{role:?}");
        c
    }

    #[test]
    fn a_match_room_holds_after_the_trade_and_keeps_only_the_ping() {
        // 始める側: peerを受けたらHeldで閉じずに保つ。期限は持たずpingの時計だけが残る。
        let mut c = RoomClient::for_room(&match_room());
        joined(&mut c, ROOM_OFFER, 0);
        c.submit_code("OfferCode").unwrap();
        c.on_text(t(5_000), r#"{"t":"matched","expires_at":1790000065}"#);
        assert_eq!(
            c.on_text(t(9_000), r#"{"t":"peer","code":"AnswerCode"}"#),
            vec![
                Action::Notify(Event::PeerCode("AnswerCode".into())),
                Action::Notify(Event::Held)
            ]
        );
        assert_eq!(c.state(), State::Holding);
        assert!(!c.is_finished());
        assert_eq!(c.deadline_at(), None);
        assert_eq!(c.next_wake(), Some(30_000));
        // 1時間保っても30秒ごとのpingのほかに何もしない（answer・ルームの期限で切らない）。
        let mut pings = 0;
        for s in 1..=120u64 {
            let at = 20 + s * PING_INTERVAL_MS;
            for a in c.poll(t(at)) {
                assert_eq!(a, send(PING_TEXT), "{at}");
                pings += 1;
            }
            assert_eq!(c.on_text(t(at + 10), PONG_TEXT), vec![]);
        }
        assert_eq!(pings, 120);
        assert_eq!(c.state(), State::Holding);
        assert_eq!(
            c.submit_code("Again"),
            Err(SubmitError::NotNow(State::Holding))
        );

        // 返す側: answerを渡し終えた次のpollでHeld。閉じる番号を待たずanswerの期限（NoClose）でも切らない。
        let mut c = RoomClient::for_room(&match_room());
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(1_000), r#"{"t":"peer","code":"OfferCode"}"#);
        assert_eq!(
            c.submit_code("AnswerCode"),
            Ok(vec![send(r#"{"t":"answer","code":"AnswerCode"}"#)])
        );
        assert_eq!(c.state(), State::AwaitClose);
        assert_eq!(c.poll(t(1_010)), vec![Action::Notify(Event::Held)]);
        assert_eq!(c.state(), State::Holding);
        for at in [
            1_000 + ANSWER_TTL_MS + DEADLINE_GRACE_MS,
            20 + ROOM_TTL_MS + DEADLINE_GRACE_MS,
        ] {
            assert_eq!(c.poll(t(at)), vec![send(PING_TEXT)], "{at}");
            assert_eq!(c.on_text(t(at + 10), PONG_TEXT), vec![]);
        }
        assert_eq!(c.state(), State::Holding);
    }

    #[test]
    fn a_held_room_ends_quietly_on_any_close_error_or_silence() {
        for role in [Role::Offer, Role::Answer] {
            for n in [1000, 1006, 4400, 4408, 4410, 4999] {
                let mut c = held(role);
                assert_eq!(
                    c.on_close(n),
                    vec![Action::Notify(Event::Done)],
                    "{role:?} {n}"
                );
                assert_eq!(c.state(), State::Done);
                assert_eq!(c.on_close(n), vec![], "二重に知らせる");
            }
            for e in ["expired", "peer_left", "bad_message", "zzz"] {
                let mut c = held(role);
                assert_eq!(
                    c.on_text(t(50), &format!(r#"{{"t":"error","code":"{e}"}}"#)),
                    vec![Action::Notify(Event::Done)],
                    "{role:?} {e}"
                );
            }
            let mut c = held(role);
            assert_eq!(c.on_transport_failed(), vec![Action::Notify(Event::Done)]);
            // pongが途絶えても「応答しません」にしない。
            let mut c = held(role);
            let mut out = Vec::new();
            for s in 1..=4u64 {
                out.extend(c.poll(t(20 + s * PING_INTERVAL_MS)));
            }
            assert!(!has_failure(&out), "{out:?}");
            assert_eq!(out.last(), Some(&Action::Notify(Event::Done)), "{role:?}");
            assert!(c.is_finished());
        }
    }

    #[test]
    fn only_rooms_that_are_not_watch_rooms_hold() {
        let watch = match_room().spectate_of();
        let mut c = RoomClient::for_room(&watch);
        joined(&mut c, ROOM_OFFER, 0);
        c.submit_code("OfferCode").unwrap();
        c.on_text(t(5_000), r#"{"t":"matched","expires_at":1790000065}"#);
        assert_eq!(
            c.on_text(t(9_000), r#"{"t":"peer","code":"AnswerCode"}"#),
            vec![
                Action::Notify(Event::PeerCode("AnswerCode".into())),
                Action::Notify(Event::Done)
            ],
            "観戦用のルームは保たない"
        );
        let mut c = RoomClient::for_room(&watch);
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(30), r#"{"t":"peer","code":"OfferCode"}"#);
        c.submit_code("AnswerCode").unwrap();
        assert_eq!(c.poll(t(40)), vec![]);
        assert_eq!(c.state(), State::AwaitClose);
        // 割り当てのルーム（ランダムマッチのq.）は保つ。
        let assigned = RoomName::parse_server_rules(&format!("q.{}", "0a".repeat(15))).unwrap();
        let mut c = RoomClient::for_room(&assigned);
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(30), r#"{"t":"peer","code":"OfferCode"}"#);
        c.submit_code("AnswerCode").unwrap();
        assert_eq!(c.poll(t(40)), vec![Action::Notify(Event::Held)]);
    }

    #[test]
    fn an_answer_that_could_not_be_sent_fails_before_holding() {
        // answerを送れなかったときは通信の処理がpollより先にon_transport_failedを呼ぶ。
        let mut c = RoomClient::for_room(&match_room());
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(30), r#"{"t":"peer","code":"OfferCode"}"#);
        c.submit_code("AnswerCode").unwrap();
        assert_eq!(c.on_transport_failed(), failed(Why::Lost));
        assert_eq!(c.poll(t(40)), vec![]);
        // answerの後pollより先にサーバーが相手の抜けを知らせたら成立にしない。
        let mut c = RoomClient::for_room(&match_room());
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(30), r#"{"t":"peer","code":"OfferCode"}"#);
        c.submit_code("AnswerCode").unwrap();
        assert_eq!(
            c.on_text(t(35), r#"{"t":"error","code":"peer_left"}"#),
            vec![Action::Notify(Event::Failed(Failure::PeerLeft))]
        );
    }

    #[test]
    fn each_side_keeps_the_ticket_of_its_own_message_after_the_room_is_done() {
        let ticket = SeatTicket::parse(TICKET).unwrap();
        let matched = format!(r#"{{"t":"matched","expires_at":1790000065,"ticket":"{TICKET}"}}"#);
        let peer = format!(r#"{{"t":"peer","code":"OfferCode","ticket":"{TICKET}"}}"#);

        let mut host = RoomClient::new();
        joined(&mut host, ROOM_OFFER, 0);
        host.submit_code("OfferCode").unwrap();
        assert_eq!(host.ticket(), None);
        host.on_text(t(100), &matched);
        assert_eq!(host.ticket(), Some(ticket));
        // 始める側へ来るpeerのチケットは読まない（サーバーは載せない）。
        host.on_text(
            t(200),
            r#"{"t":"peer","code":"AnswerCode","ticket":"0.0.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#,
        );
        assert_eq!(host.state(), State::Done);
        assert_eq!(host.ticket(), Some(ticket), "手順を終えた後も残す");

        let mut guest = RoomClient::new();
        joined(&mut guest, ROOM_ANSWER, 0);
        assert_eq!(
            guest.on_text(t(30), &peer),
            vec![Action::Notify(Event::PeerCode("OfferCode".into()))]
        );
        assert_eq!(guest.ticket(), Some(ticket));
        guest.submit_code("AnswerCode").unwrap();
        guest.on_close(1000);
        assert_eq!(guest.ticket(), Some(ticket));

        // チケットの無い成立（古いサーバー・鍵の無いサーバー）も手順は同じに進む。
        let mut plain = RoomClient::new();
        joined(&mut plain, ROOM_ANSWER, 0);
        plain.on_text(t(30), r#"{"t":"peer","code":"OfferCode","ticket":"bad"}"#);
        assert_eq!(plain.state(), State::NeedOwnCode);
        assert_eq!(plain.ticket(), None);
    }

    #[test]
    fn a_refusing_offer_side_stays_until_the_room_expires_and_only_the_offer_side_may_refuse() {
        let mut c = RoomClient::new();
        assert_eq!(
            c.refuse(),
            Err(SubmitError::NotNow(State::Connecting)),
            "room の前"
        );
        joined(&mut c, ROOM_OFFER, 0);
        assert_eq!(c.refuse(), Ok(vec![send(REFUSE_TEXT)]));
        assert_eq!(REFUSE_TEXT, r#"{"t":"refuse"}"#);
        assert_eq!(c.state(), State::Refusing);
        assert_eq!(c.refuse(), Err(SubmitError::NotNow(State::Refusing)));
        assert_eq!(
            c.submit_code("OfferCode"),
            Err(SubmitError::NotNow(State::Refusing)),
            "断った後に offer を出さない"
        );
        // pingは続け、pongでは終わらない。
        assert_eq!(c.poll(t(PING_INTERVAL_MS)), vec![send(PING_TEXT)]);
        assert_eq!(c.on_text(t(PING_INTERVAL_MS + 10), PONG_TEXT), vec![]);
        assert!(!c.is_finished());
        // ルームの期限の4408で終わる（配信側は入り直してまた断る）。
        assert_eq!(
            c.on_close(4408),
            vec![Action::Notify(Event::Failed(Failure::Expired))]
        );
        // こちらの予備の期限でも同じ。
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        c.refuse().unwrap();
        let d = c.deadline_at().expect("ルームの期限を持たない");
        assert!(d >= 600_000, "{d}");
        assert_eq!(
            c.poll(t(d)),
            vec![Action::Notify(Event::Failed(Failure::Expired))]
        );
        // 返す側（観戦者）は断れない。
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        assert_eq!(c.refuse(), Err(SubmitError::NotNow(State::AwaitPeerCode)));
        c.on_text(t(30), r#"{"t":"peer","code":"OfferCode"}"#);
        assert_eq!(c.refuse(), Err(SubmitError::NotNow(State::NeedOwnCode)));
    }

    #[test]
    fn a_spectator_closed_with_4403_or_the_refused_error_fails_as_refused() {
        for by_error in [false, true] {
            let mut c = RoomClient::new();
            joined(&mut c, ROOM_ANSWER, 0);
            let got = if by_error {
                c.on_text(t(30), r#"{"t":"error","code":"refused"}"#)
            } else {
                c.on_close(4403)
            };
            assert_eq!(
                got,
                vec![Action::Notify(Event::Failed(Failure::Refused))],
                "{by_error}"
            );
            assert_eq!(c.on_close(4403), vec![], "二重に知らせる");
        }
    }

    fn failed(why: Why) -> Vec<Action> {
        vec![Action::Notify(Event::Failed(Failure::Unresponsive(why)))]
    }

    #[test]
    fn out_of_order_messages_fail() {
        let peer = r#"{"t":"peer","code":"AAAA"}"#;
        let matched = r#"{"t":"matched","expires_at":1790000060}"#;
        let ooo = |k| failed(Why::OutOfOrder(k));

        for (msg, k) in [
            (peer, MsgKind::Peer),
            (matched, MsgKind::Matched),
            (ROOM_OFFER, MsgKind::Room),
        ] {
            let mut c = RoomClient::new();
            joined(&mut c, ROOM_OFFER, 0);
            assert_eq!(c.on_text(t(50), msg), ooo(k), "offer の前に {msg}");
        }
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        c.submit_code("OfferCode").unwrap();
        assert_eq!(c.on_text(t(50), peer), ooo(MsgKind::Peer));
        assert!(c.is_finished());
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        c.submit_code("OfferCode").unwrap();
        c.on_text(t(50), matched);
        assert_eq!(c.on_text(t(60), matched), ooo(MsgKind::Matched));

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        assert_eq!(c.on_text(t(50), matched), ooo(MsgKind::Matched));
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(50), peer);
        assert_eq!(c.on_text(t(60), peer), ooo(MsgKind::Peer));
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        assert_eq!(c.on_text(t(50), ROOM_ANSWER), ooo(MsgKind::Room));
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(50), peer);
        c.submit_code("AnswerCode").unwrap();
        assert_eq!(c.on_text(t(60), peer), ooo(MsgKind::Peer));

        let mut c = RoomClient::new();
        c.on_upgraded(t(0));
        assert_eq!(c.on_text(t(10), peer), ooo(MsgKind::Peer));
        let mut c = RoomClient::new();
        assert_eq!(c.on_text(t(0), ROOM_OFFER), ooo(MsgKind::Room));

        let mut c = RoomClient::new();
        c.on_upgraded(t(0));
        assert_eq!(
            c.submit_code("AAAA"),
            Err(SubmitError::NotNow(State::AwaitRoom))
        );

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        assert_eq!(
            c.on_text(t(30), r#"{"t":"room""#),
            failed(Why::BadMessage(MsgError::NotJson { at: 11 }))
        );
        assert_eq!(c.on_text(t(40), ROOM_OFFER), vec![]);
        assert_eq!(c.on_close(4408), vec![]);
    }

    #[test]
    fn closes_and_errors() {
        let states: [fn() -> RoomClient; 4] = [
            || {
                let mut c = RoomClient::new();
                joined(&mut c, ROOM_OFFER, 0);
                c
            },
            || {
                let mut c = RoomClient::new();
                joined(&mut c, ROOM_OFFER, 0);
                c.submit_code("OfferCode").unwrap();
                c
            },
            || {
                let mut c = RoomClient::new();
                joined(&mut c, ROOM_ANSWER, 0);
                c
            },
            || {
                let mut c = RoomClient::new();
                joined(&mut c, ROOM_ANSWER, 0);
                c.on_text(t(30), r#"{"t":"peer","code":"AAAA"}"#);
                c
            },
        ];
        for make in states {
            let fail = |f: Failure| vec![Action::Notify(Event::Failed(f))];
            assert_eq!(make().on_close(4408), fail(Failure::Expired));
            assert_eq!(make().on_close(4410), fail(Failure::PeerLeft));
            for n in [1000, 1001, 1006, 4400, 4413, 4422, 4999] {
                assert_eq!(make().on_close(n), failed(Why::Closed(n)), "close {n}");
            }
            assert_eq!(make().on_transport_failed(), failed(Why::Lost));
            assert_eq!(
                make().on_text(t(40), r#"{"t":"error","code":"expired"}"#),
                fail(Failure::Expired)
            );
            assert_eq!(
                make().on_text(t(40), r#"{"t":"error","code":"peer_left"}"#),
                fail(Failure::PeerLeft)
            );
            assert_eq!(
                make().on_text(t(40), r#"{"t":"error","code":"bad_code"}"#),
                failed(Why::ErrorMessage(Some(CloseCode::BadCode)))
            );
            assert_eq!(
                make().on_text(t(40), r#"{"t":"error","code":"done"}"#),
                failed(Why::ErrorMessage(Some(CloseCode::Done)))
            );
            assert_eq!(
                make().on_text(t(40), r#"{"t":"error","code":"zzz"}"#),
                failed(Why::ErrorMessage(None))
            );
            // errorの文の後に閉じる番号が来てももう何も知らせない（先に来た方で終える）。
            let mut c = make();
            c.on_text(t(40), r#"{"t":"error","code":"expired"}"#);
            assert_eq!(c.on_close(4408), vec![]);
        }
        let mut c = RoomClient::new();
        assert_eq!(c.on_transport_failed(), failed(Why::NoAnswer));
    }

    #[test]
    fn no_room_after_upgrade_times_out() {
        let mut c = RoomClient::new();
        c.on_upgraded(t(1_000));
        assert_eq!(c.deadline_at(), Some(1_000 + ROOM_GREETING_TIMEOUT_MS));
        assert_eq!(c.poll(t(1_000 + ROOM_GREETING_TIMEOUT_MS - 1)), vec![]);
        assert_eq!(
            c.poll(t(1_000 + ROOM_GREETING_TIMEOUT_MS)),
            failed(Why::NoRoom)
        );
        assert!(c.is_finished());
        let mut c = RoomClient::new();
        assert_eq!(c.poll(t(u64::MAX / 2)), vec![]);
        assert_eq!(c.next_wake(), None);
    }

    fn has_failure(a: &[Action]) -> bool {
        a.iter()
            .any(|x| matches!(x, Action::Notify(Event::Failed(_))))
    }

    fn expired() -> Vec<Action> {
        vec![Action::Notify(Event::Failed(Failure::Expired))]
    }

    #[test]
    fn room_deadline() {
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        let d = 20 + 600_000 + DEADLINE_GRACE_MS;
        assert_eq!(c.deadline_at(), Some(d));
        c.submit_code("OfferCode").unwrap();
        assert_eq!(c.deadline_at(), Some(d), "offer では期限は変わらない");
        assert!(!has_failure(&c.poll(t(d - 1))), "期限の 1 ms 前は切れない");
        assert_eq!(c.poll(t(d)), expired());

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        assert_eq!(c.deadline_at(), Some(20 + 300_000 + DEADLINE_GRACE_MS));

        // 時計のずれ: 利用者の時計が1時間進んでいる（残りが負）・1時間遅れている（残りが上限より長い）。
        for skew in [3_600i64, -3_600] {
            let mut c = RoomClient::new();
            c.on_upgraded(t(0));
            let now = Now {
                ms: 20,
                unix_secs: (1_790_000_000i64 + skew) as u64,
            };
            c.on_text(now, ROOM_OFFER);
            assert_eq!(
                c.deadline_at(),
                Some(20 + ROOM_TTL_MS + DEADLINE_GRACE_MS),
                "skew {skew}"
            );
        }
        // 残りがちょうど0も上限で代える（サーバーは期限の過ぎたルームに入れないので0は時計のずれを示す）。
        let mut c = RoomClient::new();
        c.on_upgraded(t(0));
        c.on_text(
            Now {
                ms: 20,
                unix_secs: 1_790_000_600,
            },
            ROOM_OFFER,
        );
        assert_eq!(c.deadline_at(), Some(20 + ROOM_TTL_MS + DEADLINE_GRACE_MS));
    }

    #[test]
    fn answer_deadline() {
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        c.submit_code("OfferCode").unwrap();
        c.on_text(t(5_000), r#"{"t":"matched","expires_at":1790000065}"#);
        let d = 5_000 + 60_000 + DEADLINE_GRACE_MS;
        assert_eq!(c.deadline_at(), Some(d));
        assert!(!has_failure(&c.poll(t(d - 1))), "期限の 1 ms 前は切れない");
        assert_eq!(c.poll(t(d)), expired());

        // 始める側: ルームの残りがanswerの期限より短ければそちら（時計のずれでmatchedの値が使えず上限60秒、ルームの残りは30秒）。
        let mut c = RoomClient::new();
        c.on_upgraded(t(0));
        c.on_text(
            Now {
                ms: 20,
                unix_secs: 1_790_000_570,
            },
            ROOM_OFFER,
        );
        c.submit_code("OfferCode").unwrap();
        c.on_text(
            Now {
                ms: 100,
                unix_secs: 1_790_009_000,
            },
            r#"{"t":"matched","expires_at":1790000600}"#,
        );
        assert_eq!(c.deadline_at(), Some(20 + 30_000 + DEADLINE_GRACE_MS));

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(1_000), r#"{"t":"peer","code":"AAAA"}"#);
        let d = 1_000 + ANSWER_TTL_MS + DEADLINE_GRACE_MS;
        assert_eq!(c.deadline_at(), Some(d));
        assert!(!has_failure(&c.poll(t(d - 1))), "期限の 1 ms 前は切れない");
        assert_eq!(c.poll(t(d)), expired());

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(1_000), r#"{"t":"peer","code":"AAAA"}"#);
        c.submit_code("AnswerCode").unwrap();
        assert_eq!(c.poll(t(d)), failed(Why::NoClose));

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_text(t(290_000), r#"{"t":"peer","code":"AAAA"}"#);
        assert_eq!(c.deadline_at(), Some(20 + 300_000 + DEADLINE_GRACE_MS));
    }

    #[test]
    fn ping_every_30_seconds() {
        let rules = parse_flat(RULES).unwrap();
        let fixture_ping = match rules.iter().find(|(k, _)| k == "ping_text") {
            Some((_, JsonValue::Str(s))) => s.to_string(),
            other => panic!("{other:?}"),
        };
        let mut c = RoomClient::new();
        assert_eq!(c.next_ping_at(), None);
        joined(&mut c, ROOM_OFFER, 1_000);
        assert_eq!(c.next_ping_at(), Some(31_000));
        assert_eq!(c.next_wake(), Some(31_000));
        assert_eq!(c.poll(t(30_999)), vec![]);
        assert_eq!(c.poll(t(31_000)), vec![send(&fixture_ping)]);
        assert_eq!(c.poll(t(31_001)), vec![]);
        assert_eq!(c.next_ping_at(), Some(61_000));
        assert_eq!(c.poll(t(100_000)), vec![send(PING_TEXT)]);
        assert_eq!(c.poll(t(100_001)), vec![]);
        assert_eq!(c.next_ping_at(), Some(130_000));
        let mut c = RoomClient::new();
        c.on_upgraded(t(0));
        assert_eq!(c.next_wake(), Some(ROOM_GREETING_TIMEOUT_MS));
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        c.on_close(4410);
        assert_eq!(c.next_ping_at(), None);
        assert_eq!(c.poll(t(1_000_000)), vec![]);
    }

    #[test]
    fn silence_after_ping_is_unresponsive() {
        assert_eq!(PONG_TIMEOUT_MS, 90_000, "§5-2 の (1): 3 周 = 90 秒");
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        assert_eq!(c.silence_deadline_at(), None, "ping の前は数えない");
        assert_eq!(c.poll(t(30_000)), vec![send(PING_TEXT)]);
        assert_eq!(c.silence_deadline_at(), Some(120_000));
        assert_eq!(c.poll(t(60_000)), vec![send(PING_TEXT)]);
        assert_eq!(c.poll(t(90_000)), vec![send(PING_TEXT)]);
        assert_eq!(
            c.silence_deadline_at(),
            Some(120_000),
            "続けて送った ping で数え直さない"
        );
        assert_eq!(c.next_wake(), Some(120_000));
        assert_eq!(c.poll(t(119_999)), vec![]);
        assert_eq!(c.poll(t(120_000)), failed(Why::NoPong));
        assert_eq!(c.next_wake(), None);
        assert_eq!(c.poll(t(200_000)), vec![]);

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        c.poll(t(30_000));
        assert_eq!(c.on_text(t(30_050), PONG_TEXT), vec![]);
        assert_eq!(c.silence_deadline_at(), None);
        assert_eq!(c.poll(t(60_000)), vec![send(PING_TEXT)]);
        assert_eq!(c.silence_deadline_at(), Some(150_000));
        assert!(!has_failure(&c.poll(t(120_000))));
        assert_eq!(c.poll(t(150_000)), failed(Why::NoPong));

        let mut c = RoomClient::new();
        joined(&mut c, ROOM_OFFER, 0);
        c.submit_code("OfferCode").unwrap();
        c.poll(t(30_000));
        c.on_text(t(40_000), r#"{"t":"matched","expires_at":1790000100}"#);
        assert_eq!(c.silence_deadline_at(), None);
        assert_eq!(c.poll(t(60_000)), vec![send(PING_TEXT)]);
        assert_eq!(c.silence_deadline_at(), Some(150_000));

        // answerの期限がNoPongより先に来る（期限が優先）。
        let mut c = RoomClient::new();
        joined(&mut c, ROOM_ANSWER, 0);
        c.poll(t(30_000));
        c.on_text(t(30_010), r#"{"t":"peer","code":"AAAA"}"#);
        c.submit_code("AnswerCode").unwrap();
        c.poll(t(60_010));
        let d = 30_010 + ANSWER_TTL_MS + DEADLINE_GRACE_MS;
        assert_eq!(c.silence_deadline_at(), Some(60_010 + PONG_TIMEOUT_MS));
        assert_eq!(c.poll(t(d)), failed(Why::NoClose));
    }

    // ---- 観戦用のルームの名（インターネット越しの観戦） ----

    #[test]
    fn spectate_room_names_follow_the_derivation() {
        let of = |s: &str| RoomName::parse_server_rules(s).unwrap().spectate_of();
        assert_eq!(of("room-1").as_str(), "w.2cb5ddf410150fdf");
        assert_eq!(of("a").as_str(), "w.ca978112ca1bbdca");
        assert_eq!(of(&"x".repeat(32)).as_str(), "w.c62e4615bd39e222");
        for same in ["Room-1", "ROOM-1", "rOoM-1", " room-1 ", "room-1  "] {
            assert_eq!(of(same), of("room-1"), "{same:?}");
        }
        for s in ["room-1", "a", "Z9_-"] {
            assert_eq!(of(s).as_str().len(), SPECTATE_ROOM_NAME_LEN, "{s}");
        }
        assert_ne!(of("room-1"), of("room-2"));
        assert_ne!(of("room-1"), of("room_1"));
    }

    #[test]
    fn spectate_room_names_cannot_be_typed_and_pass_the_server_rules() {
        let own = RoomName::parse("Room-1").unwrap();
        let spec = own.spectate_of();
        assert_ne!(spec, own);
        assert_eq!(
            RoomName::parse(spec.as_str()),
            Err(RoomNameError::ReservedPrefix)
        );
        assert_eq!(RoomName::parse_server_rules(spec.as_str()), Ok(spec));
        assert_eq!(spec.path(), "/v0/rooms/w.2cb5ddf410150fdf");
        assert!(!spec.path().contains('%'));
        assert_eq!(
            spec.url("wss://example.workers.dev/"),
            "wss://example.workers.dev/v0/rooms/w.2cb5ddf410150fdf"
        );
        let longest = RoomName::parse(&"A".repeat(ROOM_NAME_MAX))
            .unwrap()
            .spectate_of();
        assert_eq!(longest.as_str().len(), 18);
        assert!(RoomName::parse_server_rules(longest.as_str()).is_ok());
        // `w.`で始まる名は打てない。`w`だけで始まる名は打てるが、観戦用の名とは別の文字列。
        assert_ne!(
            RoomName::parse("w2cb5ddf410150fdf").unwrap().as_str(),
            spec.as_str()
        );
    }
}
