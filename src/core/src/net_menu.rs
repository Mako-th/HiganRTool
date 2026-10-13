//! ゲーム内メニューの状態機械と、画面に出す固定の文の表（pure。確保しない）。入口は `ui_entry`（Match の「妖怪 対 妖怪」の決定を借りる）。
//!
//! - 状態機械 `Menu`: ゲームのスレッドで 1 tick に 1 回進める（カーソルの反応を遅らせないため）。入力はゲームへ流さない（呼び手が 0 を feed する）。
//!   自分の語（`0x0042BE40` / poll の席 0 の値）の押した瞬間で動く。閉じるときはローカルの語が 0 になるまで 0 を流し続けてからゲームへ返す
//!   （押したままのキーをゲームが「押した瞬間」と読まないため）。
//! - 見張りのスレッドから来る状態 `Note`: 文字列でなく「種類（`Event` / `CodeState`）＋数」。メニューが下の固定の文から文を選ぶので、画面に出る文字は
//!   全部この表の文字と数字・ASCII だけになる（文字の表 `overlay::ui_glyphs` に無い文字を出さない）。相手の名前だけは表に無い文字がありうるので、書き手（見張り）が
//!   `sanitize_name` で `?` に直してから渡す。
//! - 描く行 `LineBuf`: スタックの固定の配列（確保しない）。
//!
//! 文言の規律: この道具が何をどう描いたか・画面から読めない数値（文字数・時間・席・形）は書く。ゲームの仕様は説明しない。状態の文には次にすることを 1 つ添える。
//! adonis の語（「同期ズレ」）に合わせる。
#![forbid(unsafe_code)]

use crate::code::DEFAULT_TTL_SECS;
use crate::delay_pick::DelayChoice;
use crate::handshake::FailReason;
use crate::input_model::keys;
use crate::lang::{Lang, Tx};
use crate::link::Restarts;
use crate::match_gate::{MATCH_WAIT_MS, NoMatch};
use crate::menu_model::{CANCEL_BITS, DECIDE_BITS};
use crate::mode_pick::{MAX_PICK_DELAY, Pick, Shape};
use crate::nat::FailKind;
use crate::player_name::{NameBuf, NameInput, NameWhy};
use crate::room_input::{Edit, RoomBuf, RoomInput};
use crate::spectate_room::{JOINER_GIVE_UP_MS, JoinerFailure, JoinerPhase, JoinerStatus};
use crate::ui_layout::RowStyle;
use crate::update::BUILD_NUMBER;
use crate::watch_list::{ListStatus, WATCH_NAME_MAX, WatchRow, WatchSnapshot};
use crate::wire::{Mode, PROTO_VER, Seat, SeatWish};

/// 文の定数と、その全部を並べた表 `TEXTS` を 1 つの書き方から作る（定数を足して表に入れ忘れることが無い）。
/// 1 項目は日本語と英語の組（`lang::Tx`）。語順が違うので、数や名前を挟む文は前後の片方が空のことがある。
macro_rules! texts {
    ($($(#[$m:meta])* $name:ident = $ja:expr, $en:expr;)*) => {
        $($(#[$m])* pub const $name: Tx = Tx { ja: $ja, en: $en };)*
        /// 画面に出す固定の文の全部（両方の言語）。ここに無い文字は文字の表に無く、`?` で描かれる。
        pub const TEXTS: &[Tx] = &[$($name),*];
    };
}

texts! {
    T_GROUP_PLAY = "対戦", "Play";
    T_GROUP_WATCH = "観戦", "Watch";
    T_GROUP_CODE = "接続コードで対戦", "Play with a code";
    T_GROUP_SETTINGS = "設定", "Settings";
    T_CREATE = "コード作成", "Create code";
    T_PASTE = "コード入力", "Paste code";
    T_PASTE_REPLY = "返答コード入力", "Paste reply code";
    /// 設定の 2 行「同期方式 ◀ rollback ▶」（rollback が既定・← → で delay）と「ディレイ ◀ auto ▶」（並びは auto, 0F, 1F, …, 8F）。
    T_FORMAT = "同期方式", "Netcode";
    T_DELAY_ROW = "ディレイ", "Delay";
    /// フレーム数の値の後ろに付ける 1 文字（ディレイの値と左下の行）。日英で同じ。
    T_FRAMES_MARK = "F", "F";
    /// 「同期方式」の値。マッチ中の行の語（`T_HUD_*`）と同じ ASCII。
    T_FORMAT_ROLLBACK = "rollback", "rollback";
    T_FORMAT_DELAY = "delay", "delay";
    /// キャラセレクトの左下の行の形式の語: この組は rollback へ替えられない（片方が rollback ができない）ので、Ctrl+Tab を押しても効かない。画面から読み取れない事実なので delay の語に添えて出す。
    T_FORMAT_DELAY_ONLY = "delay only", "delay only";
    T_DELAY_AUTO = "auto", "auto";
    /// 設定の「言語」の行。値は言語ごとの自分の名前（`T_LANG_JA` / `T_LANG_EN`）で、どちらの言語でも同じ文字。
    T_LANG = "言語", "Language";
    /// 「ラベル　◀ 値 ▶」の行のラベルと矢印の間（日本語は全角の空白、英語は同じ幅の半角 2 つ）。
    T_ROW_SEP = "　", "  ";
    T_LANG_JA = "日本語", "日本語";
    T_LANG_EN = "English", "English";
    T_CANCEL = "待機をやめる", "Stop waiting";
    T_BACK = "もどる", "Back";
    T_ARROWS = "◀▶", "◀▶";
    /// つながった後の左上の行（`connected_line`）の文字: 「rollback・delay 1」「delay 方式」。
    T_ROLLBACK = "rollback・delay ", "rollback, delay ";
    T_DELAY = "delay 方式・delay ", "delay mode, delay ";
    T_DELAY_PLAIN = "delay 方式", "delay mode";
    /// キーの行。パッドだけでも操作できるので、キーボードのキーの名前（Z・X）は出さず「決定」「キャンセル」と書く。
    T_KEYS = "↑↓ メニューの移動　← → 値の変更　決定で確定　キャンセルでもどる", "↑↓ Move   ← → Change value   Confirm: Select   Cancel: Back";
    T_KEYS_CONFIRM = "↑↓ メニューの移動　決定で確定　キャンセルでいいえ", "↑↓ Move   Confirm: Select   Cancel: No";
    T_OPENING = "ネットワークの準備をしています。しばらくお待ちください。", "Preparing the network. Please wait.";
    T_LEAVE_TITLE = "対戦から抜けますか？", "Leave the match?";
    T_YES = "はい", "Yes";
    T_NO = "いいえ", "No";
    T_MADE = "クリップボードにコピーしました。相手に送ってください。", "Copied to the clipboard. Send it to your opponent.";
    T_REPLY_MADE = "返答コードをクリップボードにコピーしました。相手に送ってください。", "Copied the reply code. Send it to your opponent.";
    T_LINKING = "接続しています。接続するまでお待ちください。", "Connecting. Please wait.";
    T_GOT = "受け取った文字数 ", "Received ";
    T_CHARS = " 文字", " chars";
    T_CODE_LEN_OPEN = "（", " (";
    T_CODE_LEN_CLOSE = " 文字）", " chars)";
    T_ELLIPSIS = "…", "...";
    T_WAITED = "待機時間 ", "Waited ";
    T_REMAIN = "　有効期限の残り ", "   expires in ";
    T_SHORT = "コードが短すぎます。相手のコードを全部コピーしてから、もう一度貼り直してください。", "Too short. Copy the whole code and paste it again.";
    T_EMPTY = "クリップボードが空です。", "The clipboard was empty.";
    T_EMPTY_NEXT = "相手から届いたコードをコピーしてから、もう一度貼り直してください。", "Copy the code from your opponent, then paste again.";
    T_SAME = "もう読み込んだコードです。接続するまでお待ちください。", "This code was already read. Please wait.";
    T_BROKEN = "相手のコードが欠けているか、間違っているようです。", "The opponent's code is incomplete or wrong.";
    T_BROKEN_NEXT = "全部をコピーし直して、貼り直してください。", "Copy the whole code again and paste it.";
    T_OWN = "貼られたのは自分の接続コードです。", "You pasted your own connection code.";
    T_OWN_NEXT = "相手から届いたコードをコピーしてから、もう一度貼り直してください。", "Copy the code from your opponent, then paste again.";
    T_BOTH_OFFER = "貼られたのは相手が作った接続コードです。", "You pasted a code your opponent created.";
    T_BOTH_OFFER_NEXT = "「待機をやめる」を選んでから、もう一度貼り直してください。", "Choose \"Stop waiting\", then paste it again.";
    T_BOTH_ANSWER = "貼られたのは相手の返答コードです。", "You pasted your opponent's reply code.";
    T_BOTH_ANSWER_NEXT = "先に「コード作成」を選んで、相手に送ってください。", "Choose \"Create code\" first and send it to your opponent.";
    T_EXPIRED_OPEN = "相手のコードは期限（", "The opponent's code has expired (";
    T_EXPIRED_CLOSE = "）が切れています。", ").";
    T_EXPIRED_NEXT = "相手に新しいコードを生成してもらってください。", "Ask your opponent for a new code.";
    T_WRONG_SESSION = "これは前回使用した返答コードです。", "This is the reply code from last time.";
    T_WRONG_SESSION_NEXT = "「コード作成」を選び直して、相手に送ってください。", "Choose \"Create code\" again and send it to your opponent.";
    T_WRONG_KIND = "相手のコードが今回のものと合いません。", "The opponent's code does not match this session.";
    T_WRONG_KIND_NEXT = "相手に今のコードを送り直してもらってください。", "Ask your opponent to send the current code again.";
    T_NO_ROUTE = "今の設定では相手に接続できません。", "Cannot reach the opponent with the current settings.";
    T_NO_ROUTE_NEXT = "HiganRTool.ini の scope を直して、起動し直してください。", "Fix scope in HiganRTool.ini and restart the game.";
    T_INTERNAL = "接続の準備に失敗しました（詳細はログを確認してください）。", "Failed to prepare the connection (details in the log).";
    /// 起動し直さないと直らないとき（準備に失敗した）だけの次にすること。つながりが終わっただけなら、この道具がネットワークの組を片づけて「つなぐ前」へ戻すので `T_AGAIN`。
    T_RESTART = "ゲームを閉じて起動し直してください。", "Close the game and start it again.";
    /// つながりが終わった・成立しないまま閉じた・つながらないまま終わった後の次にすること（ゲーム内のメニューの実行だけ。マッチの外へ戻るとネットワークの組を片づけ、メニューが「つなぐ前」に戻る）。キーの名前は出さない。
    /// ゲームの項目の名前は、どの言語でもゲームの画面の文字のまま書く。
    T_AGAIN = "もう一度接続するときは「妖怪 対 妖怪」を選んでください。", "To connect again, choose 妖怪 対 妖怪 in Match Mode.";
    /// 前のつながり（この起動の中でつながった回）で使ったコードを貼った（`Event::Spent`。次にすることは `T_EXPIRED_NEXT`）。
    T_SPENT = "このコードは前回の接続で使いました。", "This code was used for a previous connection.";
    T_UNREADABLE = "クリップボードが読み込めません。", "Could not read the clipboard text.";
    T_UNREADABLE_NEXT = "もう一度コピー＆ペーストしてください。", "Copy it again, then paste.";
    T_CLIP_WRITE = "クリップボードにコピーできませんでした。", "Could not copy to the clipboard.";
    T_CLIP_WRITE_NEXT = "「コード作成」を選び直してください。", "Choose \"Create code\" again.";
    T_SHAPE_LATE = "同期方式とディレイの設定はこの接続では変わりません。", "Netcode and delay do not change for this connection.";
    T_SHAPE_LATE_NEXT = "設定を変える場合「待機をやめる」を選んでから、もう一度接続してください。", "To change them, choose \"Stop waiting\" and connect again.";
    T_GAVE_UP_OPEN = "待機をやめました（", "Stopped waiting (";
    T_GAVE_UP_CLOSE = "たちました）。", " passed).";
    T_GAVE_UP_NEXT = "「コード作成」を選び直してください。", "Choose \"Create code\" again.";
    T_ENDED = "相手と接続できないまま終わりました。", "Ended without connecting.";
    T_FINISHED = "接続が終わりました。", "The connection has ended.";
    /// 同期ズレ（CHECK の食い違い）でマッチを止めている間の左上の 2 行（`Conn::Desynced`）。次にすること 1 つは「両者のログを送る」で、戻り方は括弧に添える。
    /// 2 行目は左上の幅（ゲームの文字で描く幅は 640 − 2 × 6 = 628 画素）に収まるよう「メニューへ」に縮めた（608 画素）。
    T_DESYNC = "同期がずれたため、試合を終了しました。", "Desync detected. This match was stopped.";
    T_DESYNC_NEXT = "HiganRTool の logs フォルダのログを開発者へ送ってください（Esc でメニューへ）。", "Send the logs in the logs folder to the developer (Esc: menu).";
    T_CANCELLED = "待機をやめました。", "Stopped waiting.";
    T_CANCELLED_NEXT = "もう一度接続するときは「コード作成」か「コード入力」を選んでください。", "To connect again, choose \"Create code\" or \"Paste code\".";
    /// 「相手「名前」のコードを読みました。」。
    T_NAME_OPEN = "相手「", "Read the code from \"";
    T_NAME_CLOSE_CODE = "」のコードを読みました。", "\".";
    T_NAME_CLOSE_REPLY = "」の返答コードを読みました。", "\" (reply).";
    T_CONNECTED_PRE = "接続しました。あなたは ", "Connected. You are ";
    /// 席の後ろに添える（ホスト役の側だけ）。遅れの量はホストが決めるので、どちらが決める側かを画面に出す。
    T_HOST = "（ホスト）", " (host)";
    T_CONNECTED_POST = " です。", ".";
    T_CONNECTED_SHAPE_OPEN = "（", " (";
    T_CONNECTED_SHAPE_CLOSE = "）", ")";
    /// 接続コードで待つ間の左上の行（`corner_is_band`）: 「接続コードで待機中 0:42 残り 29:18」。マッチ中の行と同じ GDI の小さい文字の 1 行。次にすることの 2 行目は付けない（メニューを開けば項目が並ぶ）。
    T_WAITING = "接続コードで待機中 ", "Waiting with code ";
    T_WAITING_REMAIN = " 残り ", "  left ";
    /// 待ちを始めた項目の右に出す印（「ランダムマッチ  待機中 0:42」）。時計は待った時間。
    T_WAITING_MARK = "  待機中 ", "  waiting ";
    T_NEXT_LINKING = "接続するまでお待ちください。", "Please wait until connected.";
    /// つながったが、対戦はまだ成立していない（`Conn::Found`。両者が「妖怪 対 妖怪」を選ぶまで）。閉じるまでの残りは画面から読めない数なので出す。
    T_FOUND = "対戦相手が見つかりました（閉じるまで ", "Opponent found (closes in ";
    T_FOUND_READY = "相手が「妖怪 対 妖怪」を選ぶのを待っています（閉じるまで ", "Waiting for the opponent to start (closes in ";
    T_FOUND_CLOSE = "）", ")";
    T_NEXT_FOUND = "「妖怪 対 妖怪」を選んでください。", "Choose 妖怪 対 妖怪 in Match Mode.";
    T_NEXT_FOUND_READY = "やめるときはキャンセルで抜けてください。", "To quit, press Cancel.";
    /// 成立しないまま時間切れで閉じた（`Conn::Unmatched(NoMatch::TimedOut)`）。長さは `match_gate::MATCH_WAIT_MS` から作る（`push_span`）。
    T_UNMATCHED_OPEN = "対戦が始まらないまま ", "No match started for ";
    T_UNMATCHED_CLOSE = " たったので、接続を閉じました。", ", so the connection was closed.";
    /// 相手に成立のゲートが無いと見て閉じた（`NoMatch::PeerHasNoGate`）。相手が古いバージョンか、`offer` / `answer` でつないだ。キーの名前・内部の語は出さない。
    /// 画面の幅（全角 45 文字ぶん）に近いので「メニューからつないでいない」を「メニューを使っていない」に縮めた（幅は `each_unmatched_reason_has_its_own_line_and_fits` が見る）。
    T_NO_GATE = "相手の HiganRTool が古いか、ゲーム内のメニューを使っていないので接続を閉じました。", "Closed: the opponent's HiganRTool is old or not in the game menu.";
    /// 成立の前に、相手が BYE(取消) で閉じた（`NoMatch::PeerLeft`。相手の 10 分が先に切れた・相手が抜けた）。
    T_PEER_LEFT = "相手が対戦を始めないまま、接続を閉じました。", "The opponent closed the connection before starting.";
    /// 相手の HiganRTool のバージョン（プロトコルのバージョン `wire::PROTO_VER`）が違う（`Conn::OtherVersion`・`Event::OtherVersion`）。2 つのビルド番号は画面から読めないので出す。
    /// 内部の語は出さない。本ログと試験のまとめも同じ文字（`other_version_text`）。
    T_OTHER_VER_OPEN = "相手の HiganRTool のバージョンが違います（自分 ", "The opponent's HiganRTool protocol differs (you ";
    T_OTHER_VER_MID = "・相手 ", ", opponent ";
    T_OTHER_VER_CLOSE = "）。", ").";
    T_OTHER_VER_NEXT = "相手と同じ zip の HiganRTool を使ってください。", "Use the same HiganRTool zip as your opponent.";
    /// 相手の HiganRTool のビルド（`update::BUILD_NUMBER`。配った zip ごとの数）が違う（`Conn::OtherBuild`・`Event::OtherBuild`・`Event::SpectateOtherBuild`）。
    /// プロトコルのバージョンの違いと並べ、2 行目・区切り・閉じは同じ文字を使う。2 つのビルドは画面から読めないので出す（書き方は `push_build`）。
    T_OTHER_BUILD_OPEN = "相手とバージョンが違います（自分 ", "Version mismatch (you ";
    /// 開発用のビルド（ビルド番号 0。配布用の道具を通さずに建てた物）の書き方（`push_build`）。
    T_BUILD_DEV = "開発版", "dev";
    /// マッチ中の行の語（`hud_line`）。全部 ASCII。項目の区切りは空白 2 つ。
    /// 行は上の枠の左（タイマーの手前。`ui_layout::HUD_TEXT_MAX_W`）に収める必要があるが、ゲームの文字（9 画素送り）では 2 つ空けの形が収まらないので、
    /// この行だけ GDI の文字（半角 7 画素。`ui_layout::push_text_plain`）で描く。幅は `every_match_line_fits_before_the_timer` が数える。
    T_HUD_DELAY = "Delay", "Delay";
    T_HUD_PING = "  ping ", "  ping ";
    T_HUD_MS = "ms", "ms";
    T_HUD_REWIND = "  rollback ", "  rollback ";
    /// 遅れの推奨値（`delay_pick::recommend`。RTT から決まる、画面から読めない数）を、今の遅れの値の横に括弧で添える。「推奨」の文字は出さない。RTT が測れていなければ括弧ごと出さない。
    T_HUD_RECOMMEND_OPEN = " (", " (";
    T_HUD_RECOMMEND_CLOSE = ")", ")";
    /// auto のとき、推奨値の代わりに括弧の中へ出す 1 文字（「Delay 2F (A)」）。auto の間は遅れの値そのものが推奨値なので、括弧の推奨値と並べない。
    T_HUD_AUTO_MARK = "A", "A";
    /// キャラセレクトの左下の行の先頭に出す「形式・」の区切り（`hud_line` の `format`。マッチ中の行にはこれは付かない）。
    T_HUD_FORMAT_SEP = "・", ", ";
    /// 対戦中の上の枠の左右の名前の帯の席の文字（`name_band_line`。左は「1P 名前」、右は「名前 2P」）。
    T_HUD_SEAT_1 = "1P", "1P";
    T_HUD_SEAT_2 = "2P", "2P";
    /// 観戦者のマッチ中の通信の状態の行の語（`watch_hud_line`: 「ping 14ms Delay 1F Rollback 2 Watch delay 3」）。前半の「ping・Delay・Rollback」は対戦者 2 人の間の値
    /// （ホストが 1 秒ごとに観戦の流れの `Stats` の記録で流す。定義は `hud_line` と同じ。ping は観戦者からホストへの往復ではない）、後半の「Watch delay」は観戦者自身の遅れ
    /// （手元に届いてまだ使っていないマッチのフレームの数。画面から読めない数）。区切りは空白 1 つで、ふつうの文字では収まらないので観戦者の行だけ小さい文字で描く（`overlay::SMALL_CELL_H`）。
    /// 全部 ASCII。「>>」は遅れて入った観戦者が溜まりを追いつく間（画面が速く進む）。
    T_HUD_WATCH_PING = "ping ", "ping ";
    T_HUD_WATCH_DELAY = " Delay ", " Delay ";
    T_HUD_WATCH_ROLLBACK = " Rollback ", " Rollback ";
    T_HUD_WATCH = " Watch delay ", " Watch delay ";
    T_HUD_WATCH_FF = " >>", " >>";
    /// ルームサーバーの項目。ini の `room_server` があるときだけ出す（`RoomItem`）。選ぶとルーム名の入力になる。
    T_ROOM = "ルームマッチ", "Room match";
    /// 「ルーム名　<今のルーム名>」（対戦の中）。決めてあれば「ルームマッチ」はその名で入力なしに始まる（空なら入力の画面から）。
    T_ROOM_NAME = "ルーム名", "Room name";
    T_ROOM_UNSET = "未設定", "(not set)";
    /// ルーム名の入力の画面（`Menu::render`。打った文字の後ろに `T_ROOM_CURSOR`）。この画面だけキーの名前（Enter・Esc）を出す:
    /// ルーム名はキーボードで打つので、文字を打つキーと決めるキーの名前を書かないと何を押すのか読めない。
    T_ROOM_INPUT_LABEL = "ルーム名: ", "Room name: ";
    /// 打つ文字の位置の印。`_` はルーム名に使える文字なので、文字と見間違えない `|` にする。
    T_ROOM_CURSOR = "|", "|";
    T_ROOM_INPUT_KEYS = "使用可能な文字は「英数字」「-」「_」「@」です。　Enterで決定。　Escでもどる。", "Allowed: A-Z 0-9 - _ @   Enter: Confirm   Esc: Back";
    /// 空のまま Enter（`RoomNameWhy::Empty`。文字の決まりの外はほかの `T_ROOM_NAME_*`）。
    T_ROOM_INPUT_EMPTY = "ルーム名が空です。", "The room name is empty.";
    /// ウィンドウから文字を受ける仕組み（`text_input_win`）が置けなかった（ゲームのウィンドウが見つからない・差し替えを断られた）。ルームの入力は何も打てないので、次の 1 手（接続コード）を添える。
    T_ROOM_INPUT_NO_KEYS = "キーボードの文字が受け取れません（詳細はログを確認してください）。", "Cannot receive keyboard input (details in the log).";
    T_ROOM_INPUT_NO_KEYS_NEXT = "Esc でもどり、「接続コードで対戦」から接続してください。", "Press Esc and use \"Play with a code\".";
    /// 使えない文字（半角カナ・記号・日本語など）を打って捨てたとき（`Edit::Rejected`）。文字の表に無い文字は `?` で描かれるので、何が起きたかをここで言う。
    T_ROOM_INPUT_DROPPED = "ルーム名に使用可能な文字は「英数字」「-」「_」「@」です。", "Only A-Z, 0-9, -, _ and @ can be used.";
    /// プレイヤー名の項目と入力の画面（`Item::Name`）。項目の行は「名前　<今の名前>」（全角の空白で区切る）。入力の画面の題は項目と同じ文字、
    /// 打つ文字の組はルーム名に空白を足したもの（`player_name::accepts_char`）。この画面もキーの名前を出す（ルーム名の入力と同じ理由）。
    T_NAME = "プレイヤー名", "Player name";
    /// 「サイド」の行（席の希望。`seat_pick.rs`）。1P・2P は `T_HUD_SEAT_1` / `T_HUD_SEAT_2` を使う。
    T_SEAT = "サイド", "Side";
    /// 「サイド」の値の「おまかせ」。片方だけなら相手の希望に合わせ、両方のときだけ乱数なので、「ランダム」と書くと片方のときに読み違える。
    T_SEAT_ANY = "おまかせ", "Any";
    T_NAME_INPUT_LABEL = "プレイヤー名: ", "Player name: ";
    T_NAME_INPUT_KEYS = "使用可能な文字は「英数字」「-」「_」「@」です。　Enterで決定。　Escでもどる。", "Allowed: A-Z 0-9 - _ @   Enter: Confirm   Esc: Back";
    T_NAME_INPUT_EMPTY = "プレイヤー名が空です。", "The player name is empty.";
    T_NAME_INPUT_DROPPED = "プレイヤー名に使用可能な文字は「英数字」「-」「_」「@」です。", "Only A-Z, 0-9, -, _ and @ can be used.";
    /// ウィンドウから文字を受けられないとき（`T_ROOM_INPUT_NO_KEYS` の次の 1 手）。名前は変えなくても使える。
    T_NAME_INPUT_NO_KEYS_NEXT = "Esc で戻ってください。", "Press Esc to go back.";
    T_ROOM_JOINING = "ルームのサーバーに接続しています。", "Connecting to the room server.";
    /// 「ルーム 名前 で待機中 12:34」。時計は待った時間（期限が切れると自動で入り直して待ち続けるので、残りでなく待った時間を出す）。
    /// 左上ではマッチ中の行と同じ GDI の小さい文字の 1 行なので、鉤括弧（全角で 2 枠）は使わず空白で区切る。
    T_ROOM_OPEN = "ルーム ", "Room ";
    T_ROOM_WAITING_CLOSE = " で待機中", "  waiting";
    T_NEXT_ROOM_WAITING = "相手に同じルーム名で「ルームマッチ」を選んでもらってください。", "Ask your opponent to choose \"Room match\" with the same room name.";
    /// 「相手「名前」がルームに来ました。」。
    T_ROOM_PEER_OPEN = "相手「", "Opponent \"";
    T_ROOM_PEER_CLOSE = "」がルームに来ました。", "\" joined the room.";
    T_ROOM_LINKING = "ルームで会えた相手と接続しています。", "Connecting to the opponent in the room.";
    /// サーバーが応答しない（`room::FailureKind::Unresponsive`）。
    T_ROOM_UNRESPONSIVE = "サーバーが応答しません。", "The server is not responding.";
    T_ROOM_UNRESPONSIVE_NEXT = "「コード作成」か「コード入力」で接続してください。", "Connect with \"Create code\" or \"Paste code\".";
    /// 429（`FailureKind::WaitThenRetry`）。待つ秒はサーバーの `retry_after`（画面から読めない数）から作る。
    T_ROOM_BUSY = "サーバーが混み合っています。", "The server is busy.";
    /// 秒の数（`retry_after`）の前と後ろ。分に直さない（文字の表に入る文字を固定するため）。
    T_ROOM_BUSY_NEXT_OPEN = "", "Wait about ";
    T_ROOM_BUSY_NEXT = " 秒ほど待ってから、「ルームマッチ」を選び直してください。", " seconds, then choose \"Room match\" again.";
    T_ROOM_FULL = "ルームが満員です。", "The room is full.";
    /// 400（`FailureKind::ChangeRoomName`。文字の決まりは通ったが、サーバーが揃えた後の長さなどで断った）。
    T_ROOM_REFUSED = "サーバーがこのルーム名を受け付けませんでした。", "The server did not accept this room name.";
    T_ROOM_RENAME = "別のルーム名で「ルームマッチ」を選び直してください。", "Choose \"Room match\" again with another room name.";
    /// 4408（`FailureKind::Expired`。ルームの期限 10 分か、相手のコードを返す期限 60 秒。どちらかは分からないので長さは書かない）。
    T_ROOM_EXPIRED = "ルームの期限が切れました。", "The room has expired.";
    T_ROOM_PEER_LEFT = "ルームの相手が途中で抜けました。", "The opponent left the room.";
    /// ルームで受けた相手のコードが読めなかった（バージョン違い・範囲の外・作りの誤りは `Event::OtherVersion` ほかの既存の文）。
    T_ROOM_BAD_CODE = "ルームの相手から届いたコードを読めませんでした。", "Could not read the code from the opponent in the room.";
    T_ROOM_AGAIN = "「ルームマッチ」を選び直してください。", "Choose \"Room match\" again.";
    /// ランダムマッチ（メニューの一番上。ルーム名を決めずに、来た人どうしで対戦する）。ルーム名は割り当てられるが、画面にもログにも出さない。
    /// 待機の間はルームの待機と同じ状態・同じ左上の行の形（「ランダムマッチで待機中 12:34」）。時計は待ち始めてからの時間で、期限が切れて自動で入り直しても変えない。
    T_RANDOM = "ランダムマッチ", "Random match";
    T_RANDOM_WAITING = "ランダムマッチで待機中", "Random match  waiting";
    T_NEXT_RANDOM_WAITING = "相手が来るまでお待ちください。", "Please wait for an opponent.";
    /// 「対戦相手が見つかりました「名前」」。
    T_RANDOM_PEER_OPEN = "対戦相手が見つかりました「", "Opponent found: \"";
    T_RANDOM_PEER_CLOSE = "」", "\"";
    T_RANDOM_LINKING = "見つかった相手と接続しています。", "Connecting to the opponent.";
    T_RANDOM_BUSY_NEXT = " 秒ほど待ってから、「ランダムマッチ」を選び直してください。", " seconds, then choose \"Random match\" again.";
    T_RANDOM_AGAIN = "「ランダムマッチ」を選び直してください。", "Choose \"Random match\" again.";
    /// こちらの文字の決まり（`room::RoomName::parse`）で断ったルーム名。名そのものは出さない（断った文字は表に無いことがある）。入力の画面は決まりの内だけを受けるので、ふつうは来ない。
    T_ROOM_NAME_LONG_OPEN = "ルーム名が長すぎます（", "The room name is too long (max ";
    T_ROOM_NAME_LONG_CLOSE = " 文字まで）。", " chars).";
    T_ROOM_NAME_CHAR = "ルーム名に使えない文字（改行や / など）があります。", "The room name has a character not allowed (newline, / etc.).";
    T_ROOM_NAME_DOT = "ルーム名を「.」や「..」にはできません。", "The room name cannot be \".\" or \"..\".";
    /// ASCII の英数字と - _ 以外（日本語・全角・空白・記号）。文字の表に無い漢字は「?」で描かれ、相手と同じ名かを画面で見分けられないため。
    T_ROOM_NAME_ASCII = "ルーム名に使用可能な文字は「英数字」「-」「_」「@」です。", "Room names can use only A-Z, 0-9, -, _ and @.";
    T_ROOM_FIX = "ルーム名を直して、「ルームマッチ」を選び直してください。", "Fix the room name and choose \"Room match\" again.";
    /// 観戦者の入口。観戦の「ルームマッチ」は対戦の「ルームマッチ」と同じルーム名の入力へ入る（題だけがこの文字）。待ちの間の項目は「観戦をやめる」だけ。
    /// つなぐ途中は `T_ROOM_JOINING`、失敗の 1 行目のうち応答しない・400 は `T_ROOM_UNRESPONSIVE`・`T_ROOM_REFUSED`、バージョン違いは `other_version_line` を使い、次にすることだけ「観戦」に替える。
    T_SPECTATE = "観戦", "Watch";
    T_SPECTATE_CANCEL = "観戦をやめる", "Stop watching";
    /// 観戦の「ランダムマッチ」（進行中のランダムマッチの一覧から選ぶ画面）。題は「観戦 ─ ランダムマッチ」。1 行は「1P 名 vs 2P 名　観戦者の数/上限」で、
    /// 8 行を越える分は上下の「ほか N 件」で知らせる。満員の行は灰色で選べない。
    T_WATCH_DASH = " ─ ", " ─ ";
    T_WATCH_VS = " vs ", " vs ";
    T_WATCH_EMPTY = "いま観戦できるランダムマッチはありません", "There are no random matches to watch";
    T_WATCH_LOADING = "一覧を取得しています", "Loading the list...";
    T_WATCH_FAILED = "一覧を取得できませんでした。もどってやり直してください。", "Could not get the list. Go back and try again.";
    T_WATCH_MORE_UP = "▲ ほか ", "▲ ";
    T_WATCH_MORE_DOWN = "▼ ほか ", "▼ ";
    T_WATCH_MORE_CLOSE = " 件", " more";
    T_KEYS_WATCH = "↑↓ メニューの移動　決定で観戦　キャンセルでもどる", "↑↓ Move   Confirm: Watch   Cancel: Back";
    /// 「観戦 名前 で待機中 12:34」（`T_SPECTATE_OPEN` ＋ 名前 ＋ `T_ROOM_WAITING_CLOSE` ＋ 時計）。配る側がルームに来るまでの入り直しの間（`JoinerStatus::waiting_for_caster`）。時計は待った時間。
    T_SPECTATE_OPEN = "観戦 ", "Watch ";
    /// 「相手「名前」に接続しています。」。観戦用のコードを受け、対戦者へつなぎに行く間。
    T_SPECTATE_PEER_OPEN = "相手「", "Connecting to player \"";
    T_SPECTATE_PEER_CLOSE = "」に接続しています。", "\".";
    T_SPECTATE_UNRESPONSIVE_NEXT = "時間をおいて、観戦の「ルームマッチ」を選び直してください。", "Wait a while, then choose \"Room match\" under Watch again.";
    T_SPECTATE_RENAME = "別のルーム名で、観戦の「ルームマッチ」を選び直してください。", "Choose \"Room match\" under Watch again with another room name.";
    T_SPECTATE_BAD_CODE = "相手から届いたコードを読めませんでした。", "Could not read the code from the player.";
    T_SPECTATE_AGAIN = "観戦の「ルームマッチ」を選び直してください。", "Choose \"Room match\" under Watch again.";
    /// 「30 分待っても、相手がルームに来ませんでした。」（「30 分」は `spectate_room::JOINER_GIVE_UP_MS` から作る）。
    T_SPECTATE_GAVE_UP_OPEN = "", "No player came to the room in ";
    T_SPECTATE_GAVE_UP_TAIL = "待っても、相手がルームに来ませんでした。", ".";
    T_SPECTATE_GAVE_UP_NEXT = "ルーム名を確かめて、観戦の「ルームマッチ」を選び直してください。", "Check the room name and choose \"Room match\" under Watch again.";
    /// 返しを送れなかった・つなぎに行って届かない。原因は画面から読めないので、次にすることはログを送ること。
    T_SPECTATE_NO_LINK = "相手と接続できませんでした。", "Could not connect to the player.";
    T_SPECTATE_NO_LINK_NEXT = "ログを送ってください。", "Please send the log.";
    T_SPECTATE_ENDED = "観戦を終えました。", "Finished watching.";
    T_SPECTATE_CANCELLED = "観戦をやめました。", "Stopped watching.";
    /// 時間の長さの単位（`push_span`）。
    T_SPAN_MIN = " 分", " min";
    T_SPAN_SEC = " 秒", " s";
    /// 接続の手順が時間切れで作り直された後、相手から届いているのにつながらないときの左上の 3 行（`stall_pair`）。1 行目は理由＋「待機時間 0:42」
    /// （接続コードの経路は「・残り 29:18」も。ルームの経路はルームの期限が自動で入り直すので残りが無い）。2 行目は次にすること 1 つ、3 行目は README の項。
    T_STALL_NO_HANDSHAKE = "相手からパケットは届いていますが、接続できません（", "Receiving from opponent, cannot connect (";
    T_STALL_REMAIN = "・残り ", ", left ";
    T_STALL_CLOSE = "）", ")";
    T_STALL_NEXT_CODE = "2 人とも待機をやめ、片方が新しいコードを作って、すぐ送ってください。", "Both stop waiting; one creates a new code and sends it at once.";
    T_STALL_NEXT_ROOM = "待機をやめ、接続コード方式に切り替えてください。", "Stop waiting and switch to a connection code.";
    T_STALL_REF = "（README「接続できないとき」5）", "(README \"When you cannot connect\" 5)";
    /// つながらなかったときの複数行の文（`fail_block`。回線の型は `nat::FailKind`）。文字は日本語の文面を変えない。最後の行の番号は README の「接続できないとき」の項。
    T_FAIL_HEAD = "接続失敗：", "Connection failed:";
    T_FAIL_HEAD_NO = "接続不可：", "Cannot connect:";
    T_FAIL1_A = "不明な原因で相手からの UDP パケットが届きません", "UDP packets from the opponent do not arrive (unknown cause).";
    T_FAIL1_B = "iniをdetail=1に書き換えてもう一度試し、開発者にログを送ってください。", "Set detail=1 in the ini, retry, and send the log to the developer.";
    T_FAIL1_REF = "（README「接続できないとき」1）", "(README \"When you cannot connect\" 1)";
    T_FAIL2_A = "相手がシンメトリック NATであり、こちらはUPnPが使用不可です。", "The opponent is on a symmetric NAT and UPnP is unavailable here.";
    T_FAIL2_B = "ルーターのUPnPをオンにするか、ポート開放してください。", "Turn on UPnP on your router, or open a port.";
    T_FAIL2_REF = "（README「接続できないとき」2）", "(README \"When you cannot connect\" 2)";
    T_FAIL3_A = "こちらがシンメトリック NATです。", "Your network is a symmetric NAT.";
    T_FAIL3_B = "相手側のポート開放が必要です。", "The opponent needs to open a port.";
    T_FAIL3_C = "相手もシンメトリックNATなら通信できません。", "If the opponent is also on a symmetric NAT, you cannot connect.";
    T_FAIL3_REF = "（README「接続できないとき」3）", "(README \"When you cannot connect\" 3)";
    T_FAIL4_A = "こちらも相手もシンメトリック NAT のため、直接接続できません。", "Both sides are symmetric NATs, so a direct connection is impossible.";
    T_FAIL4_REF = "（README「接続できないとき」4）", "(README \"When you cannot connect\" 4)";
}

/// `LineBuf` の大きさ（バイト）。全角 1 文字 3 バイトで 53 文字、半角だけなら 160 文字。1 行は画面の幅（640 画素 = 全角 45 文字）に収まる。
pub const LINE_BYTES: usize = 160;

/// 固定の長さの 1 行（UTF-8）。確保しない・パニックしない: 入りきらない分は文字の境で落とす。
#[derive(Clone, Copy)]
pub struct LineBuf {
    buf: [u8; LINE_BYTES],
    len: usize,
}

impl Default for LineBuf {
    fn default() -> Self {
        LineBuf::new()
    }
}

impl core::fmt::Debug for LineBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl PartialEq for LineBuf {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for LineBuf {}

impl LineBuf {
    pub const fn new() -> LineBuf {
        LineBuf {
            buf: [0; LINE_BYTES],
            len: 0,
        }
    }

    pub fn clear(&mut self) -> &mut Self {
        self.len = 0;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_str(&self) -> &str {
        // 書くのは `push_char` だけで、文字の境で入れるので常に正しい UTF-8。
        core::str::from_utf8(self.buf.get(..self.len).unwrap_or(&[])).unwrap_or("")
    }

    pub fn push_char(&mut self, c: char) -> &mut Self {
        let n = c.len_utf8();
        if let Some(dst) = self.buf.get_mut(self.len..self.len + n) {
            c.encode_utf8(dst);
            self.len += n;
        }
        self
    }

    pub fn push_str(&mut self, s: &str) -> &mut Self {
        for c in s.chars() {
            let n = c.len_utf8();
            if self.len + n > LINE_BYTES {
                break;
            }
            self.push_char(c);
        }
        self
    }

    pub fn push_u32(&mut self, mut v: u32) -> &mut Self {
        let mut digits = [0u8; 10];
        let mut n = 0;
        loop {
            if let Some(d) = digits.get_mut(n) {
                *d = b'0' + (v % 10) as u8;
            }
            n += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        while n > 0 {
            n -= 1;
            self.push_char(digits.get(n).copied().map_or('0', char::from));
        }
        self
    }

    pub fn push_clock(&mut self, secs: u64) -> &mut Self {
        let m = u32::try_from(secs / 60).unwrap_or(u32::MAX);
        let s = (secs % 60) as u32;
        self.push_u32(m).push_char(':');
        if s < 10 {
            self.push_char('0');
        }
        self.push_u32(s)
    }

    /// ASCII のバイト列（接続コードの先頭と末尾）。ASCII でないバイトは `?`。
    pub fn push_ascii(&mut self, bytes: &[u8]) -> &mut Self {
        for &b in bytes {
            self.push_char(if (0x20..0x7F).contains(&b) {
                char::from(b)
            } else {
                '?'
            });
        }
        self
    }
}

pub const NAME_UNITS: usize = 12;

/// 相手の名前（接続コードの `name`。UTF-8 の 0〜32 B）を画面に出せる形にする: 文字の表に無い文字（`in_table` が偽）・基本面の外の文字・制御文字は `?` にして、
/// `NAME_UNITS` 文字までを UTF-16 の単位で返す（0 は「ここで終わり」）。書き手（見張りのスレッド）が呼ぶ。
pub fn sanitize_name(raw: &[u8], in_table: impl Fn(char) -> bool) -> [u16; NAME_UNITS] {
    let mut out = [0u16; NAME_UNITS];
    let text = String::from_utf8_lossy(raw);
    for (slot, c) in out.iter_mut().zip(text.chars()) {
        let ok = c.len_utf16() == 1 && !c.is_control() && in_table(c);
        let shown = if ok { c } else { '?' };
        *slot = shown as u32 as u16;
    }
    out
}

fn push_name(line: &mut LineBuf, name: &[u16; NAME_UNITS]) {
    push_units(line, name);
}

/// 自分のコードの状態（続く状態。`Note::state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodeState {
    #[default]
    Idle,
    /// 始める側: 自分のコードを作って、相手の返すコードを待っている。
    Offered,
    /// 返す側: 相手のコードを読んで、返すコードを渡した。つながるのを待っている。
    Replied,
    /// 始める側: 相手の返すコードを読んで、つないでいる。
    Linking,
    RoomJoining,
    /// ルームに入って、相手を待っている（期限は `Note::until`）。
    RoomWaiting,
    /// ルームで相手のコードを受け取り、セッションの設定を置いた（つながるのを待っている）。始める側も返す側もこれ。
    RoomLinking,
    SpectateJoining,
    /// 観戦用のルームで、配る側（対戦者）が来るのを待っている（期限は 30 分）。
    SpectateWaiting,
    SpectateLinking,
    /// 観戦者のセッションを置いたまま、待ちは終わっている（取りやめた・失敗した・観戦を終えた後で、セッションを対戦のセッションへ戻さない実行）。項目は「観戦」と「戻る」だけ。
    /// セッションを元へ戻せた実行は `Idle` を置く。どちらにするかは観戦者のセッションを置く側が決める。
    SpectateIdle,
}

impl CodeState {
    pub fn code(self) -> u32 {
        match self {
            CodeState::Idle => 0,
            CodeState::Offered => 1,
            CodeState::Replied => 2,
            CodeState::Linking => 3,
            CodeState::RoomJoining => 4,
            CodeState::RoomWaiting => 5,
            CodeState::RoomLinking => 6,
            CodeState::SpectateJoining => 7,
            CodeState::SpectateWaiting => 8,
            CodeState::SpectateLinking => 9,
            CodeState::SpectateIdle => 10,
        }
    }

    pub fn from_code(c: u32) -> CodeState {
        match c {
            1 => CodeState::Offered,
            2 => CodeState::Replied,
            3 => CodeState::Linking,
            4 => CodeState::RoomJoining,
            5 => CodeState::RoomWaiting,
            6 => CodeState::RoomLinking,
            7 => CodeState::SpectateJoining,
            8 => CodeState::SpectateWaiting,
            9 => CodeState::SpectateLinking,
            10 => CodeState::SpectateIdle,
            _ => CodeState::Idle,
        }
    }

    pub fn is_room(self) -> bool {
        matches!(
            self,
            CodeState::RoomJoining | CodeState::RoomWaiting | CodeState::RoomLinking
        ) || self.is_spectate()
    }

    pub fn is_spectate(self) -> bool {
        matches!(
            self,
            CodeState::SpectateJoining
                | CodeState::SpectateWaiting
                | CodeState::SpectateLinking
                | CodeState::SpectateIdle
        )
    }

    /// 観戦の待ちの間か（つなぐ途中・配る側を待つ・つなぎに行く。`SpectateIdle` は含まない）。この間のメニューの項目は「観戦をやめる」だけ。
    pub fn is_spectate_busy(self) -> bool {
        matches!(
            self,
            CodeState::SpectateJoining | CodeState::SpectateWaiting | CodeState::SpectateLinking
        )
    }
}

/// こちらの文字の決まりでルーム名を断った理由（`Note::room_why`。`room::RoomNameError` を画面の文の種類へ寄せたもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoomNameWhy {
    #[default]
    Empty,
    Long,
    BadChar,
    Dot,
    /// ASCII の英数字と `-` `_` 以外の文字がある（日本語・全角・空白・記号）。
    NotAscii,
}

impl RoomNameWhy {
    const ALL: [RoomNameWhy; 5] = [
        RoomNameWhy::Empty,
        RoomNameWhy::Long,
        RoomNameWhy::BadChar,
        RoomNameWhy::Dot,
        RoomNameWhy::NotAscii,
    ];

    fn code(self) -> u32 {
        RoomNameWhy::ALL
            .iter()
            .position(|w| *w == self)
            .unwrap_or(0) as u32
    }

    fn from_code(c: u32) -> RoomNameWhy {
        RoomNameWhy::ALL
            .get(c as usize)
            .copied()
            .unwrap_or_default()
    }

    pub fn of(e: crate::room::RoomNameError) -> RoomNameWhy {
        use crate::room::RoomNameError as E;
        match e {
            E::Empty | E::Blank => RoomNameWhy::Empty,
            E::TooManyBytes { .. } | E::TooLong { .. } => RoomNameWhy::Long,
            E::BadChar { .. } => RoomNameWhy::BadChar,
            E::DotName => RoomNameWhy::Dot,
            E::NotAscii { .. } => RoomNameWhy::NotAscii,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Event {
    #[default]
    None,
    Made,
    ReplyMade,
    PeerRead,
    /// 貼った文字が短すぎる（`code::MIN_CODE_CHARS` より少ない）。
    Short,
    Empty,
    /// 同じコードをもう読んでいる。
    Same,
    Broken,
    OwnCode,
    BothOffer,
    BothAnswer,
    Expired,
    WrongSession,
    WrongKind,
    NoRoute,
    Internal,
    Unreadable,
    ClipWrite,
    /// 対戦の形を、つなぎ始めた後に変えた（この接続では変わらない）。
    ShapeLate,
    /// 使えるコードが届かないまま待つ長さが切れた。
    GaveUp,
    /// 相手とつながらないままネットワークが終わった。
    Ended,
    Cancelled,
    /// 相手のコードのバージョンが違う。相手のバージョンは `Note::peer_ver`。
    OtherVersion,
    /// ルームサーバーへつなぎ始めた（以下 `Room*` はルームの経路）。
    RoomJoining,
    RoomWaiting,
    /// ルームで相手のコードを受け取り、つなぎ始めた（相手の名前は `Note::name`）。
    RoomPeer,
    /// ルーム名をこちらの文字の決まりで断った（理由は `Note::room_why`）。
    RoomNameBad,
    RoomUnresponsive,
    /// 429（待つ秒は `Note::retry_secs`）。
    RoomBusy,
    RoomFull,
    RoomRefused,
    RoomExpired,
    RoomPeerLeft,
    RoomBadCode,
    /// 貼られたコードは、この起動の前のつながりで使ったもの（session が同じ）。読まない（クリップボードに残った前の相手のコードを貼っても前の相手へつなぎに行かない）。
    Spent,
    /// 観戦者: 観戦用のコードを受け、対戦者へつなぎに行く（対戦者の名前は `Note::name`）。
    SpectatePeer,
    SpectateUnresponsive,
    SpectateRefused,
    SpectateBadCode,
    /// 観戦者: 観戦用のコードのバージョンが違う（相手のバージョンは `Note::peer_ver`）。
    SpectateOtherVersion,
    SpectateGaveUp,
    /// 観戦者: 返しを送れなかった・つなぎに行って届かなかった。
    SpectateNoLink,
    SpectateEnded,
    SpectateCancelled,
    /// 相手のコードのビルドが違う（相手のビルドは `Note::peer_build`）。
    OtherBuild,
    /// 観戦者: 観戦用のコードのビルドが違う（相手のビルドは `Note::peer_build`）。
    SpectateOtherBuild,
}

/// 出来事の番号の表（番号 = 並びの位置。原子変数で渡すので、足すときは末尾に足す）。
const EVENTS: [Event; 46] = [
    Event::None,
    Event::Made,
    Event::ReplyMade,
    Event::PeerRead,
    Event::Short,
    Event::Empty,
    Event::Same,
    Event::Broken,
    Event::OwnCode,
    Event::BothOffer,
    Event::BothAnswer,
    Event::Expired,
    Event::WrongSession,
    Event::WrongKind,
    Event::NoRoute,
    Event::Internal,
    Event::Unreadable,
    Event::ClipWrite,
    Event::ShapeLate,
    Event::GaveUp,
    Event::Ended,
    Event::Cancelled,
    Event::OtherVersion,
    Event::RoomJoining,
    Event::RoomWaiting,
    Event::RoomPeer,
    Event::RoomNameBad,
    Event::RoomUnresponsive,
    Event::RoomBusy,
    Event::RoomFull,
    Event::RoomRefused,
    Event::RoomExpired,
    Event::RoomPeerLeft,
    Event::RoomBadCode,
    Event::Spent,
    Event::SpectatePeer,
    Event::SpectateUnresponsive,
    Event::SpectateRefused,
    Event::SpectateBadCode,
    Event::SpectateOtherVersion,
    Event::SpectateGaveUp,
    Event::SpectateNoLink,
    Event::SpectateEnded,
    Event::SpectateCancelled,
    Event::OtherBuild,
    Event::SpectateOtherBuild,
];

impl Event {
    pub fn code(self) -> u32 {
        EVENTS.iter().position(|e| *e == self).unwrap_or(0) as u32
    }

    pub fn from_code(c: u32) -> Event {
        EVENTS.get(c as usize).copied().unwrap_or(Event::None)
    }

    /// 貼った文字数を最初の行に出す出来事（貼って、読もうとした結果。読めたときも出す）。
    fn shows_got(self) -> bool {
        matches!(
            self,
            Event::ReplyMade
                | Event::PeerRead
                | Event::Short
                | Event::Same
                | Event::Broken
                | Event::OwnCode
                | Event::BothOffer
                | Event::BothAnswer
                | Event::Expired
                | Event::WrongSession
                | Event::WrongKind
                | Event::NoRoute
                | Event::OtherVersion
                | Event::OtherBuild
                | Event::Spent
        )
    }

    fn is_trouble(self) -> bool {
        !matches!(
            self,
            Event::None
                | Event::Made
                | Event::ReplyMade
                | Event::PeerRead
                | Event::Cancelled
                | Event::Same
                | Event::RoomJoining
                | Event::RoomWaiting
                | Event::RoomPeer
                | Event::SpectatePeer
                | Event::SpectateEnded
                | Event::SpectateCancelled
        )
    }

    /// ルームの経路の断り・失敗か。左上の行は、待ちが終わった（状態が `Idle`）後もこれを出す（ルームで待つ間に別のモードで遊んでいた側が、なぜ待ちが終わったかを知る）。
    /// コードの経路の断りはメニューを開いて操作した直後にしか起きないので出さない。観戦の失敗と「観戦を終えました。」も同じ。観戦を取りやめた行は、取りやめたのは本人なので左上へは残さない（メニューの状況の行にだけ出る）。
    fn is_room_trouble(self) -> bool {
        matches!(
            self,
            Event::RoomNameBad
                | Event::RoomUnresponsive
                | Event::RoomBusy
                | Event::RoomFull
                | Event::RoomRefused
                | Event::RoomExpired
                | Event::RoomPeerLeft
                | Event::RoomBadCode
                | Event::SpectateUnresponsive
                | Event::SpectateRefused
                | Event::SpectateBadCode
                | Event::SpectateOtherVersion
                | Event::SpectateOtherBuild
                | Event::SpectateGaveUp
                | Event::SpectateNoLink
                | Event::SpectateEnded
        )
    }
}

/// ルーム名を画面に出す文字数（UTF-16 の単位）。ルーム名は揃えて 32 文字までだが、左上の行の幅に収めるため 16 文字で切り、切ったら最後の文字を `…` にする（`sanitize_room`）。
pub const ROOM_UNITS: usize = 16;

/// ルーム名を画面に出せる形にする（`sanitize_name` と同じく、文字の表に無い文字・制御文字は `?`）。`ROOM_UNITS` 文字より長ければ最後の文字を `…` にする。書き手（見張り）が呼ぶ。ルーム名は画面にだけ出し、本ログには書かない。
pub fn sanitize_room(raw: &str, in_table: impl Fn(char) -> bool) -> [u16; ROOM_UNITS] {
    let mut out = [0u16; ROOM_UNITS];
    let n = raw.chars().count();
    for (i, (slot, c)) in out.iter_mut().zip(raw.chars()).enumerate() {
        let ok = c.len_utf16() == 1 && !c.is_control() && in_table(c);
        let shown = if n > ROOM_UNITS && i == ROOM_UNITS - 1 {
            '…'
        } else if ok {
            c
        } else {
            '?'
        };
        *slot = shown as u32 as u16;
    }
    out
}

fn push_units(line: &mut LineBuf, units: &[u16]) {
    for &u in units.iter().take_while(|&&u| u != 0) {
        line.push_char(char::from_u32(u32::from(u)).unwrap_or('?'));
    }
}

/// `push_units` の、文字の枠の数（`overlay::glyph_cells`）が `max_cells` を越えるなら、収まるところまでで切って末尾に「…」（2 枠）を付ける形。名前は `max_cells` 枠に収まる。
fn push_units_within(line: &mut LineBuf, units: &[u16], max_cells: u32) {
    let chars = || {
        units
            .iter()
            .take_while(|&&u| u != 0)
            .map(|&u| char::from_u32(u32::from(u)).unwrap_or('?'))
    };
    let total: u32 = chars().map(crate::overlay::glyph_cells).sum();
    if total <= max_cells {
        push_units(line, units);
        return;
    }
    let ellipsis = '…';
    let room = max_cells.saturating_sub(crate::overlay::glyph_cells(ellipsis));
    let mut used = 0u32;
    for c in chars() {
        used += crate::overlay::glyph_cells(c);
        if used > room {
            break;
        }
        line.push_char(c);
    }
    line.push_char(ellipsis);
}

/// `Note` を原子変数の語に詰めた長さ（`to_words`）。先頭の 7 語・名前・ルームの 1 語（断りの理由と待つ秒）・ルーム名・相手のビルド。
pub const NOTE_WORDS: usize = 7 + NAME_UNITS / 2 + 1 + ROOM_UNITS / 2 + 1;
const _: () = assert!(NAME_UNITS.is_multiple_of(2));
const _: () = assert!(ROOM_UNITS.is_multiple_of(2));
const W_ROOM_DETAIL: usize = 7 + NAME_UNITS / 2;
const W_ROOM_NAME: usize = W_ROOM_DETAIL + 1;
const W_PEER_BUILD: usize = W_ROOM_NAME + ROOM_UNITS / 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Note {
    pub seq: u32,
    pub event: Event,
    pub state: CodeState,
    pub paste_len: u32,
    /// 自分のコードの文字数と、先頭・末尾の 4 文字（ASCII）。
    pub code_len: u32,
    pub head: [u8; 4],
    pub tail: [u8; 4],
    /// 自分のコードを作った時刻（UNIX 秒の下位 32 ビット。有効期限 `DEFAULT_TTL_SECS` はここから）。ルームの経路では、ルームで待ち始めた時刻（自動で入り直しても変えない。左上の「待った時間」の起点）。
    pub since: u32,
    pub name: [u16; NAME_UNITS],
    /// 貼られた相手のコードのバージョン（`Event::OtherVersion` のときだけ意味がある。語 1 の bit 16〜23 に詰める）。
    pub peer_ver: u8,
    /// ルーム名を断った理由（`Event::RoomNameBad` のときだけ）。
    pub room_why: RoomNameWhy,
    /// 429 で待つ秒（`Event::RoomBusy` のときだけ。サーバーの `retry_after`）。
    pub retry_secs: u16,
    pub room: [u16; ROOM_UNITS],
    /// 貼られた・ルームで受けた相手のコードのビルド番号（`Event::OtherBuild`・`Event::SpectateOtherBuild` のときだけ）。
    pub peer_build: u32,
    /// ランダムマッチの経路にいる（待ち列でも、割り当てられたルームでも）。ルーム名は画面に出さず（`room` は空）、文の次にすることが「ランダムマッチ」になる。語 1 の bit 24。
    pub random: bool,
}

fn pack_units(dst: &mut [u32], units: &[u16]) {
    for (d, pair) in dst.iter_mut().zip(units.chunks(2)) {
        let lo = pair.first().copied().unwrap_or(0);
        let hi = pair.get(1).copied().unwrap_or(0);
        *d = u32::from(lo) | u32::from(hi) << 16;
    }
}

fn unpack_units(dst: &mut [u16], src: &[u32]) {
    for (pair, s) in dst.chunks_mut(2).zip(src) {
        if let Some(lo) = pair.first_mut() {
            *lo = *s as u16;
        }
        if let Some(hi) = pair.get_mut(1) {
            *hi = (*s >> 16) as u16;
        }
    }
}

impl Note {
    pub fn to_words(&self) -> [u32; NOTE_WORDS] {
        let mut w = [0u32; NOTE_WORDS];
        w[0] = self.seq;
        w[1] = self.event.code()
            | self.state.code() << 8
            | u32::from(self.peer_ver) << 16
            | u32::from(self.random) << 24;
        w[2] = self.paste_len;
        w[3] = self.code_len;
        w[4] = u32::from_le_bytes(self.head);
        w[5] = u32::from_le_bytes(self.tail);
        w[6] = self.since;
        pack_units(&mut w[7..W_ROOM_DETAIL], &self.name);
        w[W_ROOM_DETAIL] = u32::from(self.retry_secs) | self.room_why.code() << 16;
        pack_units(&mut w[W_ROOM_NAME..W_PEER_BUILD], &self.room);
        w[W_PEER_BUILD] = self.peer_build;
        w
    }

    pub fn from_words(w: &[u32; NOTE_WORDS]) -> Note {
        let mut name = [0u16; NAME_UNITS];
        unpack_units(&mut name, &w[7..W_ROOM_DETAIL]);
        let mut room = [0u16; ROOM_UNITS];
        unpack_units(&mut room, &w[W_ROOM_NAME..W_PEER_BUILD]);
        let detail = w[W_ROOM_DETAIL];
        Note {
            seq: w[0],
            event: Event::from_code(w[1] & 0xFF),
            state: CodeState::from_code((w[1] >> 8) & 0xFF),
            peer_ver: (w[1] >> 16) as u8,
            paste_len: w[2],
            code_len: w[3],
            head: w[4].to_le_bytes(),
            tail: w[5].to_le_bytes(),
            since: w[6],
            name,
            retry_secs: detail as u16,
            room_why: RoomNameWhy::from_code((detail >> 16) & 0xFF),
            room,
            peer_build: w[W_PEER_BUILD],
            random: (w[1] >> 24) & 1 != 0,
        }
    }

    /// 自分のコードの先頭と末尾を、メニューへ渡す形に（`code` は ASCII。4 文字に満たなければ空白で埋める）。
    pub fn edges_of(code: &str) -> ([u8; 4], [u8; 4]) {
        let b = code.as_bytes();
        let mut head = [b' '; 4];
        let mut tail = [b' '; 4];
        for (d, s) in head.iter_mut().zip(b.iter()) {
            *d = *s;
        }
        let start = b.len().saturating_sub(4);
        for (d, s) in tail.iter_mut().zip(b.get(start..).unwrap_or(&[])) {
            *d = *s;
        }
        (head, tail)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conn {
    /// まだつながっていない。
    Waiting,
    /// つながった（対戦が成立した。`code_side = game` 以外の実行は、つながったときに成立している）。席と、ホスト役か（遅れの量を決める側）と、形（モードと、rollback なら D・delay 方式なら今の遅れ。ホストの画面ではホストが選んだ値。無ければ None）。
    Connected {
        seat: Seat,
        host: bool,
        shape: Option<(Mode, u8)>,
    },
    /// つながったが、対戦はまだ成立していない（両者が「妖怪 対 妖怪」を選ぶまで。`match_gate`。`code_side = game` の実行だけ）。`ready` は自分がもう選んだか、`left_secs` は成立しないまま閉じるまでの残り（秒）。
    Found {
        seat: Seat,
        shape: Option<(Mode, u8)>,
        ready: bool,
        left_secs: u32,
    },
    /// つながらないままネットワークが終わった。マッチの外へ戻るとネットワークの組を片づけて「つなぐ前」（`Waiting`）へ戻る（起動し直さずにつなぎ直せる。下の 3 つも同じ）。
    Ended,
    /// つながって遊んだ後、ネットワークが終わった（相手が抜けた・沈黙）。
    Finished,
    /// つながったが、成立しないまま閉じた。理由は `match_gate::NoMatch`。
    Unmatched(NoMatch),
    /// 相手の HiganRTool のバージョンが違うので、つながる前にネットワークが終わった（`handshake::FailReason::OtherVersion`。`theirs` は相手のバージョン、自分のバージョンは `wire::PROTO_VER`）。
    OtherVersion { theirs: u8 },
    /// 相手のビルドが違うので、つながる前にネットワークが終わった（`handshake::FailReason::OtherBuild`。`theirs` は相手のビルド番号、自分は `update::BUILD_NUMBER`）。
    OtherBuild { theirs: u32 },
    /// マッチの最中に CHECK が食い違い（自分が見つけた・相手の BYE(同期ズレ) を受けた）、そのマッチを止めている間。ゲームの更新を進めず、Esc を待つ（Esc で止めたままネットワークを閉じてゲームの入力へ戻す。戻った後は `Finished`）。
    /// `is_end` は偽: 片づけた後のメニューの場面に「Esc でメニューへ」を残さない。
    Desynced,
}

impl Conn {
    pub fn is_linked(self) -> bool {
        matches!(self, Conn::Connected { .. } | Conn::Found { .. })
    }

    /// ネットワークが終わった状態か（つながらないまま・遊んだ後・成立しないまま・バージョン違い）。ゲーム内のメニューは、ネットワークの組を片づけた後もこの行を左上に残す。
    pub fn is_end(self) -> bool {
        matches!(
            self,
            Conn::Ended
                | Conn::Finished
                | Conn::Unmatched(_)
                | Conn::OtherVersion { .. }
                | Conn::OtherBuild { .. }
        )
    }
}

const FORMATS: [Mode; 2] = [Mode::Rollback, Mode::Delay];

/// メニューの「同期方式」と「ディレイ」の今の選び。形式は ini の `mode`・遅延は ini の `delay` から始まり、← → で変える（並びは `DelayChoice::step`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeSel {
    pub mode: Mode,
    pub delay: DelayChoice,
    /// ini の `delay` から作った D（auto なら `rollback::DEFAULT_DELAY`）。遅延が auto のとき rollback の AGREE の D になる（auto はつなぐ前に D を決めない。接続の始まりにホストが旗を立てて、最初の取得で推奨値にする）。
    ini_delay: u8,
    /// メニューで変えたか（変えていなければ見張りへ指示を出さず、ini の形のまま）。
    pub touched: bool,
}

impl ShapeSel {
    fn from_ini(ini: Shape) -> ShapeSel {
        let d = ini.delay.min(MAX_PICK_DELAY);
        ShapeSel {
            mode: ini.mode,
            delay: DelayChoice::Fixed(d),
            ini_delay: d,
            touched: false,
        }
    }

    /// 見張りへ渡す、名乗る形（`Cmd::Shape` の前半）。delay 方式なら `Pick::Delay`、rollback なら D（遅延が固定ならその値、auto なら ini の値）。
    pub fn pick(self) -> Pick {
        match self.mode {
            Mode::Delay => Pick::Delay,
            Mode::Rollback => Pick::Rollback(match self.delay {
                DelayChoice::Fixed(d) => d,
                DelayChoice::Auto => self.ini_delay,
            }),
        }
    }

    fn mode_text(self, lang: Lang) -> &'static str {
        match self.mode {
            Mode::Rollback => T_FORMAT_ROLLBACK.get(lang),
            Mode::Delay => T_FORMAT_DELAY.get(lang),
        }
    }

    fn delay_text(self, line: &mut LineBuf, lang: Lang) {
        match self.delay {
            DelayChoice::Auto => {
                line.push_str(T_DELAY_AUTO.get(lang));
            }
            DelayChoice::Fixed(d) => {
                line.push_u32(u32::from(d))
                    .push_str(T_FRAMES_MARK.get(lang));
            }
        }
    }
}

/// メニューの「サイド」で選べる希望（左から 1P・2P・おまかせ。← → で端に止まる）。
const SEATS: [SeatWish; 3] = [SeatWish::P1, SeatWish::P2, SeatWish::Any];

fn seat_text(w: SeatWish, lang: Lang) -> &'static str {
    match w {
        SeatWish::P1 => T_HUD_SEAT_1.get(lang),
        SeatWish::P2 => T_HUD_SEAT_2.get(lang),
        SeatWish::Any => T_SEAT_ANY.get(lang),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// 「ランダムマッチ」（対戦の先頭。開いたときのカーソルはここ。ルームサーバーがあるときだけ）。
    Random,
    /// 対戦の「ルームマッチ」。
    Room,
    /// 観戦の「ランダムマッチ」（進行中のランダムマッチの一覧の画面へ入る。観戦の先頭。待ちが何も動いていない間だけ）。
    SpectateRandom,
    /// 観戦の「ルームマッチ」（待ちが何も動いていない間だけ）。
    Spectate,
    /// 「観戦をやめる」（観戦の待ちの間の唯一の項目）。
    SpectateCancel,
    Create,
    Paste,
    /// 「ルーム名　<今のルーム名>」（対戦の「ルームマッチ」の下。決めると対戦・観戦の「ルームマッチ」がその名で入力なしに始まる。待ちの最中は出さない）。
    RoomName,
    /// 「同期方式　◀ rollback ▶」（設定）。
    Format,
    /// 「ディレイ　◀ auto ▶」（設定。並びは auto, 0F, 1F, …, 8F）。
    Delay,
    /// 「サイド　◀ 1P ▶」（席の希望。待ち・つながりの最中は出さない: 名乗る希望は相手のコードを読んだときに決まるので、待ちの最中に変えると効くか効かないかが相手の来る時刻で揺れる）。
    Seat,
    /// 「プレイヤー名　<今の名前>」（プレイヤー名の入力へ入る。待ち・つながりの最中は出さない）。
    Name,
    /// 「言語　◀ 日本語 ▶」（設定。← → で替わると、その tick から画面の文字が替わる）。
    Lang,
    Cancel,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Play,
    Watch,
    Code,
    Settings,
}

impl Group {
    fn text(self, lang: Lang) -> &'static str {
        match self {
            Group::Play => T_GROUP_PLAY.get(lang),
            Group::Watch => T_GROUP_WATCH.get(lang),
            Group::Code => T_GROUP_CODE.get(lang),
            Group::Settings => T_GROUP_SETTINGS.get(lang),
        }
    }
}

impl Item {
    /// 「ラベル　値」の形の行のラベル（値の列を見出しごとに揃える相手）。値の無い行は `None`。
    fn value_label(self) -> Option<Tx> {
        match self {
            Item::RoomName => Some(T_ROOM_NAME),
            Item::Name => Some(T_NAME),
            Item::Seat => Some(T_SEAT),
            Item::Format => Some(T_FORMAT),
            Item::Delay => Some(T_DELAY_ROW),
            Item::Lang => Some(T_LANG),
            Item::Random
            | Item::Room
            | Item::SpectateRandom
            | Item::Spectate
            | Item::SpectateCancel
            | Item::Create
            | Item::Paste
            | Item::Cancel
            | Item::Back => None,
        }
    }

    fn group(self) -> Option<Group> {
        match self {
            Item::Random | Item::Room | Item::RoomName => Some(Group::Play),
            Item::SpectateRandom | Item::Spectate => Some(Group::Watch),
            Item::Create | Item::Paste => Some(Group::Code),
            Item::Name | Item::Seat | Item::Format | Item::Delay | Item::Lang => {
                Some(Group::Settings)
            }
            Item::SpectateCancel | Item::Cancel | Item::Back => None,
        }
    }
}

/// 「ランダムマッチ」「ルームマッチ」の項目を出すか（ini の `room_server` から決まる。呼び手が `Menu::with_room` で置く）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoomItem {
    #[default]
    Off,
    /// 出す。選ぶとルーム名の入力になり、入力は持っている名（初めの値）から始まる（`room_input::initial`: ini の `room` → 前に使った名 → 空）。
    On(RoomBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// ランダムマッチ: サーバーの待ち列に入り、来た人と組になる（ルーム名は決めない。割り当てられたルームへは受け取る側が入る）。
    Random,
    /// ルームサーバーのルームで相手を待つ（ルーム名は入力で決めた名。文字の決まり `RoomName::parse` を通っている）。
    Room(RoomBuf),
    /// 観戦者として、観戦用のルーム（対戦者が同じ名で対戦の「ルームマッチ」を選んで繋いだマッチの配る側が入るルーム）で待つ。観戦者のセッションを置く・ルームへ入るのは受け取る側。
    Spectate(RoomBuf),
    /// 観戦の「ランダムマッチ」の一覧から選んだ行。観戦用のルーム名は行のハッシュから導く。受け取る側は一覧の接続を閉じ、`Spectate` と同じに観戦者のセッションを置く。
    SpectateListed(WatchRow),
    /// 設定の「ルーム名」を決めた（ルームへは入らない）。受け取る側は、次の起動のルーム名の初めの値として覚える（ゲームのスレッドでは書かない）。
    RoomSet(RoomBuf),
    /// 観戦の待機をやめる。受け取る側は観戦の待ちをやめ、セッションを元へ戻せるなら `CodeState::Idle`、戻さないなら `CodeState::SpectateIdle` を状態に置く。
    SpectateCancel,
    Create,
    /// クリップボードを読む（自分のコードがまだ無ければ返す側として、あれば相手の返すコードとして）。
    Paste,
    Cancel,
    /// プレイヤー名を決めた（文字の決まり `NameBuf::parse` を通っている）。受け取る側は、次に開くネットワーク・次に作る接続コードからこの名前で名乗り、覚えるファイルへ書く（ゲームのスレッドでは書かない）。
    Name(NameBuf),
    /// 同期方式か遅延を変えた（つなぎ始める前なら次の接続に効く）。前半は名乗る形（rollback の D も入る）、後半は遅延の選び（auto か 0〜8。接続の始まりにホストが押下にする）。どちらの行を変えても両方を送る（指示は 1 tick に 1 つ）。
    Shape(Pick, DelayChoice),
    /// 席の希望を変えた（次の接続に効く。受け取る側は相手のコードを読んでセッションの設定を置くときに読む）。
    Seat(SeatWish),
    /// 画面の言語を替えた。受け取る側は今の言語を更新し、次の起動のために ini の `[ui] lang` へ書く（ゲームのスレッドでは書かない）。
    Lang(Lang),
}

/// 閉じ始めてから、ローダーの語が 0 にならなくてもゲームへ返すまでの tick の数（約 2 秒）。押しっぱなしの語（パッドの傾きなど）でゲームが固まらないための上限。上限で返すと、押したままのキーがゲームに「押した瞬間」と読まれうる。
pub const CLOSE_MAX_TICKS: u32 = 120;

/// 1 tick に受ける文字（UTF-16 の単位）の数の上限。人が打つ速さに十分で、台本が一度に入れる長い名前と Enter（最大 33 単位）は 3 tick で受けきる。
pub const TEXT_PER_TICK: usize = 16;

/// 1 tick の間にウィンドウへ打たれた文字（`WM_CHAR` の `wParam`）。ルーム名の入力の間だけ意味がある。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextIn {
    pub units: [u16; TEXT_PER_TICK],
    pub len: usize,
}

impl TextIn {
    pub const NONE: TextIn = TextIn {
        units: [0; TEXT_PER_TICK],
        len: 0,
    };

    pub fn as_slice(&self) -> &[u16] {
        self.units.get(..self.len).unwrap_or(&[])
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TickIn {
    /// 自分の語（キーボード・パッドを読んだ生の語。ゲームへ流す語ではない）。
    pub word: u16,
    pub note: Note,
    pub conn: Conn,
    /// この tick にウィンドウへ打たれた文字（ルーム名の入力の間だけ読む）。
    pub text: TextIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// はい: 対戦を終える（呼び手が BYE を送る）。取消はメニューを閉じた後でゲームへ通す（`Menu::take_pending_cancel`）。
    Leave,
    Stay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickOut {
    pub cmd: Option<Cmd>,
    /// このメニューがゲームの入力を握っている（呼び手はゲームへ 0 を 3 本 feed する）。
    pub hold_game: bool,
    pub answer: Option<Answer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    Open,
    /// 確かめの枠「対戦から抜けますか？」（`open_confirm`。つながった後、難易度の画面より前へ戻ろうとしたとき）。
    Confirm,
    /// ルーム名の入力（「ルームマッチ」を決めた後。ウィンドウへ打たれた文字を受ける）。
    RoomName,
    /// 観戦の「ランダムマッチ」の一覧（進行中のランダムマッチから選ぶ。一覧は呼び手が毎 tick 渡す）。
    WatchList,
    /// 閉じ始めた。語が 0 になるまで（か `CLOSE_MAX_TICKS`）ゲームへ 0 を流す。数は経った tick。
    Closing(u32),
}

/// ルーム名の入力が何のためか（決めたときの指示と、入力の画面の題が変わる。文字の受け方・初めの値・決まりは同じ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditFor {
    Wait,
    Spectate,
    /// プレイヤー名（`Cmd::Name`。文字の組はルーム名に空白を足したもの）。
    Name,
    /// 設定の「ルーム名」（決めるだけ。指示は出さない。次の「ルームマッチ」がこの名で始まる）。
    RoomSet,
}

#[derive(Debug, Clone, Copy)]
pub struct Menu {
    state: State,
    cursor: usize,
    shape: ShapeSel,
    /// 席の希望（`with_seat`。置かなければ「サイド」の項目を出さない）。
    seat: Option<SeatWish>,
    prev: u16,
    /// 確かめの枠で「はい」と答えた後、メニューを閉じ終えたらゲームへ通す取消が残っている。
    pending_cancel: bool,
    /// 見張りのネットワークがまだ開いていない（最初の操作で開く。開く間は「準備しています」を出す。呼び手が毎 tick 置く）。
    opening: bool,
    /// 「ルームマッチ」の項目（`with_room`。`On` の中身は入力の初めの値）。
    room: RoomItem,
    edit: RoomInput,
    edit_for: EditFor,
    edit_err: Option<RoomNameWhy>,
    /// 使えない文字を捨てた直後か（入力の画面に `T_ROOM_INPUT_DROPPED` を出す。次に使える文字か Enter・Backspace を受けると消える）。
    edit_dropped: bool,
    /// Enter の文字（`WM_CHAR` の 0x0D）を決定として読んでよいか。入力を開いたときは false で、ゲームの語の決定のビット（`DECIDE_BITS`）が 0 の tick を 1 回見たら true（決めた Enter を離したのを見てから）。
    /// 「ルームマッチ」を決めた Enter の文字が遅れて届くのも、押したままのキーの自動繰り返しの文字も、その Enter が押されている間に来るので、tick の数の猶予でなく離したことを条件にして 1 つで塞ぐ。文字そのもの（Enter 以外）は armed でなくても受ける。
    enter_armed: bool,
    /// ウィンドウから文字を受ける仕組みが使えない（呼び手が置く。入力の画面に断りの行を出す）。
    text_unavailable: bool,
    /// 入力から戻った後、ゲームの語が全部離れるまで押下を読まない（決めた Enter の語の立ち上がりが遅れて届いて、戻った先の項目「ルームマッチ」をもう一度決めてしまわないため）。数は待った tick（`CLOSE_MAX_TICKS` で切る）。
    quiet: Option<u32>,
    /// 今のプレイヤー名（`with_player_name`。`None` なら「名前」の項目を出さない）。打てない文字があれば `?` に直した表示用の形（`NameBuf::lossy`）。決めると今決めた名前へ替わる。
    name: Option<NameBuf>,
    name_edit: NameInput,
    name_err: Option<NameWhy>,
    /// 画面の言語（呼び手が `with_lang` で置く。「言語」の行で替えると自分で更新し、`Cmd::Lang` を返す）。
    lang: Lang,
    /// 開いた直後で、カーソルをまだ初めの項目（先頭の「ランダムマッチ」。ルームサーバーが無ければ「コード作成」）へ置いていない。
    home: bool,
    /// カーソルが前の tick に指していた項目。ルーム名・名前・サイドの行が出入りしても、同じ項目に残すために覚える。
    at: Option<Item>,
    /// 観戦の「ランダムマッチ」の一覧の画面のカーソル（行の中身で覚える。一覧が入れ替わっても、別の行を選ばない）と、表示の先頭の行の番号。
    watch_sel: WatchSel,
    watch_top: usize,
}

/// 一覧の画面のカーソルの位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchSel {
    /// 動かしていない。選べる最初の行（無ければ「もどる」）。
    Auto,
    /// この観戦用のルームの行（ハッシュで覚える）。
    Row([u8; 8]),
    Back,
}

/// 一覧の画面に出す行の数（これを越える分はスクロールして、上下の「ほか N 件」で知らせる）。
pub const WATCH_VISIBLE: usize = 8;
/// 一覧の 1 行の名前の欄の幅（文字）。越える名前は切って出す。
const WATCH_NAME_CELLS: usize = 12;

/// 項目の数の上限（ランダムマッチ・ルームマッチ・ルーム名・観戦のランダムマッチ・観戦のルームマッチ・作成・入力・名前・サイド・同期方式・ディレイ・言語・もどる = 13。待ちの間は名前・ルーム名・サイドが無く、代わりに待機をやめるが 1 つ入る）に余裕を 1 つ足して 14。
const ITEMS_MAX: usize = 14;

type Items = ([Item; ITEMS_MAX], usize);

/// メニューの行の数の上限（隙間の行も数える）。待ちの外の Idle で全部の項目が出る組が最大で、見出し 4・項目 13・隙間 6（対戦と観戦のランダムマッチの下に小 2・見出しの前に中 3・もどるの前に中）・状況の前後の隙間 2・状況 3（貼った文字数・文 2 行）・キー 1 = 29。
/// 待ちの間は項目が最大 9（ランダムマッチ・ルームマッチ・コード作成・コード入力・同期方式・ディレイ・言語・待機をやめる・もどる）・見出し 3・隙間 4・状況 5 行・その前後の隙間 2・キー 1 = 24 で届かない。
/// 検査 `every_menu_row_fits_the_box_and_the_screen` が数え、`tallest == MENU_ROWS`（29）で縛る。枠の高さ（隙間は行の高さより低い。大きな隙間は 3 か所とも中くらいにして 468 画素）は同じ検査が画面の 480 画素と比べる。
pub const MENU_ROWS: usize = 29;
const STATUS_ROWS: usize = 5;

#[derive(Debug, Clone, Copy)]
pub struct MenuRows {
    pub lines: [LineBuf; MENU_ROWS],
    pub styles: [RowStyle; MENU_ROWS],
    pub len: usize,
}

impl MenuRows {
    pub fn new() -> MenuRows {
        MenuRows {
            lines: [LineBuf::new(); MENU_ROWS],
            styles: [RowStyle::Normal; MENU_ROWS],
            len: 0,
        }
    }

    fn next(&mut self, style: RowStyle) -> &mut LineBuf {
        let i = self.len.min(MENU_ROWS - 1);
        if self.len < MENU_ROWS {
            self.len += 1;
        }
        if let Some(s) = self.styles.get_mut(i) {
            *s = style;
        }
        self.lines[i].clear();
        &mut self.lines[i]
    }
}

impl Default for MenuRows {
    fn default() -> Self {
        MenuRows::new()
    }
}

impl Menu {
    /// `ini` は ini の形（メニューの「同期方式」「ディレイ」の初めの値）。
    pub fn new(ini: Shape) -> Menu {
        Menu {
            state: State::Closed,
            cursor: 0,
            shape: ShapeSel::from_ini(ini),
            seat: None,
            prev: 0,
            pending_cancel: false,
            opening: false,
            room: RoomItem::Off,
            edit: RoomInput::default(),
            edit_for: EditFor::Wait,
            edit_err: None,
            edit_dropped: false,
            enter_armed: false,
            text_unavailable: false,
            quiet: None,
            name: None,
            name_edit: NameInput::default(),
            name_err: None,
            lang: Lang::default(),
            home: false,
            at: None,
            watch_sel: WatchSel::Auto,
            watch_top: 0,
        }
    }

    pub fn with_lang(mut self, lang: Lang) -> Menu {
        self.lang = lang;
        self
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    pub fn with_seat(mut self, wish: SeatWish) -> Menu {
        self.seat = Some(wish);
        self
    }

    pub fn seat(&self) -> Option<SeatWish> {
        self.seat
    }

    pub fn with_player_name(mut self, name: NameBuf) -> Menu {
        self.name = Some(name);
        self
    }

    pub fn with_room(mut self, room: RoomItem) -> Menu {
        self.room = room;
        self
    }

    pub fn set_text_available(&mut self, ok: bool) {
        self.text_unavailable = !ok;
    }

    /// ルーム名の入力の間か（呼び手がこれを見て、ウィンドウから文字を貯めるゲートを開け閉めする）。
    pub fn wants_text(&self) -> bool {
        self.state == State::RoomName
    }

    fn room_initial(&self) -> RoomBuf {
        match self.room {
            RoomItem::On(b) => b,
            RoomItem::Off => RoomBuf::new(),
        }
    }

    fn begin_room_input(&mut self, for_: EditFor) {
        self.state = State::RoomName;
        self.edit_for = for_;
        self.edit = RoomInput::new(self.room_initial());
        self.edit_err = None;
        self.edit_dropped = false;
        self.enter_armed = false;
    }

    /// 「ルームマッチ」を決めた。ルーム名が決まっていれば（設定の「ルーム名」・ini の `room`・前に使った名）その名で始まる指示を返し、空か文字の決まりの外ならルーム名の入力の画面へ入る（入力で決めると、そのまま始まる）。
    fn start_room(&mut self, for_: EditFor) -> Option<Cmd> {
        match self.room {
            RoomItem::On(name) if name.parse().is_ok() => Some(match for_ {
                EditFor::Spectate => Cmd::Spectate(name),
                EditFor::Wait | EditFor::Name | EditFor::RoomSet => Cmd::Room(name),
            }),
            RoomItem::On(_) | RoomItem::Off => {
                self.begin_room_input(for_);
                None
            }
        }
    }

    fn begin_name_input(&mut self) {
        self.state = State::RoomName;
        self.edit_for = EditFor::Name;
        let now = self.name.unwrap_or_default();
        self.name_edit = NameInput::new(if now.is_editable() {
            now
        } else {
            NameBuf::new()
        });
        self.name_err = None;
        self.edit_dropped = false;
        self.enter_armed = false;
    }

    fn end_room_input(&mut self) {
        self.state = State::Open;
        self.quiet = Some(0);
    }

    /// 確かめの枠を開く（つながった後、難易度の画面より前へ戻ろうとした取消を飲み込んだとき）。初めの項目は「いいえ」（間違えて決定しても何も起きない側）。`held` は開く tick の語。
    pub fn open_confirm(&mut self, held: u16) {
        self.state = State::Confirm;
        self.cursor = 1;
        self.prev = held;
        self.pending_cancel = false;
    }

    pub fn is_confirm(&self) -> bool {
        self.state == State::Confirm
    }

    pub fn has_pending_cancel(&self) -> bool {
        self.pending_cancel
    }

    pub fn take_pending_cancel(&mut self) -> bool {
        core::mem::take(&mut self.pending_cancel)
    }

    pub fn set_opening(&mut self, on: bool) {
        self.opening = on;
    }

    /// 入口（`ui_entry::leaks`）で呼ぶ。`held` はそのときの自分の語（押したままのキーを押した瞬間と読まない）。
    pub fn open(&mut self, held: u16) {
        self.state = State::Open;
        self.cursor = 0;
        self.home = true;
        self.at = None;
        self.prev = held;
        self.pending_cancel = false;
        self.quiet = None;
    }

    pub fn is_active(&self) -> bool {
        self.state != State::Closed
    }

    /// 入口のメニューが開いている（閉じ始め・確かめの枠は含まない）。つながった瞬間にこれが真の側は「妖怪 対 妖怪」を選んだものとして待つ。
    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Open | State::RoomName | State::WatchList)
    }

    /// 観戦の「ランダムマッチ」の一覧の画面か。呼び手はこれが真の間だけ一覧の接続を持ち（偽になったら閉じ）、毎 tick の一覧を `tick_with` / `render_with` へ渡す。
    pub fn wants_watch_list(&self) -> bool {
        self.state == State::WatchList
    }

    fn begin_watch_list(&mut self) {
        self.state = State::WatchList;
        self.watch_sel = WatchSel::Auto;
        self.watch_top = 0;
    }

    /// 一覧の画面のカーソルの位置（0..行数。行数と同じ値は「もどる」）と、表示の先頭の行。副作用なし（`tick` が結果を覚え、`render` が同じ値を使う）。
    fn watch_view(&self, rows: &[WatchRow]) -> (usize, usize) {
        let n = rows.len();
        let cursor = match self.watch_sel {
            WatchSel::Back => n,
            WatchSel::Row(h) => rows
                .iter()
                .position(|r| r.hash == h)
                .unwrap_or_else(|| watch_first_selectable(rows)),
            WatchSel::Auto => watch_first_selectable(rows),
        };
        let last_top = n.saturating_sub(WATCH_VISIBLE);
        let mut top = self.watch_top.min(last_top);
        if cursor == n {
            // 「もどる」は最後の行の下。最後の行が見える位置まで送る。
            top = last_top;
        } else if cursor < top {
            top = cursor;
        } else if cursor >= top + WATCH_VISIBLE {
            top = cursor + 1 - WATCH_VISIBLE;
        }
        (cursor, top)
    }

    /// 一覧の画面の行を作る。題・（8 行を越えるときだけ）上の「ほか N 件」・行・下の「ほか N 件」・「もどる」・キーの行。
    /// 8 行を越える一覧は、上下の行を空の行でも残して、スクロールしても枠の高さが変わらないようにする。
    fn render_watch_list(&self, rows: &mut MenuRows, watch: &WatchSnapshot) {
        let lang = self.lang;
        let list = watch.rows();
        let n = list.len();
        let (cursor, top) = self.watch_view(list);
        rows.next(RowStyle::Title)
            .push_str(T_SPECTATE.get(lang))
            .push_str(T_WATCH_DASH.get(lang))
            .push_str(T_RANDOM.get(lang));
        if n == 0 {
            match watch.status {
                ListStatus::Loading => {
                    rows.next(RowStyle::Hint)
                        .push_str("  ")
                        .push_str(T_WATCH_LOADING.get(lang));
                }
                ListStatus::Ready => {
                    rows.next(RowStyle::Hint)
                        .push_str("  ")
                        .push_str(T_WATCH_EMPTY.get(lang));
                }
                ListStatus::Failed => {
                    rows.next(RowStyle::Warn)
                        .push_str("  ")
                        .push_str(T_WATCH_FAILED.get(lang));
                }
            }
        } else {
            let scroll = n > WATCH_VISIBLE;
            let more = |line: &mut LineBuf, open: Tx, count: usize| {
                if count > 0 {
                    line.push_str("  ")
                        .push_str(open.get(lang))
                        .push_u32(count as u32)
                        .push_str(T_WATCH_MORE_CLOSE.get(lang));
                }
            };
            if scroll {
                more(rows.next(RowStyle::Hint), T_WATCH_MORE_UP, top);
            }
            for (i, r) in list.iter().enumerate().skip(top).take(WATCH_VISIBLE) {
                let sel = i == cursor;
                let style = if sel {
                    RowStyle::Selected
                } else if r.is_full() {
                    RowStyle::Hint
                } else {
                    RowStyle::Normal
                };
                let line = rows.next(style);
                line.push_str(if sel { "> " } else { "  " });
                watch_row_text(line, r, lang);
            }
            if scroll {
                let below = n.saturating_sub(top + WATCH_VISIBLE);
                more(rows.next(RowStyle::Hint), T_WATCH_MORE_DOWN, below);
            }
        }
        rows.next(RowStyle::GapMedium);
        let back = cursor == n;
        rows.next(if back {
            RowStyle::Selected
        } else {
            RowStyle::Normal
        })
        .push_str(if back { "> " } else { "  " })
        .push_str(T_BACK.get(lang));
        rows.next(RowStyle::GapMedium);
        rows.next(RowStyle::Hint).push_str(T_KEYS_WATCH.get(lang));
    }

    /// 一覧の画面を 1 tick 進める。↑↓ は選べる行（満員でない行と「もどる」）だけを回る。
    fn tick_watch_list(&mut self, pressed: u16, rows: &[WatchRow], out: &mut TickOut) {
        let n = rows.len();
        let (cursor, top) = self.watch_view(rows);
        self.watch_top = top;
        // 動かしていない間は、一覧が変わっても「選べる最初の行」を指し続ける。動かしたら行の中身で覚える。
        let sel_of = |idx: usize| match rows.get(idx) {
            Some(r) => WatchSel::Row(r.hash),
            None => WatchSel::Back,
        };
        if pressed & CANCEL_BITS != 0 {
            self.state = State::Open;
            return;
        }
        let step = |from: usize, forward: bool| {
            let mut i = from;
            for _ in 0..=n {
                i = if forward {
                    (i + 1) % (n + 1)
                } else {
                    (i + n) % (n + 1)
                };
                if rows.get(i).is_none_or(|r| !r.is_full()) {
                    return i;
                }
            }
            n
        };
        if pressed & keys::UP != 0 {
            let to = step(cursor, false);
            self.watch_sel = sel_of(to);
            self.watch_top = self.watch_view(rows).1;
        } else if pressed & keys::DOWN != 0 {
            let to = step(cursor, true);
            self.watch_sel = sel_of(to);
            self.watch_top = self.watch_view(rows).1;
        } else if pressed & DECIDE_BITS != 0 {
            match rows.get(cursor) {
                // 満員の行は選べない（満員になった行の上にカーソルが残っていても何も起きない）。
                Some(r) if r.is_full() => {}
                Some(r) => {
                    out.cmd = Some(Cmd::SpectateListed(*r));
                    self.state = State::Open;
                }
                None => self.state = State::Open,
            }
        }
    }

    pub fn shape(&self) -> ShapeSel {
        self.shape
    }

    /// 遅延の初めの値を auto にする（ini の `delay = auto` か、書いていない）。メニューで変えたことにはしない（見張りへ指示は出さない。名乗る形は ini のまま）。
    pub fn with_start_auto(mut self, auto: bool) -> Menu {
        if auto {
            self.shape.delay = DelayChoice::Auto;
        }
        self
    }

    pub fn reset(&mut self) {
        self.state = State::Closed;
        self.pending_cancel = false;
        self.quiet = None;
    }

    fn items(&self, conn: Conn, state: CodeState) -> Items {
        let mut v = [Item::Back; ITEMS_MAX];
        if conn != Conn::Waiting {
            return (v, 1);
        }
        let mut n = 0;
        let mut push = |it: Item| {
            if let Some(s) = v.get_mut(n) {
                *s = it;
                n += 1;
            }
        };
        // 観戦の待ちの間は「観戦をやめる」だけ（作る・貼る・ルームで待つ・対戦の形は観戦者のセッションでは使えない）。
        if state.is_spectate_busy() {
            push(Item::SpectateCancel);
            return (v, n);
        }
        if state == CodeState::SpectateIdle {
            if self.room != RoomItem::Off {
                push(Item::SpectateRandom);
                push(Item::Spectate);
            }
            push(Item::Back);
            return (v, n);
        }
        // 対戦の「ランダムマッチ」「ルームマッチ」と、観戦の「ルームマッチ」は ini に `room_server` があるときだけ。対戦の2つは待っている間も出して、選び直せる。
        // 観戦は何かを待っている間は出さない: 項目が増えて枠が 480 画素の画面を越えるのを避けるのと、待ちを動かしたまま観戦者のセッションへ替わる半端な状態を作らないため。
        // ルーム名の行は対戦の「ルームマッチ」の下。何も待っていない間だけ（待ちの中に変えてもその待ちには効かない）。ネットワークを開いている最中も出さない。
        let idle = state == CodeState::Idle && !self.opening;
        if self.room != RoomItem::Off {
            push(Item::Random);
            push(Item::Room);
            if idle {
                push(Item::RoomName);
            }
            if state == CodeState::Idle {
                push(Item::SpectateRandom);
                push(Item::Spectate);
            }
        }
        push(Item::Create);
        push(Item::Paste);
        // 設定の並びは プレイヤー名・サイド・同期方式・ディレイ・言語。プレイヤー名・サイドは何も待っていない間だけ。
        if self.name.is_some() && idle {
            push(Item::Name);
        }
        if self.seat.is_some() && idle {
            push(Item::Seat);
        }
        push(Item::Format);
        push(Item::Delay);
        push(Item::Lang);
        if state != CodeState::Idle {
            push(Item::Cancel);
        }
        push(Item::Back);
        (v, n)
    }

    /// 今の項目の並びに合わせたカーソルの位置。開いた直後は先頭の項目（「ランダムマッチ」）へ置く。
    /// 行が出入りしたときは、前に指していた項目の今の位置へ動かして、同じ項目に残す（その項目が消えたら元の位置の近く）。
    fn settled_cursor(&self, items: &Items) -> usize {
        let (list, n) = items;
        let last = n.saturating_sub(1);
        if self.home {
            return 0;
        }
        if let Some(at) = self.at
            && let Some(i) = list.iter().take(*n).position(|&it| it == at)
        {
            return i;
        }
        self.cursor.min(last)
    }

    /// 一覧の無い呼び手（一覧の画面を開く項目を使わない検査）用。
    pub fn tick(&mut self, i: &TickIn) -> TickOut {
        self.tick_with(i, &WatchSnapshot::EMPTY)
    }

    /// `watch` は観戦の「ランダムマッチ」の一覧（一覧の画面の間だけ読む）。
    pub fn tick_with(&mut self, i: &TickIn, watch: &WatchSnapshot) -> TickOut {
        let word = i.word;
        let pressed = word & !self.prev;
        self.prev = word;
        match self.state {
            State::Closed => TickOut::default(),
            State::WatchList => {
                let mut out = TickOut {
                    cmd: None,
                    hold_game: true,
                    answer: None,
                };
                if i.conn.is_linked() {
                    self.state = State::Closing(0);
                    return out;
                }
                self.tick_watch_list(pressed, watch.rows(), &mut out);
                out
            }
            State::Closing(n) => {
                self.state = if word == 0 || n >= CLOSE_MAX_TICKS {
                    State::Closed
                } else {
                    State::Closing(n + 1)
                };
                TickOut {
                    cmd: None,
                    hold_game: true,
                    answer: None,
                }
            }
            State::Confirm => {
                let mut out = TickOut {
                    cmd: None,
                    hold_game: true,
                    answer: None,
                };
                if !i.conn.is_linked() {
                    self.state = State::Closing(0);
                    return out;
                }
                if pressed & CANCEL_BITS != 0 {
                    out.answer = Some(Answer::Stay);
                    self.state = State::Closing(0);
                } else if pressed & (keys::UP | keys::DOWN) != 0 {
                    self.cursor = usize::from(self.cursor == 0);
                } else if pressed & DECIDE_BITS != 0 {
                    if self.cursor == 0 {
                        out.answer = Some(Answer::Leave);
                        self.pending_cancel = true;
                    } else {
                        out.answer = Some(Answer::Stay);
                    }
                    self.state = State::Closing(0);
                }
                out
            }
            State::RoomName => {
                let mut out = TickOut {
                    cmd: None,
                    hold_game: true,
                    answer: None,
                };
                if i.conn.is_linked() {
                    self.state = State::Closing(0);
                    return out;
                }
                // やめる（Esc。パッドの Esc のボタンも同じ語）。Z・X は文字なので決定・取消にしない。文字（`i.text`）より先に見る: 同じ tick に Esc と文字が来ても、やめるが勝つ。
                if pressed & keys::ESC != 0 {
                    self.end_room_input();
                    return out;
                }
                // この tick の文字は、前の tick までに離したのを見た Enter だけを決定にする（離した tick に届く文字は、押している間に生まれた文字の最後の 1 つかもしれない）。
                let armed = self.enter_armed;
                if word & DECIDE_BITS == 0 {
                    self.enter_armed = true;
                }
                for &unit in i.text.as_slice() {
                    let edit = match self.edit_for {
                        EditFor::Name => self.name_edit.feed(unit),
                        EditFor::Wait | EditFor::Spectate | EditFor::RoomSet => {
                            self.edit.feed(unit)
                        }
                    };
                    // 使えない文字の一言は、次に何か受けたら消す。同じ tick の中は後に来たものが勝つ。
                    self.edit_dropped = edit == Edit::Rejected;
                    match edit {
                        Edit::Nothing | Edit::Rejected => {}
                        Edit::Changed => {
                            self.edit_err = None;
                            self.name_err = None;
                        }
                        Edit::Enter if self.edit_for == EditFor::Name => {
                            if !armed {
                                continue;
                            }
                            match self.name_edit.text().parse() {
                                Ok(n) => {
                                    self.name = Some(n);
                                    out.cmd = Some(Cmd::Name(n));
                                    self.end_room_input();
                                    return out;
                                }
                                Err(e) => self.name_err = Some(e),
                            }
                        }
                        Edit::Enter => {
                            if !armed {
                                // 入力を開いた Enter（まだ押されている）の文字、またはその自動繰り返し。
                                continue;
                            }
                            match self.edit.text().parse() {
                                Ok(_) => {
                                    let name = *self.edit.text();
                                    // 次の「ルームマッチ」の名は、今決めた名（ini や前回の名より新しい）。
                                    self.room = RoomItem::On(name);
                                    out.cmd = match self.edit_for {
                                        // 名前の入力の Enter は上の枠が先に受ける（ここへは来ない）。
                                        EditFor::Wait | EditFor::Name => Some(Cmd::Room(name)),
                                        EditFor::Spectate => Some(Cmd::Spectate(name)),
                                        // 設定の「ルーム名」はルームへは入らない。決めた名を覚える指示だけ出す。
                                        EditFor::RoomSet => Some(Cmd::RoomSet(name)),
                                    };
                                    self.end_room_input();
                                    return out;
                                }
                                Err(e) => self.edit_err = Some(RoomNameWhy::of(e)),
                            }
                        }
                    }
                }
                out
            }
            State::Open => {
                let mut out = TickOut {
                    cmd: None,
                    hold_game: true,
                    answer: None,
                };
                if i.conn.is_linked() {
                    self.state = State::Closing(0);
                    return out;
                }
                let pressed = match self.quiet {
                    Some(n) => {
                        self.quiet = if word == 0 || n >= CLOSE_MAX_TICKS {
                            None
                        } else {
                            Some(n + 1)
                        };
                        0
                    }
                    None => pressed,
                };
                let (items, n) = self.items(i.conn, i.note.state);
                self.cursor = self.settled_cursor(&(items, n));
                self.home = false;
                let cur = items.get(self.cursor).copied().unwrap_or(Item::Back);
                if pressed & CANCEL_BITS != 0 {
                    self.state = State::Closing(0);
                    return out;
                }
                if pressed & keys::UP != 0 {
                    self.cursor = (self.cursor + n - 1) % n.max(1);
                } else if pressed & keys::DOWN != 0 {
                    self.cursor = (self.cursor + 1) % n.max(1);
                } else if pressed & (keys::LEFT | keys::RIGHT) != 0 {
                    let by: i8 = if pressed & keys::LEFT != 0 { -1 } else { 1 };
                    if cur == Item::Format {
                        let idx = FORMATS
                            .iter()
                            .position(|&m| m == self.shape.mode)
                            .unwrap_or(0);
                        let next = if by < 0 {
                            idx.saturating_sub(1)
                        } else {
                            (idx + 1).min(FORMATS.len() - 1)
                        };
                        if next != idx {
                            self.shape.mode = FORMATS[next];
                            self.shape.touched = true;
                            out.cmd = Some(Cmd::Shape(self.shape.pick(), self.shape.delay));
                        }
                    } else if cur == Item::Delay {
                        let next = self.shape.delay.step(by);
                        if next != self.shape.delay {
                            self.shape.delay = next;
                            self.shape.touched = true;
                            out.cmd = Some(Cmd::Shape(self.shape.pick(), self.shape.delay));
                        }
                    } else if cur == Item::Lang {
                        self.lang = self.lang.next();
                        out.cmd = Some(Cmd::Lang(self.lang));
                    } else if cur == Item::Seat
                        && let Some(now) = self.seat
                    {
                        let idx = SEATS.iter().position(|&w| w == now).unwrap_or(0);
                        let next = if pressed & keys::LEFT != 0 {
                            idx.saturating_sub(1)
                        } else {
                            (idx + 1).min(SEATS.len() - 1)
                        };
                        if next != idx {
                            self.seat = Some(SEATS[next]);
                            out.cmd = Some(Cmd::Seat(SEATS[next]));
                        }
                    }
                } else if pressed & DECIDE_BITS != 0 {
                    match cur {
                        Item::Random => out.cmd = Some(Cmd::Random),
                        Item::Room => out.cmd = self.start_room(EditFor::Wait),
                        Item::SpectateRandom => self.begin_watch_list(),
                        Item::Spectate => out.cmd = self.start_room(EditFor::Spectate),
                        Item::RoomName => self.begin_room_input(EditFor::RoomSet),
                        Item::Name => self.begin_name_input(),
                        Item::SpectateCancel => out.cmd = Some(Cmd::SpectateCancel),
                        Item::Create => out.cmd = Some(Cmd::Create),
                        Item::Paste => out.cmd = Some(Cmd::Paste),
                        Item::Cancel => out.cmd = Some(Cmd::Cancel),
                        Item::Back => self.state = State::Closing(0),
                        Item::Format | Item::Delay | Item::Seat | Item::Lang => {}
                    }
                }
                self.at = items.get(self.cursor).copied();
                out
            }
        }
    }

    /// 開いているメニューの行を作る（毎 tick か、行が変わりうるときに呼ぶ）。`now` は UNIX 秒。
    pub fn render(&self, conn: Conn, note: &Note, now: u64, rows: &mut MenuRows) {
        self.render_with(conn, note, now, rows, &WatchSnapshot::EMPTY);
    }

    /// `watch` は観戦の「ランダムマッチ」の一覧（一覧の画面の間だけ読む）。
    pub fn render_with(
        &self,
        conn: Conn,
        note: &Note,
        now: u64,
        rows: &mut MenuRows,
        watch: &WatchSnapshot,
    ) {
        let lang = self.lang;
        rows.len = 0;
        if self.state == State::WatchList {
            self.render_watch_list(rows, watch);
            return;
        }
        if self.state == State::Confirm {
            rows.next(RowStyle::Title).push_str(T_LEAVE_TITLE.get(lang));
            rows.next(RowStyle::Normal);
            for (i, label) in [T_YES.get(lang), T_NO.get(lang)].into_iter().enumerate() {
                let sel = i == self.cursor.min(1);
                let line = rows.next(if sel {
                    RowStyle::Selected
                } else {
                    RowStyle::Normal
                });
                line.push_str(if sel { "> " } else { "  " }).push_str(label);
            }
            rows.next(RowStyle::Normal);
            rows.next(RowStyle::Hint).push_str(T_KEYS_CONFIRM.get(lang));
            return;
        }
        if self.state == State::RoomName && self.edit_for == EditFor::Name {
            rows.next(RowStyle::Title).push_str(T_NAME.get(lang));
            rows.next(RowStyle::Normal);
            rows.next(RowStyle::Selected)
                .push_str(T_NAME_INPUT_LABEL.get(lang))
                .push_str(self.name_edit.text().as_str())
                .push_str(T_ROOM_CURSOR.get(lang));
            if self.name_err.is_some() {
                rows.next(RowStyle::Warn)
                    .push_str(T_NAME_INPUT_EMPTY.get(lang));
            }
            if self.edit_dropped {
                rows.next(RowStyle::Warn)
                    .push_str(T_NAME_INPUT_DROPPED.get(lang));
            }
            if self.text_unavailable {
                rows.next(RowStyle::Warn)
                    .push_str(T_ROOM_INPUT_NO_KEYS.get(lang));
                rows.next(RowStyle::Warn)
                    .push_str(T_NAME_INPUT_NO_KEYS_NEXT.get(lang));
            }
            rows.next(RowStyle::Normal);
            rows.next(RowStyle::Hint)
                .push_str(T_NAME_INPUT_KEYS.get(lang));
            return;
        }
        if self.state == State::RoomName {
            rows.next(RowStyle::Title).push_str(match self.edit_for {
                EditFor::Wait => T_ROOM.get(lang),
                EditFor::Spectate => T_SPECTATE.get(lang),
                EditFor::RoomSet => T_ROOM_NAME.get(lang),
                EditFor::Name => T_NAME.get(lang),
            });
            rows.next(RowStyle::Normal);
            rows.next(RowStyle::Selected)
                .push_str(T_ROOM_INPUT_LABEL.get(lang))
                .push_str(self.edit.text().as_str())
                .push_str(T_ROOM_CURSOR.get(lang));
            if let Some(why) = self.edit_err {
                room_name_why_line(rows.next(RowStyle::Warn), lang, why);
            }
            if self.edit_dropped {
                rows.next(RowStyle::Warn)
                    .push_str(T_ROOM_INPUT_DROPPED.get(lang));
            }
            if self.text_unavailable {
                rows.next(RowStyle::Warn)
                    .push_str(T_ROOM_INPUT_NO_KEYS.get(lang));
                rows.next(RowStyle::Warn)
                    .push_str(T_ROOM_INPUT_NO_KEYS_NEXT.get(lang));
            }
            rows.next(RowStyle::Normal);
            rows.next(RowStyle::Hint)
                .push_str(T_ROOM_INPUT_KEYS.get(lang));
            return;
        }
        let (items, n) = self.items(conn, note.state);
        let cursor = self.settled_cursor(&(items, n));
        let waiting_on = if conn == Conn::Waiting {
            wait_origin(note, now)
        } else {
            None
        };
        let mut group = None;
        for (i, item) in items.iter().take(n).enumerate() {
            if item.group() != group {
                group = item.group();
                match group {
                    // 見出しの前は、上に項目があるときだけ中くらいの隙間で離す。
                    Some(g) => {
                        if i > 0 {
                            rows.next(RowStyle::GapMedium);
                        }
                        rows.next(RowStyle::Title).push_str(g.text(lang));
                    }
                    // 見出しの付かない項目（待機をやめる・もどる）は、上に項目があるときだけ中くらいの隙間で離す。
                    None if i > 0 => {
                        rows.next(RowStyle::GapMedium);
                    }
                    None => {}
                }
            } else if (*item == Item::Room && i > 0 && items.get(i - 1) == Some(&Item::Random))
                || (*item == Item::Spectate
                    && i > 0
                    && items.get(i - 1) == Some(&Item::SpectateRandom))
            {
                // 対戦・観戦のそれぞれで、ランダムマッチとルームマッチを小さな隙間で分ける。
                rows.next(RowStyle::GapSmall);
            }
            // 同じ見出しの「ラベル　値」の行は、値の始まりを揃える。
            let label_cells = items
                .iter()
                .take(n)
                .filter(|o| o.group() == item.group())
                .filter_map(|o| o.value_label())
                .map(|t| cells_of(t.get(lang)))
                .max()
                .unwrap_or(0);
            let sel = i == cursor;
            let style = if sel {
                RowStyle::Selected
            } else {
                RowStyle::Normal
            };
            let line = rows.next(style);
            line.push_str(if sel { "> " } else { "  " });
            match item {
                Item::Random => {
                    line.push_str(T_RANDOM.get(lang));
                }
                Item::Room => {
                    line.push_str(T_ROOM.get(lang));
                }
                Item::SpectateRandom => {
                    line.push_str(T_RANDOM.get(lang));
                }
                Item::Spectate => {
                    line.push_str(T_ROOM.get(lang));
                }
                Item::SpectateCancel => {
                    line.push_str(T_SPECTATE_CANCEL.get(lang));
                }
                Item::Create => {
                    line.push_str(T_CREATE.get(lang));
                }
                Item::Paste => {
                    line.push_str(
                        if matches!(note.state, CodeState::Offered | CodeState::Linking) {
                            T_PASTE_REPLY.get(lang)
                        } else {
                            T_PASTE.get(lang)
                        },
                    );
                }
                Item::Format => {
                    push_choice(line, lang, T_FORMAT, label_cells, |l| {
                        l.push_str(self.shape.mode_text(lang));
                    });
                }
                Item::Delay => {
                    push_choice(line, lang, T_DELAY_ROW, label_cells, |l| {
                        self.shape.delay_text(l, lang);
                    });
                }
                Item::Seat => {
                    push_choice(line, lang, T_SEAT, label_cells, |l| {
                        l.push_str(seat_text(self.seat.unwrap_or(SeatWish::P1), lang));
                    });
                }
                Item::Lang => {
                    push_choice(line, lang, T_LANG, label_cells, |l| {
                        l.push_str(
                            match self.lang {
                                Lang::Ja => T_LANG_JA,
                                Lang::En => T_LANG_EN,
                            }
                            .get(lang),
                        );
                    });
                }
                Item::Name => {
                    push_label(line, lang, T_NAME, label_cells);
                    line.push_str(self.name.unwrap_or_default().as_str());
                }
                Item::RoomName => {
                    push_label(line, lang, T_ROOM_NAME, label_cells);
                    let name = self.room_initial();
                    line.push_str(if name.is_empty() {
                        T_ROOM_UNSET.get(lang)
                    } else {
                        name.as_str()
                    });
                }
                Item::Cancel => {
                    line.push_str(T_CANCEL.get(lang));
                }
                Item::Back => {
                    line.push_str(T_BACK.get(lang));
                }
            }
            if let Some((from, secs)) = waiting_on
                && from == *item
            {
                line.push_str(T_WAITING_MARK.get(lang)).push_clock(secs);
            }
        }
        let mut status = [LineBuf::new(); STATUS_ROWS];
        let mut warn = [false; STATUS_ROWS];
        let k = status_lines(conn, note, now, self.opening, lang, &mut status, &mut warn);
        if k > 0 {
            rows.next(RowStyle::GapMedium);
            for (l, w) in status.iter().zip(warn).take(k) {
                let style = if w { RowStyle::Warn } else { RowStyle::Normal };
                rows.next(style).push_str(l.as_str());
            }
        }
        rows.next(RowStyle::GapMedium);
        rows.next(RowStyle::Hint).push_str(T_KEYS.get(lang));
    }
}

/// 一覧の中で選べる（満員でない）最初の行の番号。無ければ行数（「もどる」）。
fn watch_first_selectable(rows: &[WatchRow]) -> usize {
    rows.iter().position(|r| !r.is_full()).unwrap_or(rows.len())
}

/// 一覧の 1 行の本文: 「1P 名 vs 2P 名　観戦者の数/上限」。名前は `WATCH_NAME_CELLS` 文字の欄に左寄せ（越える分は切る）、
/// 数は右寄せで 5 文字（「10/10」）。名前は英数字と `- _ @ ?` だけ（`WatchName`）なので、欄の幅は文字数のままで揃う。
fn watch_row_text(line: &mut LineBuf, r: &WatchRow, lang: Lang) {
    for (i, name) in [r.p1, r.p2].into_iter().enumerate() {
        if i == 1 {
            line.push_str(T_WATCH_VS.get(lang));
        }
        let s = name.as_str();
        let shown = s.get(..WATCH_NAME_CELLS.min(WATCH_NAME_MAX)).unwrap_or(s);
        line.push_str(shown);
        for _ in shown.len()..WATCH_NAME_CELLS {
            line.push_char(' ');
        }
    }
    line.push_str("  ");
    let digits = |n: u8| {
        if n >= 100 {
            3
        } else if n >= 10 {
            2
        } else {
            1
        }
    };
    let width = digits(r.watchers) + 1 + digits(r.cap);
    for _ in width..5 {
        line.push_char(' ');
    }
    line.push_u32(u32::from(r.watchers))
        .push_char('/')
        .push_u32(u32::from(r.cap));
}

/// 待ちを始めた行（その行の右に「待機中 0:42」を出す）と、待った秒。ランダムマッチ・ルームマッチ・コード作成（返す側はコード入力）。
/// 観戦の待ちは観戦の行が待ちの間は出ないので対象にしない。時計は左上の行と同じ値。
fn wait_origin(note: &Note, now: u64) -> Option<(Item, u64)> {
    match note.state {
        CodeState::RoomJoining | CodeState::RoomWaiting | CodeState::RoomLinking => {
            let from = if note.random {
                Item::Random
            } else {
                Item::Room
            };
            Some((from, room_waited(note, now)))
        }
        CodeState::Offered | CodeState::Linking => Some((Item::Create, times(note, now).0)),
        CodeState::Replied => Some((Item::Paste, times(note, now).0)),
        CodeState::Idle
        | CodeState::SpectateJoining
        | CodeState::SpectateWaiting
        | CodeState::SpectateLinking
        | CodeState::SpectateIdle => None,
    }
}

/// 「ラベル」の後ろに、見出しの中でいちばん長いラベル（`widest` 枠）との差を空白で埋め、区切りを置く。
/// 全角の文字のラベルは全角の空白（2 枠）、半角のラベルは半角の空白（1 枠）で埋める: どちらも文字と同じ幅で進むので、
/// ゲームの文字（半角 9 画素）でも GDI の文字（半角 7 画素）でも値の始まりが揃う。
fn push_label(line: &mut LineBuf, lang: Lang, label: Tx, widest: u32) {
    let text = label.get(lang);
    line.push_str(text);
    let gap = widest.saturating_sub(cells_of(text));
    if text.is_ascii() {
        for _ in 0..gap {
            line.push_char(' ');
        }
    } else {
        for _ in 0..gap / 2 {
            line.push_char('　');
        }
        if gap % 2 == 1 {
            line.push_char(' ');
        }
    }
    line.push_str(T_ROW_SEP.get(lang));
}

/// 「ラベル　◀ 値 ▶」（← → で選ぶ行）。
fn push_choice(
    line: &mut LineBuf,
    lang: Lang,
    label: Tx,
    widest: u32,
    value: impl FnOnce(&mut LineBuf),
) {
    let mut arrows = T_ARROWS.get(lang).chars();
    push_label(line, lang, label, widest);
    line.push_char(arrows.next().unwrap_or('<')).push_char(' ');
    value(line);
    line.push_char(' ').push_char(arrows.next().unwrap_or('>'));
}

fn got_line(line: &mut LineBuf, lang: Lang, len: u32) {
    line.push_str(T_GOT.get(lang))
        .push_u32(len)
        .push_str(T_CHARS.get(lang));
}

fn code_summary(line: &mut LineBuf, lang: Lang, note: &Note) {
    line.push_ascii(&note.head)
        .push_str(T_ELLIPSIS.get(lang))
        .push_ascii(&note.tail)
        .push_str(T_CODE_LEN_OPEN.get(lang))
        .push_u32(note.code_len)
        .push_str(T_CODE_LEN_CLOSE.get(lang));
}

fn time_line(line: &mut LineBuf, lang: Lang, note: &Note, now: u64) {
    let (waited, remain) = times(note, now);
    line.push_str(T_WAITED.get(lang))
        .push_clock(waited)
        .push_str(T_REMAIN.get(lang))
        .push_clock(remain);
}

/// （自分のコードを作ってから経った秒, 有効期限までの残りの秒）。時計が戻っても 0 で止める。
fn times(note: &Note, now: u64) -> (u64, u64) {
    // `since` は UNIX 秒の下位 32 ビット。差は 32 ビットで取る（2106 年の桁あふれを跨いでも差は合う）。
    let waited = u64::from((now as u32).wrapping_sub(note.since));
    let waited = if waited > 86_400 { 0 } else { waited };
    (waited, DEFAULT_TTL_SECS.saturating_sub(waited))
}

/// 出来事の文（最大 3 行。貼った文字数・文・次にすること）を `out` の先最初から書く。戻りは行数。
fn event_lines(note: &Note, lang: Lang, out: &mut [LineBuf], warn: &mut [bool]) -> usize {
    let ev = note.event;
    let mut n = 0;
    let mut line = |f: &mut dyn FnMut(&mut LineBuf)| {
        if let (Some(l), Some(w)) = (out.get_mut(n), warn.get_mut(n)) {
            l.clear();
            f(l);
            *w = ev.is_trouble();
            n += 1;
        }
    };
    if ev.shows_got() {
        line(&mut |l| got_line(l, lang, note.paste_len));
    }
    let who = |l: &mut LineBuf, open: Tx, close: Tx| {
        l.push_str(open.get(lang));
        push_name(l, &note.name);
        l.push_str(close.get(lang));
    };
    match ev {
        Event::None => {}
        Event::Made => line(&mut |l| {
            l.push_str(T_MADE.get(lang));
        }),
        Event::ReplyMade => {
            line(&mut |l| who(l, T_NAME_OPEN, T_NAME_CLOSE_CODE));
            line(&mut |l| {
                l.push_str(T_REPLY_MADE.get(lang));
            });
        }
        Event::PeerRead => {
            line(&mut |l| who(l, T_NAME_OPEN, T_NAME_CLOSE_REPLY));
            line(&mut |l| {
                l.push_str(T_LINKING.get(lang));
            });
        }
        Event::Short => line(&mut |l| {
            l.push_str(T_SHORT.get(lang));
        }),
        Event::Empty => {
            line(&mut |l| {
                l.push_str(T_EMPTY.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_EMPTY_NEXT.get(lang));
            });
        }
        Event::Same => line(&mut |l| {
            l.push_str(T_SAME.get(lang));
        }),
        Event::Broken => {
            line(&mut |l| {
                l.push_str(T_BROKEN.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_BROKEN_NEXT.get(lang));
            });
        }
        Event::OwnCode => {
            line(&mut |l| {
                l.push_str(T_OWN.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_OWN_NEXT.get(lang));
            });
        }
        Event::BothOffer => {
            line(&mut |l| {
                l.push_str(T_BOTH_OFFER.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_BOTH_OFFER_NEXT.get(lang));
            });
        }
        Event::BothAnswer => {
            line(&mut |l| {
                l.push_str(T_BOTH_ANSWER.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_BOTH_ANSWER_NEXT.get(lang));
            });
        }
        Event::Expired => {
            line(&mut |l| {
                l.push_str(T_EXPIRED_OPEN.get(lang));
                // 期限の長さは `DEFAULT_TTL_SECS` から作る（数字を文字にコピーすると、期限を変えたときに文だけ古いままになる）。
                push_span(l, lang, DEFAULT_TTL_SECS);
                l.push_str(T_EXPIRED_CLOSE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_EXPIRED_NEXT.get(lang));
            });
        }
        Event::WrongSession => {
            line(&mut |l| {
                l.push_str(T_WRONG_SESSION.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_WRONG_SESSION_NEXT.get(lang));
            });
        }
        Event::WrongKind => {
            line(&mut |l| {
                l.push_str(T_WRONG_KIND.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_WRONG_KIND_NEXT.get(lang));
            });
        }
        Event::NoRoute => {
            line(&mut |l| {
                l.push_str(T_NO_ROUTE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_NO_ROUTE_NEXT.get(lang));
            });
        }
        Event::Internal => {
            line(&mut |l| {
                l.push_str(T_INTERNAL.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_RESTART.get(lang));
            });
        }
        Event::Unreadable => {
            line(&mut |l| {
                l.push_str(T_UNREADABLE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_UNREADABLE_NEXT.get(lang));
            });
        }
        Event::ClipWrite => {
            line(&mut |l| {
                l.push_str(T_CLIP_WRITE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_CLIP_WRITE_NEXT.get(lang));
            });
        }
        Event::ShapeLate => {
            line(&mut |l| {
                l.push_str(T_SHAPE_LATE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_SHAPE_LATE_NEXT.get(lang));
            });
        }
        Event::GaveUp => {
            line(&mut |l| {
                l.push_str(T_GAVE_UP_OPEN.get(lang));
                push_span(l, lang, DEFAULT_TTL_SECS);
                l.push_str(T_GAVE_UP_CLOSE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_GAVE_UP_NEXT.get(lang));
            });
        }
        Event::Ended => {
            line(&mut |l| {
                l.push_str(T_ENDED.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_AGAIN.get(lang));
            });
        }
        Event::Spent => {
            line(&mut |l| {
                l.push_str(T_SPENT.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_EXPIRED_NEXT.get(lang));
            });
        }
        Event::Cancelled => {
            line(&mut |l| {
                l.push_str(T_CANCELLED.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_CANCELLED_NEXT.get(lang));
            });
        }
        Event::OtherVersion => {
            line(&mut |l| other_version_line(l, lang, PROTO_VER, note.peer_ver));
            line(&mut |l| {
                l.push_str(T_OTHER_VER_NEXT.get(lang));
            });
        }
        Event::OtherBuild | Event::SpectateOtherBuild => {
            line(&mut |l| other_build_line(l, lang, BUILD_NUMBER, note.peer_build));
            line(&mut |l| {
                l.push_str(T_OTHER_VER_NEXT.get(lang));
            });
        }
        Event::RoomJoining => line(&mut |l| {
            l.push_str(T_ROOM_JOINING.get(lang));
        }),
        Event::RoomWaiting => {}
        Event::RoomPeer => {
            line(&mut |l| {
                if note.random {
                    who(l, T_RANDOM_PEER_OPEN, T_RANDOM_PEER_CLOSE)
                } else {
                    who(l, T_ROOM_PEER_OPEN, T_ROOM_PEER_CLOSE)
                }
            });
            line(&mut |l| {
                l.push_str(T_LINKING.get(lang));
            });
        }
        Event::RoomNameBad => {
            line(&mut |l| room_name_why_line(l, lang, note.room_why));
            line(&mut |l| {
                l.push_str(T_ROOM_FIX.get(lang));
            });
        }
        Event::RoomUnresponsive => {
            line(&mut |l| {
                l.push_str(T_ROOM_UNRESPONSIVE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_ROOM_UNRESPONSIVE_NEXT.get(lang));
            });
        }
        Event::RoomBusy => {
            line(&mut |l| {
                l.push_str(T_ROOM_BUSY.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_ROOM_BUSY_NEXT_OPEN.get(lang))
                    .push_u32(u32::from(note.retry_secs))
                    .push_str(if note.random {
                        T_RANDOM_BUSY_NEXT.get(lang)
                    } else {
                        T_ROOM_BUSY_NEXT.get(lang)
                    });
            });
        }
        Event::RoomFull | Event::RoomRefused => {
            line(&mut |l| {
                l.push_str(if ev == Event::RoomFull {
                    T_ROOM_FULL.get(lang)
                } else {
                    T_ROOM_REFUSED.get(lang)
                });
            });
            line(&mut |l| {
                l.push_str(if note.random {
                    T_RANDOM_AGAIN.get(lang)
                } else {
                    T_ROOM_RENAME.get(lang)
                });
            });
        }
        Event::RoomExpired | Event::RoomPeerLeft | Event::RoomBadCode => {
            line(&mut |l| {
                l.push_str(match ev {
                    Event::RoomExpired => T_ROOM_EXPIRED.get(lang),
                    Event::RoomPeerLeft => T_ROOM_PEER_LEFT.get(lang),
                    _ => T_ROOM_BAD_CODE.get(lang),
                });
            });
            line(&mut |l| {
                l.push_str(if note.random {
                    T_RANDOM_AGAIN.get(lang)
                } else {
                    T_ROOM_AGAIN.get(lang)
                });
            });
        }
        // 観戦者の失敗は 2 行（理由と、次にすること 1 つ）。ルーム名は出さない。
        Event::SpectatePeer => line(&mut |l| spectate_peer_line(l, lang, note)),
        Event::SpectateUnresponsive => {
            line(&mut |l| {
                l.push_str(T_ROOM_UNRESPONSIVE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_SPECTATE_UNRESPONSIVE_NEXT.get(lang));
            });
        }
        Event::SpectateRefused => {
            line(&mut |l| {
                l.push_str(T_ROOM_REFUSED.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_SPECTATE_RENAME.get(lang));
            });
        }
        Event::SpectateBadCode => {
            line(&mut |l| {
                l.push_str(T_SPECTATE_BAD_CODE.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_SPECTATE_AGAIN.get(lang));
            });
        }
        Event::SpectateOtherVersion => {
            line(&mut |l| other_version_line(l, lang, PROTO_VER, note.peer_ver));
            line(&mut |l| {
                l.push_str(T_OTHER_VER_NEXT.get(lang));
            });
        }
        Event::SpectateGaveUp => {
            line(&mut |l| {
                // 長さは待つ上限（`JOINER_GIVE_UP_MS`）から作る（数字を文字にコピーすると、上限を変えたときに文だけ古いままになる）。
                l.push_str(T_SPECTATE_GAVE_UP_OPEN.get(lang));
                push_span(l, lang, JOINER_GIVE_UP_MS / 1000);
                l.push_str(T_SPECTATE_GAVE_UP_TAIL.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_SPECTATE_GAVE_UP_NEXT.get(lang));
            });
        }
        Event::SpectateNoLink => {
            line(&mut |l| {
                l.push_str(T_SPECTATE_NO_LINK.get(lang));
            });
            line(&mut |l| {
                l.push_str(T_SPECTATE_NO_LINK_NEXT.get(lang));
            });
        }
        Event::SpectateEnded => line(&mut |l| {
            l.push_str(T_SPECTATE_ENDED.get(lang));
        }),
        Event::SpectateCancelled => line(&mut |l| {
            l.push_str(T_SPECTATE_CANCELLED.get(lang));
        }),
    }
    n
}

fn room_name_why_line(l: &mut LineBuf, lang: Lang, why: RoomNameWhy) {
    match why {
        RoomNameWhy::Empty => {
            l.push_str(T_ROOM_INPUT_EMPTY.get(lang));
        }
        RoomNameWhy::Long => {
            l.push_str(T_ROOM_NAME_LONG_OPEN.get(lang))
                .push_u32(crate::room::ROOM_NAME_MAX as u32)
                .push_str(T_ROOM_NAME_LONG_CLOSE.get(lang));
        }
        RoomNameWhy::BadChar => {
            l.push_str(T_ROOM_NAME_CHAR.get(lang));
        }
        RoomNameWhy::Dot => {
            l.push_str(T_ROOM_NAME_DOT.get(lang));
        }
        RoomNameWhy::NotAscii => {
            l.push_str(T_ROOM_NAME_ASCII.get(lang));
        }
    }
}

/// 文字の枠の数（`overlay::glyph_cells` の const の形。ASCII は 1、ほかは 2 と多めに見積もる）。
const fn cells_of(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        if b[i] < 0x80 {
            n += 1;
        } else if b[i] >= 0xC0 {
            n += 2;
        }
        i += 1;
    }
    n
}

/// 左上の「待機中」の行（マッチ中の行と同じ置き場。`ui_layout::HUD_TEXT_MAX_W` = 282 画素 = 半角 40 枠）で、ルーム名に残る文字の枠の数（半角 1・全角 2）。
/// 固定の文字（先頭 `open`・尾 `close`・空白 1・時計 `clock` 枠）を引いた残りで、言語ごとに違う。
const fn band_name_cells(open: Tx, close: Tx, clock: u32, lang: Lang) -> u32 {
    crate::ui_layout::HUD_TEXT_MAX_W / crate::ui_layout::HUD_CELL_W
        - cells_of(open.get(lang))
        - cells_of(close.get(lang))
        - 1
        - clock
}

/// 待機中の時計の上限（9999:59。7 文字）。これを越えて待っても、行が幅を越えて時計の末尾が切れない（約 166 時間）。
const WAIT_CLOCK_MAX_SECS: u64 = 9999 * 60 + 59;
/// 「ルーム 名前 で待機中 12:34」の名前に残る枠の数。ふつうのルーム名（ASCII。`ROOM_UNITS` = 16 文字）は全部出る。
const fn room_band_cells(lang: Lang) -> u32 {
    band_name_cells(T_ROOM_OPEN, T_ROOM_WAITING_CLOSE, 7, lang)
}

/// 「ルーム 名前 で待機中 12:34」（ルームで待ち始めてから経った時間 `Note::since` から。時計が戻っても 0 で止める）。名前は `name_cells` 枠を越えるなら末尾を「…」にする（左上の 1 行は幅が狭い。メニューの中は幅に余裕があるので全部出す — `u32::MAX`）。
fn room_waiting_line(line: &mut LineBuf, lang: Lang, note: &Note, now: u64, name_cells: u32) {
    line.clear().push_str(T_ROOM_OPEN.get(lang));
    push_units_within(line, &note.room, name_cells);
    line.push_str(T_ROOM_WAITING_CLOSE.get(lang))
        .push_char(' ')
        .push_clock(room_waited(note, now));
}

/// 「ランダムマッチで待機中 12:34」（時計は待ち始めてから経った時間 `Note::since`。ルーム名は無いので、左上の幅に収まる）。
fn random_waiting_line(line: &mut LineBuf, lang: Lang, note: &Note, now: u64) {
    line.clear()
        .push_str(T_RANDOM_WAITING.get(lang))
        .push_char(' ')
        .push_clock(room_waited(note, now));
}

/// 観戦の待ちの行の時計の上限（99:59。5 文字）。観戦者は 30 分（`JOINER_GIVE_UP_MS`）で諦めるので越えない。幅を `ROOM_BAND_CELLS` の行より取るので、時計の文字数を 7 から 5 に絞って名前の枠を稼ぐ。
const SPECTATE_CLOCK_MAX_SECS: u64 = 99 * 60 + 59;
/// 左上の「観戦 名前 で待機中 m:ss」の行の名前に残る枠の数（長い名前は「…」で切る。メニューの中は全部出す）。
const fn spectate_band_cells(lang: Lang) -> u32 {
    band_name_cells(T_SPECTATE_OPEN, T_ROOM_WAITING_CLOSE, 5, lang)
}

/// 言語ごとに、名前の枠がルーム名（`ROOM_UNITS` 文字）を入れられる。
const _: () = {
    let mut i = 0;
    while i < Lang::ALL.len() {
        assert!(
            room_band_cells(Lang::ALL[i]) >= ROOM_UNITS as u32,
            "待機中の行の固定の文字が長く、ルーム名が試合中の行の幅に収まらない"
        );
        assert!(
            spectate_band_cells(Lang::ALL[i]) >= ROOM_UNITS as u32,
            "観戦の待機中の行の固定の文字が長く、ルーム名が試合中の行の幅に収まらない"
        );
        assert!(
            cells_of(T_RANDOM_WAITING.get(Lang::ALL[i])) + 1 + 7
                <= crate::ui_layout::HUD_TEXT_MAX_W / crate::ui_layout::HUD_CELL_W,
            "ランダムマッチの待機中の行が試合中の行の幅に収まらない"
        );
        i += 1;
    }
};

/// 「対戦者「名前」につないでいます。」（メニューの状況の行と左上の行が同じ文字）。
fn spectate_peer_line(line: &mut LineBuf, lang: Lang, note: &Note) {
    line.clear().push_str(T_SPECTATE_PEER_OPEN.get(lang));
    push_name(line, &note.name);
    line.push_str(T_SPECTATE_PEER_CLOSE.get(lang));
}

/// 「観戦 名前 で待機中 12:34」（観戦者が、配る側がルームに来るのを待つ間。時計は待った時間 `Note::since` から）。名前の切り方は `room_waiting_line` と同じ。
fn spectate_waiting_line(line: &mut LineBuf, lang: Lang, note: &Note, now: u64, name_cells: u32) {
    line.clear().push_str(T_SPECTATE_OPEN.get(lang));
    push_units_within(line, &note.room, name_cells);
    line.push_str(T_ROOM_WAITING_CLOSE.get(lang))
        .push_char(' ')
        .push_clock(room_waited(note, now).min(SPECTATE_CLOCK_MAX_SECS));
}

/// 観戦者の状態（`spectate_room::Joiner::status`）を、メニューが読む状態にする（見張りが `Note::state` に置く）。配る側がまだルームにいなくて入り直しを待つ間は `SpectateWaiting`、
/// 観戦用のコードを受けて返しを出した後と成功（セッションが対戦者へつなぎに行く間）は `SpectateLinking`、それまでは `SpectateJoining`。終わった（失敗・やめた）後は `SpectateIdle`（セッションを元へ戻せたなら呼び手が `Idle` に置き直す）。
pub fn spectate_state_of(s: &JoinerStatus) -> CodeState {
    if s.waiting_for_caster() {
        return CodeState::SpectateWaiting;
    }
    match s.phase {
        JoinerPhase::Waiting | JoinerPhase::Joining | JoinerPhase::InRoom => {
            CodeState::SpectateJoining
        }
        JoinerPhase::AnswerSent | JoinerPhase::Succeeded => CodeState::SpectateLinking,
        JoinerPhase::Failed(_) | JoinerPhase::Stopped => CodeState::SpectateIdle,
    }
}

/// 観戦者が失敗で終わった理由を、画面の出来事と、バージョン違いのときの相手のバージョン（`Note::peer_ver`）・ビルド違いのときの相手のビルド（`Note::peer_build`）にする（違い以外は 0）。見張りが `Note::event` に置く。
pub fn spectate_failure_event(f: JoinerFailure) -> (Event, u8, u32) {
    use crate::code::CodeError;
    match f {
        JoinerFailure::Unresponsive(_) => (Event::SpectateUnresponsive, 0, 0),
        JoinerFailure::BadRoomName => (Event::SpectateRefused, 0, 0),
        JoinerFailure::CodeUnreadable(CodeError::BadVersion(v)) => {
            (Event::SpectateOtherVersion, v, 0)
        }
        JoinerFailure::CodeUnreadable(CodeError::OtherBuild { theirs }) => {
            (Event::SpectateOtherBuild, 0, theirs)
        }
        JoinerFailure::CodeUnreadable(_) => (Event::SpectateBadCode, 0, 0),
        JoinerFailure::ReplyNotSent => (Event::SpectateNoLink, 0, 0),
        JoinerFailure::GaveUp => (Event::SpectateGaveUp, 0, 0),
    }
}

/// ルームで待ち始めてから経った秒（`WAIT_CLOCK_MAX_SECS` で止める）。`since` は UNIX 秒の下位 32 ビットで、差は 32 ビットで取る。待つ長さに上限は無い（自動で入り直して待ち続ける）ので 1 日を越えても 0 にしない。時計が戻った（差が 32 ビットの半分を越える）ときだけ 0。
fn room_waited(note: &Note, now: u64) -> u64 {
    let waited = (now as u32).wrapping_sub(note.since);
    let waited = if waited > i32::MAX as u32 {
        0
    } else {
        u64::from(waited)
    };
    waited.min(WAIT_CLOCK_MAX_SECS)
}

/// 「相手の HiganRTool のバージョンが違います（自分 2・相手 1）。」（`mine` は自分のバージョン）。
pub fn other_version_line(line: &mut LineBuf, lang: Lang, mine: u8, theirs: u8) {
    line.clear()
        .push_str(T_OTHER_VER_OPEN.get(lang))
        .push_u32(u32::from(mine))
        .push_str(T_OTHER_VER_MID.get(lang))
        .push_u32(u32::from(theirs))
        .push_str(T_OTHER_VER_CLOSE.get(lang));
}

/// バージョン違いの 2 行を 1 つの文字列にしたもの。ゲームのスレッドでは呼ばない（`String` を作る）。
pub fn other_version_text_in(lang: Lang, mine: u8, theirs: u8) -> String {
    let mut l = LineBuf::new();
    other_version_line(&mut l, lang, mine, theirs);
    format!("{}{}", l.as_str(), T_OTHER_VER_NEXT.get(lang))
}

/// `other_version_text_in` の日本語（本ログ・試験のまとめ・peer_sim の記録。ログは言語を切り替えない）。
pub fn other_version_text(mine: u8, theirs: u8) -> String {
    other_version_text_in(Lang::Ja, mine, theirs)
}

/// ビルド番号を画面の形で足す: 日本の時刻の `YYYYMMDD-HHMM`（`update::build_stamp`）。0（開発用）は「開発版」。確保しない。
pub fn push_build(line: &mut LineBuf, lang: Lang, build: u32) {
    let Some((y, mo, d, h, mi)) = crate::update::build_stamp(build) else {
        line.push_str(T_BUILD_DEV.get(lang));
        return;
    };
    line.push_u32(y);
    for v in [mo, d] {
        if v < 10 {
            line.push_char('0');
        }
        line.push_u32(v);
    }
    line.push_char('-');
    for v in [h, mi] {
        if v < 10 {
            line.push_char('0');
        }
        line.push_u32(v);
    }
}

/// 「相手とバージョンが違います（自分 20261008-2100・相手 開発版）。」（`mine` は自分のビルド）。
pub fn other_build_line(line: &mut LineBuf, lang: Lang, mine: u32, theirs: u32) {
    line.clear().push_str(T_OTHER_BUILD_OPEN.get(lang));
    push_build(line, lang, mine);
    line.push_str(T_OTHER_VER_MID.get(lang));
    push_build(line, lang, theirs);
    line.push_str(T_OTHER_VER_CLOSE.get(lang));
}

/// ビルド違いの 2 行を 1 つの文字列にしたもの。ゲームのスレッドでは呼ばない。
pub fn other_build_text_in(lang: Lang, mine: u32, theirs: u32) -> String {
    let mut l = LineBuf::new();
    other_build_line(&mut l, lang, mine, theirs);
    format!("{}{}", l.as_str(), T_OTHER_VER_NEXT.get(lang))
}

/// `other_build_text_in` の日本語（`other_version_text` と同じ使い経路）。
pub fn other_build_text(mine: u32, theirs: u32) -> String {
    other_build_text_in(Lang::Ja, mine, theirs)
}

/// 「30 分」（`code::span_text` と同じ文字。ASCII の数字と「分」「秒」）。毎 tick 組む行にも使うので確保しない。2 つが同じ文字を出すことは `the_span_matches_the_code_text` が縛る。
fn push_span(line: &mut LineBuf, lang: Lang, secs: u64) {
    if secs >= 60 && secs.is_multiple_of(60) {
        line.push_u32(u32::try_from(secs / 60).unwrap_or(u32::MAX))
            .push_str(T_SPAN_MIN.get(lang));
    } else {
        line.push_u32(u32::try_from(secs).unwrap_or(u32::MAX))
            .push_str(T_SPAN_SEC.get(lang));
    }
}

fn push_left(line: &mut LineBuf, lang: Lang, left_secs: u32) {
    line.push_clock(u64::from(left_secs))
        .push_str(T_FOUND_CLOSE.get(lang));
}

fn unmatched_line(line: &mut LineBuf, lang: Lang, why: NoMatch) {
    line.clear();
    match why {
        NoMatch::TimedOut => {
            line.push_str(T_UNMATCHED_OPEN.get(lang));
            push_span(line, lang, u64::from(MATCH_WAIT_MS / 1000));
            line.push_str(T_UNMATCHED_CLOSE.get(lang));
        }
        NoMatch::PeerHasNoGate => {
            line.push_str(T_NO_GATE.get(lang));
        }
        NoMatch::PeerLeft => {
            line.push_str(T_PEER_LEFT.get(lang));
        }
    }
}

/// メニューの下半分の状況の行（最大 `STATUS_ROWS`）。つながった・終わった状態は、その行だけ。戻りは行数。
fn status_lines(
    conn: Conn,
    note: &Note,
    now: u64,
    opening: bool,
    lang: Lang,
    out: &mut [LineBuf; STATUS_ROWS],
    warn: &mut [bool; STATUS_ROWS],
) -> usize {
    match conn {
        Conn::Connected { seat, host, shape } => {
            connected_line(&mut out[0], lang, seat, host, shape);
            warn[0] = false;
            1
        }
        Conn::Found {
            ready, left_secs, ..
        } => {
            found_line(&mut out[0], lang, ready, left_secs);
            warn[0] = false;
            1
        }
        Conn::Unmatched(why) => {
            unmatched_line(&mut out[0], lang, why);
            out[1].clear().push_str(T_AGAIN.get(lang));
            warn[0] = true;
            warn[1] = true;
            2
        }
        Conn::OtherVersion { theirs } => {
            other_version_line(&mut out[0], lang, PROTO_VER, theirs);
            out[1].clear().push_str(T_OTHER_VER_NEXT.get(lang));
            warn[0] = true;
            warn[1] = true;
            2
        }
        Conn::OtherBuild { theirs } => {
            other_build_line(&mut out[0], lang, BUILD_NUMBER, theirs);
            out[1].clear().push_str(T_OTHER_VER_NEXT.get(lang));
            warn[0] = true;
            warn[1] = true;
            2
        }
        Conn::Desynced => {
            out[0].clear().push_str(T_DESYNC.get(lang));
            out[1].clear().push_str(T_DESYNC_NEXT.get(lang));
            warn[0] = true;
            warn[1] = true;
            2
        }
        Conn::Ended | Conn::Finished => {
            out[0].clear().push_str(if conn == Conn::Ended {
                T_ENDED.get(lang)
            } else {
                T_FINISHED.get(lang)
            });
            out[1].clear().push_str(T_AGAIN.get(lang));
            warn[0] = true;
            warn[1] = true;
            2
        }
        Conn::Waiting => {
            let mut n = event_lines(note, lang, out, warn);
            if opening && let (Some(l), Some(w)) = (out.get_mut(n), warn.get_mut(n)) {
                l.clear().push_str(T_OPENING.get(lang));
                *w = false;
                n += 1;
            }
            if note.state == CodeState::RoomWaiting {
                if let (Some(l), Some(w)) = (out.get_mut(n), warn.get_mut(n)) {
                    if note.random {
                        random_waiting_line(l, lang, note, now);
                    } else {
                        room_waiting_line(l, lang, note, now, u32::MAX);
                    }
                    *w = false;
                    n += 1;
                }
                if let (Some(l), Some(w)) = (out.get_mut(n), warn.get_mut(n)) {
                    l.clear().push_str(if note.random {
                        T_NEXT_RANDOM_WAITING.get(lang)
                    } else {
                        T_NEXT_ROOM_WAITING.get(lang)
                    });
                    *w = false;
                    n += 1;
                }
            } else if note.state == CodeState::SpectateWaiting {
                if let (Some(l), Some(w)) = (out.get_mut(n), warn.get_mut(n)) {
                    spectate_waiting_line(l, lang, note, now, u32::MAX);
                    *w = false;
                    n += 1;
                }
            } else if note.state != CodeState::Idle && !note.state.is_room() {
                if let (Some(l), Some(w)) = (out.get_mut(n), warn.get_mut(n)) {
                    l.clear();
                    code_summary(l, lang, note);
                    *w = false;
                    n += 1;
                }
                if let (Some(l), Some(w)) = (out.get_mut(n), warn.get_mut(n)) {
                    l.clear();
                    time_line(l, lang, note, now);
                    *w = false;
                    n += 1;
                }
            }
            n
        }
    }
}

pub fn connected_line(
    line: &mut LineBuf,
    lang: Lang,
    seat: Seat,
    host: bool,
    shape: Option<(Mode, u8)>,
) {
    line.clear();
    line.push_str(T_CONNECTED_PRE.get(lang))
        .push_str(match seat {
            Seat::P1 => "1P",
            Seat::P2 => "2P",
        });
    if host {
        line.push_str(T_HOST.get(lang))
            .push_str(T_CONNECTED_POST.get(lang).trim_start());
    } else {
        line.push_str(T_CONNECTED_POST.get(lang));
    }
    match shape {
        Some((Mode::Rollback, d)) => {
            line.push_str(T_CONNECTED_SHAPE_OPEN.get(lang))
                .push_str(T_ROLLBACK.get(lang))
                .push_u32(u32::from(d))
                .push_str(T_CONNECTED_SHAPE_CLOSE.get(lang));
        }
        Some((Mode::Delay, d)) => {
            line.push_str(T_CONNECTED_SHAPE_OPEN.get(lang))
                .push_str(T_DELAY.get(lang))
                .push_u32(u32::from(d))
                .push_str(T_CONNECTED_SHAPE_CLOSE.get(lang));
        }
        None => {}
    }
}

/// 「対戦相手が見つかりました（閉じるまで 9:59）」か、自分がもう選んでいれば「相手が「妖怪 対 妖怪」を選ぶのを待っています（閉じるまで 9:59）」。
fn found_line(line: &mut LineBuf, lang: Lang, ready: bool, left_secs: u32) {
    line.clear().push_str(if ready {
        T_FOUND_READY.get(lang)
    } else {
        T_FOUND.get(lang)
    });
    push_left(line, lang, left_secs);
}

/// 左上の行のうち、マッチ中の行と同じ置き場・文字（GDI の小さい文字。上の枠の左）で描く 1 行か。待っている間（ルームで待つ・接続コードで待つ）の行がそう。真のとき `corner_lines` は 1 行だけ返し、その行が帯の行。接続の手順が時間切れで作り直されている間（`stall` が `Some`）は偽（ゲームの文字の警告。`stall_block`）。
pub fn corner_is_band(conn: Conn, note: &Note, stall: Option<Stall>) -> bool {
    conn == Conn::Waiting
        && matches!(
            note.state,
            CodeState::RoomWaiting
                | CodeState::SpectateWaiting
                | CodeState::Offered
                | CodeState::Replied
                | CodeState::Linking
        )
        && stall_route(note, stall).is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallWhy {
    /// 10 秒のあいだ 1 通も届かなかった（`FailReason::NothingReceived`）。自分のコードへの跳ね返り（`FailReason::SameNonce`）も、相手からは何も届いていないので同じ扱いにする。
    NothingReceived,
    NoHandshake,
    /// 作り直しの前に、相手のコードを読んだ時点で両方が直接つなげない型と分かった予告（`Stall::predicted`）。手順は止めずに続ける（予測が外れることがある）。
    Predicted,
}

/// 接続の手順が 1 回以上時間切れになって作り直されている間（か、両方が直接つなげない型と分かった間）の、左上の警告の材料（`stall_block`）。
/// 作り直しが 0 回の間は `None`（今の小さい文字の 1 行のまま）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    pub why: StallWhy,
    /// 出す文の種類（自分と相手の回線の型から。`nat::choose`）。
    pub fail: FailKind,
}

impl Stall {
    /// `Link::restarts()` から。作り直しが 1 回以上で、理由が作り直す種類のときだけ `Some`（バージョン違いは作り直さず、`Conn::OtherVersion` の行へ行く）。
    pub fn from_restarts(r: Restarts, fail: FailKind) -> Option<Stall> {
        if r.count == 0 {
            return None;
        }
        let why = match r.last?.0 {
            FailReason::SameNonce | FailReason::NothingReceived => StallWhy::NothingReceived,
            FailReason::NoHandshake => StallWhy::NoHandshake,
            FailReason::OtherVersion { .. } | FailReason::OtherBuild { .. } => return None,
        };
        Some(Stall { why, fail })
    }

    /// 作り直しの前の予告。両方がシンメトリックと分かったときだけ `Some`（ほかの型は作り直しを待つ）。
    pub fn predicted(fail: FailKind) -> Option<Stall> {
        (fail == FailKind::BothSymmetric).then_some(Stall {
            why: StallWhy::Predicted,
            fail,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StallRoute {
    Code,
    Room,
}

/// 警告を出す場面か。セッションが今の相手へつなぎに行っている場面だけ（接続コードの経路は `Replied`・`Linking`、ルームの経路は `RoomLinking`）。コードを作って返事を待つ間（`Offered`）・ルームで相手を待つ間（`RoomWaiting`）は、
/// セッションは今の相手につなぎに行っていない（候補の無いセッションや、前の相手のセッションの作り直しの数が残っているだけ）ので、偽の警告を出さず帯の 1 行のまま。取りやめた後（`Idle`）・ルームに入る前（`RoomJoining`）も出さない。
fn stall_route(note: &Note, stall: Option<Stall>) -> Option<(Stall, StallRoute)> {
    let st = stall?;
    match note.state {
        CodeState::Replied | CodeState::Linking => Some((st, StallRoute::Code)),
        CodeState::RoomLinking => Some((st, StallRoute::Room)),
        // 観戦者は対戦者のセッションの「作り直しの数」を持たない。
        CodeState::Offered
        | CodeState::RoomWaiting
        | CodeState::Idle
        | CodeState::RoomJoining
        | CodeState::SpectateJoining
        | CodeState::SpectateWaiting
        | CodeState::SpectateLinking
        | CodeState::SpectateIdle => None,
    }
}

/// 相手から届いているのにつながらないとき（`StallWhy::NoHandshake`）で、型からは原因が言えないときの 3 行（理由・次にすること・README の項）。
fn stall_pair(out: &mut [LineBuf], lang: Lang, note: &Note, now: u64, route: StallRoute) -> usize {
    let [l0, l1, l2, ..] = out else {
        return 0;
    };
    l0.push_str(T_STALL_NO_HANDSHAKE.get(lang));
    l0.push_str(T_WAITED.get(lang));
    match route {
        StallRoute::Code => {
            let (waited, remain) = times(note, now);
            l0.push_clock(waited)
                .push_str(T_STALL_REMAIN.get(lang))
                .push_clock(remain);
        }
        StallRoute::Room => {
            l0.push_clock(room_waited(note, now));
        }
    }
    l0.push_str(T_STALL_CLOSE.get(lang));
    l1.push_str(if route == StallRoute::Room {
        T_STALL_NEXT_ROOM.get(lang)
    } else {
        T_STALL_NEXT_CODE.get(lang)
    });
    l2.push_str(T_STALL_REF.get(lang));
    3
}

/// つながらなかったときの文（見出し・理由・次にすること・README の項の行）。戻りは行数。
fn fail_block(out: &mut [LineBuf; STALL_ROWS], lang: Lang, fail: FailKind) -> usize {
    let (head, lines, reference): (Tx, [Option<Tx>; 3], Tx) = match fail {
        FailKind::Unknown => (
            T_FAIL_HEAD,
            [Some(T_FAIL1_A), Some(T_FAIL1_B), None],
            T_FAIL1_REF,
        ),
        FailKind::PeerSymmetricNoUpnp => (
            T_FAIL_HEAD,
            [Some(T_FAIL2_A), Some(T_FAIL2_B), None],
            T_FAIL2_REF,
        ),
        FailKind::OwnSymmetric => (
            T_FAIL_HEAD,
            [Some(T_FAIL3_A), Some(T_FAIL3_B), Some(T_FAIL3_C)],
            T_FAIL3_REF,
        ),
        FailKind::BothSymmetric => (T_FAIL_HEAD_NO, [Some(T_FAIL4_A), None, None], T_FAIL4_REF),
    };
    let mut n = 0;
    for t in [Some(head)]
        .into_iter()
        .chain(lines)
        .chain([Some(reference)])
        .flatten()
    {
        if let Some(l) = out.get_mut(n) {
            l.push_str(t.get(lang));
            n += 1;
        }
    }
    n
}

/// 警告の最大の行数（見出し・理由・次にすること 2 行・README の項）。
pub const STALL_ROWS: usize = 5;

/// 接続の手順が時間切れで作り直されている間（か、両方が直接つなげないと分かった間）の左上の警告。ゲームの文字の最大 `STALL_ROWS` 行で、全部 `RowStyle::Warn`。
/// 戻りは行数。警告の場面でなければ 0 で、そのとき左上は `corner_lines` が決める（`corner_lines` はこの場面で 0 を返す）。
///
/// 文は `nat::FailKind` の 4 通り。ただし相手から届いているのにつながらない（`StallWhy::NoHandshake`）で型から原因が言えないときは、
/// 「届きません」と言うと事実と違うので、別の 3 行（理由・待った時間と次にすること・README の 5）。
pub fn stall_block(
    conn: Conn,
    note: &Note,
    now: u64,
    stall: Option<Stall>,
    lang: Lang,
    out: &mut [LineBuf; STALL_ROWS],
    styles: &mut [RowStyle; STALL_ROWS],
) -> usize {
    for l in out.iter_mut() {
        l.clear();
    }
    if conn != Conn::Waiting {
        return 0;
    }
    let Some((st, route)) = stall_route(note, stall) else {
        return 0;
    };
    let n = if st.why == StallWhy::NoHandshake && st.fail == FailKind::Unknown {
        stall_pair(out, lang, note, now, route)
    } else {
        fail_block(out, lang, st.fail)
    };
    for s in styles.iter_mut().take(n) {
        *s = RowStyle::Warn;
    }
    n
}

/// 左上に出す行（メニューを閉じている間。最大 2 行）。戻りは行数。成立していれば 1 行（つながった行。成立したら両者が自動で難易度の画面まで進む（`ui_entry::advance`）ので次にすることは出さない）、
/// つながったが成立の前なら 2 行（見つかった・待っている行と、次にすること）、待っているなら 1 行（待った時間。置き場は `corner_is_band`）、終わっていれば 2 行。何も無ければ 0。
/// 待っている間でも、接続の手順が時間切れで作り直されていれば（`stall` が `Some`）警告の 2 行（`stall_lines`）。
pub fn corner_lines(
    conn: Conn,
    note: &Note,
    now: u64,
    stall: Option<Stall>,
    lang: Lang,
    out: &mut [LineBuf; 2],
    styles: &mut [RowStyle; 2],
) -> usize {
    out[0].clear();
    out[1].clear();
    match conn {
        Conn::Connected { seat, host, shape } => {
            connected_line(&mut out[0], lang, seat, host, shape);
            styles[0] = RowStyle::Normal;
            1
        }
        Conn::Found {
            ready, left_secs, ..
        } => {
            found_line(&mut out[0], lang, ready, left_secs);
            styles[0] = RowStyle::Normal;
            out[1].push_str(if ready {
                T_NEXT_FOUND_READY.get(lang)
            } else {
                T_NEXT_FOUND.get(lang)
            });
            styles[1] = RowStyle::Hint;
            2
        }
        Conn::Unmatched(why) => {
            unmatched_line(&mut out[0], lang, why);
            out[1].push_str(T_AGAIN.get(lang));
            styles[0] = RowStyle::Warn;
            styles[1] = RowStyle::Warn;
            2
        }
        Conn::OtherVersion { theirs } => {
            other_version_line(&mut out[0], lang, PROTO_VER, theirs);
            out[1].push_str(T_OTHER_VER_NEXT.get(lang));
            styles[0] = RowStyle::Warn;
            styles[1] = RowStyle::Warn;
            2
        }
        Conn::OtherBuild { theirs } => {
            other_build_line(&mut out[0], lang, BUILD_NUMBER, theirs);
            out[1].push_str(T_OTHER_VER_NEXT.get(lang));
            styles[0] = RowStyle::Warn;
            styles[1] = RowStyle::Warn;
            2
        }
        Conn::Desynced => {
            out[0].push_str(T_DESYNC.get(lang));
            out[1].push_str(T_DESYNC_NEXT.get(lang));
            styles[0] = RowStyle::Warn;
            styles[1] = RowStyle::Warn;
            2
        }
        Conn::Ended | Conn::Finished => {
            out[0].push_str(if conn == Conn::Ended {
                T_ENDED.get(lang)
            } else {
                T_FINISHED.get(lang)
            });
            out[1].push_str(T_AGAIN.get(lang));
            styles[0] = RowStyle::Warn;
            styles[1] = RowStyle::Warn;
            2
        }
        Conn::Waiting => {
            // 接続の手順が時間切れで作り直されている間は、小さい 1 行でなくゲームの文字の警告（`stall_block`。複数行なのでここでは返さない）。
            if stall_route(note, stall).is_some() {
                return 0;
            }
            match note.state {
                CodeState::Idle | CodeState::SpectateIdle => {
                    // ルームの待ちが断り・失敗で終わった後は、その理由と次にすることを出す（ルームで待つ間に別のモードで遊んでいた側が知る）。コードの経路の断りは出さない。観戦が終わった後の理由も同じ。
                    if !note.event.is_room_trouble() {
                        return 0;
                    }
                    let mut warn = [false; 2];
                    let k = event_lines(note, lang, out, &mut warn);
                    for (s, w) in styles.iter_mut().zip(warn) {
                        *s = if w { RowStyle::Warn } else { RowStyle::Normal };
                    }
                    return k;
                }
                CodeState::RoomJoining | CodeState::RoomLinking | CodeState::SpectateJoining => {
                    out[0].push_str(match (note.state == CodeState::RoomLinking, note.random) {
                        (true, true) => T_RANDOM_LINKING.get(lang),
                        (true, false) => T_ROOM_LINKING.get(lang),
                        (false, _) => T_ROOM_JOINING.get(lang),
                    });
                    out[1].push_str(T_NEXT_LINKING.get(lang));
                    styles[0] = RowStyle::Normal;
                    styles[1] = RowStyle::Hint;
                    return 2;
                }
                CodeState::SpectateLinking => {
                    spectate_peer_line(&mut out[0], lang, note);
                    styles[0] = RowStyle::Normal;
                    return 1;
                }
                CodeState::SpectateWaiting => {
                    spectate_waiting_line(&mut out[0], lang, note, now, spectate_band_cells(lang));
                    styles[0] = RowStyle::Normal;
                    return 1;
                }
                // 待っている間の行は 1 行だけ（次にすることの 2 行目は付けない）。描くときはマッチ中の行と同じ置き場・文字（`corner_is_band`）。
                CodeState::RoomWaiting => {
                    if note.random {
                        random_waiting_line(&mut out[0], lang, note, now);
                    } else {
                        room_waiting_line(&mut out[0], lang, note, now, room_band_cells(lang));
                    }
                    styles[0] = RowStyle::Normal;
                    return 1;
                }
                CodeState::Offered | CodeState::Replied | CodeState::Linking => {}
            }
            let (waited, remain) = times(note, now);
            out[0]
                .push_str(T_WAITING.get(lang))
                .push_clock(waited)
                .push_str(T_WAITING_REMAIN.get(lang))
                .push_clock(remain);
            styles[0] = RowStyle::Normal;
            1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HudView {
    /// 遅れ（rollback なら今の D、delay 方式なら今の遅れ。自分か相手が Ctrl＋数字で変えて決まった値・要求している値があればそれ）。
    pub latency: Option<u8>,
    pub rtt_ms: Option<u32>,
    /// 推奨値（`delay_pick::recommend`。RTT が測れていなければ `None` で、出さない）。
    pub recommend: Option<u8>,
    pub rewind: Option<u32>,
    /// auto の旗が立っているか。立っていれば括弧の中を推奨値でなく `A` にする。ホストは自分の旗、ゲストはホストの旗か自分の要求（`Connected::shown_auto`）。
    pub auto: bool,
    /// キャラセレクトの行のとき、先頭に出す同期方式（「rollback・Delay 2F (A)  ping 33ms」）。マッチ中は `None`（形式は右端の `rollback N` で分かる。delay 方式のマッチは出さない）。
    pub format: Option<Mode>,
    /// キャラセレクトの行で、この組が rollback へ替えられない（片方が rollback ができない）。`format` が delay 方式のとき、語を「delay only」にする（Tab が効かないことが画面から分かるように）。
    pub delay_only: bool,
}

/// 「Delay NF (推奨F)  ping NNms  rollback N」。一時停止の間も同じ形。auto の旗が立っていれば括弧の中は `A`（推奨値は出さない。ping が測れていなくても出す）。キャラセレクトの行は先頭に「rollback・」か「delay・」が付き、末尾の `rollback N` は無い。
/// 推奨は RTT が測れていなければ括弧ごと出さない。`rewind` が `None`（delay 方式）なら `rollback` は出さない。ping が測れていなければ `ping -`。描くときは ASCII だけの行を GDI の文字で描く（`ui_layout::push_text_plain`。幅は `every_match_line_fits_before_the_timer`）。
pub fn hud_line(line: &mut LineBuf, lang: Lang, h: &HudView) {
    line.clear();
    if let Some(m) = h.format {
        line.push_str(match (m, h.delay_only) {
            (Mode::Rollback, _) => T_FORMAT_ROLLBACK.get(lang),
            (Mode::Delay, false) => T_FORMAT_DELAY.get(lang),
            (Mode::Delay, true) => T_FORMAT_DELAY_ONLY.get(lang),
        })
        .push_str(T_HUD_FORMAT_SEP.get(lang));
    }
    line.push_str(T_HUD_DELAY.get(lang));
    line.push_char(' ');
    match h.latency {
        Some(d) => {
            line.push_u32(u32::from(d))
                .push_str(T_FRAMES_MARK.get(lang));
        }
        None => {
            line.push_char('-');
        }
    }
    if h.auto {
        line.push_str(T_HUD_RECOMMEND_OPEN.get(lang))
            .push_str(T_HUD_AUTO_MARK.get(lang))
            .push_str(T_HUD_RECOMMEND_CLOSE.get(lang));
    } else if let Some(r) = h.recommend {
        line.push_str(T_HUD_RECOMMEND_OPEN.get(lang))
            .push_u32(u32::from(r))
            .push_str(T_FRAMES_MARK.get(lang))
            .push_str(T_HUD_RECOMMEND_CLOSE.get(lang));
    }
    line.push_str(T_HUD_PING.get(lang));
    match h.rtt_ms {
        Some(r) => {
            line.push_u32(r).push_str(T_HUD_MS.get(lang));
        }
        None => {
            line.push_char('-');
        }
    }
    if let Some(r) = h.rewind {
        line.push_str(T_HUD_REWIND.get(lang)).push_u32(r);
    }
}

/// 観戦者の行に出す、対戦者 2 人の間の状態（観戦の流れの `Stats` の記録の最新の値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerStats {
    pub ping_ms: Option<u16>,
    pub latency: u8,
    pub rollback: Option<u8>,
}

impl PeerStats {
    pub fn from_record(ping_ms: u16, latency: u8, rollback: u8) -> PeerStats {
        PeerStats {
            ping_ms: (ping_ms != crate::wire::STATS_PING_UNKNOWN).then_some(ping_ms),
            latency,
            rollback: (rollback != crate::wire::STATS_ROLLBACK_NONE).then_some(rollback),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WatchHudView {
    /// ホストが流した対戦者 2 人の間の状態（最新の値。まだ受けていなければ `None` で、前半を出さない）。
    pub stats: Option<PeerStats>,
    /// 使う直前に溜まっていたマッチのフレームの数（直近の期間の最大）。
    pub delay_frames: u32,
    pub fast_forward: bool,
}

/// 画面の行に出す値の頭打ち（桁の数を決め、行の最長の文字数が決まる）。ping は 999 ms まで・ほかは 99 まで。
pub const WATCH_PING_MAX: u32 = 999;
pub const WATCH_COUNT_MAX: u32 = 99;

/// 観戦者のマッチ中の通信の状態の行: 「ping 14ms Delay 1F Rollback 2 Watch delay 3」。`stats` を受けるまでは「Watch delay 3」だけ（先頭の空白は落とす）。
/// delay 方式のマッチは `Rollback` を出さない・ping が測れていなければ `ping -`（`hud_line` と同じ）。早送りの間は後ろに「 >>」。
/// 値は桁が増えて行が下の枠の左から溢れないよう頭打ちする（ping 999・ほかは 99）。ふつうの文字では収まらないので、描くときは小さい文字（`ui_layout::push_text_small`）。幅は `the_watch_line_fits_the_bottom_band_in_the_small_font`。
pub fn watch_hud_line(line: &mut LineBuf, lang: Lang, h: &WatchHudView) {
    line.clear();
    if let Some(s) = h.stats {
        line.push_str(T_HUD_WATCH_PING.get(lang));
        match s.ping_ms {
            Some(p) => {
                line.push_u32(u32::from(p).min(WATCH_PING_MAX))
                    .push_str(T_HUD_MS.get(lang));
            }
            None => {
                line.push_char('-');
            }
        }
        line.push_str(T_HUD_WATCH_DELAY.get(lang))
            .push_u32(u32::from(s.latency).min(WATCH_COUNT_MAX))
            .push_str(T_FRAMES_MARK.get(lang));
        if let Some(r) = s.rollback {
            line.push_str(T_HUD_WATCH_ROLLBACK.get(lang))
                .push_u32(u32::from(r).min(WATCH_COUNT_MAX));
        }
        line.push_str(T_HUD_WATCH.get(lang));
    } else {
        // 先頭の空白（区切り）は前半があるときだけ。
        line.push_str(T_HUD_WATCH.get(lang).trim_start());
    }
    line.push_u32(h.delay_frames.min(WATCH_COUNT_MAX));
    if h.fast_forward {
        line.push_str(T_HUD_WATCH_FF.get(lang));
    }
}
/// 対戦中の名前の帯の 1 行: 1P は「1P 名前」（上の枠の左）、2P は「名前 2P」（上の枠の右。右寄せ）。`name` は HELLO で交換した名前（UTF-8 の 32 バイトまで）。制御文字は `?` にする。
/// 長さは文字の枠の数（半角 1・全角 2。`overlay::glyph_cells`）で `HUD_NAME_CELLS` までに切る（ラベルは残す: 右の帯は右寄せなので、後ろ（ラベル）を切ると席が読めなくなる）。
/// 表に無い全角の文字は描くと 1 枠の `?` になるので、枠の数は多めに見積もる向き。
pub fn name_band_line(line: &mut LineBuf, lang: Lang, seat: Seat, name: &[u8]) {
    line.clear();
    let label = match seat {
        Seat::P1 => T_HUD_SEAT_1.get(lang),
        Seat::P2 => T_HUD_SEAT_2.get(lang),
    };
    if seat == Seat::P1 {
        line.push_str(label).push_char(' ');
    }
    let mut cells = 0u32;
    for c in core::str::from_utf8(name).unwrap_or("").chars() {
        let c = if c.is_control() { '?' } else { c };
        cells += crate::overlay::glyph_cells(c);
        if cells > crate::ui_layout::HUD_NAME_CELLS {
            break;
        }
        line.push_char(c);
    }
    if seat == Seat::P2 {
        line.push_char(' ').push_str(label);
    }
}

/// 直近 `REWIND_WINDOW` フレームのうち、1 フレームでいちばん長く巻き戻したフレーム数（「戻し」）。1 フレームごとに `push` する。60 フレーム = 約 1 秒。
pub const REWIND_WINDOW: usize = 60;

#[derive(Debug, Clone, Copy)]
pub struct RewindWindow {
    ring: [u8; REWIND_WINDOW],
    next: usize,
}

impl Default for RewindWindow {
    fn default() -> Self {
        RewindWindow::new()
    }
}

impl RewindWindow {
    pub const fn new() -> RewindWindow {
        RewindWindow {
            ring: [0; REWIND_WINDOW],
            next: 0,
        }
    }

    pub fn push(&mut self, frames: u32) {
        if let Some(s) = self.ring.get_mut(self.next) {
            *s = u8::try_from(frames).unwrap_or(u8::MAX);
        }
        self.next = (self.next + 1) % REWIND_WINDOW;
    }

    pub fn max(&self) -> u32 {
        self.ring.iter().copied().max().map_or(0, u32::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{AtlasSpec, ui_glyphs};

    const Z: u16 = 0x0001;
    const X: u16 = 0x0002;
    const ESC: u16 = 0x0008;
    const UP: u16 = 0x0010;
    const DOWN: u16 = 0x0020;
    const LEFT: u16 = 0x0040;
    const RIGHT: u16 = 0x0080;

    fn ini() -> Shape {
        Shape {
            mode: Mode::Rollback,
            delay: 1,
        }
    }

    fn input(word: u16, note: Note, conn: Conn) -> TickIn {
        TickIn {
            word,
            note,
            conn,
            text: TextIn::NONE,
        }
    }

    fn quiet() -> Note {
        Note::default()
    }

    /// 設定の「ラベル　値」の行のラベル部分（見出しの中でいちばん長い「プレイヤー名」に値の始まりを揃えた形）。
    fn lab(label: Tx, lang: Lang) -> String {
        let mut l = LineBuf::new();
        push_label(&mut l, lang, label, cells_of(T_NAME.get(lang)));
        l.as_str().to_string()
    }

    /// 名前の行が無いとき（いちばん長いのが「同期方式」）の `lab`。
    fn lab8(label: Tx, lang: Lang) -> String {
        let mut l = LineBuf::new();
        push_label(&mut l, lang, label, cells_of(T_FORMAT.get(lang)));
        l.as_str().to_string()
    }

    struct Driver {
        m: Menu,
        note: Note,
        conn: Conn,
        cmds: Vec<Cmd>,
    }

    impl Driver {
        fn new() -> Driver {
            let mut m = Menu::new(ini());
            m.open(0);
            Driver {
                m,
                note: quiet(),
                conn: Conn::Waiting,
                cmds: Vec::new(),
            }
        }

        fn tap(&mut self, key: u16) -> TickOut {
            let down = self.m.tick(&input(key, self.note, self.conn));
            self.cmds.extend(down.cmd);
            let up = self.m.tick(&input(0, self.note, self.conn));
            self.cmds.extend(up.cmd);
            down
        }

        fn rows(&self) -> MenuRows {
            let mut r = MenuRows::new();
            self.m.render(self.conn, &self.note, 1_000_000, &mut r);
            r
        }

        fn text(&self) -> Vec<String> {
            let r = self.rows();
            r.lines[..r.len]
                .iter()
                .map(|l| l.as_str().to_string())
                .collect()
        }
    }

    #[test]
    fn opening_ignores_the_held_key_and_holds_the_game() {
        let mut m = Menu::new(ini());
        assert!(!m.is_active());
        assert!(
            !m.tick(&input(Z, quiet(), Conn::Waiting)).hold_game,
            "閉じている間は握らない"
        );
        m.open(Z);
        assert!(m.is_active());
        let out = m.tick(&input(Z, quiet(), Conn::Waiting));
        assert_eq!(
            out,
            TickOut {
                cmd: None,
                hold_game: true,
                answer: None
            }
        );
        m.tick(&input(0, quiet(), Conn::Waiting));
        let out = m.tick(&input(Z, quiet(), Conn::Waiting));
        assert_eq!(out.cmd, Some(Cmd::Create));
    }

    #[test]
    fn up_down_and_decide_choose_items() {
        let mut d = Driver::new();
        assert_eq!(d.m.cursor, 0);
        d.tap(DOWN);
        assert_eq!(d.m.cursor, 1);
        d.tap(DOWN);
        d.tap(DOWN);
        d.tap(DOWN);
        d.tap(DOWN);
        assert_eq!(d.m.cursor, 5, "戻る");
        d.tap(DOWN);
        assert_eq!(d.m.cursor, 0, "下の端で折り返す");
        d.tap(UP);
        assert_eq!(d.m.cursor, 5, "上の端で折り返す");
        d.tap(UP);
        d.tap(UP);
        d.tap(UP);
        d.tap(UP);
        assert_eq!(d.m.cursor, 1);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::Paste]);
        d.tap(UP);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::Paste, Cmd::Create]);
        d.tap(0x1000);
        assert_eq!(d.cmds.last(), Some(&Cmd::Create));
        assert!(!d.text().iter().any(|l| l.contains(T_CANCEL.ja)));
        d.note.state = CodeState::Offered;
        assert!(d.text().iter().any(|l| l.contains(T_CANCEL.ja)));
        for _ in 0..5 {
            d.tap(DOWN);
        }
        d.tap(Z);
        assert_eq!(d.cmds.last(), Some(&Cmd::Cancel));
        d.tap(DOWN);
        assert!(d.m.is_active());
        d.tap(Z);
        assert!(!d.m.is_active());
        for key in [X, ESC] {
            let mut d = Driver::new();
            d.tap(key);
            assert!(!d.m.is_active(), "key {key:#x}");
        }
    }

    #[test]
    fn closing_feeds_zero_until_the_word_is_zero() {
        let mut m = Menu::new(ini());
        m.open(0);
        assert!(m.tick(&input(X, quiet(), Conn::Waiting)).hold_game);
        for _ in 0..10 {
            let out = m.tick(&input(X, quiet(), Conn::Waiting));
            assert!(out.hold_game && m.is_active(), "X を押している間は返さない");
        }
        let out = m.tick(&input(0, quiet(), Conn::Waiting));
        assert!(out.hold_game, "語が 0 になった tick も 0 を流す");
        assert!(!m.is_active());
        assert!(!m.tick(&input(0, quiet(), Conn::Waiting)).hold_game);
        let mut m = Menu::new(ini());
        m.open(0);
        m.tick(&input(X, quiet(), Conn::Waiting));
        let mut held = 1;
        while m.is_active() {
            held += 1;
            assert!(held < 1000, "上限で返らない");
            m.tick(&input(0x0010, quiet(), Conn::Waiting));
        }
        assert_eq!(
            held,
            CLOSE_MAX_TICKS + 2,
            "閉じ始めてから上限の tick 後に返す"
        );
    }

    #[test]
    fn connecting_closes_the_menu_and_a_connected_menu_has_only_back() {
        let mut d = Driver::new();
        d.conn = Conn::Connected {
            seat: Seat::P1,
            host: false,
            shape: Some((Mode::Rollback, 1)),
        };
        let out = d.m.tick(&input(Z, quiet(), d.conn));
        assert!(out.hold_game, "つながった tick もゲームへ返さない");
        assert_eq!(out.cmd, None, "つながった tick の決定は指示にしない");
        d.m.tick(&input(0, quiet(), d.conn));
        assert!(!d.m.is_active());
        let mut d2 = Driver::new();
        d2.conn = d.conn;
        let mut m = Menu::new(ini());
        m.open(0);
        let mut rows = MenuRows::new();
        m.render(d2.conn, &quiet(), 0, &mut rows);
        let texts: Vec<_> = rows.lines[..rows.len].iter().map(LineBuf::as_str).collect();
        assert!(texts.iter().any(|l| l.contains(T_BACK.ja)));
        assert!(
            !texts
                .iter()
                .any(|l| l.contains(T_CREATE.ja) || l.contains(T_PASTE.ja))
        );
        assert!(
            texts
                .iter()
                .any(|l| l.contains("接続しました。あなたは 1P です。"))
        );
        let mut m = Menu::new(ini());
        m.open(0);
        m.render(Conn::Ended, &quiet(), 0, &mut rows);
        let texts: Vec<_> = rows.lines[..rows.len].iter().map(LineBuf::as_str).collect();
        assert!(texts.iter().any(|l| l.contains(T_ENDED.ja)));
        assert!(texts.contains(&T_AGAIN.ja), "{texts:?}");
        assert!(!texts.contains(&T_RESTART.ja), "{texts:?}");
        assert!(!texts.iter().any(|l| l.contains(T_CREATE.ja)));
    }

    #[test]
    fn ended_lines_say_connect_again_and_only_setup_failures_say_restart() {
        let mut out = [LineBuf::new(); 2];
        let mut st = [RowStyle::Normal; 2];
        for conn in [
            Conn::Ended,
            Conn::Finished,
            Conn::Unmatched(NoMatch::TimedOut),
            Conn::Unmatched(NoMatch::PeerHasNoGate),
            Conn::Unmatched(NoMatch::PeerLeft),
        ] {
            let n = corner_lines(conn, &quiet(), 0, None, Lang::Ja, &mut out, &mut st);
            assert_eq!(n, 2, "{conn:?}");
            assert_eq!(out[1].as_str(), T_AGAIN.ja, "{conn:?}");
            assert!(conn.is_end(), "{conn:?}");
        }
        assert!(Conn::OtherVersion { theirs: 0 }.is_end());
        for conn in [
            Conn::Waiting,
            Conn::Connected {
                seat: Seat::P1,
                host: false,
                shape: None,
            },
            Conn::Found {
                seat: Seat::P1,
                shape: None,
                ready: false,
                left_secs: 1,
            },
        ] {
            assert!(!conn.is_end(), "{conn:?}");
        }
        let mut lines = [LineBuf::new(); STATUS_ROWS];
        let mut warn = [false; STATUS_ROWS];
        let note = |event| Note {
            event,
            paste_len: 77,
            ..quiet()
        };
        let k = event_lines(&note(Event::Ended), Lang::Ja, &mut lines, &mut warn);
        assert_eq!(lines[k - 1].as_str(), T_AGAIN.ja);
        let k = event_lines(&note(Event::Internal), Lang::Ja, &mut lines, &mut warn);
        assert_eq!(
            lines[k - 1].as_str(),
            T_RESTART.ja,
            "準備の失敗は起動し直し"
        );
        let k = event_lines(&note(Event::Spent), Lang::Ja, &mut lines, &mut warn);
        let got: Vec<&str> = lines[..k].iter().map(LineBuf::as_str).collect();
        assert_eq!(
            got,
            ["受け取った文字数 77 文字", T_SPENT.ja, T_EXPIRED_NEXT.ja],
            "前のつながりのコードの断り"
        );
        assert!(warn[..k].iter().all(|w| *w));
    }

    #[test]
    fn left_right_change_the_format_and_the_delay_only_on_their_rows() {
        use crate::delay_pick::DelayChoice::{Auto, Fixed};
        let mut d = Driver::new();
        d.tap(RIGHT);
        d.tap(LEFT);
        assert!(d.cmds.is_empty());
        let sel = d.m.shape();
        assert_eq!(
            (sel.mode, sel.delay, sel.touched),
            (Mode::Rollback, Fixed(1), false)
        );
        d.tap(DOWN);
        d.tap(DOWN);
        let t = d.text().join("\n");
        assert!(t.contains("同期方式　◀ rollback ▶"), "{t}");
        assert!(t.contains("ディレイ　◀ 1F ▶"), "{t}");
        d.tap(LEFT);
        assert!(d.cmds.is_empty(), "{:?}", d.cmds);
        d.tap(RIGHT);
        d.tap(RIGHT);
        assert_eq!(d.cmds, vec![Cmd::Shape(Pick::Delay, Fixed(1))]);
        assert!(d.text().join("\n").contains("同期方式　◀ delay ▶"));
        d.tap(LEFT);
        assert_eq!(d.cmds[1], Cmd::Shape(Pick::Rollback(1), Fixed(1)));
        d.cmds.clear();
        d.tap(DOWN);
        for _ in 0..4 {
            d.tap(LEFT);
        }
        assert_eq!(
            d.cmds,
            vec![
                Cmd::Shape(Pick::Rollback(0), Fixed(0)),
                Cmd::Shape(Pick::Rollback(1), Auto),
            ]
        );
        assert!(d.text().join("\n").contains("ディレイ　◀ auto ▶"));
        d.cmds.clear();
        for _ in 0..12 {
            d.tap(RIGHT);
        }
        let seen: Vec<(Pick, crate::delay_pick::DelayChoice)> = d
            .cmds
            .iter()
            .map(|c| match c {
                Cmd::Shape(p, c) => (*p, *c),
                other => panic!("{other:?}"),
            })
            .collect();
        let want: Vec<_> = (0..=8u8).map(|k| (Pick::Rollback(k), Fixed(k))).collect();
        assert_eq!(seen, want, "auto → 0 → … → 8");
        assert_eq!(d.m.shape().delay, Fixed(8));
        assert!(d.text().join("\n").contains("ディレイ　◀ 8F ▶"));
        d.cmds.clear();
        d.tap(UP);
        d.tap(RIGHT);
        d.tap(DOWN);
        d.tap(LEFT);
        assert_eq!(
            d.cmds,
            vec![
                Cmd::Shape(Pick::Delay, Fixed(8)),
                Cmd::Shape(Pick::Delay, Fixed(7))
            ]
        );
        assert!(d.m.shape().touched);
    }

    #[test]
    fn left_right_change_the_seat_wish_only_on_its_row_and_only_while_idle() {
        let d = Driver::new();
        assert!(!d.text().iter().any(|l| l.contains("サイド　")));
        assert_eq!(d.m.seat(), None);
        let mut d = Driver::new();
        d.m = Menu::new(ini()).with_seat(SeatWish::P1);
        d.m.open(0);
        let t = d.text().join("\n");
        assert!(
            t.contains(&format!("{}◀ 1P ▶", lab8(T_SEAT, Lang::Ja))),
            "{t}"
        );
        d.tap(RIGHT);
        d.tap(DOWN);
        d.tap(DOWN);
        d.tap(DOWN);
        d.tap(LEFT);
        assert!(d.cmds.iter().all(|c| !matches!(c, Cmd::Seat(_))));
        assert_eq!(d.m.seat(), Some(SeatWish::P1));
        d.cmds.clear();
        d.tap(UP);
        for _ in 0..3 {
            d.tap(RIGHT);
        }
        assert_eq!(
            d.cmds,
            vec![Cmd::Seat(SeatWish::P2), Cmd::Seat(SeatWish::Any)]
        );
        let t = d.text().join("\n");
        assert!(
            t.contains(&format!("{}◀ おまかせ ▶", lab8(T_SEAT, Lang::Ja))),
            "{t}"
        );
        for _ in 0..4 {
            d.tap(LEFT);
        }
        assert_eq!(
            d.cmds[2..],
            [Cmd::Seat(SeatWish::P2), Cmd::Seat(SeatWish::P1)]
        );
        assert_eq!(d.m.seat(), Some(SeatWish::P1));
        d.note.state = CodeState::Offered;
        assert!(
            !d.text().iter().any(|l| l.contains("サイド　")),
            "{:?}",
            d.text()
        );
        assert_eq!(d.m.seat(), Some(SeatWish::P1));
        d.note.state = CodeState::Idle;
        assert!(d.text().iter().any(|l| l.contains("サイド　")));
    }

    #[test]
    fn the_rows_start_from_the_ini_shape() {
        use crate::delay_pick::DelayChoice::{Auto, Fixed};
        let rows_of = |shape: Shape| {
            let mut m = Menu::new(shape);
            m.open(0);
            let mut d = Driver::new();
            d.m = m;
            d.text()
        };
        for (shape, fmt, delay) in [
            (
                Shape {
                    mode: Mode::Delay,
                    delay: 1,
                },
                "同期方式　◀ delay ▶",
                "ディレイ　◀ 1F ▶",
            ),
            (
                Shape {
                    mode: Mode::Rollback,
                    delay: 5,
                },
                "同期方式　◀ rollback ▶",
                "ディレイ　◀ 5F ▶",
            ),
            (
                Shape {
                    mode: Mode::Rollback,
                    delay: 0,
                },
                "同期方式　◀ rollback ▶",
                "ディレイ　◀ 0F ▶",
            ),
            (
                Shape {
                    mode: Mode::Rollback,
                    delay: 9,
                },
                "同期方式　◀ rollback ▶",
                "ディレイ　◀ 8F ▶",
            ),
        ] {
            let t = rows_of(shape);
            assert!(t.iter().any(|l| l.contains(fmt)), "{fmt}: {t:?}");
            assert!(t.iter().any(|l| l.contains(delay)), "{delay}: {t:?}");
        }
        let at_delay_row = |shape: Shape, key: u16| {
            let mut d = Driver::new();
            d.m = Menu::new(shape);
            d.m.open(0);
            d.cursor_to(Item::Delay);
            d.tap(key);
            d.cmds
        };
        let rb5 = Shape {
            mode: Mode::Rollback,
            delay: 5,
        };
        assert_eq!(
            at_delay_row(rb5, LEFT),
            vec![Cmd::Shape(Pick::Rollback(4), Fixed(4))]
        );
        assert_eq!(
            at_delay_row(rb5, RIGHT),
            vec![Cmd::Shape(Pick::Rollback(6), Fixed(6))]
        );
        let mut d = Driver::new();
        d.m = Menu::new(rb5).with_start_auto(true);
        d.m.open(0);
        assert!(d.text().iter().any(|l| l.contains("ディレイ　◀ auto ▶")));
        assert!(d.cmds.is_empty() && !d.m.shape().touched);
        d.cursor_to(Item::Format);
        d.tap(RIGHT);
        assert_eq!(
            d.cmds,
            vec![Cmd::Shape(Pick::Delay, Auto)],
            "auto のまま形式だけ変える"
        );
        d.tap(LEFT);
        assert_eq!(d.cmds[1], Cmd::Shape(Pick::Rollback(5), Auto));
    }

    fn made_note() -> Note {
        let (head, tail) =
            Note::edges_of("AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_AbCdEfGhIjKlMnOpQrSt");
        Note {
            seq: 1,
            event: Event::Made,
            state: CodeState::Offered,
            code_len: 58,
            head,
            tail,
            since: 1_000_000 - 42,
            ..Note::default()
        }
    }

    #[test]
    fn after_creating_the_menu_shows_the_code_edges_and_the_clock() {
        let mut d = Driver::new();
        d.note = made_note();
        let t = d.text();
        assert!(
            t.contains(&"クリップボードにコピーしました。相手に送ってください。".to_string()),
            "{t:?}"
        );
        assert!(t.contains(&"AbCd…QrSt（58 文字）".to_string()), "{t:?}");
        assert!(
            t.contains(&"待機時間 0:42　有効期限の残り 29:18".to_string()),
            "{t:?}"
        );
        assert!(
            t.iter().any(|l| l.contains(T_PASTE_REPLY.ja)),
            "貼るが返すコードの名前になる"
        );
        assert!(
            !t.iter()
                .any(|l| l.contains(T_PASTE.ja) && !l.contains(T_PASTE_REPLY.ja))
        );
        let mut n = made_note();
        n.state = CodeState::Replied;
        n.event = Event::ReplyMade;
        d.note = n;
        let t = d.text();
        assert!(
            t.iter()
                .any(|l| l.contains(T_PASTE.ja) && !l.contains("返す"))
        );
    }

    #[test]
    fn a_paste_shows_how_many_characters_arrived_and_why_it_was_refused() {
        let mut d = Driver::new();
        let mut n = quiet();
        n.event = Event::Short;
        n.paste_len = 7;
        d.note = n;
        let t = d.text();
        assert!(t.contains(&"受け取った文字数 7 文字".to_string()), "{t:?}");
        assert!(t.iter().any(|l| l.contains("短すぎます")), "{t:?}");
        let mut troubles = 0;
        for ev in EVENTS {
            let mut n = quiet();
            n.event = ev;
            n.paste_len = 100;
            let mut out = [LineBuf::new(); STATUS_ROWS];
            let mut w = [false; STATUS_ROWS];
            let k = event_lines(&n, Lang::Ja, &mut out, &mut w);
            if ev.is_trouble() {
                troubles += 1;
                assert!(k >= 2, "{ev:?} の文が {k} 行");
                assert!(w[..k].iter().all(|x| *x), "{ev:?} が断りの色でない");
            }
            if ev == Event::None {
                assert_eq!(k, 0);
            }
        }
        // 出来事の数。足したら `EVENTS` と合わせて直す。
        assert_eq!(
            troubles, 34,
            "断り・失敗の出来事の数（増減したら文を見直す）"
        );
        let name = sanitize_name("テスト名".as_bytes(), |c| c != '名');
        let mut n = quiet();
        n.event = Event::ReplyMade;
        n.state = CodeState::Replied;
        n.paste_len = 100;
        n.name = name;
        d.note = n;
        let t = d.text();
        assert!(
            t.contains(&"相手「テスト?」のコードを読みました。".to_string()),
            "{t:?}"
        );
        assert!(t.contains(&"受け取った文字数 100 文字".to_string()));
    }

    #[test]
    fn corner_lines_show_waiting_and_connected() {
        let mut out = [LineBuf::new(); 2];
        let mut st = [RowStyle::Normal; 2];
        assert_eq!(
            corner_lines(
                Conn::Waiting,
                &quiet(),
                0,
                None,
                Lang::Ja,
                &mut out,
                &mut st
            ),
            0
        );
        let n = corner_lines(
            Conn::Waiting,
            &made_note(),
            1_000_000,
            None,
            Lang::Ja,
            &mut out,
            &mut st,
        );
        assert_eq!(
            n, 1,
            "待っている間は 1 行だけ（次にすることの 2 行目は出さない）"
        );
        assert_eq!(out[0].as_str(), "接続コードで待機中 0:42 残り 29:18");
        assert!(out[1].is_empty());
        assert!(corner_is_band(Conn::Waiting, &made_note(), None));
        let conn = Conn::Connected {
            seat: Seat::P1,
            host: true,
            shape: Some((Mode::Rollback, 1)),
        };
        let n = corner_lines(conn, &quiet(), 0, None, Lang::Ja, &mut out, &mut st);
        assert_eq!(n, 1);
        assert_eq!(
            out[0].as_str(),
            "接続しました。あなたは 1P（ホスト）です。（rollback・delay 1）"
        );
        assert!(out[1].is_empty());
        let conn2 = Conn::Connected {
            seat: Seat::P2,
            host: false,
            shape: Some((Mode::Delay, 3)),
        };
        corner_lines(conn2, &quiet(), 0, None, Lang::Ja, &mut out, &mut st);
        assert_eq!(
            out[0].as_str(),
            "接続しました。あなたは 2P です。（delay 方式・delay 3）"
        );
        let conn3 = Conn::Connected {
            seat: Seat::P2,
            host: true,
            shape: Some((Mode::Delay, 2)),
        };
        corner_lines(conn3, &quiet(), 0, None, Lang::Ja, &mut out, &mut st);
        assert_eq!(
            out[0].as_str(),
            "接続しました。あなたは 2P（ホスト）です。（delay 方式・delay 2）"
        );
        let n = corner_lines(Conn::Ended, &quiet(), 0, None, Lang::Ja, &mut out, &mut st);
        assert_eq!(n, 2);
        assert_eq!(st[0], RowStyle::Warn);
    }

    #[test]
    fn corner_lines_show_found_ready_and_unmatched() {
        let mut out = [LineBuf::new(); 2];
        let mut st = [RowStyle::Normal; 2];
        let found = |ready, left_secs| Conn::Found {
            seat: Seat::P2,
            shape: Some((Mode::Rollback, 1)),
            ready,
            left_secs,
        };
        let n = corner_lines(
            found(false, 599),
            &made_note(),
            0,
            None,
            Lang::Ja,
            &mut out,
            &mut st,
        );
        assert_eq!(n, 2);
        assert_eq!(
            out[0].as_str(),
            "対戦相手が見つかりました（閉じるまで 9:59）"
        );
        assert_eq!(out[1].as_str(), "「妖怪 対 妖怪」を選んでください。");
        assert_eq!(st, [RowStyle::Normal, RowStyle::Hint]);
        corner_lines(
            found(true, 61),
            &quiet(),
            0,
            None,
            Lang::Ja,
            &mut out,
            &mut st,
        );
        assert_eq!(
            out[0].as_str(),
            "相手が「妖怪 対 妖怪」を選ぶのを待っています（閉じるまで 1:01）"
        );
        assert_eq!(out[1].as_str(), "やめるときはキャンセルで抜けてください。");
        let n = corner_lines(
            Conn::Unmatched(NoMatch::TimedOut),
            &quiet(),
            0,
            None,
            Lang::Ja,
            &mut out,
            &mut st,
        );
        assert_eq!(n, 2);
        assert_eq!(
            out[0].as_str(),
            "対戦が始まらないまま 10 分 たったので、接続を閉じました。"
        );
        assert_eq!(out[1].as_str(), T_AGAIN.ja);
        assert_eq!(st, [RowStyle::Warn, RowStyle::Warn]);
        let mut m = Menu::new(ini());
        m.open(0);
        let mut rows = MenuRows::new();
        m.render(found(false, 5), &quiet(), 0, &mut rows);
        assert!(
            rows.lines[..rows.len]
                .iter()
                .any(|l| l.as_str() == "対戦相手が見つかりました（閉じるまで 0:05）"),
            "{:?}",
            &rows.lines[..rows.len]
        );
    }

    #[test]
    fn each_unmatched_reason_has_its_own_line_and_fits() {
        for lang in Lang::ALL {
            each_unmatched_reason_has_its_own_line_and_fits_in(lang);
        }
    }

    fn each_unmatched_reason_has_its_own_line_and_fits_in(lang: Lang) {
        let spec = game_font_spec("");
        let limit = 640
            - 2 * crate::ui_layout::SCREEN_MARGIN
            - 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        let mut seen = Vec::new();
        for (why, ja, en) in [
            (
                NoMatch::TimedOut,
                "対戦が始まらないまま 10 分 たったので、接続を閉じました。",
                "No match started for 10 min, so the connection was closed.",
            ),
            (
                NoMatch::PeerHasNoGate,
                "相手の HiganRTool が古いか、ゲーム内のメニューを使っていないので接続を閉じました。",
                "Closed: the opponent's HiganRTool is old or not in the game menu.",
            ),
            (
                NoMatch::PeerLeft,
                "相手が対戦を始めないまま、接続を閉じました。",
                "The opponent closed the connection before starting.",
            ),
        ] {
            let want = match lang {
                Lang::Ja => ja,
                Lang::En => en,
            };
            let mut out = [LineBuf::new(); 2];
            let mut st = [RowStyle::Normal; 2];
            let n = corner_lines(
                Conn::Unmatched(why),
                &quiet(),
                0,
                None,
                lang,
                &mut out,
                &mut st,
            );
            assert_eq!(n, 2, "{why:?}");
            assert_eq!(out[0].as_str(), want, "{why:?}");
            assert_eq!(out[1].as_str(), T_AGAIN.get(lang), "{why:?}");
            assert_eq!(st, [RowStyle::Warn, RowStyle::Warn], "{why:?}");
            for l in &out {
                let w = crate::ui_layout::text_width(&spec, l.as_str());
                assert!(w <= limit, "{why:?}: {w} 画素 > {limit}（{}）", l.as_str());
                assert!(l.as_str().chars().count() <= crate::overlay::MAX_TEXT);
                for bad in ["READY", "code_side", "offer", "answer", " Z", " X"] {
                    assert!(!l.as_str().contains(bad), "{why:?}: {bad}");
                }
            }
            let mut m = Menu::new(ini()).with_lang(lang);
            m.open(0);
            let mut rows = MenuRows::new();
            m.render(Conn::Unmatched(why), &quiet(), 0, &mut rows);
            let lines: Vec<&str> = rows.lines[..rows.len].iter().map(|l| l.as_str()).collect();
            assert!(
                lines.contains(&want) && lines.contains(&T_AGAIN.get(lang)),
                "{lines:?}"
            );
            seen.push(out[0].as_str().to_string());
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 3, "理由の行が重なった");
    }

    #[test]
    fn the_span_matches_the_code_text() {
        for secs in [0u64, 1, 59, 60, 61, 119, 120, 600, 1800, 3599, 3600] {
            let mut l = LineBuf::new();
            push_span(&mut l, Lang::Ja, secs);
            assert_eq!(l.as_str(), crate::code::span_text(secs), "{secs} 秒");
        }
    }

    #[test]
    fn the_menu_treats_found_as_linked() {
        let found = Conn::Found {
            seat: Seat::P1,
            shape: None,
            ready: true,
            left_secs: 100,
        };
        let mut m = Menu::new(ini());
        m.open(0);
        assert!(m.is_open());
        let o = m.tick(&input(0, quiet(), found));
        assert!(o.hold_game);
        assert!(!m.is_open(), "見つかったら閉じ始める");
        m.tick(&input(0, quiet(), found));
        assert!(!m.is_active());
        m.open_confirm(0);
        m.tick(&input(0, quiet(), found));
        assert!(m.is_confirm(), "答える前に閉じない");
        m.tick(&input(UP, quiet(), found));
        m.tick(&input(0, quiet(), found));
        let o = m.tick(&input(Z, quiet(), found));
        assert_eq!(o.answer, Some(Answer::Leave));
        let mut m = Menu::new(ini());
        m.open_confirm(0);
        m.tick(&input(0, quiet(), Conn::Unmatched(NoMatch::TimedOut)));
        assert!(!m.is_confirm());
    }

    #[test]
    fn the_match_line_has_latency_ping_recommend_and_rollback() {
        let mut l = LineBuf::new();
        let v = |latency, rtt_ms, recommend, rewind| HudView {
            latency,
            rtt_ms,
            recommend,
            rewind,
            ..HudView::default()
        };
        hud_line(&mut l, Lang::Ja, &v(Some(1), Some(33), Some(0), Some(3)));
        assert_eq!(l.as_str(), "Delay 1F (0F)  ping 33ms  rollback 3");
        hud_line(&mut l, Lang::Ja, &v(Some(3), Some(16), Some(1), None));
        assert_eq!(l.as_str(), "Delay 3F (1F)  ping 16ms");
        hud_line(&mut l, Lang::Ja, &v(Some(1), Some(120), Some(4), None));
        assert_eq!(l.as_str(), "Delay 1F (4F)  ping 120ms");
        hud_line(&mut l, Lang::Ja, &v(Some(2), None, None, None));
        assert_eq!(
            l.as_str(),
            "Delay 2F  ping -",
            "測れていない間は括弧を出さない"
        );
        hud_line(&mut l, Lang::Ja, &v(None, None, None, Some(0)));
        assert_eq!(l.as_str(), "Delay -  ping -  rollback 0");
        assert!(!l.as_str().contains('*'), "印が戻った");
    }

    #[test]
    fn auto_shows_a_in_the_parentheses_and_the_select_row_starts_with_the_format() {
        let mut l = LineBuf::new();
        let mut line = |v: HudView| {
            hud_line(&mut l, Lang::Ja, &v);
            l.as_str().to_string()
        };
        let base = HudView {
            latency: Some(2),
            rtt_ms: Some(33),
            recommend: Some(2),
            ..HudView::default()
        };
        let auto = HudView { auto: true, ..base };
        assert_eq!(line(base), "Delay 2F (2F)  ping 33ms");
        assert_eq!(line(auto), "Delay 2F (A)  ping 33ms");
        assert_eq!(
            line(HudView {
                rewind: Some(1),
                ..auto
            }),
            "Delay 2F (A)  ping 33ms  rollback 1"
        );
        assert_eq!(
            line(HudView {
                latency: Some(1),
                rtt_ms: None,
                recommend: None,
                rewind: Some(0),
                ..auto
            }),
            "Delay 1F (A)  ping -  rollback 0",
            "推奨値が無くても auto の印は出る"
        );
        assert_eq!(
            line(HudView {
                latency: Some(3),
                ..base
            }),
            "Delay 3F (2F)  ping 33ms"
        );
        assert_eq!(
            line(auto).len() + 1,
            line(base).len(),
            "(A) は (2F) より 1 文字短い"
        );
        assert_eq!(
            line(HudView {
                format: Some(Mode::Rollback),
                ..auto
            }),
            "rollback・Delay 2F (A)  ping 33ms"
        );
        assert_eq!(
            line(HudView {
                format: Some(Mode::Delay),
                ..base
            }),
            "delay・Delay 2F (2F)  ping 33ms"
        );
        assert_eq!(
            line(HudView {
                format: Some(Mode::Delay),
                delay_only: true,
                ..base
            }),
            "delay only・Delay 2F (2F)  ping 33ms"
        );
        assert_eq!(
            line(HudView {
                format: Some(Mode::Rollback),
                delay_only: true,
                ..auto
            }),
            "rollback・Delay 2F (A)  ping 33ms",
            "rollback で走っている組は替えられる組"
        );
        assert_eq!(
            line(HudView {
                delay_only: true,
                ..base
            }),
            "Delay 2F (2F)  ping 33ms",
            "試合中の行に形式の語は付かない"
        );
    }

    #[test]
    fn the_watch_line_has_peer_stats_and_delay() {
        let mut l = LineBuf::new();
        let line = |l: &mut LineBuf, stats, delay_frames, fast_forward| {
            watch_hud_line(
                l,
                Lang::Ja,
                &WatchHudView {
                    stats,
                    delay_frames,
                    fast_forward,
                },
            );
            l.as_str().to_string()
        };
        let st = |p, lat, rb| Some(PeerStats::from_record(p, lat, rb));
        assert_eq!(
            line(&mut l, st(14, 1, 2), 3, false),
            "ping 14ms Delay 1F Rollback 2 Watch delay 3"
        );
        assert_eq!(
            line(&mut l, st(14, 1, 0), 3, false),
            "ping 14ms Delay 1F Rollback 0 Watch delay 3"
        );
        assert_eq!(
            line(
                &mut l,
                st(14, 3, crate::wire::STATS_ROLLBACK_NONE),
                3,
                false
            ),
            "ping 14ms Delay 3F Watch delay 3"
        );
        assert_eq!(
            line(&mut l, st(crate::wire::STATS_PING_UNKNOWN, 1, 2), 3, false),
            "ping - Delay 1F Rollback 2 Watch delay 3"
        );
        assert_eq!(line(&mut l, None, 3, false), "Watch delay 3");
        assert_eq!(line(&mut l, None, 41, true), "Watch delay 41 >>");
        assert_eq!(
            line(&mut l, st(14, 1, 2), 41, true),
            "ping 14ms Delay 1F Rollback 2 Watch delay 41 >>"
        );
        assert_eq!(
            line(&mut l, st(60_000, 255, 254), u32::MAX, false),
            "ping 999ms Delay 99F Rollback 99 Watch delay 99"
        );
    }

    #[test]
    fn the_watch_line_fits_the_bottom_band_in_the_small_font() {
        use crate::overlay::{SMALL_CELL_W, small_key};
        use crate::ui_layout::{HUD_BOTTOM_TEXT_MAX_W, text_width_plain, text_width_small};
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let mut longest = (0, String::new());
        let mut l = LineBuf::new();
        for (lang, stats) in Lang::ALL.into_iter().flat_map(|lang| {
            [
                None,
                Some(PeerStats::from_record(999, 99, 99)),
                Some(PeerStats::from_record(65_534, 255, 254)),
                Some(PeerStats::from_record(
                    1,
                    0,
                    crate::wire::STATS_ROLLBACK_NONE,
                )),
            ]
            .map(|s| (lang, s))
        }) {
            for delay_frames in [0, 9, 99, u32::MAX] {
                for fast_forward in [false, true] {
                    watch_hud_line(
                        &mut l,
                        lang,
                        &WatchHudView {
                            stats,
                            delay_frames,
                            fast_forward,
                        },
                    );
                    let s = l.as_str();
                    assert!(s.is_ascii() && !s.contains("  "), "{s}");
                    assert!(
                        s.chars()
                            .all(|c| c == ' '
                                || small_key(c).and_then(|k| spec.slot_of(k)).is_some()),
                        "文字の表に小さい文字が無い: {s}"
                    );
                    let w = text_width_small(&spec, s);
                    assert_eq!(w, s.chars().count() as u32 * SMALL_CELL_W, "{s}");
                    assert!(w <= HUD_BOTTOM_TEXT_MAX_W, "「{s}」{w} 画素");
                    if w > longest.0 {
                        longest = (w, s.to_string());
                    }
                }
            }
        }
        assert_eq!(
            longest.1,
            "ping 999ms Delay 99F Rollback 99 Watch delay 99 >>"
        );
        assert_eq!(longest.0, 50 * SMALL_CELL_W);
        watch_hud_line(
            &mut l,
            Lang::Ja,
            &WatchHudView {
                stats: Some(PeerStats::from_record(14, 1, 2)),
                delay_frames: 3,
                fast_forward: false,
            },
        );
        assert_eq!(l.as_str().len(), 43);
        assert!(text_width_plain(&spec, l.as_str()) > HUD_BOTTOM_TEXT_MAX_W);
    }

    /// ゲームの文字（ascii.anm）をコピーした表: 英数字と空白が `GAME_ADVANCE`（9 画素）ずつ進む。文字の絵の大きさは送りに効かないので仮の 8×12 の一色にした。`extra` は文字の表に足す文字。
    fn game_font_spec(extra: &str) -> AtlasSpec {
        use crate::overlay::{GAME_ADVANCE, GameGlyphSrc, apply_game_font, texels};
        let mut glyphs = ui_glyphs();
        glyphs.extend(extra.chars());
        glyphs.sort_unstable();
        glyphs.dedup();
        let base = AtlasSpec::fit_game(7, 14, &glyphs).unwrap();
        let t = texels(&base, &vec![0u8; base.texel_count()]).unwrap();
        let px = vec![0xFFFF_FFFFu32; 8 * 12];
        let srcs: Vec<GameGlyphSrc> = (0x21u8..=0x7E)
            .filter(|&b| b != b'`')
            .map(|b| GameGlyphSrc {
                c: b as char,
                w: 8,
                h: 12,
                pixels: &px,
                quad_w: 8,
                quad_h: 12,
            })
            .collect();
        apply_game_font(&base, &t, &srcs, GAME_ADVANCE).unwrap().0
    }

    #[test]
    fn every_match_line_fits_before_the_timer() {
        use crate::frame::MAX_DELAY;
        use crate::overlay::MAX_TEXT;
        use crate::ui_layout::{HUD_TEXT_MAX_W, HUD_X, TIMER_LEFT, text_width, text_width_plain};
        let game = game_font_spec("");
        assert_eq!(
            text_width(&game, "a "),
            18,
            "英数字と空白は 9 画素（ゲームの文字）"
        );
        assert_eq!(
            text_width_plain(&game, "a "),
            14,
            "この行は 7 画素（GDI の文字）。空白も 7"
        );
        let plain = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        const { assert!(HUD_X + HUD_TEXT_MAX_W < TIMER_LEFT) };
        let mut l = LineBuf::new();
        let mut checked = 0u32;
        let mut widest = (0u32, String::new());
        for (lang, rewind) in Lang::ALL
            .into_iter()
            .flat_map(|lang| [None, Some(0), Some(99), Some(255)].map(|r| (lang, r)))
        {
            for rtt in [None, Some(0), Some(9), Some(99), Some(999), Some(9_999u32)] {
                for (latency, auto) in [
                    (None, false),
                    (Some(0), false),
                    (Some(MAX_DELAY), false),
                    (Some(MAX_DELAY), true),
                ] {
                    hud_line(
                        &mut l,
                        lang,
                        &HudView {
                            latency,
                            rtt_ms: rtt,
                            recommend: rtt.map(|_| MAX_DELAY),
                            rewind,
                            auto,
                            ..HudView::default()
                        },
                    );
                    let s = l.as_str();
                    assert!(s.chars().count() <= MAX_TEXT, "{s}");
                    assert!(s.is_ascii(), "全角が混ざった: {s}");
                    assert!(!s.contains("   "), "区切りが 3 つ空き以上: {s}");
                    for spec in [&game, &plain] {
                        let w = text_width_plain(spec, s);
                        assert!(
                            w <= HUD_TEXT_MAX_W,
                            "「{s}」が {w} 画素（上限 {HUD_TEXT_MAX_W}）"
                        );
                        if w > widest.0 {
                            widest = (w, s.to_string());
                        }
                    }
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 2 * 4 * 6 * 4, "言語 × 戻し × RTT × 遅れ");
        println!(
            "いちばん広い試合中の行: 「{}」 {} 画素 / 上限 {HUD_TEXT_MAX_W}",
            widest.1, widest.0
        );
        assert!(
            text_width(&game, &widest.1) > HUD_TEXT_MAX_W,
            "{}",
            widest.1
        );
        let olds = [
            "Latency ◀ 3 ▶  ping 16ms  推奨 1",
            "Latency 1（ホストが決める）  ping 0ms  推奨 1",
        ];
        let then = game_font_spec(&olds.concat());
        for old in olds {
            assert!(old.chars().all(|c| then.slot_of(c).is_some()), "{old}");
            assert!(text_width(&then, old) > HUD_TEXT_MAX_W, "{old}");
        }
    }

    #[test]
    fn the_match_line_is_drawn_in_gdi_glyphs_seven_pixels_apart() {
        use crate::overlay::{GAME_ADVANCE, Vertex};
        use crate::ui_layout::{push_text, push_text_plain};
        let game = game_font_spec("");
        let mut l = LineBuf::new();
        hud_line(
            &mut l,
            Lang::Ja,
            &HudView {
                latency: Some(3),
                rtt_ms: Some(16),
                recommend: Some(1),
                rewind: None,
                ..HudView::default()
            },
        );
        let text = l.as_str();
        let mut out = [Vertex::default(); 6 * 64];
        let (x0, y0) = (6u32, 1u32);
        let rects = |out: &[Vertex], n: usize| -> Vec<(f32, f32, f32)> {
            (0..n / 6)
                .map(|q| {
                    let (tl, bl) = (out[q * 6], out[q * 6 + 2]);
                    (tl.x + 0.5, tl.y + 0.5, bl.y + 0.5)
                })
                .collect()
        };
        let glyphs: Vec<(usize, char)> = text
            .chars()
            .enumerate()
            .filter(|&(_, c)| c != ' ')
            .collect();
        let (n, used) = push_text_plain(&mut out, 0, &game, (x0, y0), text, 0xFFFF_FFFF, 1000);
        let quads = rects(&out, n);
        assert_eq!(quads.len(), glyphs.len(), "空白は四角を置かない: {text}");
        for (&(x, top, bottom), &(i, c)) in quads.iter().zip(&glyphs) {
            assert_eq!(x, (x0 + 7 * i as u32) as f32, "{c:?}（{i} 文字目）の左端");
            assert_eq!(
                (top, bottom),
                (y0 as f32, (y0 + 14) as f32),
                "{c:?} の高さ（枠 14）"
            );
        }
        assert_eq!(used, 7 * text.chars().count() as u32);
        let (n, used) = push_text(&mut out, 0, &game, (x0, y0), text, 0xFFFF_FFFF, 1000);
        let quads = rects(&out, n);
        assert_eq!(quads.len(), glyphs.len());
        for (&(x, top, bottom), &(i, c)) in quads.iter().zip(&glyphs) {
            assert_eq!(
                x,
                (x0 + GAME_ADVANCE * i as u32) as f32,
                "{c:?}（{i} 文字目）の左端"
            );
            assert_eq!(bottom - top, 12.0, "{c:?} はゲームの文字の高さ");
        }
        assert_eq!(used, GAME_ADVANCE * text.chars().count() as u32);
    }

    #[test]
    fn the_rewind_window_is_the_max_of_the_last_sixty_frames() {
        let mut w = RewindWindow::new();
        assert_eq!(w.max(), 0);
        w.push(5);
        w.push(2);
        w.push(0);
        assert_eq!(w.max(), 5, "合計でも直近でもなく最大");
        for _ in 0..(REWIND_WINDOW - 3) {
            w.push(1);
        }
        assert_eq!(w.max(), 5, "ちょうど 60 フレーム前の値はまだ入っている");
        w.push(1);
        assert_eq!(w.max(), 2, "61 フレーム前の 5 は出ていき、次は 2");
        w.push(1);
        w.push(1);
        assert_eq!(w.max(), 1);
        w.push(100_000);
        assert_eq!(w.max(), 255);
    }

    #[test]
    fn a_note_round_trips_through_words() {
        let mut n = made_note();
        n.name = sanitize_name(b"Abc", |_| true);
        n.paste_len = 99;
        n.event = Event::Expired;
        let back = Note::from_words(&n.to_words());
        assert_eq!(back, n);
        let mut w = n.to_words();
        w[1] = 0xFFFF;
        let odd = Note::from_words(&w);
        assert_eq!((odd.event, odd.state), (Event::None, CodeState::Idle));
        let (h, t) = Note::edges_of("ab");
        assert_eq!((&h, &t), (b"ab  ", b"ab  "));
        let (h, t) = Note::edges_of("");
        assert_eq!((&h, &t), (b"    ", b"    "));
        for (i, e) in EVENTS.iter().enumerate() {
            assert_eq!(Event::from_code(e.code()), *e);
            assert_eq!(e.code() as usize, i);
        }
    }

    #[test]
    fn names_are_cut_to_known_glyphs() {
        let known = |c: char| c.is_ascii() || c == 'あ';
        let show = |raw: &[u8]| {
            let mut l = LineBuf::new();
            push_name(&mut l, &sanitize_name(raw, known));
            l.as_str().to_string()
        };
        assert_eq!(show("Abc".as_bytes()), "Abc");
        assert_eq!(show("あaい".as_bytes()), "あa?");
        assert_eq!(show("😀x".as_bytes()), "?x");
        assert_eq!(show(b"a\x07b"), "a?b");
        assert_eq!(show(&[0xFF, b'a']), "?a", "UTF-8 でない");
        assert_eq!(show(b""), "");
        assert_eq!(show(&[b'z'; 32]).chars().count(), NAME_UNITS);
    }

    #[test]
    fn every_drawn_character_is_in_the_glyph_table() {
        for lang in Lang::ALL {
            every_drawn_character_is_in_the_glyph_table_in(lang);
        }
    }

    fn every_drawn_character_is_in_the_glyph_table_in(lang: Lang) {
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let known = |c: char| spec.slot_of(c).is_some();
        for tx in TEXTS {
            for t in [tx.ja, tx.en] {
                for c in t.chars() {
                    assert!(known(c), "TEXTS の文字 {c:?} が文字の表に無い（{t}）");
                }
            }
        }
        for t in [
            T_HUD_DELAY.get(lang),
            T_HUD_PING.get(lang),
            T_HUD_MS.get(lang),
            T_HUD_REWIND.get(lang),
            T_HUD_RECOMMEND_OPEN.get(lang),
            T_HUD_RECOMMEND_CLOSE.get(lang),
            T_HUD_AUTO_MARK.get(lang),
            T_HUD_FORMAT_SEP.get(lang),
            T_FORMAT.get(lang),
            T_DELAY_ROW.get(lang),
            T_FORMAT_ROLLBACK.get(lang),
            T_FORMAT_DELAY.get(lang),
            T_DELAY_AUTO.get(lang),
            T_HUD_WATCH_PING.get(lang),
            T_HUD_WATCH_DELAY.get(lang),
            T_HUD_WATCH_ROLLBACK.get(lang),
            T_HUD_WATCH.get(lang),
            T_HUD_WATCH_FF.get(lang),
        ] {
            for c in t.chars() {
                assert!(known(c), "文字 {c:?}（{t}）");
            }
        }
        for (rtt, rewind) in [(Some(999), Some(255)), (None, None)] {
            let mut l = LineBuf::new();
            hud_line(
                &mut l,
                lang,
                &HudView {
                    latency: Some(9),
                    rtt_ms: rtt,
                    recommend: rtt.map(|_| 9),
                    rewind,
                    ..HudView::default()
                },
            );
            for c in l.as_str().chars() {
                assert!(known(c), "試合中の行の文字 {c:?}（{}）", l.as_str());
            }
        }
        let conns = [
            Conn::Waiting,
            Conn::Ended,
            Conn::Finished,
            Conn::Connected {
                seat: Seat::P1,
                host: true,
                shape: Some((Mode::Rollback, 2)),
            },
            Conn::Connected {
                seat: Seat::P2,
                host: false,
                shape: Some((Mode::Delay, 9)),
            },
            Conn::Connected {
                host: true,
                seat: Seat::P2,
                shape: None,
            },
            Conn::Found {
                seat: Seat::P1,
                shape: Some((Mode::Rollback, 1)),
                ready: false,
                left_secs: 599,
            },
            Conn::Found {
                seat: Seat::P2,
                shape: None,
                ready: true,
                left_secs: 0,
            },
            Conn::Unmatched(NoMatch::TimedOut),
            Conn::Unmatched(NoMatch::PeerHasNoGate),
            Conn::Unmatched(NoMatch::PeerLeft),
            Conn::OtherVersion { theirs: 255 },
            Conn::OtherBuild { theirs: 0 },
            Conn::OtherBuild { theirs: 20_261_008 },
            Conn::Desynced,
        ];
        let mut checked = 0usize;
        for (k, ev) in EVENTS.into_iter().enumerate() {
            for st in [
                CodeState::Idle,
                CodeState::Offered,
                CodeState::Replied,
                CodeState::Linking,
                CodeState::RoomJoining,
                CodeState::RoomWaiting,
                CodeState::RoomLinking,
                CodeState::SpectateJoining,
                CodeState::SpectateWaiting,
                CodeState::SpectateLinking,
                CodeState::SpectateIdle,
            ] {
                for conn in conns {
                    for (shape, room) in [
                        (
                            Shape {
                                mode: Mode::Rollback,
                                delay: 1,
                            },
                            RoomItem::On(RoomBuf::from_name("Abc-_09").unwrap()),
                        ),
                        (
                            Shape {
                                mode: Mode::Delay,
                                delay: 0,
                            },
                            RoomItem::Off,
                        ),
                    ] {
                        let mut note = made_note();
                        note.event = ev;
                        note.state = st;
                        note.paste_len = 12345;
                        note.name = sanitize_name("鬱abc".as_bytes(), known);
                        note.room = sanitize_room("鬱の部屋room", known);
                        note.room_why = RoomNameWhy::ALL[k % RoomNameWhy::ALL.len()];
                        note.retry_secs = 65_535;
                        note.since = 1_000_000 - 599;
                        let mut m = Menu::new(shape).with_room(room).with_lang(lang);
                        m.open(0);
                        let mut rows = MenuRows::new();
                        m.render(conn, &note, 1_000_000, &mut rows);
                        let mut corner = [LineBuf::new(); 2];
                        let mut cs = [RowStyle::Normal; 2];
                        let nc =
                            corner_lines(conn, &note, 1_000_000, None, lang, &mut corner, &mut cs);
                        for l in rows.lines[..rows.len].iter().chain(&corner[..nc]) {
                            for c in l.as_str().chars() {
                                assert!(
                                    known(c),
                                    "文字 {c:?} が文字の表に無い（{:?}, {ev:?}, {st:?}, {conn:?}）",
                                    l.as_str()
                                );
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 10_000, "文字を見た数 {checked}");
        assert!(spec.slot_of('鬱').is_none());
    }

    #[test]
    fn the_desync_lines_are_the_two_warning_lines_the_user_asked_for() {
        for lang in Lang::ALL {
            the_desync_lines_are_the_two_warning_lines_the_user_asked_for_in(lang);
        }
    }

    fn the_desync_lines_are_the_two_warning_lines_the_user_asked_for_in(lang: Lang) {
        let spec = game_font_spec("");
        let limit = 640 - 2 * 6;
        let mut out = [LineBuf::new(); 2];
        let mut st = [RowStyle::Normal; 2];
        let n = corner_lines(Conn::Desynced, &quiet(), 0, None, lang, &mut out, &mut st);
        assert_eq!(n, 2);
        let (first, second) = match lang {
            Lang::Ja => (
                "同期がずれたため、試合を終了しました。",
                "HiganRTool の logs フォルダのログを開発者へ送ってください（Esc でメニューへ）。",
            ),
            Lang::En => (
                "Desync detected. This match was stopped.",
                "Send the logs in the logs folder to the developer (Esc: menu).",
            ),
        };
        assert_eq!(out[0].as_str(), first);
        assert_eq!(out[1].as_str(), second);
        assert_eq!(st, [RowStyle::Warn, RowStyle::Warn]);
        for l in &out {
            let w = crate::ui_layout::text_width(&spec, l.as_str());
            assert!(w <= limit, "{w} 画素 > {limit}（{}）", l.as_str());
        }
        assert!(!corner_is_band(Conn::Desynced, &quiet(), None));
        assert!(!Conn::Desynced.is_end());
        assert!(!Conn::Desynced.is_linked());
        let mut m = Menu::new(ini()).with_lang(lang);
        m.open(0);
        let mut rows = MenuRows::new();
        m.render(Conn::Desynced, &quiet(), 0, &mut rows);
        let texts: Vec<_> = rows.lines[..rows.len].iter().map(LineBuf::as_str).collect();
        assert!(texts.contains(&first), "{texts:?}");
        assert!(texts.contains(&second), "{texts:?}");
    }

    #[test]
    fn the_other_version_lines_name_both_versions_everywhere() {
        for lang in Lang::ALL {
            the_other_version_lines_name_both_versions_everywhere_in(lang);
        }
    }

    fn the_other_version_lines_name_both_versions_everywhere_in(lang: Lang) {
        let spec = game_font_spec("");
        let limit = 640
            - 2 * crate::ui_layout::SCREEN_MARGIN
            - 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        for theirs in [0u8, 8] {
            assert_ne!(theirs, PROTO_VER);
            let (want, next) = match lang {
                Lang::Ja => (
                    format!(
                        "相手の HiganRTool のバージョンが違います（自分 {PROTO_VER}・相手 {theirs}）。"
                    ),
                    "相手と同じ zip の HiganRTool を使ってください。",
                ),
                Lang::En => (
                    format!(
                        "The opponent's HiganRTool protocol differs (you {PROTO_VER}, opponent {theirs})."
                    ),
                    "Use the same HiganRTool zip as your opponent.",
                ),
            };
            let conn = Conn::OtherVersion { theirs };
            assert!(!conn.is_linked());
            let mut out = [LineBuf::new(); 2];
            let mut st = [RowStyle::Normal; 2];
            let n = corner_lines(conn, &quiet(), 0, None, lang, &mut out, &mut st);
            assert_eq!(
                (n, out[0].as_str(), out[1].as_str()),
                (2, want.as_str(), next)
            );
            assert_eq!(st, [RowStyle::Warn, RowStyle::Warn]);
            for l in &out {
                let w = crate::ui_layout::text_width(&spec, l.as_str());
                assert!(w <= limit, "{w} 画素 > {limit}（{}）", l.as_str());
                for bad in ["proto_", "PROTO", "HELLO", "READY", " Z", " X"] {
                    assert!(!l.as_str().contains(bad), "{bad}");
                }
            }
            let mut m = Menu::new(ini()).with_lang(lang);
            m.open(0);
            let mut rows = MenuRows::new();
            m.render(conn, &quiet(), 0, &mut rows);
            let lines: Vec<&str> = rows.lines[..rows.len].iter().map(|l| l.as_str()).collect();
            assert!(
                lines.contains(&want.as_str()) && lines.contains(&next),
                "{lines:?}"
            );
            let mut note = made_note();
            note.event = Event::OtherVersion;
            note.peer_ver = theirs;
            let note = Note::from_words(&note.to_words());
            assert_eq!(
                note.peer_ver, theirs,
                "状態の入れ物の往復で相手のバージョンが落ちた"
            );
            let mut m = Menu::new(ini()).with_lang(lang);
            m.open(0);
            let mut rows = MenuRows::new();
            m.render(Conn::Waiting, &note, 1_000_000, &mut rows);
            let lines: Vec<&str> = rows.lines[..rows.len].iter().map(|l| l.as_str()).collect();
            assert!(
                lines.contains(&want.as_str()) && lines.contains(&next),
                "{lines:?}"
            );
            assert_eq!(
                other_version_text_in(lang, PROTO_VER, theirs),
                format!("{want}{next}")
            );
            if lang == Lang::Ja {
                assert_eq!(
                    other_version_text(PROTO_VER, theirs),
                    format!("{want}{next}")
                );
            }
        }
    }

    #[test]
    fn every_menu_row_fits_the_box_and_the_screen() {
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let known = |c: char| spec.slot_of(c).is_some();
        let mut widest = 0;
        let mut tallest = 0;
        let mut tallest_px = 0;
        let mut widest_corner = 0;
        let game = game_font_spec("");
        let mut widest_game = [0u32; 2];
        for lang in Lang::ALL {
            for ev in EVENTS {
                for st in ALL_STATES {
                    for (room, with_name) in [
                        (RoomItem::Off, false),
                        (RoomItem::On(RoomBuf::new()), false),
                        (RoomItem::On(RoomBuf::new()), true),
                        (
                            RoomItem::On(RoomBuf::from_name(&"W".repeat(32)).unwrap()),
                            true,
                        ),
                    ] {
                        for opening in [false, true] {
                            let mut note = made_note();
                            note.event = ev;
                            note.state = st;
                            note.paste_len = 4_294_967_295;
                            note.code_len = 304;
                            note.name = sanitize_name(&[b'W'; 32], known);
                            note.room = sanitize_room(&"部".repeat(40), known);
                            note.retry_secs = u16::MAX;
                            note.since = 1_000_000u32.wrapping_sub(86_399 * 100);
                            let mut m = Menu::new(ini()).with_room(room).with_lang(lang);
                            if with_name {
                                m = m
                                    .with_player_name(NameBuf::lossy(&"W".repeat(32)))
                                    .with_seat(SeatWish::Any);
                            }
                            m.open(0);
                            m.set_opening(opening);
                            let mut rows = MenuRows::new();
                            m.render(Conn::Waiting, &note, 1_000_000, &mut rows);
                            assert!(rows.len <= MENU_ROWS);
                            tallest = tallest.max(rows.len);
                            tallest_px = tallest_px.max(crate::ui_layout::rows_height(
                                &spec,
                                &rows.styles[..rows.len],
                            ));
                            assert_eq!(
                                rows.lines[rows.len - 1].as_str(),
                                T_KEYS.get(lang),
                                "{lang:?} {ev:?} {st:?}"
                            );
                            assert_eq!(
                                rows.lines[rows.len - 2].as_str(),
                                "",
                                "{ev:?} {st:?}: 行が `MENU_ROWS` を越えて最後の行を潰した"
                            );
                            for l in &rows.lines[..rows.len] {
                                assert!(l.as_str().chars().count() <= crate::overlay::MAX_TEXT);
                                widest =
                                    widest.max(crate::ui_layout::text_width(&spec, l.as_str()));
                                let g = &mut widest_game[lang.index()];
                                *g = (*g).max(crate::ui_layout::text_width(&game, l.as_str()));
                            }
                            let mut corner = [LineBuf::new(); 2];
                            let mut cs = [RowStyle::Normal; 2];
                            let nc = corner_lines(
                                Conn::Waiting,
                                &note,
                                1_000_000,
                                None,
                                lang,
                                &mut corner,
                                &mut cs,
                            );
                            for l in &corner[..nc] {
                                widest_corner = widest_corner
                                    .max(crate::ui_layout::text_width(&spec, l.as_str()));
                            }
                        }
                    }
                }
            }
        }
        let limit = 640
            - 2 * crate::ui_layout::SCREEN_MARGIN
            - 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        assert!(widest <= limit, "いちばん広い行 {widest} 画素 > {limit}");
        // 実機は英数字がゲームの文字（9 画素送り）になるので、そちらでも枠に収める。
        for lang in Lang::ALL {
            let g = widest_game[lang.index()];
            assert!(
                g > 0 && g <= limit,
                "{lang:?}: ゲームの文字でいちばん広い行 {g} 画素 > {limit}"
            );
        }
        assert!(
            widest_corner <= limit,
            "いちばん広い左上の行 {widest_corner} 画素 > {limit}"
        );
        assert_eq!(tallest, MENU_ROWS, "行が全部埋まる組が無い");
        let h = tallest_px + 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        assert!(
            h <= 480 - 2 * crate::ui_layout::SCREEN_MARGIN,
            "枠の高さ {h} 画素"
        );
    }

    #[test]
    fn the_line_buffer_never_overflows_or_splits_a_character() {
        let mut l = LineBuf::new();
        for _ in 0..200 {
            l.push_str("あ");
        }
        assert!(l.as_str().chars().all(|c| c == 'あ'));
        assert_eq!(l.as_str().len() % 3, 0);
        assert!(l.as_str().len() <= LINE_BYTES);
        let mut l = LineBuf::new();
        l.push_u32(0)
            .push_char(' ')
            .push_u32(4_294_967_295)
            .push_char(' ');
        l.push_clock(0)
            .push_char(' ')
            .push_clock(59)
            .push_char(' ')
            .push_clock(61)
            .push_char(' ')
            .push_clock(1789);
        assert_eq!(l.as_str(), "0 4294967295 0:00 0:59 1:01 29:49");
    }

    #[test]
    fn times_clamp_a_clock_that_went_backwards() {
        let n = Note {
            since: 5_000,
            ..Note::default()
        };
        assert_eq!(times(&n, 5_042), (42, DEFAULT_TTL_SECS - 42));
        assert_eq!(
            times(&n, 5_000 + DEFAULT_TTL_SECS + 10),
            (DEFAULT_TTL_SECS + 10, 0)
        );
        assert_eq!(times(&n, 100), (0, DEFAULT_TTL_SECS), "時計が戻った");
    }

    fn connected() -> Conn {
        Conn::Connected {
            seat: Seat::P2,
            host: false,
            shape: Some((Mode::Rollback, 1)),
        }
    }

    struct ConfirmDriver {
        m: Menu,
        answers: Vec<Answer>,
        conn: Conn,
    }

    impl ConfirmDriver {
        fn new() -> ConfirmDriver {
            let mut m = Menu::new(ini());
            m.open_confirm(0);
            ConfirmDriver {
                m,
                answers: Vec::new(),
                conn: connected(),
            }
        }

        fn tap(&mut self, key: u16) {
            for w in [key, 0] {
                let out = self.m.tick(&input(w, quiet(), self.conn));
                assert!(out.hold_game, "確かめの枠の間はゲームの入力を握る");
                assert_eq!(out.cmd, None);
                self.answers.extend(out.answer);
            }
        }

        fn text(&self) -> Vec<String> {
            let mut r = MenuRows::new();
            self.m.render(self.conn, &quiet(), 0, &mut r);
            r.lines[..r.len]
                .iter()
                .map(|l| l.as_str().to_string())
                .collect()
        }
    }

    #[test]
    fn the_leave_confirmation_defaults_to_no_and_passes_a_cancel_only_after_yes() {
        let mut d = ConfirmDriver::new();
        assert!(d.m.is_active() && d.m.is_confirm());
        let t = d.text();
        assert_eq!(t[0], "対戦から抜けますか？", "{t:?}");
        assert!(
            t.contains(&"  はい".to_string()) && t.contains(&"> いいえ".to_string()),
            "{t:?}"
        );
        assert_eq!(t.last().map(String::as_str), Some(T_KEYS_CONFIRM.ja));
        let mut held = ConfirmDriver::new();
        held.m.open_confirm(Z);
        let out = held.m.tick(&input(Z, quiet(), held.conn));
        assert_eq!(out.answer, None);
        d.tap(Z);
        assert_eq!(d.answers, vec![Answer::Stay]);
        assert!(!d.m.is_active() && !d.m.take_pending_cancel());
        for key in [X, ESC] {
            let mut d = ConfirmDriver::new();
            d.tap(key);
            assert_eq!(d.answers, vec![Answer::Stay], "key {key:#x}");
            assert!(!d.m.take_pending_cancel());
        }
        let mut d = ConfirmDriver::new();
        d.tap(UP);
        assert!(d.text().contains(&"> はい".to_string()));
        d.tap(Z);
        assert_eq!(d.answers, vec![Answer::Leave]);
        assert!(!d.m.is_active(), "語が 0 になった tick で閉じ終わる");
        assert!(d.m.take_pending_cancel());
        assert!(!d.m.take_pending_cancel(), "取消は 1 回だけ");
        let mut d = ConfirmDriver::new();
        d.tap(DOWN);
        assert!(d.text().contains(&"> はい".to_string()));
        d.tap(DOWN);
        assert!(d.text().contains(&"> いいえ".to_string()));
        let mut d = ConfirmDriver::new();
        d.tap(UP);
        d.tap(0x1000);
        assert_eq!(d.answers, vec![Answer::Leave]);
    }

    #[test]
    fn the_confirmation_closes_without_an_answer_when_the_link_is_gone() {
        let mut d = ConfirmDriver::new();
        d.conn = Conn::Finished;
        let out = d.m.tick(&input(Z, quiet(), d.conn));
        assert_eq!(out.answer, None);
        assert!(out.hold_game, "閉じ終わるまで 0 を流す");
        d.m.tick(&input(0, quiet(), d.conn));
        assert!(!d.m.is_active());
        assert!(!d.m.take_pending_cancel());
        let mut d = ConfirmDriver::new();
        d.tap(UP);
        d.m.tick(&input(Z, quiet(), d.conn));
        d.m.reset();
        assert!(!d.m.take_pending_cancel());
    }

    #[test]
    fn the_opening_line_shows_only_while_the_net_is_opening() {
        let mut m = Menu::new(ini());
        m.open(0);
        let mut rows = MenuRows::new();
        m.render(Conn::Waiting, &quiet(), 0, &mut rows);
        let plain: Vec<String> = rows.lines[..rows.len]
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        assert!(!plain.iter().any(|l| l.contains("準備")), "{plain:?}");
        m.set_opening(true);
        m.render(Conn::Waiting, &quiet(), 0, &mut rows);
        let t: Vec<String> = rows.lines[..rows.len]
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        assert!(t.contains(&T_OPENING.ja.to_string()), "{t:?}");
        assert!(
            t.iter().any(|l| l.contains(T_CREATE.ja)) && t.iter().any(|l| l.contains(T_PASTE.ja))
        );
        m.set_opening(false);
        m.render(Conn::Waiting, &quiet(), 0, &mut rows);
        let t2: Vec<String> = rows.lines[..rows.len]
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        assert_eq!(t2, plain);
    }

    #[test]
    fn texts_name_buttons_not_keys_and_say_delay_not_okure() {
        for line in [T_KEYS.ja, T_KEYS_CONFIRM.ja] {
            assert!(
                line.contains("決定") && line.contains("キャンセル"),
                "{line}"
            );
            for key in [" Z ", " X ", " V ", "Z ", "X ", "Zキー", "Xキー", "Vキー"] {
                assert!(!line.contains(key), "{line} に {key:?}");
            }
        }
        for t in TEXTS.iter().map(|t| t.ja) {
            assert!(!t.contains("遅れ"), "「遅れ」は「delay」に替えた: {t}");
            assert!(!t.contains("フレーム"), "{t}");
        }
    }

    fn texts_of(m: &Menu, note: &Note) -> Vec<String> {
        let mut r = MenuRows::new();
        m.render(Conn::Waiting, note, 1_000_000, &mut r);
        r.lines[..r.len]
            .iter()
            .map(|l| l.as_str().to_string())
            .collect()
    }

    #[test]
    fn the_room_item_shows_only_with_a_room_server_and_sits_above_create() {
        let off = Menu::new(ini());
        let mut m = off;
        m.open(0);
        let t = texts_of(&m, &quiet());
        assert!(
            !t.iter()
                .any(|l| l.trim_start_matches(['>', ' ']) == T_ROOM.ja),
            "{t:?}"
        );
        assert!(!t.iter().any(|l| l == T_GROUP_PLAY.ja), "{t:?}");
        assert_eq!(t[0], T_GROUP_CODE.ja, "{t:?}");
        assert_eq!(t[1], format!("> {}", T_CREATE.ja), "{t:?}");
        let mut d = Driver::new();
        d.m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
        d.m.open(0);
        let t = d.text();
        assert_eq!(t[0], T_GROUP_PLAY.ja, "{t:?}");
        assert_eq!(t[1], format!("> {}", T_RANDOM.ja), "{t:?}");
        assert_eq!(t[2], "", "ランダムマッチとルームマッチの間は小さな隙間");
        assert_eq!(t[3], format!("  {}", T_ROOM.ja), "{t:?}");
        assert!(t[4].starts_with(&format!("  {}", T_ROOM_NAME.ja)), "{t:?}");
        assert_eq!(t[5], "");
        assert_eq!(t[6], T_GROUP_WATCH.ja, "{t:?}");
        assert_eq!(
            t[7],
            format!("  {}", T_RANDOM.ja),
            "観戦の先頭はランダムマッチ"
        );
        assert_eq!(
            t[8], "",
            "観戦のランダムマッチとルームマッチの間も小さな隙間"
        );
        assert_eq!(t[9], format!("  {}", T_ROOM.ja), "観戦の中のルームマッチ");
        assert_eq!(t[10], "");
        assert_eq!(t[11], T_GROUP_CODE.ja, "{t:?}");
        assert_eq!(t[12], format!("  {}", T_CREATE.ja), "{t:?}");
        d.cursor_to(Item::Room);
        d.tap(Z);
        assert_eq!(d.cmds, vec![], "入力を経ずに指示が出た");
        assert!(d.m.wants_text(), "入力の状態にならない");
        d.tap(ESC);
        assert!(!d.m.wants_text() && d.m.is_active());
        for _ in 0..4 {
            d.tap(DOWN);
        }
        d.tap(Z);
        assert_eq!(d.cmds.last(), Some(&Cmd::Create));
        for _ in 0..4 {
            d.tap(UP);
        }
        assert_eq!(d.m.cursor, 1, "ランダムマッチの下はルームマッチ");
        d.tap(UP);
        assert_eq!(d.m.cursor, 0, "先頭はランダムマッチ");
        d.tap(UP);
        assert_eq!(d.m.cursor, 10, "上の端で折り返して戻る");
        d.note.state = CodeState::RoomWaiting;
        let t = d.text();
        assert!(t.iter().any(|l| l.contains(T_CANCEL.ja)), "{t:?}");
        assert_eq!(t[1], format!("  {}", T_RANDOM.ja), "待ちの間も選び直せる");
        d.cursor_to(Item::Random);
        for _ in 0..9 {
            d.tap(DOWN);
        }
        assert_eq!(d.m.cursor, 0, "9 つで 1 周");
        for _ in 0..7 {
            d.tap(DOWN);
        }
        d.tap(Z);
        assert_eq!(d.cmds.last(), Some(&Cmd::Cancel));
        let mut m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
        m.open(0);
        let mut rows = MenuRows::new();
        m.render(connected(), &quiet(), 0, &mut rows);
        assert!(
            !rows.lines[..rows.len]
                .iter()
                .any(|l| l.as_str().contains(T_ROOM.ja))
        );
    }

    /// 名前・席・ルーム名（空）が全部ある、待ちの外のメニュー（項目が最大の組）。
    fn full_menu_driver() -> Driver {
        let mut d = Driver::new();
        d.m = Menu::new(ini())
            .with_room(RoomItem::On(RoomBuf::new()))
            .with_player_name(NameBuf::lossy("Alice"))
            .with_seat(SeatWish::P1);
        d.m.open(0);
        d
    }

    #[test]
    fn group_headings_come_once_before_their_first_item_and_the_cursor_never_rests_on_them() {
        let d = full_menu_driver();
        assert_eq!(
            d.text(),
            [
                T_GROUP_PLAY.ja.to_string(),
                format!("> {}", T_RANDOM.ja),
                String::new(),
                format!("  {}", T_ROOM.ja),
                format!("  {}　{}", T_ROOM_NAME.ja, T_ROOM_UNSET.ja),
                String::new(),
                T_GROUP_WATCH.ja.to_string(),
                format!("  {}", T_RANDOM.ja),
                String::new(),
                format!("  {}", T_ROOM.ja),
                String::new(),
                T_GROUP_CODE.ja.to_string(),
                format!("  {}", T_CREATE.ja),
                format!("  {}", T_PASTE.ja),
                String::new(),
                T_GROUP_SETTINGS.ja.to_string(),
                format!("  {}Alice", lab(T_NAME, Lang::Ja)),
                format!("  {}◀ 1P ▶", lab(T_SEAT, Lang::Ja)),
                format!("  {}◀ rollback ▶", lab(T_FORMAT, Lang::Ja)),
                format!("  {}◀ 1F ▶", lab(T_DELAY_ROW, Lang::Ja)),
                format!("  {}◀ 日本語 ▶", lab(T_LANG, Lang::Ja)),
                String::new(),
                format!("  {}", T_BACK.ja),
                String::new(),
                T_KEYS.ja.to_string(),
            ]
        );
        let styles: Vec<RowStyle> = d.rows().styles[..d.rows().len].to_vec();
        assert_eq!(
            styles[..15],
            [
                RowStyle::Title,
                RowStyle::Selected,
                RowStyle::GapSmall,
                RowStyle::Normal,
                RowStyle::Normal,
                RowStyle::GapMedium,
                RowStyle::Title,
                RowStyle::Normal,
                RowStyle::GapSmall,
                RowStyle::Normal,
                RowStyle::GapMedium,
                RowStyle::Title,
                RowStyle::Normal,
                RowStyle::Normal,
                RowStyle::GapMedium,
            ]
        );
        assert_eq!(
            styles[21],
            RowStyle::GapMedium,
            "もどるの前は中くらいの隙間（大きいと枠が画面の高さを越える）"
        );
        let r = d.rows();
        let styles: Vec<(&str, RowStyle)> = r.lines[..r.len]
            .iter()
            .zip(&r.styles[..r.len])
            .filter(|(l, _)| {
                [
                    T_GROUP_PLAY.ja,
                    T_GROUP_WATCH.ja,
                    T_GROUP_CODE.ja,
                    T_GROUP_SETTINGS.ja,
                ]
                .contains(&l.as_str())
            })
            .map(|(l, s)| (l.as_str(), *s))
            .collect();
        assert_eq!(
            styles,
            [
                (T_GROUP_PLAY.ja, RowStyle::Title),
                (T_GROUP_WATCH.ja, RowStyle::Title),
                (T_GROUP_CODE.ja, RowStyle::Title),
                (T_GROUP_SETTINGS.ja, RowStyle::Title)
            ]
        );
        let mut m = Menu::new(ini());
        m.open(0);
        assert_eq!(
            texts_of(&m, &quiet())[..10],
            [
                T_GROUP_CODE.ja.to_string(),
                format!("> {}", T_CREATE.ja),
                format!("  {}", T_PASTE.ja),
                String::new(),
                T_GROUP_SETTINGS.ja.to_string(),
                format!("  {}◀ rollback ▶", lab8(T_FORMAT, Lang::Ja)),
                format!("  {}◀ 1F ▶", lab8(T_DELAY_ROW, Lang::Ja)),
                format!("  {}◀ 日本語 ▶", lab8(T_LANG, Lang::Ja)),
                String::new(),
                format!("  {}", T_BACK.ja),
            ]
        );
        let mut d = full_menu_driver();
        d.note.state = CodeState::RoomWaiting;
        d.note.since = 1_000_000 - 42;
        let t = d.text();
        assert_eq!(
            t[..17],
            [
                T_GROUP_PLAY.ja.to_string(),
                format!("> {}", T_RANDOM.ja),
                String::new(),
                format!("  {}{}0:42", T_ROOM.ja, T_WAITING_MARK.ja),
                String::new(),
                T_GROUP_CODE.ja.to_string(),
                format!("  {}", T_CREATE.ja),
                format!("  {}", T_PASTE.ja),
                String::new(),
                T_GROUP_SETTINGS.ja.to_string(),
                format!("  {}◀ rollback ▶", lab8(T_FORMAT, Lang::Ja)),
                format!("  {}◀ 1F ▶", lab8(T_DELAY_ROW, Lang::Ja)),
                format!("  {}◀ 日本語 ▶", lab8(T_LANG, Lang::Ja)),
                String::new(),
                format!("  {}", T_CANCEL.ja),
                format!("  {}", T_BACK.ja),
                String::new(),
            ],
            "{t:?}"
        );
        let mut d = full_menu_driver();
        d.note.state = CodeState::SpectateIdle;
        assert_eq!(
            d.text()[..6],
            [
                T_GROUP_WATCH.ja.to_string(),
                format!("> {}", T_RANDOM.ja),
                String::new(),
                format!("  {}", T_ROOM.ja),
                String::new(),
                format!("  {}", T_BACK.ja),
            ]
        );
        let mut d = full_menu_driver();
        d.conn = connected();
        assert_eq!(d.text()[0], format!("> {}", T_BACK.ja));
        let mut d = full_menu_driver();
        d.note = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
        assert_eq!(d.text()[0], format!("> {}", T_SPECTATE_CANCEL.ja));
        let mut m = Menu::new(ini());
        m.open(0);
        let note = spectate_note(CodeState::SpectateIdle, Event::None);
        assert_eq!(texts_of(&m, &note)[0], format!("> {}", T_BACK.ja));
        let mut d = full_menu_driver();
        let (items, n) = d.m.items(d.conn, d.note.state);
        assert_eq!(n, 13, "{:?}", &items[..n]);
        for i in 0..2 * n {
            let r = d.rows();
            let marked: Vec<&str> = r.lines[..r.len]
                .iter()
                .map(LineBuf::as_str)
                .filter(|l| l.starts_with("> "))
                .collect();
            assert_eq!(marked.len(), 1, "{i} 回目: {marked:?}");
            let selected: Vec<RowStyle> = r.styles[..r.len]
                .iter()
                .copied()
                .filter(|s| *s == RowStyle::Selected)
                .collect();
            assert_eq!(selected.len(), 1, "{i} 回目");
            for (l, s) in r.lines[..r.len].iter().zip(&r.styles[..r.len]) {
                if *s == RowStyle::Title {
                    assert!(!l.as_str().starts_with("> "), "見出しに印: {}", l.as_str());
                }
            }
            let at = d.m.settled_cursor(&d.m.items(d.conn, d.note.state));
            assert_eq!(
                at,
                i % n,
                "{i} 回目: 見出しや隙間の行を数えて動いた（開いたときは「ランダムマッチ」から）"
            );
            d.tap(DOWN);
        }
        assert_eq!(d.m.cursor, 0, "2 周して戻る");
        d.tap(UP);
        assert_eq!(d.m.cursor, n - 1);
        assert!(d.text().contains(&format!("> {}", T_BACK.ja)));
    }

    #[test]
    fn settings_run_name_room_name_seat_then_shape() {
        let order = [
            Item::Name,
            Item::RoomName,
            Item::Seat,
            Item::Format,
            Item::Delay,
            Item::Lang,
        ];
        let settings = |m: &Menu, state: CodeState| {
            items_of(m, Conn::Waiting, state)
                .into_iter()
                .filter(|i| order.contains(i))
                .collect::<Vec<_>>()
        };
        let full = full_menu_driver().m;
        assert_eq!(
            items_of(&full, Conn::Waiting, CodeState::Idle),
            [
                Item::Random,
                Item::Room,
                Item::RoomName,
                Item::SpectateRandom,
                Item::Spectate,
                Item::Create,
                Item::Paste,
                Item::Name,
                Item::Seat,
                Item::Format,
                Item::Delay,
                Item::Lang,
                Item::Back
            ]
        );
        assert!(items_of(&full, Conn::Waiting, CodeState::Idle).len() <= ITEMS_MAX);
        for (name, room, seat) in (0..8).map(|b| (b & 1 != 0, b & 2 != 0, b & 4 != 0)) {
            let mut m = Menu::new(ini());
            if name {
                m = m.with_player_name(NameBuf::lossy("Alice"));
            }
            if room {
                m = m.with_room(RoomItem::On(RoomBuf::new()));
            }
            if seat {
                m = m.with_seat(SeatWish::P1);
            }
            let want: Vec<Item> = [
                (room, Item::RoomName),
                (name, Item::Name),
                (seat, Item::Seat),
                (true, Item::Format),
                (true, Item::Delay),
                (true, Item::Lang),
            ]
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, i)| i)
            .collect();
            assert_eq!(settings(&m, CodeState::Idle), want, "{name} {room} {seat}");
        }
        let mut off = Menu::new(ini())
            .with_player_name(NameBuf::lossy("Alice"))
            .with_seat(SeatWish::P1);
        off.open(0);
        assert_eq!(
            settings(&off, CodeState::Idle),
            [
                Item::Name,
                Item::Seat,
                Item::Format,
                Item::Delay,
                Item::Lang
            ]
        );
        for st in [
            CodeState::Offered,
            CodeState::RoomWaiting,
            CodeState::Linking,
        ] {
            assert_eq!(
                settings(&full, st),
                [Item::Format, Item::Delay, Item::Lang],
                "{st:?}"
            );
        }
        let mut opening = full_menu_driver();
        opening.m.set_opening(true);
        let (v, n) = opening.m.items(Conn::Waiting, CodeState::Idle);
        assert_eq!(
            v[..n]
                .iter()
                .filter(|i| order.contains(i))
                .copied()
                .collect::<Vec<_>>(),
            [Item::Format, Item::Delay, Item::Lang]
        );
        let t = full_menu_driver().text();
        let pos = |label: &str| {
            t.iter()
                .position(|l| l.trim_start_matches(['>', ' ']).starts_with(label))
                .unwrap_or_else(|| panic!("{label} の行が無い: {t:?}"))
        };
        assert!(pos(T_ROOM.ja) < pos(T_ROOM_NAME.ja), "{t:?}");
        assert!(pos(T_ROOM_NAME.ja) < pos(T_NAME.ja), "{t:?}");
        assert!(pos(T_NAME.ja) < pos(T_SEAT.ja), "{t:?}");
        assert!(pos(T_SEAT.ja) < pos(T_FORMAT.ja), "{t:?}");
        assert!(pos(T_FORMAT.ja) < pos(T_DELAY_ROW.ja), "{t:?}");
        assert!(pos(T_DELAY_ROW.ja) < pos(T_LANG.ja), "{t:?}");
        assert!(pos(T_GROUP_SETTINGS.ja) < pos(T_NAME.ja), "{t:?}");
    }

    #[test]
    fn a_decided_room_name_starts_without_the_input_and_an_empty_one_opens_it() {
        let abc = RoomBuf::from_name("abc").unwrap();
        let with_name = |name: RoomBuf| {
            let mut d = Driver::new();
            d.m = Menu::new(ini()).with_room(RoomItem::On(name));
            d.m.open(0);
            d.cursor_to(Item::Room);
            d
        };
        let mut d = with_name(abc);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::Room(abc)]);
        assert!(!d.m.wants_text(), "決まった名なのに入力が開いた");
        assert!(d.m.is_active(), "メニューが閉じた");
        d.cursor_to(Item::Spectate);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::Room(abc), Cmd::Spectate(abc)]);
        assert!(!d.m.wants_text(), "決まった名なのに観戦の入力が開いた");
        let mut d = with_name(RoomBuf::new());
        d.tap(Z);
        assert_eq!(d.cmds, vec![], "空の名で指示が出た");
        assert!(d.m.wants_text(), "空なのに入力が開かない");
        assert_eq!(d.m.edit_for, EditFor::Wait);
        d.tick_text(ESC, "");
        d.idle(2);
        assert!(!d.m.wants_text() && d.m.is_active());
        d.cursor_to(Item::Spectate);
        d.tap(Z);
        assert_eq!(d.cmds, vec![], "空の名で観戦の指示が出た");
        assert!(d.m.wants_text(), "空なのに観戦の入力が開かない");
        assert_eq!(d.m.edit_for, EditFor::Spectate);
        d.tick_text(0, "xyz");
        let out = d.tick_text(0, ENTER);
        let xyz = RoomBuf::from_name("xyz").unwrap();
        assert_eq!(out.cmd, Some(Cmd::Spectate(xyz)));
        d.idle(3);
        d.tap(Z);
        assert!(!d.m.wants_text());
        assert_eq!(d.cmds.last(), Some(&Cmd::Spectate(xyz)));
        assert_eq!(d.cmds.len(), 2, "{:?}", d.cmds);
    }

    #[test]
    fn deciding_the_room_name_setting_only_asks_to_remember_it_and_the_next_start_uses_it() {
        let row = |d: &Driver, name: &str| {
            d.text()
                .iter()
                .any(|l| l.trim_start_matches(['>', ' ']) == format!("{}　{name}", T_ROOM_NAME.ja))
        };
        let mut d = Driver::open_room_set_input("");
        assert_eq!(d.text()[0], T_ROOM_NAME.ja);
        d.tick_text(0, "my-room");
        let out = d.tick_text(0, ENTER);
        let mine = RoomBuf::from_name("my-room").unwrap();
        assert_eq!(
            out.cmd,
            Some(Cmd::RoomSet(mine)),
            "設定の Enter の指示が違う"
        );
        assert_eq!(d.cmds, vec![Cmd::RoomSet(mine)]);
        assert!(
            !d.m.wants_text() && d.m.is_active(),
            "決めても入力が閉じない"
        );
        d.idle(2);
        assert!(row(&d, "my-room"), "{:?}", d.text());
        d.cursor_to(Item::Room);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::RoomSet(mine), Cmd::Room(mine)]);
        d.cursor_to(Item::Spectate);
        d.tap(Z);
        assert_eq!(
            d.cmds,
            vec![Cmd::RoomSet(mine), Cmd::Room(mine), Cmd::Spectate(mine)]
        );
        assert!(!d.m.wants_text());
        d.cursor_to(Item::RoomName);
        d.tap(Z);
        assert_eq!(d.input_line(), "ルーム名: my-room|");
        d.tick_text(0, "\u{8}".repeat(7).as_str());
        d.tick_text(0, "new");
        let out = d.tick_text(0, ENTER);
        let new = RoomBuf::from_name("new").unwrap();
        assert_eq!(out.cmd, Some(Cmd::RoomSet(new)));
        d.idle(2);
        assert!(row(&d, "new") && !row(&d, "my-room"), "{:?}", d.text());
        d.cursor_to(Item::Room);
        d.tap(Z);
        assert_eq!(d.cmds.last(), Some(&Cmd::Room(new)));
        assert_eq!(d.cmds.len(), 5, "{:?}", d.cmds);
        d.cursor_to(Item::RoomName);
        d.tap(Z);
        d.tick_text(0, "zzz");
        d.tick_text(ESC, "");
        d.idle(3);
        assert!(!d.m.wants_text() && d.m.is_active());
        assert!(row(&d, "new"), "{:?}", d.text());
        assert_eq!(d.cmds.len(), 5, "Esc で指示が出た");
        d.tap(Z);
        d.tick_text(0, "\u{8}".repeat(3).as_str());
        let out = d.tick_text(0, ENTER);
        assert_eq!(out.cmd, None);
        assert!(d.m.wants_text(), "空の名で入力が閉じた");
        assert!(d.text().iter().any(|l| l == T_ROOM_INPUT_EMPTY.ja));
    }

    #[test]
    fn the_two_rows_use_the_hud_wording() {
        let value = |d: &Driver, label: Tx| {
            let want = format!("{}◀ ", lab8(label, Lang::Ja));
            d.text()
                .iter()
                .find_map(|l| {
                    l.trim_start_matches(['>', ' '])
                        .strip_prefix(&want)
                        .and_then(|v| v.strip_suffix(" ▶"))
                        .map(str::to_string)
                })
                .unwrap_or_else(|| panic!("{} の行が無い: {:?}", label.ja, d.text()))
        };
        for (mode, delay, fmt, dl) in [
            (Mode::Rollback, 0u8, T_FORMAT_ROLLBACK.ja, "0F"),
            (Mode::Rollback, 5, T_FORMAT_ROLLBACK.ja, "5F"),
            (Mode::Delay, 1, T_FORMAT_DELAY.ja, "1F"),
        ] {
            let mut d = Driver::new();
            d.m = Menu::new(Shape { mode, delay });
            d.m.open(0);
            assert_eq!(value(&d, T_FORMAT), fmt, "{mode:?}");
            assert_eq!(value(&d, T_DELAY_ROW), dl, "{mode:?}");
        }
        assert_eq!(
            (T_FORMAT_ROLLBACK.ja, T_FORMAT_DELAY.ja),
            ("rollback", "delay")
        );
        let mut d = Driver::new();
        d.cursor_to(Item::Delay);
        let mut seen = vec![value(&d, T_DELAY_ROW)];
        for key in [LEFT, LEFT, LEFT, RIGHT] {
            d.tap(key);
            seen.push(value(&d, T_DELAY_ROW));
        }
        assert_eq!(seen, ["1F", "0F", "auto", "auto", "0F"]);
        let t = d.text().join("\n");
        for old in ["通信方式", "Rollback（", "Delay", "delay 方式"] {
            assert!(!t.contains(old), "{old}: {t}");
        }
    }

    /// `CodeState` の全部（番号 0〜10）。
    const ALL_STATES: [CodeState; 11] = [
        CodeState::Idle,
        CodeState::Offered,
        CodeState::Replied,
        CodeState::Linking,
        CodeState::RoomJoining,
        CodeState::RoomWaiting,
        CodeState::RoomLinking,
        CodeState::SpectateJoining,
        CodeState::SpectateWaiting,
        CodeState::SpectateLinking,
        CodeState::SpectateIdle,
    ];

    fn spectate_note(state: CodeState, event: Event) -> Note {
        Note {
            seq: 1,
            event,
            state,
            room: sanitize_room("abc", |_| true),
            name: sanitize_name(b"peer", |_| true),
            since: 1_000_000 - 552,
            ..Note::default()
        }
    }

    fn items_of(m: &Menu, conn: Conn, state: CodeState) -> Vec<Item> {
        let (v, n) = m.items(conn, state);
        v[..n].to_vec()
    }

    #[test]
    fn the_spectate_item_follows_the_wait_state() {
        let on = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
        let off = Menu::new(ini());
        let w = Conn::Waiting;
        for st in ALL_STATES {
            let its = items_of(&off, w, st);
            assert!(
                !its.contains(&Item::Spectate)
                    && !its.contains(&Item::SpectateRandom)
                    && !its.contains(&Item::Room)
                    && !its.contains(&Item::Random),
                "{st:?}: {its:?}"
            );
        }
        assert_eq!(
            items_of(&on, w, CodeState::Idle),
            [
                Item::Random,
                Item::Room,
                Item::RoomName,
                Item::SpectateRandom,
                Item::Spectate,
                Item::Create,
                Item::Paste,
                Item::Format,
                Item::Delay,
                Item::Lang,
                Item::Back
            ]
        );
        for st in [
            CodeState::Offered,
            CodeState::Replied,
            CodeState::Linking,
            CodeState::RoomJoining,
            CodeState::RoomWaiting,
            CodeState::RoomLinking,
        ] {
            assert_eq!(
                items_of(&on, w, st),
                [
                    Item::Random,
                    Item::Room,
                    Item::Create,
                    Item::Paste,
                    Item::Format,
                    Item::Delay,
                    Item::Lang,
                    Item::Cancel,
                    Item::Back
                ],
                "{st:?}"
            );
        }
        for st in [
            CodeState::SpectateJoining,
            CodeState::SpectateWaiting,
            CodeState::SpectateLinking,
        ] {
            assert_eq!(items_of(&on, w, st), [Item::SpectateCancel], "{st:?}");
            assert_eq!(items_of(&off, w, st), [Item::SpectateCancel], "{st:?}");
        }
        assert_eq!(
            items_of(&on, w, CodeState::SpectateIdle),
            [Item::SpectateRandom, Item::Spectate, Item::Back]
        );
        assert_eq!(items_of(&off, w, CodeState::SpectateIdle), [Item::Back]);
        for st in ALL_STATES {
            assert_eq!(items_of(&on, connected(), st), [Item::Back], "{st:?}");
        }
        for st in ALL_STATES {
            assert!(items_of(&on, w, st).len() <= ITEMS_MAX, "{st:?}");
        }
        let mut d = Driver::new();
        d.m = on;
        d.m.open(0);
        d.note = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
        let t = d.text();
        assert_eq!(t[0], format!("> {}", T_SPECTATE_CANCEL.ja), "{t:?}");
        for gone in [
            T_CREATE.ja,
            T_PASTE.ja,
            T_FORMAT.ja,
            T_DELAY_ROW.ja,
            T_RANDOM.ja,
            T_ROOM.ja,
            T_ROOM_NAME.ja,
            T_CANCEL.ja,
        ] {
            assert!(!t.iter().any(|l| l.contains(gone)), "{gone}: {t:?}");
        }
        for heading in [
            T_GROUP_PLAY.ja,
            T_GROUP_WATCH.ja,
            T_GROUP_CODE.ja,
            T_GROUP_SETTINGS.ja,
        ] {
            assert!(!t.iter().any(|l| l == heading), "{heading}: {t:?}");
        }
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::SpectateCancel]);
        d.tap(DOWN);
        d.tap(UP);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::SpectateCancel, Cmd::SpectateCancel]);
    }

    #[test]
    fn a_spectate_cancel_returns_to_the_old_items_or_keeps_the_watcher_items() {
        let mut d = Driver::new();
        d.m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
        d.m.open(0);
        d.note = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::SpectateCancel]);
        d.note = spectate_note(CodeState::Idle, Event::SpectateCancelled);
        let t = d.text();
        assert_eq!(t[0], T_GROUP_PLAY.ja, "{t:?}");
        assert_eq!(t[1], format!("> {}", T_RANDOM.ja), "{t:?}");
        assert!(t.iter().any(|l| l.contains(T_CREATE.ja)), "{t:?}");
        assert!(t.contains(&T_SPECTATE_CANCELLED.ja.to_string()), "{t:?}");
        assert_eq!(corner(&d.note).0, 0, "取りやめた行が左上に残った");
        d.note = spectate_note(CodeState::SpectateIdle, Event::SpectateCancelled);
        let t = d.text();
        assert_eq!(t[0], T_GROUP_WATCH.ja, "{t:?}");
        assert_eq!(t[1], format!("> {}", T_RANDOM.ja), "{t:?}");
        assert_eq!(t[3], format!("  {}", T_ROOM.ja), "{t:?}");
        assert!(!t.iter().any(|l| l.contains(T_CREATE.ja)), "{t:?}");
        d.m.cursor = 4;
        let t = d.text();
        assert_eq!(
            t.iter().filter(|l| l.starts_with("> ")).collect::<Vec<_>>(),
            [&format!("> {}", T_BACK.ja)]
        );
        d.tap(Z);
        assert!(!d.m.wants_text() && !d.m.is_active(), "戻るで閉じない");
        assert_eq!(d.cmds, vec![Cmd::SpectateCancel], "決定が別の指示になった");
        d.m.open(0);
        d.cursor_to(Item::Spectate);
        d.tap(Z);
        assert!(d.m.wants_text());
        assert_eq!(d.m.edit_for, EditFor::Spectate);
    }

    /// ルーム名が空のメニューで観戦の「ルームマッチ」を決めて入力を開く。
    fn open_spectate_input() -> Driver {
        let mut d = Driver::new();
        d.m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
        d.m.open(0);
        d.cursor_to(Item::Spectate);
        d.tap(Z);
        assert!(d.m.wants_text(), "観戦の入力が開かない");
        assert_eq!(d.m.edit_for, EditFor::Spectate);
        d
    }

    #[test]
    fn spectate_input_is_the_room_input_with_its_own_title_and_command() {
        let mut d = open_spectate_input();
        let t = d.text();
        assert_eq!(t[0], T_SPECTATE.ja, "{t:?}");
        assert_eq!(d.input_line(), "ルーム名: |");
        assert_eq!(t.last().unwrap(), T_ROOM_INPUT_KEYS.ja);
        let w = Driver::open_room_input();
        assert_eq!(w.text()[0], T_ROOM.ja);
        assert_eq!(w.input_line(), d.input_line(), "初めの値が違う");
        d.tick_text(0, "last-room");
        assert_eq!(d.input_line(), "ルーム名: last-room|");
        d.tick_text(0, "\u{8}".repeat(TEXT_PER_TICK).as_str());
        let out = d.tick_text(0, ENTER);
        assert_eq!((out.cmd, d.m.wants_text()), (None, true));
        assert!(d.text().iter().any(|l| l == T_ROOM_INPUT_EMPTY.ja));
        d.tick_text(0, "ＡＢ-あ");
        assert_eq!(d.input_line(), "ルーム名: AB-|");
        let mut e = open_spectate_input();
        e.tick_text(0, "keep");
        e.tick_text(ESC, "");
        assert_eq!(
            (e.cmds.len(), e.m.wants_text(), e.m.is_active()),
            (0, false, true)
        );
        let out = d.tick_text(0, &format!("x{ENTER}"));
        let want = RoomBuf::from_name("AB-x").unwrap();
        assert_eq!(out.cmd, Some(Cmd::Spectate(want)));
        assert!(out.hold_game && !d.m.wants_text());
        d.idle(3);
        d.cursor_to(Item::Room);
        d.cmds.clear();
        d.tap(Z);
        assert!(!d.m.wants_text());
        assert_eq!(d.cmds, vec![Cmd::Room(want)]);
        d.idle(2);
        d.cursor_to(Item::RoomName);
        d.tap(Z);
        assert_eq!(d.m.edit_for, EditFor::RoomSet);
        assert_eq!(d.text()[0], T_ROOM_NAME.ja);
        assert_eq!(d.input_line(), "ルーム名: AB-x|");
    }

    fn event_text(ev: Event, f: impl Fn(&mut Note)) -> Vec<(String, bool)> {
        let mut n = spectate_note(CodeState::SpectateIdle, ev);
        n.room = sanitize_room("secretroom", |_| true);
        f(&mut n);
        let mut out = [LineBuf::new(); STATUS_ROWS];
        let mut w = [false; STATUS_ROWS];
        let k = event_lines(&n, Lang::Ja, &mut out, &mut w);
        (0..k)
            .map(|i| (out[i].as_str().to_string(), w[i]))
            .collect()
    }

    #[test]
    fn each_spectate_failure_has_the_decided_two_lines() {
        let none = |_: &mut Note| {};
        let tx = |v: Vec<(String, bool)>| v.into_iter().map(|(l, _)| l).collect::<Vec<_>>();
        assert_eq!(
            tx(event_text(Event::SpectateUnresponsive, none)),
            [
                "サーバーが応答しません。",
                "時間をおいて、観戦の「ルームマッチ」を選び直してください。"
            ]
        );
        assert_eq!(
            tx(event_text(Event::SpectateRefused, none)),
            [
                "サーバーがこのルーム名を受け付けませんでした。",
                "別のルーム名で、観戦の「ルームマッチ」を選び直してください。"
            ]
        );
        assert_eq!(
            tx(event_text(Event::SpectateBadCode, none)),
            [
                "相手から届いたコードを読めませんでした。",
                "観戦の「ルームマッチ」を選び直してください。"
            ]
        );
        for theirs in [0u8, 8] {
            let got = tx(event_text(Event::SpectateOtherVersion, |n| {
                n.peer_ver = theirs
            }));
            assert_eq!(got.len(), 2);
            assert_eq!(
                got.concat(),
                other_version_text(PROTO_VER, theirs),
                "バージョン違いの文が既存と違う"
            );
            assert!(got[0].contains(&format!("相手 {theirs}")), "{got:?}");
        }
        assert_eq!(JOINER_GIVE_UP_MS, 30 * 60 * 1000);
        assert_eq!(
            tx(event_text(Event::SpectateGaveUp, none)),
            [
                "30 分待っても、相手がルームに来ませんでした。",
                "ルーム名を確かめて、観戦の「ルームマッチ」を選び直してください。"
            ]
        );
        assert_eq!(
            tx(event_text(Event::SpectateNoLink, none)),
            ["相手と接続できませんでした。", "ログを送ってください。"]
        );
        for ev in [
            Event::SpectateUnresponsive,
            Event::SpectateRefused,
            Event::SpectateBadCode,
            Event::SpectateOtherVersion,
            Event::SpectateGaveUp,
            Event::SpectateNoLink,
        ] {
            let v = event_text(ev, none);
            assert!(ev.is_trouble() && ev.is_room_trouble(), "{ev:?}");
            assert_eq!(v.len(), 2, "{ev:?}");
            assert!(v.iter().all(|(l, warn)| *warn && !l.contains("secretroom")));
            if !matches!(ev, Event::SpectateOtherVersion | Event::SpectateNoLink) {
                assert!(
                    v[1].0.contains("観戦の「ルームマッチ」"),
                    "{ev:?}: {}",
                    v[1].0
                );
                assert!(!v[1].0.contains("「対戦」"), "{ev:?}: {}", v[1].0);
            }
        }
        assert_eq!(
            event_text(Event::SpectateEnded, none),
            [("観戦を終えました。".to_string(), false)]
        );
        assert_eq!(
            event_text(Event::SpectateCancelled, none),
            [("観戦をやめました。".to_string(), false)]
        );
        assert_eq!(
            event_text(Event::SpectatePeer, none),
            [("相手「peer」に接続しています。".to_string(), false)]
        );
        assert_eq!(
            event_text(Event::RoomJoining, none),
            [("ルームのサーバーに接続しています。".to_string(), false)]
        );
    }

    #[test]
    fn the_spectate_corner_lines_follow_the_state() {
        let waiting = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
        let (n, l, st) = corner(&waiting);
        assert_eq!(
            (n, l[0].as_str(), l[1].as_str()),
            (1, "観戦 abc で待機中 9:12", "")
        );
        assert_eq!(st[0], RowStyle::Normal);
        assert!(corner_is_band(Conn::Waiting, &waiting, None));
        let joining = spectate_note(CodeState::SpectateJoining, Event::RoomJoining);
        let (n, l, _) = corner(&joining);
        assert_eq!(
            (n, l[0].as_str(), l[1].as_str()),
            (
                2,
                "ルームのサーバーに接続しています。",
                "接続するまでお待ちください。"
            )
        );
        assert!(!corner_is_band(Conn::Waiting, &joining, None));
        let linking = spectate_note(CodeState::SpectateLinking, Event::SpectatePeer);
        let (n, l, st) = corner(&linking);
        assert_eq!(
            (n, l[0].as_str(), l[1].as_str()),
            (1, "相手「peer」に接続しています。", "")
        );
        assert_eq!(st[0], RowStyle::Normal);
        assert!(!corner_is_band(Conn::Waiting, &linking, None));
        for st in ALL_STATES {
            let band = matches!(
                st,
                CodeState::RoomWaiting
                    | CodeState::SpectateWaiting
                    | CodeState::Offered
                    | CodeState::Replied
                    | CodeState::Linking
            );
            assert_eq!(
                corner_is_band(Conn::Waiting, &spectate_note(st, Event::None), None),
                band,
                "{st:?}"
            );
        }
        for (ago, want) in [
            (0u32, "0:00"),
            (601, "10:01"),
            (1_799, "29:59"),
            (86_400 * 3, "99:59"),
        ] {
            let mut n = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
            n.since = 1_000_000u32.wrapping_sub(ago);
            assert_eq!(
                corner(&n).1[0],
                format!("観戦 abc で待機中 {want}"),
                "{ago} 秒前"
            );
        }
        for st in [CodeState::Idle, CodeState::SpectateIdle] {
            let (n, l, s) = corner(&spectate_note(st, Event::SpectateGaveUp));
            assert_eq!(
                (n, l[0].as_str(), l[1].as_str(), s),
                (
                    2,
                    "30 分待っても、相手がルームに来ませんでした。",
                    "ルーム名を確かめて、観戦の「ルームマッチ」を選び直してください。",
                    [RowStyle::Warn, RowStyle::Warn]
                ),
                "{st:?}"
            );
            let (n, l, s) = corner(&spectate_note(st, Event::SpectateEnded));
            assert_eq!(
                (n, l[0].as_str(), s[0]),
                (1, "観戦を終えました。", RowStyle::Normal),
                "{st:?}"
            );
            assert_eq!(
                corner(&spectate_note(st, Event::SpectateCancelled)).0,
                0,
                "{st:?}"
            );
            assert_eq!(corner(&spectate_note(st, Event::None)).0, 0, "{st:?}");
        }
        let stall = Some(Stall {
            why: StallWhy::NothingReceived,
            fail: FailKind::Unknown,
        });
        let mut out = [LineBuf::new(); 2];
        let mut sty = [RowStyle::Normal; 2];
        let mut blk = [LineBuf::new(); STALL_ROWS];
        let mut bsty = [RowStyle::Normal; STALL_ROWS];
        for st in [
            CodeState::SpectateJoining,
            CodeState::SpectateWaiting,
            CodeState::SpectateLinking,
        ] {
            let note = spectate_note(st, Event::None);
            assert_eq!(
                corner_is_band(Conn::Waiting, &note, None),
                st == CodeState::SpectateWaiting
            );
            corner_lines(
                Conn::Waiting,
                &note,
                1_000_000,
                stall,
                Lang::Ja,
                &mut out,
                &mut sty,
            );
            assert!(
                !out[0].as_str().contains("接続失敗"),
                "{st:?}: {}",
                out[0].as_str()
            );
            let k = stall_block(
                Conn::Waiting,
                &note,
                1_000_000,
                stall,
                Lang::Ja,
                &mut blk,
                &mut bsty,
            );
            assert_eq!(k, 0, "{st:?}: 観戦者に対戦者の警告は出ない");
        }
        let note = spectate_note(CodeState::RoomLinking, Event::RoomPeer);
        let n = corner_lines(
            Conn::Waiting,
            &note,
            1_000_000,
            stall,
            Lang::Ja,
            &mut out,
            &mut sty,
        );
        assert_eq!(n, 0, "警告の場面の左上は stall_block が決める");
        let k = stall_block(
            Conn::Waiting,
            &note,
            1_000_000,
            stall,
            Lang::Ja,
            &mut blk,
            &mut bsty,
        );
        assert_eq!(k, 4);
        assert_eq!(blk[0].as_str(), "接続失敗：");
    }

    #[test]
    fn the_spectate_waiting_line_fits_the_match_line_width() {
        for lang in Lang::ALL {
            the_spectate_waiting_line_fits_the_match_line_width_in(lang);
        }
    }

    fn the_spectate_waiting_line_fits_the_match_line_width_in(lang: Lang) {
        use crate::ui_layout::{HUD_TEXT_MAX_W, text_width_plain};
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let known = |c: char| spec.slot_of(c).is_some();
        let mut widest = (0u32, String::new());
        for room in [
            "W".repeat(ROOM_UNITS),
            "部".repeat(ROOM_UNITS),
            "a".to_string(),
        ] {
            for ago in [0u32, 59, 1_799, 86_399, 7 * 86_400 + 1, i32::MAX as u32] {
                let mut n = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
                n.room = sanitize_room(&room, known);
                n.since = 1_000_000u32.wrapping_sub(ago);
                let l = corner_in(lang, &n).1[0].clone();
                assert!(l.chars().all(known), "{l}");
                let w = text_width_plain(&spec, &l);
                assert!(w <= HUD_TEXT_MAX_W, "「{l}」 {w} 画素 > {HUD_TEXT_MAX_W}");
                if w > widest.0 {
                    widest = (w, l);
                }
            }
        }
        std::eprintln!(
            "いちばん広い観戦の待機中の行({lang:?}): 「{}」 {} 画素 / 上限 {HUD_TEXT_MAX_W}",
            widest.1,
            widest.0
        );
        // 全角の名前は 2 枠単位で切れるので、半角の枠の余りが 1 枠まで出る。
        assert!(widest.0 + 2 * 7 > HUD_TEXT_MAX_W, "{widest:?}");
        let open = T_SPECTATE_OPEN.get(lang);
        let close = T_ROOM_WAITING_CLOSE.get(lang);
        let starts = |n: &Note, name: &str| {
            let l = corner_in(lang, n).1[0].clone();
            assert!(l.starts_with(&format!("{open}{name}{close}")), "{l}");
        };
        let mut n = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
        n.room = sanitize_room("abcdefghijklmnop", known);
        starts(&n, "abcdefghijklmnop");
        n.room = sanitize_room("abcdefghijklmnopq", known);
        starts(&n, "abcdefghijklmno…");
        n.room = sanitize_room(&"部".repeat(ROOM_UNITS), known);
        let kept = match lang {
            Lang::Ja => 9,
            Lang::En => 8,
        };
        starts(&n, &format!("{}…", "部".repeat(kept)));
        let mut m = Menu::new(ini()).with_lang(lang);
        m.open(0);
        let mut rows = MenuRows::new();
        m.render(Conn::Waiting, &n, 1_000_000, &mut rows);
        let all: Vec<&str> = rows.lines[..rows.len].iter().map(LineBuf::as_str).collect();
        let full = format!("{open}{}{close} 9:12", "部".repeat(ROOM_UNITS));
        assert!(all.contains(&full.as_str()), "{all:?}");
    }

    #[test]
    fn the_joiner_status_maps_to_the_menu_state_and_failure_lines() {
        use crate::code::CodeError;
        use crate::room::{Failure, Role, Why};
        use crate::spectate_room::{JOINER_RETRY_MS, Joiner, JoinerAction};
        let t0 = 1_000_000u64;
        let st = |j: &Joiner, now| spectate_state_of(&j.status(now));
        let mut j = Joiner::new(t0);
        assert_eq!(st(&j, t0), CodeState::SpectateJoining, "始まり");
        assert_eq!(j.poll(t0), Some(JoinerAction::Connect));
        assert_eq!(st(&j, t0), CodeState::SpectateJoining, "つなぐ途中");
        assert_eq!(j.on_joined(t0, Role::Offer), Some(JoinerAction::Close));
        assert_eq!(st(&j, t0), CodeState::SpectateWaiting);
        let t1 = t0 + JOINER_RETRY_MS;
        assert_eq!(j.poll(t1), Some(JoinerAction::Connect));
        assert_eq!(st(&j, t1), CodeState::SpectateWaiting);
        assert_eq!(j.on_joined(t1, Role::Answer), None);
        assert_eq!(st(&j, t1), CodeState::SpectateJoining);
        assert_eq!(
            j.on_peer_code("x".into()),
            Some(JoinerAction::SpectateCode("x".into()))
        );
        j.on_answer_sent();
        assert_eq!(st(&j, t1), CodeState::SpectateLinking);
        j.on_done();
        assert_eq!(st(&j, t1), CodeState::SpectateLinking);
        let mut k = Joiner::new(t0);
        k.poll(t0);
        k.on_failed(t0, Failure::Unresponsive(Why::NoAnswer));
        assert_eq!(st(&k, t0), CodeState::SpectateIdle);
        let mut s = Joiner::new(t0);
        s.poll(t0);
        s.stop();
        assert_eq!(st(&s, t0), CodeState::SpectateIdle);
        for (f, want) in [
            (
                JoinerFailure::Unresponsive(Why::NoAnswer),
                (Event::SpectateUnresponsive, 0, 0),
            ),
            (
                JoinerFailure::Unresponsive(Why::NoRoom),
                (Event::SpectateUnresponsive, 0, 0),
            ),
            (JoinerFailure::BadRoomName, (Event::SpectateRefused, 0, 0)),
            (
                JoinerFailure::CodeUnreadable(CodeError::BadVersion(8)),
                (Event::SpectateOtherVersion, 8, 0),
            ),
            (
                JoinerFailure::CodeUnreadable(CodeError::OtherBuild { theirs: 403_920 }),
                (Event::SpectateOtherBuild, 0, 403_920),
            ),
            (
                JoinerFailure::CodeUnreadable(CodeError::BadTag),
                (Event::SpectateBadCode, 0, 0),
            ),
            (
                JoinerFailure::CodeUnreadable(CodeError::Expired { expires: 1, now: 2 }),
                (Event::SpectateBadCode, 0, 0),
            ),
            (JoinerFailure::ReplyNotSent, (Event::SpectateNoLink, 0, 0)),
            (JoinerFailure::GaveUp, (Event::SpectateGaveUp, 0, 0)),
        ] {
            assert_eq!(spectate_failure_event(f), want, "{f:?}");
        }
        let mut g = Joiner::new(t0);
        g.poll(t0);
        let give_up = g.status(t0).give_up_at;
        assert_eq!(give_up - t0, 30 * 60 * 1000);
        assert_eq!(g.poll(give_up), Some(JoinerAction::Close));
        let JoinerPhase::Failed(f) = g.status(give_up).phase else {
            unreachable!("30 分で諦めていない: {:?}", g.status(give_up));
        };
        assert_eq!(st(&g, give_up), CodeState::SpectateIdle);
        let (ev, ver, _) = spectate_failure_event(f);
        let mut note = spectate_note(CodeState::SpectateIdle, ev);
        note.peer_ver = ver;
        let (n, l, s) = corner(&note);
        assert_eq!(
            (n, l[0].as_str(), s),
            (
                2,
                "30 分待っても、相手がルームに来ませんでした。",
                [RowStyle::Warn; 2]
            )
        );
    }

    #[test]
    fn spectate_states_round_trip_through_the_note_box() {
        for (i, st) in ALL_STATES.into_iter().enumerate() {
            assert_eq!(st.code() as usize, i, "番号が並びと違う: {st:?}");
            assert_eq!(CodeState::from_code(st.code()), st);
        }
        assert_eq!(CodeState::from_code(11), CodeState::Idle, "知らない番号");
        for st in ALL_STATES {
            let spectate = (7..=10).contains(&st.code());
            assert_eq!(st.is_spectate(), spectate, "{st:?}");
            assert_eq!(
                st.is_spectate_busy(),
                (7..=9).contains(&st.code()),
                "{st:?}"
            );
            if spectate {
                assert!(st.is_room(), "{st:?}");
            }
        }
        assert!(!CodeState::Offered.is_room() && !CodeState::Idle.is_room());
        for st in ALL_STATES.into_iter().filter(|s| s.is_spectate()) {
            let mut m = Menu::new(ini());
            m.open(0);
            let mut n = made_note();
            n.state = st;
            n.event = Event::None;
            let t = texts_of(&m, &n);
            assert!(
                !t.iter().any(|l| l.contains("有効期限") || l.contains("…")),
                "{st:?}: {t:?}"
            );
        }
        for (ev, ver) in [(Event::SpectateOtherVersion, 7u8), (Event::SpectatePeer, 0)] {
            let n = Note {
                seq: 4,
                event: ev,
                state: CodeState::SpectateLinking,
                peer_ver: ver,
                since: 0xCAFE_F00D,
                room: sanitize_room("部屋XYZ", |_| true),
                name: sanitize_name(b"peerpeer", |_| true),
                ..made_note()
            };
            assert_eq!(Note::from_words(&n.to_words()), n);
        }
    }

    fn text_of(s: &str) -> TextIn {
        let mut t = TextIn::NONE;
        for (d, u) in t.units.iter_mut().zip(s.encode_utf16()) {
            *d = u;
            t.len += 1;
        }
        t
    }

    const ENTER: &str = "\r";

    impl Driver {
        fn tick_text(&mut self, word: u16, typed: &str) -> TickOut {
            let out = self.m.tick(&TickIn {
                text: text_of(typed),
                ..input(word, self.note, self.conn)
            });
            self.cmds.extend(out.cmd);
            out
        }

        fn idle(&mut self, n: u32) {
            for _ in 0..n {
                self.tick_text(0, "");
            }
        }

        /// ルーム名が空のメニューで対戦の「ルームマッチ」を決めて入力を開く（ルーム名が決まっていると指示を出すので、空の名から始める。初めの値つきで開けるのは設定の「ルーム名」だけ）。
        fn open_room_input() -> Driver {
            let mut d = Driver::new();
            d.m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
            d.m.open(0);
            d.cursor_to(Item::Room);
            d.tap(Z);
            assert!(d.m.wants_text(), "入力が開かない");
            assert_eq!(d.m.edit_for, EditFor::Wait);
            d
        }

        fn cursor_to(&mut self, item: Item) {
            for _ in 0..ITEMS_MAX {
                let (v, n) = self.m.items(self.conn, self.note.state);
                if v[..n].get(self.m.settled_cursor(&(v, n))) == Some(&item) {
                    return;
                }
                self.tap(DOWN);
            }
            panic!("{item:?} へ動けない");
        }

        fn open_room_set_input(initial: &str) -> Driver {
            let mut d = Driver::new();
            d.m = Menu::new(ini()).with_room(RoomItem::On(
                RoomBuf::from_name(initial).unwrap_or_default(),
            ));
            d.m.open(0);
            d.cursor_to(Item::RoomName);
            d.tap(Z);
            assert!(d.m.wants_text(), "ルーム名の入力が開かない");
            assert_eq!(d.m.edit_for, EditFor::RoomSet);
            d
        }

        fn input_line(&self) -> String {
            self.text()[2].clone()
        }
    }

    #[test]
    fn room_input_starts_from_the_initial_value_and_decides_with_enter() {
        let mut d = Driver::open_room_input();
        let t = d.text();
        assert_eq!(t[0], T_ROOM.ja, "{t:?}");
        assert_eq!(d.input_line(), "ルーム名: |", "{t:?}");
        assert_eq!(t.last().unwrap(), T_ROOM_INPUT_KEYS.ja);
        d.tick_text(0, "2");
        d.tick_text(0, "ＡＢ-.あ");
        assert_eq!(d.input_line(), "ルーム名: 2AB-|");
        d.tick_text(0, "\u{8}\u{8}\u{8}");
        assert_eq!(d.input_line(), "ルーム名: 2|");
        let out = d.tick_text(0, &format!("x{ENTER}"));
        let want = RoomBuf::from_name("2x").unwrap();
        assert_eq!(out.cmd, Some(Cmd::Room(want)));
        assert!(out.hold_game, "決めた tick にゲームへ入力を返した");
        assert!(!d.m.wants_text(), "決めても入力が閉じない");
        assert!(d.m.is_active());
        d.idle(3);
        d.cursor_to(Item::RoomName);
        d.tap(Z);
        assert!(d.m.wants_text());
        assert_eq!(d.text()[0], T_ROOM_NAME.ja);
        assert_eq!(d.input_line(), "ルーム名: 2x|");
        let mut d = Driver::open_room_set_input("last-room");
        let t = d.text();
        assert_eq!(t[0], T_ROOM_NAME.ja, "{t:?}");
        assert_eq!(d.input_line(), "ルーム名: last-room|", "{t:?}");
        assert_eq!(t.last().unwrap(), T_ROOM_INPUT_KEYS.ja);
        d.tick_text(0, "ＡＢ-.あ");
        assert_eq!(d.input_line(), "ルーム名: last-roomAB-|");
    }

    #[test]
    fn room_input_ignores_z_and_x_as_keys_and_esc_leaves_it() {
        let mut d = Driver::open_room_input();
        for w in [Z, X, UP, DOWN, LEFT, RIGHT, Z | X] {
            d.tick_text(w, "");
            d.tick_text(0, "");
        }
        d.tick_text(Z, "zZ");
        d.tick_text(X, "xX");
        assert!(d.m.wants_text(), "Z・X・方向で入力が閉じた");
        assert_eq!(d.input_line(), "ルーム名: zZxX|");
        assert_eq!(d.cmds, vec![], "指示が出た");
        let out = d.tick_text(ESC, "");
        assert!(out.hold_game && out.cmd.is_none());
        assert!(!d.m.wants_text() && d.m.is_active());
        d.tick_text(ESC, "");
        d.tick_text(ESC, "");
        assert!(d.m.is_active(), "Esc の押しっぱなしでメニューごと閉じた");
        d.idle(2);
        d.tap(Z);
        assert!(d.m.wants_text());
        assert_eq!(d.input_line(), "ルーム名: |", "やめた入力の文字が残った");
        d.tap(ESC);
        assert!(!d.m.wants_text() && d.m.is_active());
        d.idle(2);
        d.tap(ESC);
        assert!(!d.m.is_active(), "一覧の Esc がメニューを閉じない");
    }

    #[test]
    fn room_input_refuses_empty_enter_and_skips_the_opening_enter() {
        const ENTER_WORD: u16 = 0x1000;
        let has_empty_line =
            |d: &Driver| d.text().iter().any(|l| l.contains(T_ROOM_INPUT_EMPTY.ja));
        let mut d = Driver::new();
        d.m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
        d.m.open(0);
        d.cursor_to(Item::Room);
        d.tick_text(ENTER_WORD, "");
        assert!(d.m.wants_text(), "入力が開かない");
        for _ in 0..30 {
            let out = d.tick_text(ENTER_WORD, "\r\r\r");
            assert_eq!(out.cmd, None, "押している間に決まった");
        }
        assert!(d.m.wants_text());
        assert!(!has_empty_line(&d), "押している間の Enter に断りを出した");
        assert_eq!(d.input_line(), "ルーム名: |");
        let out = d.tick_text(0, ENTER);
        assert_eq!(out.cmd, None, "離した tick の文字で決まった");
        assert!(!has_empty_line(&d), "離した tick の Enter に断りを出した");
        d.tick_text(0, "kept");
        let out = d.tick_text(0, ENTER);
        assert_eq!(
            out.cmd,
            Some(Cmd::Room(RoomBuf::from_name("kept").unwrap()))
        );
        d.idle(3);
        d.cursor_to(Item::RoomName);
        d.tap(Z);
        assert_eq!(d.input_line(), "ルーム名: kept|");
        d.tick_text(0, "\u{8}\u{8}\u{8}\u{8}\u{8}");
        assert_eq!(d.input_line(), "ルーム名: |");
        let out = d.tick_text(0, ENTER);
        assert_eq!(out.cmd, None, "空の名で決まった");
        assert!(d.m.wants_text(), "断った後は入力に留まる");
        let t = d.text();
        assert!(t.iter().any(|l| l == T_ROOM_INPUT_EMPTY.ja), "{t:?}");
        d.tick_text(0, "a");
        let t = d.text();
        assert!(!t.iter().any(|l| l == T_ROOM_INPUT_EMPTY.ja), "{t:?}");
        assert_eq!(d.input_line(), "ルーム名: a|");
        d.tick_text(ESC, "");
        d.idle(3);
        d.tick_text(ENTER_WORD, "");
        assert!(d.m.wants_text(), "開き直せない");
        let out = d.tick_text(ENTER_WORD, ENTER);
        assert_eq!(out.cmd, None, "前の入力の「離した」を持ち越した");
        assert!(d.m.wants_text());
    }

    #[test]
    fn room_input_says_why_a_character_was_dropped_and_clears_it_on_the_next_input() {
        let has = |d: &Driver, s: &str| d.text().iter().any(|l| l == s);
        let mut d = Driver::open_room_input();
        assert!(!has(&d, T_ROOM_INPUT_DROPPED.ja), "打つ前から出ている");
        for dropped in ["ｱ", "あ", "!", "."] {
            d.tick_text(0, dropped);
            assert!(has(&d, T_ROOM_INPUT_DROPPED.ja), "{dropped:?}");
            assert_eq!(d.input_line(), "ルーム名: |", "捨てた文字が名前に入った");
            d.tick_text(0, "a");
            assert!(!has(&d, T_ROOM_INPUT_DROPPED.ja), "使える文字で消えない");
            d.tick_text(0, "\u{8}");
        }
        d.tick_text(0, "ｱ");
        d.tick_text(0, "\u{8}");
        assert!(!has(&d, T_ROOM_INPUT_DROPPED.ja), "Backspace で消えない");
        d.tick_text(0, "ｱ");
        d.tick_text(0, ENTER);
        assert!(!has(&d, T_ROOM_INPUT_DROPPED.ja), "Enter で消えない");
        assert!(has(&d, T_ROOM_INPUT_EMPTY.ja));
        d.tick_text(0, "\u{1B}\t");
        assert!(!has(&d, T_ROOM_INPUT_DROPPED.ja));
        d.tick_text(0, "ｱa");
        assert!(!has(&d, T_ROOM_INPUT_DROPPED.ja));
        d.tick_text(0, "bｱ");
        assert!(has(&d, T_ROOM_INPUT_DROPPED.ja));
        assert_eq!(d.input_line(), "ルーム名: ab|");
        d.tick_text(ESC, "");
        d.idle(3);
        d.tap(Z);
        assert!(!has(&d, T_ROOM_INPUT_DROPPED.ja), "開き直しても残った");
        d.m.set_text_available(false);
        assert!(has(&d, T_ROOM_INPUT_NO_KEYS.ja));
        assert!(has(&d, T_ROOM_INPUT_NO_KEYS_NEXT.ja));
        assert!(
            T_ROOM_INPUT_NO_KEYS_NEXT.ja.contains("Esc")
                && T_ROOM_INPUT_NO_KEYS_NEXT.ja.contains("接続コード")
        );
    }

    #[test]
    fn a_late_enter_word_does_not_reopen_the_input() {
        let mut d = Driver::open_room_input();
        d.tick_text(0, "abc");
        let out = d.tick_text(0, ENTER);
        assert!(out.cmd.is_some());
        let want = RoomBuf::from_name("abc").unwrap();
        assert_eq!(d.cmds, vec![Cmd::Room(want)]);
        d.tick_text(0x1000, "");
        d.tick_text(0x1000, "");
        assert!(
            !d.m.wants_text(),
            "遅れて届いた Enter の語で入力が開き直った"
        );
        assert_eq!(
            d.cmds,
            vec![Cmd::Room(want)],
            "遅れて届いた Enter の語で「ルームマッチ」を決め直した"
        );
        d.tick_text(0, "");
        d.tick_text(Z, "");
        assert_eq!(
            d.cmds,
            vec![Cmd::Room(want), Cmd::Room(want)],
            "語が離れた後の決定が効かない"
        );
    }

    #[test]
    fn room_input_closes_with_the_menu_and_holds_the_game() {
        let mut d = Driver::open_room_input();
        assert!(d.tick_text(0, "x").hold_game);
        let mut m = d.m;
        m.reset();
        assert!(!m.wants_text() && !m.is_active());
        let mut d = Driver::open_room_input();
        d.conn = Conn::Found {
            seat: Seat::P1,
            shape: None,
            ready: false,
            left_secs: 100,
        };
        let out = d.tick_text(0, "");
        assert!(out.hold_game);
        assert!(!d.m.wants_text(), "つながっても入力が残った");
        let mut d = Driver::new();
        d.m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
        d.m.open(0);
        let out = d.tick_text(0, "abc\r");
        assert_eq!((out.cmd, d.m.wants_text()), (None, false));
    }

    #[test]
    fn room_input_rows_use_only_known_glyphs_and_fit() {
        for lang in Lang::ALL {
            room_input_rows_use_only_known_glyphs_and_fit_in(lang);
        }
    }

    fn room_input_rows_use_only_known_glyphs_and_fit_in(lang: Lang) {
        let spec = game_font_spec("");
        let limit = 640
            - 2 * crate::ui_layout::SCREEN_MARGIN
            - 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        let mut widest = 0;
        let mut tallest = 0;
        for why in RoomNameWhy::ALL {
            for unavailable in [false, true] {
                let mut d = Driver::open_room_set_input(&"W".repeat(32));
                d.m.lang = lang;
                d.m.set_text_available(!unavailable);
                d.m.edit_err = Some(why);
                d.m.edit_dropped = true;
                let t = d.text();
                assert_eq!(t.last().unwrap(), T_ROOM_INPUT_KEYS.get(lang));
                let mut r = MenuRows::new();
                d.m.render(d.conn, &d.note, 1_000_000, &mut r);
                tallest = tallest.max(r.len);
                assert!(r.len <= MENU_ROWS);
                for l in &r.lines[..r.len] {
                    for c in l.as_str().chars() {
                        assert!(spec.slot_of(c).is_some(), "文字 {c:?} が文字の表に無い");
                    }
                    widest = widest.max(crate::ui_layout::text_width(&spec, l.as_str()));
                }
            }
        }
        assert!(
            widest <= limit,
            "{lang:?}: いちばん広い行 {widest} 画素 > {limit}"
        );
        // 断りの行が 4 つ出る組の高さを測れている（出ていなければ、上の幅・高さの検査が空振り）。
        assert_eq!(tallest, 9, "見出し・空き・入力・断り 4・空き・キー");
        let mut d = Driver::new();
        d.m.lang = lang;
        for l in &d.text() {
            for key in ["Enter", "Esc", "Backspace"] {
                assert!(!l.contains(key), "一覧の行にキー名 {key}: {l}");
            }
        }
        assert!(!T_KEYS.get(lang).contains("Enter") && !T_KEYS_CONFIRM.get(lang).contains("Enter"));
    }

    fn room_note(state: CodeState, event: Event) -> Note {
        Note {
            seq: 1,
            event,
            state,
            room: sanitize_room("abc", |_| true),
            since: 1_000_000 - 552,
            ..Note::default()
        }
    }

    fn corner(note: &Note) -> (usize, [String; 2], [RowStyle; 2]) {
        corner_in(Lang::Ja, note)
    }

    fn corner_in(lang: Lang, note: &Note) -> (usize, [String; 2], [RowStyle; 2]) {
        let mut out = [LineBuf::new(); 2];
        let mut st = [RowStyle::Normal; 2];
        let n = corner_lines(
            Conn::Waiting,
            note,
            1_000_000,
            None,
            lang,
            &mut out,
            &mut st,
        );
        (
            n,
            [out[0].as_str().to_string(), out[1].as_str().to_string()],
            st,
        )
    }

    #[test]
    fn room_corner_lines_show_the_room_the_time_waited_and_the_failure() {
        let waiting = room_note(CodeState::RoomWaiting, Event::RoomWaiting);
        let (n, l, st) = corner(&waiting);
        assert_eq!(n, 1);
        assert_eq!(l[0], "ルーム abc で待機中 9:12");
        assert!(!l[0].contains("残り"), "待った時間のはずが残りを出した");
        assert_eq!(l[1], "", "2 行目が出た");
        assert_eq!(st[0], RowStyle::Normal);
        assert!(corner_is_band(Conn::Waiting, &waiting, None));
        for (state, event) in [
            (CodeState::RoomJoining, Event::RoomJoining),
            (CodeState::RoomLinking, Event::RoomPeer),
            (CodeState::Idle, Event::RoomExpired),
            (CodeState::Idle, Event::None),
        ] {
            assert!(
                !corner_is_band(Conn::Waiting, &room_note(state, event), None),
                "{state:?}"
            );
        }
        for st in [CodeState::Offered, CodeState::Replied, CodeState::Linking] {
            assert!(corner_is_band(
                Conn::Waiting,
                &room_note(st, Event::Made),
                None
            ));
        }
        assert!(!corner_is_band(Conn::Ended, &waiting, None));
        for (ago, want) in [(0u32, "0:00"), (601, "10:01"), (3 * 86_400 + 61, "4321:01")] {
            let mut n = room_note(CodeState::RoomWaiting, Event::RoomWaiting);
            n.since = 1_000_000u32.wrapping_sub(ago);
            assert_eq!(
                corner(&n).1[0],
                format!("ルーム abc で待機中 {want}"),
                "{ago} 秒前から"
            );
        }
        let mut late = room_note(CodeState::RoomWaiting, Event::RoomWaiting);
        late.since = 1_000_000 + 5;
        assert_eq!(corner(&late).1[0], "ルーム abc で待機中 0:00");
        let (n, l, _) = corner(&room_note(CodeState::RoomJoining, Event::RoomJoining));
        assert_eq!(
            (n, l[0].as_str(), l[1].as_str()),
            (2, T_ROOM_JOINING.ja, T_NEXT_LINKING.ja)
        );
        let (n, l, _) = corner(&room_note(CodeState::RoomLinking, Event::RoomPeer));
        assert_eq!(
            (n, l[0].as_str(), l[1].as_str()),
            (2, T_ROOM_LINKING.ja, T_NEXT_LINKING.ja)
        );
        for st in [
            CodeState::RoomJoining,
            CodeState::RoomWaiting,
            CodeState::RoomLinking,
        ] {
            let (_, l, _) = corner(&room_note(st, Event::None));
            assert!(!l[0].contains("接続コードで"), "{st:?}: {}", l[0]);
        }
        let (n, l, st) = corner(&room_note(CodeState::Idle, Event::RoomExpired));
        assert_eq!(
            (n, l[0].as_str(), l[1].as_str()),
            (
                2,
                "ルームの期限が切れました。",
                "「ルームマッチ」を選び直してください。"
            )
        );
        assert_eq!(st, [RowStyle::Warn, RowStyle::Warn]);
        assert_eq!(corner(&room_note(CodeState::Idle, Event::Broken)).0, 0);
    }

    #[test]
    fn every_waiting_band_line_fits_the_match_line_width() {
        for lang in Lang::ALL {
            every_waiting_band_line_fits_the_match_line_width_in(lang);
        }
    }

    fn every_waiting_band_line_fits_the_match_line_width_in(lang: Lang) {
        use crate::ui_layout::{HUD_TEXT_MAX_W, text_width_plain};
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let known = |c: char| spec.slot_of(c).is_some();
        let widest_room = "W".repeat(ROOM_UNITS);
        let wide_room = "部".repeat(ROOM_UNITS);
        let mut widest = (0u32, String::new());
        for room in [widest_room.as_str(), wide_room.as_str(), "a"] {
            for ago in [0u32, 59, 3_599, 86_399, 7 * 86_400 + 1, i32::MAX as u32] {
                let mut n = room_note(CodeState::RoomWaiting, Event::RoomWaiting);
                n.room = sanitize_room(room, known);
                n.since = 1_000_000u32.wrapping_sub(ago);
                let l = corner_in(lang, &n).1[0].clone();
                assert!(l.chars().all(known), "{l}");
                let w = text_width_plain(&spec, &l);
                assert!(w <= HUD_TEXT_MAX_W, "「{l}」 {w} 画素 > {HUD_TEXT_MAX_W}");
                if w > widest.0 {
                    widest = (w, l);
                }
            }
        }
        for ago in [0u32, 1_799, 86_399, 86_400] {
            let mut n = made_note();
            n.since = 1_000_000u32.wrapping_sub(ago);
            for st in [CodeState::Offered, CodeState::Replied, CodeState::Linking] {
                n.state = st;
                let l = corner_in(lang, &n).1[0].clone();
                let w = text_width_plain(&spec, &l);
                assert!(w <= HUD_TEXT_MAX_W, "「{l}」 {w} 画素");
                if w > widest.0 {
                    widest = (w, l);
                }
            }
        }
        std::eprintln!(
            "いちばん広い待機中の行({lang:?}): 「{}」 {} 画素 / 上限 {HUD_TEXT_MAX_W}",
            widest.1,
            widest.0
        );
        assert!(widest.0 + 3 * 7 > HUD_TEXT_MAX_W, "{widest:?}");
        let open = T_ROOM_OPEN.get(lang);
        let close = T_ROOM_WAITING_CLOSE.get(lang);
        let mut n = room_note(CodeState::RoomWaiting, Event::RoomWaiting);
        n.room = sanitize_room(&widest_room, known);
        assert!(corner_in(lang, &n).1[0].starts_with(&format!("{open}{widest_room}{close}")));
        n.room = sanitize_room(&wide_room, known);
        let l = corner_in(lang, &n).1[0].clone();
        let keep = ((room_band_cells(lang) - 1) / 2) as usize;
        assert!(
            l.starts_with(&format!("{open}{}…", "部".repeat(keep))),
            "{l}"
        );
        let mut m = Menu::new(ini()).with_lang(lang);
        m.open(0);
        let mut rows = MenuRows::new();
        m.render(Conn::Waiting, &n, 1_000_000, &mut rows);
        let all: Vec<&str> = rows.lines[..rows.len].iter().map(LineBuf::as_str).collect();
        let full = format!("{open}{}{close} 9:12", "部".repeat(ROOM_UNITS));
        assert!(all.contains(&full.as_str()), "{all:?}");
    }

    #[test]
    fn each_room_failure_has_its_own_line_and_one_next_step() {
        let lines = |ev: Event, f: &dyn Fn(&mut Note)| {
            let mut n = room_note(CodeState::Idle, ev);
            f(&mut n);
            let mut out = [LineBuf::new(); STATUS_ROWS];
            let mut w = [false; STATUS_ROWS];
            let k = event_lines(&n, Lang::Ja, &mut out, &mut w);
            assert!(w[..k].iter().all(|x| *x), "{ev:?} の色");
            out[..k]
                .iter()
                .map(|l| l.as_str().to_string())
                .collect::<Vec<_>>()
        };
        let none = |_: &mut Note| {};
        assert_eq!(
            lines(Event::RoomUnresponsive, &none),
            [
                "サーバーが応答しません。",
                "「コード作成」か「コード入力」で接続してください。"
            ]
        );
        assert_eq!(
            lines(Event::RoomBusy, &|n| n.retry_secs = 60),
            [
                "サーバーが混み合っています。",
                "60 秒ほど待ってから、「ルームマッチ」を選び直してください。"
            ]
        );
        assert_eq!(
            lines(Event::RoomBusy, &|n| n.retry_secs = 7)[1],
            "7 秒ほど待ってから、「ルームマッチ」を選び直してください。"
        );
        assert_eq!(
            lines(Event::RoomFull, &none),
            [
                "ルームが満員です。",
                "別のルーム名で「ルームマッチ」を選び直してください。"
            ]
        );
        assert_eq!(
            lines(Event::RoomRefused, &none),
            [
                "サーバーがこのルーム名を受け付けませんでした。",
                "別のルーム名で「ルームマッチ」を選び直してください。"
            ]
        );
        assert_eq!(
            lines(Event::RoomPeerLeft, &none),
            [
                "ルームの相手が途中で抜けました。",
                "「ルームマッチ」を選び直してください。"
            ]
        );
        assert_eq!(
            lines(Event::RoomBadCode, &none)[0],
            "ルームの相手から届いたコードを読めませんでした。"
        );
        for (why, want) in [
            (RoomNameWhy::Empty, "ルーム名が空です。"),
            (RoomNameWhy::Long, "ルーム名が長すぎます（32 文字まで）。"),
            (
                RoomNameWhy::BadChar,
                "ルーム名に使えない文字（改行や / など）があります。",
            ),
            (RoomNameWhy::Dot, "ルーム名を「.」や「..」にはできません。"),
            (
                RoomNameWhy::NotAscii,
                "ルーム名に使用可能な文字は「英数字」「-」「_」「@」です。",
            ),
        ] {
            let got = lines(Event::RoomNameBad, &|n| n.room_why = why);
            assert_eq!(
                got,
                [
                    want,
                    "ルーム名を直して、「ルームマッチ」を選び直してください。"
                ],
                "{why:?}"
            );
            assert!(!got.concat().contains("abc"), "ルーム名を出した");
            let mut l = LineBuf::new();
            room_name_why_line(&mut l, Lang::Ja, why);
            assert_eq!(l.as_str(), want);
        }
        for gone in ["クリップボード", "HiganRTool.ini", "起動し直"] {
            for ev in [Event::RoomFull, Event::RoomRefused, Event::RoomNameBad] {
                assert!(!lines(ev, &none).concat().contains(gone), "{ev:?} {gone}");
            }
        }
        assert_eq!(crate::room::ROOM_NAME_MAX, 32, "上の文の 32 の出どころ");
        use crate::room::RoomNameError as E;
        for (e, want) in [
            (E::Empty, RoomNameWhy::Empty),
            (E::Blank, RoomNameWhy::Empty),
            (E::TooManyBytes { bytes: 129 }, RoomNameWhy::Long),
            (E::TooLong { len: 33 }, RoomNameWhy::Long),
            (E::BadChar { pos: 0 }, RoomNameWhy::BadChar),
            (E::DotName, RoomNameWhy::Dot),
            (E::NotAscii { pos: 0 }, RoomNameWhy::NotAscii),
        ] {
            assert_eq!(RoomNameWhy::of(e), want, "{e:?}");
        }
    }

    #[test]
    fn room_fields_round_trip_and_long_names_are_cut() {
        for why in [RoomNameWhy::Dot, RoomNameWhy::NotAscii] {
            let n = Note {
                seq: 9,
                event: Event::RoomBusy,
                state: CodeState::RoomWaiting,
                since: 0xDEAD_BEEF,
                retry_secs: 0xABCD,
                room_why: why,
                room: sanitize_room("部屋XYZ", |_| true),
                name: sanitize_name(b"peer", |_| true),
                ..made_note()
            };
            assert_eq!(Note::from_words(&n.to_words()), n);
        }
        for st in [
            CodeState::RoomJoining,
            CodeState::RoomWaiting,
            CodeState::RoomLinking,
        ] {
            assert_eq!(CodeState::from_code(st.code()), st);
            assert!(st.is_room());
        }
        assert!(!CodeState::Offered.is_room());
        let show = |raw: &str| {
            let mut l = LineBuf::new();
            push_units(&mut l, &sanitize_room(raw, |c| c.is_ascii() || c == '部'));
            l.as_str().to_string()
        };
        assert_eq!(show("abc"), "abc");
        assert_eq!(show("部屋a"), "部?a");
        assert_eq!(
            show(&"x".repeat(16)),
            "x".repeat(16),
            "ちょうど 16 文字は切らない"
        );
        assert_eq!(show(&"x".repeat(17)), format!("{}…", "x".repeat(15)));
        assert_eq!(show("a\u{7}b"), "a?b");
    }

    fn restarts(count: u32, why: FailReason) -> Restarts {
        Restarts {
            count,
            last: Some((why, crate::handshake::Restart::Fresh)),
        }
    }

    fn stall_of(why: StallWhy, fail: FailKind) -> Stall {
        Stall { why, fail }
    }

    const ALL_FAILS: [FailKind; 4] = [
        FailKind::Unknown,
        FailKind::PeerSymmetricNoUpnp,
        FailKind::OwnSymmetric,
        FailKind::BothSymmetric,
    ];

    /// つながらなかったときの文（日本語）。複数行の区切りと文字は決まった文面のまま（README の項の番号を末尾の行に持つ）。
    fn want_fail_ja(fail: FailKind) -> Vec<&'static str> {
        match fail {
            FailKind::Unknown => vec![
                "接続失敗：",
                "不明な原因で相手からの UDP パケットが届きません",
                "iniをdetail=1に書き換えてもう一度試し、開発者にログを送ってください。",
                "（README「接続できないとき」1）",
            ],
            FailKind::PeerSymmetricNoUpnp => vec![
                "接続失敗：",
                "相手がシンメトリック NATであり、こちらはUPnPが使用不可です。",
                "ルーターのUPnPをオンにするか、ポート開放してください。",
                "（README「接続できないとき」2）",
            ],
            FailKind::OwnSymmetric => vec![
                "接続失敗：",
                "こちらがシンメトリック NATです。",
                "相手側のポート開放が必要です。",
                "相手もシンメトリックNATなら通信できません。",
                "（README「接続できないとき」3）",
            ],
            FailKind::BothSymmetric => vec![
                "接続不可：",
                "こちらも相手もシンメトリック NAT のため、直接接続できません。",
                "（README「接続できないとき」4）",
            ],
        }
    }

    fn want_fail_en(fail: FailKind) -> Vec<&'static str> {
        match fail {
            FailKind::Unknown => vec![
                "Connection failed:",
                "UDP packets from the opponent do not arrive (unknown cause).",
                "Set detail=1 in the ini, retry, and send the log to the developer.",
                "(README \"When you cannot connect\" 1)",
            ],
            FailKind::PeerSymmetricNoUpnp => vec![
                "Connection failed:",
                "The opponent is on a symmetric NAT and UPnP is unavailable here.",
                "Turn on UPnP on your router, or open a port.",
                "(README \"When you cannot connect\" 2)",
            ],
            FailKind::OwnSymmetric => vec![
                "Connection failed:",
                "Your network is a symmetric NAT.",
                "The opponent needs to open a port.",
                "If the opponent is also on a symmetric NAT, you cannot connect.",
                "(README \"When you cannot connect\" 3)",
            ],
            FailKind::BothSymmetric => vec![
                "Cannot connect:",
                "Both sides are symmetric NATs, so a direct connection is impossible.",
                "(README \"When you cannot connect\" 4)",
            ],
        }
    }

    /// `stall_block` を呼んで、出た行を取り出す。
    fn block(
        note: &Note,
        stall: Option<Stall>,
        lang: Lang,
    ) -> (usize, [LineBuf; STALL_ROWS], [RowStyle; STALL_ROWS]) {
        let mut out = [LineBuf::new(); STALL_ROWS];
        let mut sty = [RowStyle::Normal; STALL_ROWS];
        let n = stall_block(
            Conn::Waiting,
            note,
            1_000_000,
            stall,
            lang,
            &mut out,
            &mut sty,
        );
        (n, out, sty)
    }

    #[test]
    fn a_stall_needs_one_restart_and_maps_each_reason() {
        use crate::handshake::Restart;
        assert_eq!(
            Stall::from_restarts(Restarts::default(), FailKind::Unknown),
            None,
            "作り直し 0 回"
        );
        assert_eq!(
            Stall::from_restarts(
                Restarts {
                    count: 0,
                    last: Some((FailReason::NoHandshake, Restart::Fresh)),
                },
                FailKind::Unknown
            ),
            None,
            "count が 0 なら last があっても出さない"
        );
        assert_eq!(
            Stall::from_restarts(
                Restarts {
                    count: 1,
                    last: None
                },
                FailKind::Unknown
            ),
            None,
            "理由が無ければ出さない（count だけでは出さない）"
        );
        for (why, want) in [
            (FailReason::NothingReceived, StallWhy::NothingReceived),
            (FailReason::SameNonce, StallWhy::NothingReceived),
            (FailReason::NoHandshake, StallWhy::NoHandshake),
        ] {
            for count in [1, 2, 30] {
                for fail in ALL_FAILS {
                    assert_eq!(
                        Stall::from_restarts(restarts(count, why), fail),
                        Some(stall_of(want, fail)),
                        "{why:?} {count} 回 {fail:?}"
                    );
                }
            }
        }
        assert_eq!(
            Stall::from_restarts(
                Restarts {
                    count: 1,
                    last: Some((FailReason::NoHandshake, Restart::Resume)),
                },
                FailKind::Unknown
            ),
            Some(stall_of(StallWhy::NoHandshake, FailKind::Unknown)),
            "Resume の作り直しも Fresh と同じに変換する"
        );
        assert_eq!(
            Stall::from_restarts(
                restarts(1, FailReason::OtherVersion { theirs: 1 }),
                FailKind::Unknown
            ),
            None,
            "バージョン違いは今の Conn::OtherVersion の行のまま"
        );
    }

    #[test]
    fn only_both_symmetric_is_predicted_before_any_restart() {
        for fail in ALL_FAILS {
            let want = (fail == FailKind::BothSymmetric)
                .then_some(stall_of(StallWhy::Predicted, FailKind::BothSymmetric));
            assert_eq!(Stall::predicted(fail), want, "{fail:?}");
        }
    }

    #[test]
    fn a_stalled_wait_shows_the_chosen_text_for_each_kind_and_route() {
        let code = made_note();
        let room = room_note(CodeState::RoomLinking, Event::RoomPeer);
        let mut checked = 0;
        for (lang, want_of) in [
            (Lang::Ja, want_fail_ja as fn(FailKind) -> Vec<&'static str>),
            (Lang::En, want_fail_en),
        ] {
            for note in [
                Note {
                    state: CodeState::Replied,
                    ..code
                },
                Note {
                    state: CodeState::Linking,
                    ..code
                },
                room,
            ] {
                for fail in ALL_FAILS {
                    // 相手から 1 通も届かない・予告は、型で選んだ文。相手から届いている（NoHandshake）でも型が分かっていれば同じ文。
                    let mut whys = vec![StallWhy::NothingReceived, StallWhy::NoHandshake];
                    if fail == FailKind::BothSymmetric {
                        whys.push(StallWhy::Predicted);
                    }
                    for why in whys {
                        if why == StallWhy::NoHandshake && fail == FailKind::Unknown {
                            continue; // 別の 3 行（下の別の検査）
                        }
                        let st = stall_of(why, fail);
                        let (n, out, sty) = block(&note, Some(st), lang);
                        let want = want_of(fail);
                        assert_eq!(n, want.len(), "{lang:?} {:?} {st:?}", note.state);
                        for (i, w) in want.iter().enumerate() {
                            assert_eq!(out[i].as_str(), *w, "{lang:?} {:?} {st:?}", note.state);
                        }
                        for l in &out[n..] {
                            assert_eq!(l.as_str(), "", "余った行は空");
                        }
                        assert!(sty[..n].iter().all(|s| *s == RowStyle::Warn));
                        assert!(!corner_is_band(Conn::Waiting, &note, Some(st)));
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 2 * 3 * (1 + 2 + 2 + 3));
    }

    #[test]
    fn the_japanese_texts_are_the_decided_wording() {
        // 決まった 4 通りの文（改行の位置は行の区切り）を、行を足した 1 つの文字列にして原本と突き合わせる。
        let joined = |f: FailKind| want_fail_ja(f).join("／");
        assert_eq!(
            joined(FailKind::Unknown),
            "接続失敗：／不明な原因で相手からの UDP パケットが届きません／iniをdetail=1に書き換えてもう一度試し、開発者にログを送ってください。／（README「接続できないとき」1）"
        );
        assert_eq!(
            joined(FailKind::PeerSymmetricNoUpnp),
            "接続失敗：／相手がシンメトリック NATであり、こちらはUPnPが使用不可です。／ルーターのUPnPをオンにするか、ポート開放してください。／（README「接続できないとき」2）"
        );
        assert_eq!(
            joined(FailKind::OwnSymmetric),
            "接続失敗：／こちらがシンメトリック NATです。／相手側のポート開放が必要です。／相手もシンメトリックNATなら通信できません。／（README「接続できないとき」3）"
        );
        assert_eq!(
            joined(FailKind::BothSymmetric),
            "接続不可：／こちらも相手もシンメトリック NAT のため、直接接続できません。／（README「接続できないとき」4）"
        );
    }

    #[test]
    fn handshake_stalls_with_no_known_cause_show_three_lines() {
        let code = made_note();
        let room = room_note(CodeState::RoomLinking, Event::RoomPeer);
        const CODE_NEXT: &str =
            "2 人とも待機をやめ、片方が新しいコードを作って、すぐ送ってください。";
        const ROOM_NEXT: &str = "待機をやめ、接続コード方式に切り替えてください。";
        const NO_HS: &str = "相手からパケットは届いていますが、接続できません（待機時間 ";
        const REF: &str = "（README「接続できないとき」5）";
        let st = Some(stall_of(StallWhy::NoHandshake, FailKind::Unknown));
        for state in [CodeState::Replied, CodeState::Linking] {
            let n = Note { state, ..code };
            let (k, out, sty) = block(&n, st, Lang::Ja);
            assert_eq!(k, 3, "{state:?}");
            assert_eq!(out[0].as_str(), format!("{NO_HS}0:42・残り 29:18）"));
            assert_eq!(out[1].as_str(), CODE_NEXT);
            assert_eq!(out[2].as_str(), REF);
            assert_eq!(sty[..3], [RowStyle::Warn; 3]);
            assert!(out[3..].iter().all(|l| l.as_str().is_empty()));
            assert!(!corner_is_band(Conn::Waiting, &n, st));
        }
        let (k, out, _) = block(&room, st, Lang::Ja);
        assert_eq!(k, 3);
        assert_eq!(out[0].as_str(), format!("{NO_HS}9:12）"));
        assert!(
            !out[0].as_str().contains("残り"),
            "ルームの経路に残りは無い"
        );
        assert_eq!(out[1].as_str(), ROOM_NEXT);
        assert_eq!(out[2].as_str(), REF);
        let (_, out, _) = block(&room, st, Lang::En);
        assert_eq!(out[2].as_str(), "(README \"When you cannot connect\" 5)");
        // 待った時間と残りは別の値。
        let n = Note {
            state: CodeState::Linking,
            since: 1_000_000 - 600,
            ..code
        };
        let (_, out, _) = block(&n, st, Lang::Ja);
        assert_eq!(out[0].as_str(), format!("{NO_HS}10:00・残り 20:00）"));
        // 左上の行の幅（ゲームの文字で 640 − 2 × 6 画素）に、いちばん長い時計の組でも収まる。
        let game = game_font_spec("");
        let n = Note {
            state: CodeState::Linking,
            since: 1_000_000 - 86_399,
            ..code
        };
        for lang in Lang::ALL {
            let (k, out, _) = block(&n, st, lang);
            for l in &out[..k] {
                let w = crate::ui_layout::text_width(&game, l.as_str());
                assert!(w <= 640 - 2 * 6, "{lang:?}: 「{}」{w} 画素", l.as_str());
            }
        }
    }

    #[test]
    fn the_stall_block_stays_out_of_the_way_while_offering_or_room_waiting() {
        let st = Some(stall_of(StallWhy::NothingReceived, FailKind::OwnSymmetric));
        for note in [
            made_note(),
            room_note(CodeState::RoomWaiting, Event::RoomPeer),
        ] {
            let (k, _, _) = block(&note, st, Lang::Ja);
            assert_eq!(k, 0, "{:?}", note.state);
            let mut a = [LineBuf::new(); 2];
            let mut sa = [RowStyle::Normal; 2];
            let mut b = [LineBuf::new(); 2];
            let mut sb = [RowStyle::Normal; 2];
            let ka = corner_lines(
                Conn::Waiting,
                &note,
                1_000_000,
                st,
                Lang::Ja,
                &mut a,
                &mut sa,
            );
            let kb = corner_lines(
                Conn::Waiting,
                &note,
                1_000_000,
                None,
                Lang::Ja,
                &mut b,
                &mut sb,
            );
            assert_eq!(ka, 1, "{:?}", note.state);
            assert_eq!((ka, a, sa), (kb, b, sb), "{:?}", note.state);
            assert!(corner_is_band(Conn::Waiting, &note, st), "{:?}", note.state);
        }
    }

    #[test]
    fn the_stall_block_goes_away_when_the_wait_ends() {
        let st = Some(stall_of(StallWhy::NothingReceived, FailKind::Unknown));
        let mut out = [LineBuf::new(); 2];
        let mut sty = [RowStyle::Normal; 2];
        for state in [CodeState::Idle, CodeState::RoomJoining] {
            let n = Note {
                state,
                event: Event::Cancelled,
                ..made_note()
            };
            assert_eq!(block(&n, st, Lang::Ja).0, 0, "{state:?}");
            let k = corner_lines(
                Conn::Waiting,
                &n,
                1_000_000,
                st,
                Lang::Ja,
                &mut out,
                &mut sty,
            );
            let want = corner_lines(
                Conn::Waiting,
                &n,
                1_000_000,
                None,
                Lang::Ja,
                &mut out,
                &mut sty,
            );
            assert_eq!(k, want, "{state:?}");
            assert!(!corner_is_band(Conn::Waiting, &n, st), "{state:?}");
        }
        let n = Note {
            state: CodeState::Linking,
            ..made_note()
        };
        for conn in [
            Conn::Connected {
                seat: Seat::P1,
                host: true,
                shape: Some((Mode::Rollback, 1)),
            },
            Conn::Found {
                seat: Seat::P2,
                shape: None,
                ready: false,
                left_secs: 599,
            },
            Conn::Ended,
            Conn::Finished,
            Conn::OtherVersion { theirs: 1 },
        ] {
            let mut b = [LineBuf::new(); STALL_ROWS];
            let mut bs = [RowStyle::Normal; STALL_ROWS];
            assert_eq!(
                stall_block(conn, &n, 1_000_000, st, Lang::Ja, &mut b, &mut bs),
                0,
                "{conn:?}"
            );
            let with = corner_lines(conn, &n, 1_000_000, st, Lang::Ja, &mut out, &mut sty);
            let first = (out[0], out[1], sty);
            let without = corner_lines(conn, &n, 1_000_000, None, Lang::Ja, &mut out, &mut sty);
            assert_eq!(with, without, "{conn:?}");
            assert_eq!(first, (out[0], out[1], sty), "{conn:?}");
            assert!(!corner_is_band(conn, &n, st), "{conn:?}");
        }
    }

    #[test]
    fn the_stall_block_fits_the_screen_and_uses_known_glyphs() {
        let game = game_font_spec("");
        let glyphs = ui_glyphs();
        // ゲームの文字で左上に置く（`net_ui::draw_board` の左上の行）: 画面の幅から左右の余白を引いた幅に収まる。
        let limit = 640 - 2 * 6;
        let mut checked = 0u32;
        let mut widest = (0u32, String::new());
        for lang in Lang::ALL {
            for (state, ago) in [
                (CodeState::Linking, 86_399),
                (CodeState::Replied, 0),
                (CodeState::RoomLinking, 1_000_000),
            ] {
                for why in [StallWhy::NothingReceived, StallWhy::NoHandshake] {
                    for fail in ALL_FAILS {
                        let mut note = made_note();
                        note.state = state;
                        note.since = 1_000_000u32.wrapping_sub(ago);
                        let (n, out, _) = block(&note, Some(stall_of(why, fail)), lang);
                        assert!((3..=STALL_ROWS).contains(&n), "{n}");
                        for l in &out[..n] {
                            let s = l.as_str();
                            let w = crate::ui_layout::text_width(&game, s);
                            if w > widest.0 {
                                widest = (w, s.to_string());
                            }
                            assert!(w <= limit, "{lang:?}: 「{s}」{w} 画素 > {limit}");
                            assert!(s.len() + 4 <= LINE_BYTES, "{lang:?}: 「{s}」");
                            assert!(s.chars().count() <= crate::overlay::MAX_TEXT, "{s}");
                            for c in s.chars() {
                                assert!(glyphs.contains(&c), "文字の表に無い文字 {c:?}（{s}）");
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 100, "{checked}");
        assert!(widest.0 > 0, "{widest:?}");
    }

    /// 「名前」の項目のあるメニュー（ルームサーバーなし）。`now` は今の名前。
    fn name_driver(now: &str) -> Driver {
        let mut d = Driver::new();
        d.m = Menu::new(ini()).with_player_name(NameBuf::lossy(now));
        d.m.open(0);
        d
    }

    fn open_name_input(now: &str) -> Driver {
        let mut d = name_driver(now);
        d.cursor_to(Item::Name);
        d.tap(Z);
        assert!(d.m.wants_text(), "名前の入力が開かない");
        d
    }

    #[test]
    fn the_name_item_shows_only_while_nothing_is_pending_and_sits_before_back() {
        let off = Driver::new();
        assert!(
            !off.text().iter().any(|l| l.contains(T_NAME.ja)),
            "{:?}",
            off.text()
        );
        let mut d = name_driver("Alice-2");
        let t = d.text();
        assert_eq!(
            t[0..11],
            [
                T_GROUP_CODE.ja.to_string(),
                format!("> {}", T_CREATE.ja),
                format!("  {}", T_PASTE.ja),
                String::new(),
                T_GROUP_SETTINGS.ja.to_string(),
                format!("  {}Alice-2", lab(T_NAME, Lang::Ja)),
                format!("  {}◀ rollback ▶", lab(T_FORMAT, Lang::Ja)),
                format!("  {}◀ 1F ▶", lab(T_DELAY_ROW, Lang::Ja)),
                format!("  {}◀ 日本語 ▶", lab(T_LANG, Lang::Ja)),
                String::new(),
                format!("  {}", T_BACK.ja),
            ],
            "{t:?}"
        );
        for _ in 0..2 {
            d.tap(DOWN);
        }
        d.tap(Z);
        assert!(d.m.wants_text());
        assert_eq!(d.cmds, vec![], "入力を経ずに指示が出た");
        for st in ALL_STATES {
            if st == CodeState::Idle {
                continue;
            }
            let mut d = name_driver("Alice");
            d.note.state = st;
            assert!(
                !d.text().iter().any(|l| l.contains(T_NAME.ja)),
                "{st:?} で名前の項目が出た"
            );
        }
        let mut d = name_driver("Alice");
        d.m.set_opening(true);
        assert!(
            !d.text().iter().any(|l| l.contains(T_NAME.ja)),
            "準備中に出た"
        );
        let mut d = name_driver("Alice");
        d.conn = connected();
        assert!(
            !d.text().iter().any(|l| l.contains(T_NAME.ja)),
            "つながった後に出た"
        );
        let mut d = name_driver("Alice");
        d.note.state = CodeState::Offered;
        d.note.state = CodeState::Idle;
        assert!(d.text().iter().any(|l| l.contains(T_NAME.ja)));
    }

    #[test]
    fn name_input_starts_from_the_current_name_and_decides_with_enter() {
        let mut d = open_name_input("Alice");
        let t = d.text();
        assert_eq!(t[0], T_NAME.ja, "{t:?}");
        assert_eq!(d.input_line(), "プレイヤー名: Alice|", "{t:?}");
        assert_eq!(t.last().unwrap(), T_NAME_INPUT_KEYS.ja);
        d.tick_text(0, "_ 2");
        d.tick_text(0, "ＡＢ-.あ");
        assert_eq!(
            d.input_line(),
            "プレイヤー名: Alice_2AB-|",
            "空白は入らない"
        );
        d.tick_text(0, "\u{8}\u{8}\u{8}");
        assert_eq!(d.input_line(), "プレイヤー名: Alice_2|");
        let out = d.tick_text(0, &format!("x{ENTER}"));
        let want = NameBuf::from_name("Alice_2x").unwrap();
        assert_eq!(out.cmd, Some(Cmd::Name(want)));
        assert!(out.hold_game);
        assert!(!d.m.wants_text(), "決めても入力が閉じない");
        assert!(d.m.is_active());
        d.idle(3);
        assert!(
            d.text()
                .iter()
                .any(|l| l == &format!("> {}Alice_2x", lab(T_NAME, Lang::Ja))),
            "{:?}",
            d.text()
        );
        d.tap(Z);
        assert!(d.m.wants_text());
        assert_eq!(d.input_line(), "プレイヤー名: Alice_2x|");
    }

    #[test]
    fn name_input_refuses_empty_says_why_a_character_was_dropped_and_esc_keeps_the_old_name() {
        let has = |d: &Driver, s: &str| d.text().iter().any(|l| l == s);
        let mut d = open_name_input("Bob");
        for w in [Z, X, UP, DOWN, LEFT, RIGHT] {
            d.tick_text(w, "");
            d.tick_text(0, "");
        }
        assert!(d.m.wants_text());
        assert_eq!(d.cmds, vec![], "指示が出た");
        d.tick_text(0, "\u{8}\u{8}\u{8}");
        assert_eq!(d.input_line(), "プレイヤー名: |");
        let out = d.tick_text(0, ENTER);
        assert_eq!(out.cmd, None, "空の名前で決まった");
        assert!(d.m.wants_text(), "断った後は入力に留まる");
        assert!(has(&d, T_NAME_INPUT_EMPTY.ja));
        d.tick_text(0, " ");
        assert_eq!(d.input_line(), "プレイヤー名: |", "空白が入った");
        let out = d.tick_text(0, ENTER);
        assert_eq!(out.cmd, None, "空白だけの名前で決まった");
        assert!(has(&d, T_NAME_INPUT_EMPTY.ja));
        d.tick_text(0, "a");
        assert!(
            !has(&d, T_NAME_INPUT_EMPTY.ja),
            "文字を打っても断りが消えない"
        );
        for dropped in [" ", "ｱ", "あ", ".", "!"] {
            d.tick_text(0, dropped);
            assert!(has(&d, T_NAME_INPUT_DROPPED.ja), "{dropped:?}");
            d.tick_text(0, "b");
            assert!(!has(&d, T_NAME_INPUT_DROPPED.ja));
            d.tick_text(0, "\u{8}");
        }
        d.tick_text(0, "\u{1B}\t");
        assert!(!has(&d, T_NAME_INPUT_DROPPED.ja));
        assert_eq!(d.input_line(), "プレイヤー名: a|");
        let out = d.tick_text(ESC, "");
        assert!(out.hold_game && out.cmd.is_none());
        assert!(!d.m.wants_text() && d.m.is_active());
        d.idle(3);
        assert!(
            d.text()
                .iter()
                .any(|l| l == &format!("> {}Bob", lab(T_NAME, Lang::Ja)))
        );
        assert_eq!(d.cmds, vec![]);
        const ENTER_WORD: u16 = 0x1000;
        d.tick_text(ENTER_WORD, "");
        assert!(d.m.wants_text());
        let out = d.tick_text(ENTER_WORD, "\r\r");
        assert_eq!(out.cmd, None, "押している間に決まった");
        d.m.set_text_available(false);
        assert!(has(&d, T_ROOM_INPUT_NO_KEYS.ja));
        assert!(has(&d, T_NAME_INPUT_NO_KEYS_NEXT.ja));
    }

    #[test]
    fn a_name_with_unusable_characters_is_shown_with_question_marks_and_edited_from_empty() {
        let mut d = open_name_input("テスト");
        assert_eq!(d.input_line(), "プレイヤー名: |", "{:?}", d.text());
        d.tick_text(0, "Neo");
        let out = d.tick_text(0, ENTER);
        assert_eq!(out.cmd, Some(Cmd::Name(NameBuf::from_name("Neo").unwrap())));
        let d = name_driver("テスト");
        assert!(
            d.text()
                .iter()
                .any(|l| l == &format!("  {}???", lab(T_NAME, Lang::Ja))),
            "{:?}",
            d.text()
        );
    }

    #[test]
    fn name_input_rows_use_only_known_glyphs_and_fit() {
        for lang in Lang::ALL {
            name_input_rows_use_only_known_glyphs_and_fit_in(lang);
        }
    }

    fn name_input_rows_use_only_known_glyphs_and_fit_in(lang: Lang) {
        let spec = game_font_spec("");
        let limit = 640
            - 2 * crate::ui_layout::SCREEN_MARGIN
            - 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        let mut widest = 0;
        let mut tallest = 0;
        for unavailable in [false, true] {
            let mut d = open_name_input(&"W".repeat(32));
            d.m.lang = lang;
            d.m.set_text_available(!unavailable);
            d.m.name_err = Some(NameWhy::Empty);
            d.m.edit_dropped = true;
            let mut r = MenuRows::new();
            d.m.render(d.conn, &d.note, 1_000_000, &mut r);
            tallest = tallest.max(r.len);
            assert!(r.len <= MENU_ROWS);
            assert_eq!(r.lines[r.len - 1].as_str(), T_NAME_INPUT_KEYS.get(lang));
            for l in &r.lines[..r.len] {
                for c in l.as_str().chars() {
                    assert!(spec.slot_of(c).is_some(), "文字 {c:?} が文字の表に無い");
                }
                widest = widest.max(crate::ui_layout::text_width(&spec, l.as_str()));
            }
        }
        assert!(widest <= limit, "いちばん広い行 {widest} 画素 > {limit}");
        assert_eq!(tallest, 9, "見出し・空き・入力・断り 4・空き・キー");
        let mut d = name_driver(&"W".repeat(32));
        d.m.lang = lang;
        for l in d.text() {
            assert!(crate::ui_layout::text_width(&spec, &l) <= limit, "{l}");
            for key in ["Enter", "Esc", "Backspace"] {
                assert!(!l.contains(key), "一覧の行にキー名 {key}: {l}");
            }
        }
    }

    #[test]
    fn name_bands_put_the_seat_label_on_the_outer_side_and_never_pass_the_timer() {
        use crate::overlay::glyph_cells;
        use crate::ui_layout::{HUD_CELL_W, HUD_NAME_CELLS, HUD_NAME_MAX_W, text_width_plain};
        let mut l = LineBuf::new();
        name_band_line(&mut l, Lang::Ja, Seat::P1, b"Alice");
        assert_eq!(l.as_str(), "1P Alice");
        name_band_line(&mut l, Lang::Ja, Seat::P2, b"Bob x");
        assert_eq!(l.as_str(), "Bob x 2P");
        name_band_line(&mut l, Lang::Ja, Seat::P2, b"a\nb\x07");
        assert_eq!(l.as_str(), "a?b? 2P", "制御文字は ? にする");
        name_band_line(&mut l, Lang::Ja, Seat::P1, &[0xFF, 0xFE]);
        assert_eq!(l.as_str(), "1P ");
        let long = "あ".repeat(60);
        for seat in [Seat::P1, Seat::P2] {
            name_band_line(&mut l, Lang::Ja, seat, long.as_bytes());
            let s = l.as_str();
            let label = if seat == Seat::P1 { "1P " } else { " 2P" };
            assert!(
                s.starts_with(label) || s.ends_with(label),
                "ラベルが切れた: {s}"
            );
            let name: String = s.replace(label, "");
            assert!(name.chars().all(|c| c == 'あ'));
            let cells: u32 = name.chars().map(glyph_cells).sum();
            assert!(cells <= HUD_NAME_CELLS, "{cells}");
            assert!(cells + 2 > HUD_NAME_CELLS, "切りすぎ: {cells} 枠");
        }
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        assert_eq!(spec.cell_w, HUD_CELL_W, "文字の枠の幅が定数とずれた");
        let mut widest = 0;
        let names = [
            "W".repeat(32),
            "あ".repeat(10),
            "é".repeat(16),
            "ｱ".repeat(10),
            "a".repeat(32),
            "😀".repeat(8),
        ];
        for n in &names {
            assert!(n.len() <= crate::wire::NAME_MAX, "{n}");
            for seat in [Seat::P1, Seat::P2] {
                name_band_line(&mut l, Lang::Ja, seat, n.as_bytes());
                let w = text_width_plain(&spec, l.as_str());
                assert!(w <= HUD_NAME_MAX_W, "{} が {w} 画素", l.as_str());
                widest = widest.max(w);
            }
        }
        // 母数が 0 のまま通らないように: いちばん広いのは半角 32 文字（`1P ` ＋ 32 × 7 = 245）。
        assert_eq!(widest, 35 * 7);
    }

    #[test]
    fn the_status_line_at_the_bottom_stops_before_the_fps_text() {
        for lang in Lang::ALL {
            the_status_line_at_the_bottom_stops_before_the_fps_text_in(lang);
        }
    }

    fn the_status_line_at_the_bottom_stops_before_the_fps_text_in(lang: Lang) {
        use crate::frame::MAX_DELAY;
        use crate::ui_layout::{
            FPS_LEFT, HUD_BOTTOM_RIGHT, HUD_BOTTOM_TEXT_MAX_W, HUD_CELL_W, HUD_EDGE_X,
            HUD_TEXT_MAX_W, PAD, text_width_plain,
        };
        const { assert!(HUD_BOTTOM_RIGHT + HUD_CELL_W <= FPS_LEFT) };
        const { assert!(HUD_BOTTOM_TEXT_MAX_W < HUD_TEXT_MAX_W) };
        assert_eq!(HUD_EDGE_X + HUD_BOTTOM_TEXT_MAX_W + PAD, HUD_BOTTOM_RIGHT);
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let mut l = LineBuf::new();
        let mut widest = (0u32, String::new());
        let mut checked = 0u32;
        for rewind in [None, Some(0), Some(9), Some(99)] {
            for rtt in [None, Some(0), Some(9), Some(99), Some(999u32)] {
                for (latency, auto) in [
                    (None, false),
                    (Some(0), false),
                    (Some(MAX_DELAY), false),
                    (Some(MAX_DELAY), true),
                ] {
                    hud_line(
                        &mut l,
                        lang,
                        &HudView {
                            latency,
                            rtt_ms: rtt,
                            recommend: rtt.map(|_| MAX_DELAY),
                            rewind,
                            auto,
                            ..HudView::default()
                        },
                    );
                    let w = text_width_plain(&spec, l.as_str());
                    assert!(
                        w <= HUD_BOTTOM_TEXT_MAX_W,
                        "「{}」が {w} 画素（下の帯の上限 {HUD_BOTTOM_TEXT_MAX_W}）",
                        l.as_str()
                    );
                    if w > widest.0 {
                        widest = (w, l.as_str().to_string());
                    }
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 4 * 5 * 4);
        println!(
            "いちばん広い下の帯の行: 「{}」 {} 画素 / 上限 {HUD_BOTTOM_TEXT_MAX_W}",
            widest.1, widest.0
        );
        let mut widest_select = (0u32, String::new());
        for format in [Mode::Rollback, Mode::Delay] {
            for rtt in [None, Some(0), Some(9), Some(99), Some(999u32), Some(9_999)] {
                for latency in [None, Some(0), Some(8)] {
                    for auto in [false, true] {
                        hud_line(
                            &mut l,
                            lang,
                            &HudView {
                                latency,
                                rtt_ms: rtt,
                                recommend: rtt.map(|_| 8),
                                auto,
                                format: Some(format),
                                ..HudView::default()
                            },
                        );
                        let w = text_width_plain(&spec, l.as_str());
                        assert!(
                            w <= HUD_BOTTOM_TEXT_MAX_W,
                            "「{}」が {w} 画素（下の帯の上限 {HUD_BOTTOM_TEXT_MAX_W}）",
                            l.as_str()
                        );
                        if w > widest_select.0 {
                            widest_select = (w, l.as_str().to_string());
                        }
                    }
                }
            }
        }
        println!(
            "いちばん広いキャラセレクトの行: 「{}」 {} 画素 / 上限 {HUD_BOTTOM_TEXT_MAX_W}",
            widest_select.1, widest_select.0
        );
        hud_line(
            &mut l,
            lang,
            &HudView {
                latency: Some(MAX_DELAY),
                rtt_ms: Some(9_999),
                recommend: Some(MAX_DELAY),
                rewind: Some(255),
                ..HudView::default()
            },
        );
        assert!(text_width_plain(&spec, l.as_str()) > HUD_BOTTOM_TEXT_MAX_W);
    }

    // ---- 言語（日本語・英語） ----

    use crate::lang::is_japanese_char as is_japanese;

    #[test]
    fn a_new_menu_speaks_japanese_until_told_otherwise() {
        let m = Menu::new(ini());
        assert_eq!(m.lang(), Lang::Ja);
        assert_eq!(m.with_lang(Lang::En).lang(), Lang::En);
    }

    #[test]
    fn english_menu_rows_are_english_and_japanese_rows_are_not() {
        let rows_in = |lang: Lang| {
            let mut d = full_menu_driver();
            d.m.lang = lang;
            d.text()
        };
        let en = rows_in(Lang::En);
        for want in [
            "Play".to_string(),
            "> Random match".to_string(),
            "  Room match".to_string(),
            "Watch".to_string(),
            "Play with a code".to_string(),
            "  Create code".to_string(),
            "  Paste code".to_string(),
            "Settings".to_string(),
            format!("  {}Alice", lab(T_NAME, Lang::En)),
            "  Room name  (not set)".to_string(),
            format!("  {}◀ 1P ▶", lab(T_SEAT, Lang::En)),
            format!("  {}◀ rollback ▶", lab(T_FORMAT, Lang::En)),
            format!("  {}◀ 1F ▶", lab(T_DELAY_ROW, Lang::En)),
            format!("  {}◀ English ▶", lab(T_LANG, Lang::En)),
            "  Back".to_string(),
            T_KEYS.en.to_string(),
        ] {
            assert!(en.contains(&want), "{want:?} が無い: {en:?}");
        }
        assert!(
            en.iter().all(|l| !l.chars().any(is_japanese)),
            "英語の一覧に日本語: {en:?}"
        );
        assert_eq!(
            en.iter().filter(|l| *l == "  Room match").count(),
            2,
            "対戦と観戦の両方にルームマッチ: {en:?}"
        );
        let ja = rows_in(Lang::Ja);
        assert!(
            ja.contains(&format!("  {}◀ 日本語 ▶", lab(T_LANG, Lang::Ja))),
            "{ja:?}"
        );
        assert!(ja.contains(&format!("> {}", T_RANDOM.ja)), "{ja:?}");
        assert!(ja.contains(&format!("  {}", T_ROOM.ja)), "{ja:?}");
        assert_ne!(en, ja);
    }

    #[test]
    fn the_language_row_flips_the_language_and_asks_to_remember_it_once() {
        let mut d = Driver::new();
        d.cursor_to(Item::Lang);
        assert_eq!(d.m.lang(), Lang::Ja);
        d.tap(RIGHT);
        assert_eq!(d.cmds, vec![Cmd::Lang(Lang::En)], "1 回押して 1 回");
        assert_eq!(d.m.lang(), Lang::En);
        let t = d.text();
        assert!(
            t.contains(&format!("> {}◀ English ▶", lab8(T_LANG, Lang::En))),
            "{t:?}"
        );
        assert!(
            t.contains(&T_KEYS.en.to_string()),
            "その tick から英語: {t:?}"
        );
        d.tap(LEFT);
        assert_eq!(d.cmds, vec![Cmd::Lang(Lang::En), Cmd::Lang(Lang::Ja)]);
        assert_eq!(d.m.lang(), Lang::Ja);
        let t = d.text();
        assert!(
            t.contains(&format!("> {}◀ 日本語 ▶", lab8(T_LANG, Lang::Ja))),
            "{t:?}"
        );
        // 押しっぱなしでは繰り返さない（押した瞬間だけ）。
        let mut m = Menu::new(ini());
        m.open(0);
        let mut held = 0;
        for _ in 0..5 {
            held += usize::from(m.tick(&input(DOWN, quiet(), Conn::Waiting)).cmd.is_some());
        }
        assert_eq!(held, 0);
        // 他の行の ← → は言語に触らない。
        let mut d = Driver::new();
        d.cursor_to(Item::Delay);
        d.tap(RIGHT);
        assert_eq!(d.m.lang(), Lang::Ja);
        assert!(
            d.cmds.iter().all(|c| !matches!(c, Cmd::Lang(_))),
            "{:?}",
            d.cmds
        );
    }

    #[test]
    fn every_text_has_an_english_that_is_not_a_leftover_japanese() {
        // 日本語に残る語: ゲームの項目の名前（訳さない）と、言語の行の自分の名前。
        let allowed = ["妖怪", "対", "日本語"];
        let mut empty_ja = Vec::new();
        for tx in TEXTS {
            if tx.ja.is_empty() {
                empty_ja.push(tx.en);
            }
            assert!(!tx.en.is_empty(), "英語が空: {tx:?}");
            let mut rest = tx.en.to_string();
            for a in allowed {
                rest = rest.replace(a, "");
            }
            assert!(
                !rest.chars().any(is_japanese),
                "英語に日本語が残っている: {:?}",
                tx.en
            );
        }
        // 日本語が空なのは、語順の違いで英語にだけ先頭の文字がある 2 つだけ。
        assert_eq!(
            empty_ja,
            [T_ROOM_BUSY_NEXT_OPEN.en, T_SPECTATE_GAVE_UP_OPEN.en]
        );
        let same: Vec<_> = TEXTS
            .iter()
            .filter(|t| t.ja == t.en && t.ja.chars().any(is_japanese))
            .collect();
        assert_eq!(
            same.len(),
            1,
            "同じ文字が両方にある日本語の文は言語の行の値だけ: {same:?}"
        );
    }

    fn event_strings(lang: Lang, note: &Note) -> Vec<String> {
        let mut out = [LineBuf::new(); STATUS_ROWS];
        let mut warn = [false; STATUS_ROWS];
        let n = event_lines(note, lang, &mut out, &mut warn);
        out[..n].iter().map(|l| l.as_str().to_string()).collect()
    }

    #[test]
    fn english_sentences_put_names_and_numbers_in_english_order() {
        let en = |ev: Event, f: &dyn Fn(&mut Note)| {
            let mut n = room_note(CodeState::Idle, ev);
            n.name = sanitize_name(b"Bob", |_| true);
            f(&mut n);
            event_strings(Lang::En, &n)
        };
        assert_eq!(
            en(Event::RoomPeer, &|_| {}),
            [
                "Opponent \"Bob\" joined the room.",
                "Connecting. Please wait."
            ]
        );
        assert_eq!(
            en(Event::ReplyMade, &|n| n.paste_len = 7),
            [
                "Received 7 chars",
                "Read the code from \"Bob\".",
                "Copied the reply code. Send it to your opponent."
            ]
        );
        assert_eq!(
            en(Event::PeerRead, &|n| n.paste_len = 7)[1],
            "Read the code from \"Bob\" (reply)."
        );
        assert_eq!(
            en(Event::RoomBusy, &|n| n.retry_secs = 5),
            [
                "The server is busy.",
                "Wait about 5 seconds, then choose \"Room match\" again."
            ]
        );
        assert_eq!(
            en(Event::SpectateGaveUp, &|_| {}),
            [
                "No player came to the room in 30 min.",
                "Check the room name and choose \"Room match\" under Watch again."
            ]
        );
        assert_eq!(
            en(Event::RoomNameBad, &|n| n.room_why = RoomNameWhy::Long)[0],
            format!(
                "The room name is too long (max {} chars).",
                crate::room::ROOM_NAME_MAX
            )
        );
        assert_eq!(
            en(Event::GaveUp, &|_| {})[0],
            "Stopped waiting (30 min passed)."
        );
        assert_eq!(
            en(Event::Expired, &|n| n.paste_len = 3)[1],
            "The opponent's code has expired (30 min)."
        );
        let mut line = LineBuf::new();
        connected_line(
            &mut line,
            Lang::En,
            Seat::P1,
            true,
            Some((Mode::Rollback, 2)),
        );
        assert_eq!(
            line.as_str(),
            "Connected. You are 1P (host). (rollback, delay 2)"
        );
        connected_line(&mut line, Lang::En, Seat::P2, false, Some((Mode::Delay, 1)));
        assert_eq!(
            line.as_str(),
            "Connected. You are 2P. (delay mode, delay 1)"
        );
        found_line(&mut line, Lang::En, false, 599);
        assert_eq!(line.as_str(), "Opponent found (closes in 9:59)");
        other_build_line(&mut line, Lang::En, BUILD_NUMBER, 0);
        assert!(
            line.as_str().ends_with(", opponent dev)."),
            "{}",
            line.as_str()
        );
        hud_line(
            &mut line,
            Lang::En,
            &HudView {
                latency: Some(2),
                format: Some(Mode::Delay),
                delay_only: true,
                ..HudView::default()
            },
        );
        assert_eq!(line.as_str(), "delay only, Delay 2F  ping -");
    }

    #[test]
    fn corner_lines_fit_the_screen_in_both_languages() {
        let game = game_font_spec("");
        let plain = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let limit = 640 - 2 * 6;
        let menu_limit = 640
            - 2 * crate::ui_layout::SCREEN_MARGIN
            - 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        let mut checked = 0u32;
        let conns = [
            Conn::Waiting,
            Conn::Ended,
            Conn::Finished,
            Conn::Connected {
                seat: Seat::P2,
                host: true,
                shape: Some((Mode::Rollback, 8)),
            },
            Conn::Connected {
                seat: Seat::P2,
                host: true,
                shape: Some((Mode::Delay, 8)),
            },
            Conn::Found {
                seat: Seat::P2,
                shape: None,
                ready: true,
                left_secs: 599,
            },
            Conn::Found {
                seat: Seat::P1,
                shape: None,
                ready: false,
                left_secs: 599,
            },
            Conn::Unmatched(NoMatch::TimedOut),
            Conn::Unmatched(NoMatch::PeerHasNoGate),
            Conn::Unmatched(NoMatch::PeerLeft),
            Conn::OtherVersion { theirs: 255 },
            Conn::OtherBuild { theirs: 404_000 },
            Conn::OtherBuild { theirs: 0 },
            Conn::Desynced,
        ];
        for lang in Lang::ALL {
            // 配るバージョンでは自分も相手も日付と時刻（20261008-2100 の形）で出るので、いちばん長い組を直に測る。
            let mut line = LineBuf::new();
            for (mine, theirs) in [(404_000, 404_000), (404_000, 0), (0, 404_000)] {
                other_build_line(&mut line, lang, mine, theirs);
                let w = crate::ui_layout::text_width(&game, line.as_str());
                assert!(
                    w <= limit,
                    "{lang:?}: 「{}」{w} 画素 > {limit}",
                    line.as_str()
                );
            }
            for conn in conns {
                for (ev, st) in EVENTS.into_iter().flat_map(|e| ALL_STATES.map(|s| (e, s))) {
                    let mut note = made_note();
                    note.event = ev;
                    note.state = st;
                    note.name = sanitize_name(&[b'W'; 32], |_| true);
                    note.room = sanitize_room(&"W".repeat(40), |_| true);
                    note.retry_secs = u16::MAX;
                    note.since = 1_000_000 - 86_399;
                    let mut out = [LineBuf::new(); 2];
                    let mut sty = [RowStyle::Normal; 2];
                    let n = corner_lines(conn, &note, 1_000_000, None, lang, &mut out, &mut sty);
                    let mut m = Menu::new(ini()).with_lang(lang);
                    m.open(0);
                    let mut rows = MenuRows::new();
                    m.render(conn, &note, 1_000_000, &mut rows);
                    for l in &rows.lines[..rows.len] {
                        // 行の入れ物（`LINE_BYTES`）で黙って切れていない（切れるのは満杯のときだけ）。
                        assert!(
                            l.as_str().len() + 4 <= LINE_BYTES,
                            "{lang:?}: 「{}」",
                            l.as_str()
                        );
                        let w = crate::ui_layout::text_width(&game, l.as_str());
                        assert!(
                            w <= menu_limit,
                            "{lang:?} {conn:?} {ev:?}: メニューの「{}」{w} 画素 > {menu_limit}",
                            l.as_str()
                        );
                        checked += 1;
                    }
                    let band = corner_is_band(conn, &note, None);
                    for l in &out[..n] {
                        let s = l.as_str();
                        assert!(s.len() + 4 <= LINE_BYTES, "{lang:?}: 「{s}」");
                        if band {
                            let w = crate::ui_layout::text_width_plain(&plain, s);
                            assert!(
                                w <= crate::ui_layout::HUD_TEXT_MAX_W,
                                "{lang:?} 帯の行「{s}」{w} 画素"
                            );
                        } else {
                            let w = crate::ui_layout::text_width(&game, s);
                            assert!(
                                w <= limit,
                                "{lang:?} {conn:?} {ev:?}: 「{s}」{w} 画素 > {limit}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 2_000, "見た行 {checked}");
    }

    #[test]
    fn the_name_is_never_cut_shorter_than_a_room_name_in_either_language() {
        for lang in Lang::ALL {
            assert!(room_band_cells(lang) >= ROOM_UNITS as u32, "{lang:?}");
            assert!(spectate_band_cells(lang) >= ROOM_UNITS as u32, "{lang:?}");
        }
        assert_eq!(room_band_cells(Lang::Ja), 16);
        assert_eq!(spectate_band_cells(Lang::Ja), 20);
        assert_eq!(spectate_band_cells(Lang::En), 19);
        for (i, t) in [("a", 1), ("あ", 2), ("Room ", 5), (" で待機中", 9)] {
            assert_eq!(cells_of(i), t, "{i}");
        }
    }

    /// 名前に打てる `@` が、GDI の文字の表にもゲームの文字の表にも入っていて、入力の画面の行が `?` にならずに収まる。
    #[test]
    fn the_at_sign_is_drawn_in_both_glyph_tables() {
        let plain = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let game = game_font_spec("");
        assert!(plain.slot_of('@').is_some());
        assert!(game.slot_of('@').is_some() && game.game_glyph('@').is_some());
        for spec in [&plain, &game] {
            assert_eq!(
                crate::ui_layout::text_width(spec, "a@b"),
                crate::ui_layout::text_width(spec, "aab"),
                "@ が別の幅で描かれる"
            );
        }
        let mut d = Driver::open_room_input();
        d.tick_text(0, "a@b");
        assert_eq!(d.input_line(), "ルーム名: a@b|");
    }

    /// 値の始まりの x（画素）: 「◀」のある行は◀の手前までの幅、名前の行は `value` の手前までの幅。
    fn value_x(spec: &AtlasSpec, row: &str, value: &str) -> Option<u32> {
        let at = row.find('◀').or_else(|| row.find(value))?;
        Some(crate::ui_layout::text_width(spec, &row[..at]))
    }

    #[test]
    fn values_start_at_the_same_x_within_a_heading() {
        let plain = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let game = game_font_spec("");
        let mut measured = 0;
        for lang in Lang::ALL {
            let mut d = full_menu_driver();
            d.m.lang = lang;
            let t = d.text();
            let from = t
                .iter()
                .position(|l| l == T_GROUP_SETTINGS.get(lang))
                .unwrap();
            let rows: Vec<&String> = t[from + 1..].iter().take_while(|l| !l.is_empty()).collect();
            assert_eq!(rows.len(), 5, "{lang:?}: {t:?}");
            for spec in [&plain, &game] {
                let xs: Vec<u32> = rows
                    .iter()
                    .map(|r| value_x(spec, r, "Alice").unwrap())
                    .collect();
                measured += xs.len();
                assert!(
                    xs.iter().all(|&x| x == xs[0]),
                    "{lang:?}: 値の始まりが揃わない {xs:?} {rows:?}"
                );
            }
        }
        assert_eq!(measured, 20);
    }

    #[test]
    fn items_without_a_heading_are_set_apart_by_one_blank_row_only_when_something_is_above() {
        let blank_before = |d: &Driver, item: &str| {
            let t = d.text();
            let at = t.iter().position(|l| l.ends_with(item)).unwrap();
            (at > 0).then(|| t[at - 1].is_empty())
        };
        let d = full_menu_driver();
        assert_eq!(blank_before(&d, T_BACK.ja), Some(true));
        let mut d = full_menu_driver();
        d.note.state = CodeState::RoomWaiting;
        assert_eq!(blank_before(&d, T_CANCEL.ja), Some(true));
        assert_eq!(
            blank_before(&d, T_BACK.ja),
            Some(false),
            "やめるともどるの間には空行を置かない"
        );
        let mut d = full_menu_driver();
        d.conn = connected();
        assert_eq!(blank_before(&d, T_BACK.ja), None, "もどるだけの形");
        let mut d = full_menu_driver();
        d.note = spectate_note(CodeState::SpectateWaiting, Event::RoomWaiting);
        assert_eq!(blank_before(&d, T_SPECTATE_CANCEL.ja), None);
        let mut d = full_menu_driver();
        d.note = spectate_note(CodeState::SpectateIdle, Event::None);
        assert_eq!(blank_before(&d, T_BACK.ja), Some(true));
    }

    #[test]
    fn the_menu_opens_on_random_match_right_under_the_play_heading() {
        let mut d = full_menu_driver();
        let t = d.text();
        assert_eq!(t[0], T_GROUP_PLAY.ja, "先頭は対戦の見出し");
        assert_eq!(t[1], format!("> {}", T_RANDOM.ja), "開いたときのカーソル");
        let r = d.rows();
        assert_eq!(r.styles[0], RowStyle::Title);
        assert_eq!(r.styles[1], RowStyle::Selected);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::Random], "ランダムマッチを決めた指示");
        assert!(!d.m.wants_text(), "ランダムマッチはルーム名を打たない");
        assert!(d.m.is_active());
        d.tap(DOWN);
        d.tap(DOWN);
        let t = d.text();
        assert!(t[4].starts_with(&format!("> {}", T_ROOM_NAME.ja)), "{t:?}");
        d.tap(Z);
        assert!(d.m.wants_text());
        assert_eq!(d.m.edit_for, EditFor::RoomSet);
        let mut d = Driver::new();
        assert_eq!(
            d.text()[1],
            format!("> {}", T_CREATE.ja),
            "ルームサーバー無し"
        );
        d.m.open(0);
        d.tap(UP);
        assert_eq!(d.m.cursor, d.m.items(d.conn, d.note.state).1 - 1);
    }

    /// 待機中の印が付いた行（項目の名前の後ろに印が続く行）の項目の名前。
    fn rows_with_waiting_mark(d: &Driver, lang: Lang) -> Vec<String> {
        d.text()
            .into_iter()
            .filter(|l| {
                (l.starts_with("> ") || l.starts_with("  ")) && l.contains(T_WAITING_MARK.get(lang))
            })
            .collect()
    }

    #[test]
    fn the_waiting_mark_sits_on_the_row_that_started_the_wait_with_the_corner_clock() {
        // (状態, ランダムの印, 印の付く行)。時計は待った時間（552 秒 = 9:12）で、左上の行と同じ値。
        for (state, random, row) in [
            (CodeState::RoomJoining, true, T_RANDOM),
            (CodeState::RoomWaiting, true, T_RANDOM),
            (CodeState::RoomLinking, true, T_RANDOM),
            (CodeState::RoomJoining, false, T_ROOM),
            (CodeState::RoomWaiting, false, T_ROOM),
            (CodeState::RoomLinking, false, T_ROOM),
            (CodeState::Offered, false, T_CREATE),
            (CodeState::Linking, false, T_CREATE),
            (CodeState::Replied, false, T_PASTE),
        ] {
            for lang in Lang::ALL {
                let mut d = full_menu_driver();
                d.m = d.m.with_lang(lang);
                d.note = room_note(state, Event::None);
                d.note.random = random;
                let got: Vec<String> = rows_with_waiting_mark(&d, lang)
                    .iter()
                    .map(|l| l.trim_start_matches(['>', ' ']).to_string())
                    .collect();
                let want = format!("{}{}9:12", row.get(lang), T_WAITING_MARK.get(lang));
                assert_eq!(got, [want], "{state:?} {random} {lang:?}");
            }
        }
        // 待っていない・観戦の待ち・つながった後は、どの行にも付かない。
        for state in [
            CodeState::Idle,
            CodeState::SpectateJoining,
            CodeState::SpectateWaiting,
            CodeState::SpectateLinking,
            CodeState::SpectateIdle,
        ] {
            let mut d = full_menu_driver();
            d.note = room_note(state, Event::None);
            assert_eq!(rows_with_waiting_mark(&d, Lang::Ja), Vec::<String>::new());
        }
        let mut d = full_menu_driver();
        d.note = room_note(CodeState::RoomWaiting, Event::None);
        d.conn = connected();
        assert_eq!(rows_with_waiting_mark(&d, Lang::Ja), Vec::<String>::new());
        // 日本語の見本。
        let mut d = full_menu_driver();
        d.note = room_note(CodeState::Offered, Event::None);
        assert!(
            d.text().contains(&"  コード作成  待機中 9:12".to_string()),
            "{:?}",
            d.text()
        );
    }

    #[test]
    fn waiting_hides_the_watch_rows_and_the_paste_row_turns_into_the_reply_row_while_offered() {
        for state in [
            CodeState::RoomWaiting,
            CodeState::Offered,
            CodeState::Linking,
        ] {
            let mut d = full_menu_driver();
            d.note = room_note(state, Event::None);
            let t = d.text();
            assert!(
                !t.contains(&T_GROUP_WATCH.ja.to_string()),
                "{state:?}: {t:?}"
            );
            assert_eq!(
                t.iter()
                    .filter(|l| l.starts_with("  ") && l.trim_start().starts_with(T_ROOM.ja))
                    .count(),
                1,
                "{state:?}: 観戦のルームマッチが待ちの間に出た: {t:?}"
            );
        }
        for (state, reply) in [
            (CodeState::Idle, false),
            (CodeState::Offered, true),
            (CodeState::Linking, true),
            (CodeState::Replied, false),
        ] {
            let mut d = full_menu_driver();
            d.note = room_note(state, Event::None);
            let t = d.text();
            assert_eq!(
                t.contains(&format!("  {}", T_PASTE_REPLY.ja)),
                reply,
                "{state:?}: {t:?}"
            );
            assert_eq!(
                t.iter().any(|l| l.trim_start().starts_with(T_PASTE.ja)),
                !reply,
                "{state:?}: {t:?}"
            );
        }
    }

    #[test]
    fn a_closed_menu_opens_on_random_match_again_after_the_cursor_moved() {
        let mut d = full_menu_driver();
        d.cursor_to(Item::Lang);
        d.tap(ESC);
        d.idle(3);
        assert!(!d.m.is_active());
        d.m.open(0);
        let t = d.text();
        assert_eq!(t[1], format!("> {}", T_RANDOM.ja), "{t:?}");
    }

    #[test]
    fn the_cursor_stays_on_the_same_item_when_the_room_name_row_comes_and_goes() {
        // 待ちの間も残る項目は、ルーム名・観戦・名前・サイドの行が出入りしても同じ項目に残る。
        for start in [
            Item::Random,
            Item::Room,
            Item::Create,
            Item::Paste,
            Item::Format,
            Item::Delay,
            Item::Lang,
            Item::Back,
        ] {
            let mut d = full_menu_driver();
            d.cursor_to(start);
            let at = |d: &Driver| {
                let (v, n) = d.m.items(d.conn, d.note.state);
                v[d.m.settled_cursor(&(v, n))]
            };
            assert_eq!(at(&d), start);
            d.note.state = CodeState::Offered;
            d.tap(0);
            assert_eq!(at(&d), start, "{start:?}: 行が消えても同じ項目");
            d.note.state = CodeState::Idle;
            d.tap(0);
            assert_eq!(at(&d), start, "{start:?}: 行が戻っても同じ項目");
        }
    }

    #[test]
    fn random_match_items_are_shown_with_the_room_server_only_and_chosen_while_waiting() {
        for st in [
            CodeState::RoomJoining,
            CodeState::RoomWaiting,
            CodeState::RoomLinking,
        ] {
            let mut d = full_menu_driver();
            d.note.state = st;
            d.note.random = true;
            let t = d.text();
            assert!(t.iter().any(|l| l.contains(T_RANDOM.ja)), "{st:?}: {t:?}");
            d.cursor_to(Item::Random);
            d.tap(Z);
            assert_eq!(d.cmds, vec![Cmd::Random], "{st:?}");
        }
        let mut m = Menu::new(ini());
        m.open(0);
        let t = texts_of(&m, &quiet());
        assert!(!t.iter().any(|l| l.contains(T_RANDOM.ja)), "{t:?}");
        let mut d = Driver::new();
        d.tap(Z);
        assert_eq!(
            d.cmds,
            vec![Cmd::Create],
            "ルームサーバー無しの先頭はコード作成"
        );
    }

    #[test]
    fn the_random_waiting_lines_never_show_a_room_name_and_name_the_random_match() {
        for lang in Lang::ALL {
            let mut note = room_note(CodeState::RoomWaiting, Event::RoomWaiting);
            note.random = true;
            note.room = [0; ROOM_UNITS];
            note.since = 1_000_000 - 83;
            let mut out = [LineBuf::new(); 2];
            let mut st = [RowStyle::Normal; 2];
            let n = corner_lines(
                Conn::Waiting,
                &note,
                1_000_000,
                None,
                lang,
                &mut out,
                &mut st,
            );
            assert_eq!(n, 1);
            assert_eq!(
                out[0].as_str(),
                format!("{} 1:23", T_RANDOM_WAITING.get(lang))
            );
            assert!(corner_is_band(Conn::Waiting, &note, None));
            let mut rows = MenuRows::new();
            let mut m = Menu::new(ini()).with_room(RoomItem::On(RoomBuf::new()));
            m.lang = lang;
            m.open(0);
            m.render(Conn::Waiting, &note, 1_000_000, &mut rows);
            let t: Vec<&str> = rows.lines[..rows.len].iter().map(LineBuf::as_str).collect();
            assert!(
                t.contains(&format!("{} 1:23", T_RANDOM_WAITING.get(lang)).as_str()),
                "{t:?}"
            );
            assert!(t.contains(&T_NEXT_RANDOM_WAITING.get(lang)), "{t:?}");
            assert!(!t.contains(&T_NEXT_ROOM_WAITING.get(lang)), "{t:?}");
            // 同じ状態でルームの待機なら、ルーム名の行になる（random の印だけが違いを作る）。
            note.random = false;
            note.room = sanitize_room("abc", |_| true);
            let room_line = corner_text(&note, lang);
            assert!(room_line.contains("abc"), "{room_line}");
            assert!(!room_line.contains(T_RANDOM_WAITING.get(lang)));
        }
    }

    fn corner_text(note: &Note, lang: Lang) -> String {
        let mut out = [LineBuf::new(); 2];
        let mut st = [RowStyle::Normal; 2];
        corner_lines(
            Conn::Waiting,
            note,
            1_000_000,
            None,
            lang,
            &mut out,
            &mut st,
        );
        out[0].as_str().to_string()
    }

    #[test]
    fn random_failures_say_to_choose_random_match_again_and_never_the_room_words() {
        for lang in Lang::ALL {
            for ev in [
                Event::RoomBusy,
                Event::RoomFull,
                Event::RoomRefused,
                Event::RoomExpired,
                Event::RoomPeerLeft,
                Event::RoomBadCode,
            ] {
                let mut n = room_note(CodeState::Idle, ev);
                n.retry_secs = 45;
                n.random = false;
                let plain = event_strings(lang, &n);
                n.random = true;
                let random = event_strings(lang, &n);
                assert_eq!(plain.len(), 2, "{ev:?}");
                assert_eq!(random.len(), 2, "{ev:?}");
                assert_eq!(plain[0], random[0], "{ev:?}: 1 行目は同じ");
                assert_ne!(plain[1], random[1], "{ev:?}: 次にすることが替わる");
                assert!(random[1].contains(T_RANDOM.get(lang)), "{ev:?}: {random:?}");
                assert!(
                    !random[1].contains("「ルームマッチ」"),
                    "{ev:?}: {random:?}"
                );
                if ev == Event::RoomBusy {
                    assert!(random[1].contains("45"), "{random:?}");
                }
            }
            let mut n = room_note(CodeState::RoomLinking, Event::RoomPeer);
            n.name = sanitize_name(b"Bob", |_| true);
            n.random = true;
            let peer = event_strings(lang, &n);
            assert!(
                peer[0].contains("Bob") && peer[0].contains("Bob"),
                "{peer:?}"
            );
            assert!(!peer[0].contains(T_ROOM_PEER_CLOSE.get(lang)), "{peer:?}");
            let want = match lang {
                Lang::Ja => "対戦相手が見つかりました「Bob」",
                Lang::En => "Opponent found: \"Bob\"",
            };
            assert_eq!(peer[0], want);
            assert_eq!(corner_text(&n, lang), T_RANDOM_LINKING.get(lang));
            n.random = false;
            assert_eq!(corner_text(&n, lang), T_ROOM_LINKING.get(lang));
        }
    }

    #[test]
    fn the_random_flag_round_trips_through_the_note_box_and_is_off_by_default() {
        assert!(!Note::default().random);
        let mut n = Note {
            random: true,
            event: Event::RoomWaiting,
            state: CodeState::RoomWaiting,
            peer_ver: 200,
            ..Note::default()
        };
        assert_eq!(Note::from_words(&n.to_words()), n);
        n.random = false;
        assert!(!Note::from_words(&n.to_words()).random);
    }

    // ---------------- 観戦の「ランダムマッチ」の一覧の画面 ----------------

    fn wrow(i: u8, p1: &str, p2: &str, watchers: u8, cap: u8) -> WatchRow {
        WatchRow {
            hash: [i; 8],
            p1: crate::watch_list::WatchName::parse(p1).unwrap(),
            p2: crate::watch_list::WatchName::parse(p2).unwrap(),
            mode: Mode::Rollback,
            delay: 1,
            watchers,
            cap,
        }
    }

    fn snap(status: ListStatus, rows: &[WatchRow]) -> Box<WatchSnapshot> {
        let mut s = Box::new(WatchSnapshot::EMPTY);
        s.set(status, rows);
        s
    }

    /// 観戦の「ランダムマッチ」を決めて一覧の画面を開いた状態。
    struct ListDriver {
        m: Menu,
        w: Box<WatchSnapshot>,
        cmds: Vec<Cmd>,
        conn: Conn,
    }

    impl ListDriver {
        fn with(lang: Lang, w: Box<WatchSnapshot>) -> ListDriver {
            let mut m = Menu::new(ini())
                .with_room(RoomItem::On(RoomBuf::new()))
                .with_lang(lang);
            m.open(0);
            let mut d = ListDriver {
                m,
                w,
                cmds: Vec::new(),
                conn: Conn::Waiting,
            };
            // 対戦の 3 項目（ランダムマッチ・ルームマッチ・ルーム名）の下が観戦のランダムマッチ。
            for _ in 0..3 {
                d.tap(DOWN);
            }
            d.tap(Z);
            assert!(d.m.wants_watch_list(), "一覧の画面が開かない");
            d
        }

        fn new(rows: &[WatchRow]) -> ListDriver {
            ListDriver::with(Lang::Ja, snap(ListStatus::Ready, rows))
        }

        fn tap(&mut self, key: u16) -> TickOut {
            let down = self.m.tick_with(&input(key, quiet(), self.conn), &self.w);
            self.cmds.extend(down.cmd);
            let up = self.m.tick_with(&input(0, quiet(), self.conn), &self.w);
            self.cmds.extend(up.cmd);
            down
        }

        fn rows(&self) -> MenuRows {
            let mut r = MenuRows::new();
            self.m
                .render_with(self.conn, &quiet(), 1_000_000, &mut r, &self.w);
            r
        }

        fn text(&self) -> Vec<String> {
            let r = self.rows();
            r.lines[..r.len]
                .iter()
                .map(|l| l.as_str().to_string())
                .collect()
        }

        fn marked(&self) -> Vec<String> {
            self.text()
                .into_iter()
                .filter(|l| l.starts_with("> "))
                .collect()
        }
    }

    fn row_line(r: &WatchRow) -> String {
        let cut = |s: &str| s.chars().take(12).collect::<String>();
        format!(
            "{:<12} vs {:<12}  {:>5}",
            cut(r.p1.as_str()),
            cut(r.p2.as_str()),
            format!("{}/{}", r.watchers, r.cap)
        )
    }

    #[test]
    fn the_watch_random_item_opens_the_list_without_a_command_and_back_leaves_it() {
        let mut d = ListDriver::new(&[wrow(1, "Alice", "Bob", 3, 10)]);
        assert_eq!(d.cmds, vec![], "一覧を開くだけでは指示を出さない");
        assert!(d.m.is_active() && d.m.is_open() && !d.m.wants_text());
        let t = d.text();
        assert_eq!(
            t[0],
            format!("{}{}{}", T_SPECTATE.ja, T_WATCH_DASH.ja, T_RANDOM.ja)
        );
        assert_eq!(*t.last().unwrap(), T_KEYS_WATCH.ja);
        // キャンセルで元のメニューへ（閉じない。観戦のランダムマッチの項目にカーソルが残る）。
        d.tap(X);
        assert!(!d.m.wants_watch_list() && d.m.is_active());
        assert_eq!(d.cmds, vec![]);
        let mut rows = MenuRows::new();
        d.m.render(Conn::Waiting, &quiet(), 0, &mut rows);
        let t: Vec<&str> = rows.lines[..rows.len].iter().map(LineBuf::as_str).collect();
        assert!(t.contains(&format!("> {}", T_RANDOM.ja).as_str()), "{t:?}");
        // 「もどる」の項目を決めても元のメニューへ。
        let mut d = ListDriver::new(&[]);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.tap(Z);
        assert!(!d.m.wants_watch_list() && d.m.is_active());
        assert_eq!(d.cmds, vec![]);
    }

    #[test]
    fn a_list_row_reads_names_versus_names_and_the_watcher_count() {
        let rows = [
            wrow(1, "Alice", "Bob", 3, 10),
            wrow(2, "Carol-12345678", "Dave", 0, 10),
            wrow(3, "Erin", "Frank", 10, 10),
        ];
        let d = ListDriver::new(&rows);
        let t = d.text();
        assert_eq!(t[1], format!("> {}", row_line(&rows[0])));
        // 名前の欄は 12 文字まで（越える分は切る）。
        assert_eq!(t[2], format!("  {}", row_line(&rows[1])));
        assert!(t[2].starts_with("  Carol-123456 vs Dave"), "{:?}", t[2]);
        assert_eq!(t[3], format!("  {}", row_line(&rows[2])));
        assert!(t[3].ends_with("10/10") && t[1].ends_with(" 3/10"), "{t:?}");
        let r = d.rows();
        assert_eq!(
            r.styles[1..4],
            [RowStyle::Selected, RowStyle::Normal, RowStyle::Hint],
            "満員の行は灰色"
        );
    }

    #[test]
    fn eight_rows_show_and_the_rest_scroll_with_more_lines_above_and_below() {
        let rows: Vec<WatchRow> = (0..12)
            .map(|i| wrow(i + 1, &format!("P{i}"), &format!("Q{i}"), 0, 10))
            .collect();
        let mut d = ListDriver::new(&rows);
        let shown = |d: &ListDriver| -> Vec<String> {
            d.text()
                .into_iter()
                .filter(|l| l.contains(" vs "))
                .map(|l| l[2..].split_whitespace().next().unwrap().to_string())
                .collect()
        };
        assert_eq!(shown(&d), ["P0", "P1", "P2", "P3", "P4", "P5", "P6", "P7"]);
        let t = d.text();
        assert_eq!(
            t[0],
            format!("{}{}{}", T_SPECTATE.ja, T_WATCH_DASH.ja, T_RANDOM.ja)
        );
        assert_eq!(t[1], "", "先頭では上の行は空");
        assert_eq!(t[10], "  ▼ ほか 4 件", "{t:?}");
        let len_top = d.rows().len;
        // 8 行目を越えて下へ: 先頭が送られ、上に ▲ が出る。
        for _ in 0..8 {
            d.tap(DOWN);
        }
        assert_eq!(shown(&d), ["P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8"]);
        let t = d.text();
        assert_eq!(t[1], "  ▲ ほか 1 件");
        assert_eq!(t[10], "  ▼ ほか 3 件");
        assert_eq!(
            d.rows().len,
            len_top,
            "スクロールしても行数（枠の高さ）が変わらない"
        );
        // 一番下まで進むと「もどる」。最後の行が見える。
        for _ in 0..4 {
            d.tap(DOWN);
        }
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        assert_eq!(shown(&d)[7], "P11");
        let t = d.text();
        assert_eq!(t[1], "  ▲ ほか 4 件");
        assert_eq!(t[10], "");
        // 折り返して先頭へ・先最初から上で「もどる」へ。
        d.tap(DOWN);
        assert_eq!(d.marked(), [format!("> {}", row_line(&rows[0]))]);
        assert_eq!(shown(&d)[0], "P0");
        d.tap(UP);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        // 8 行以下なら「ほか」の行は出ない。
        let d = ListDriver::new(&rows[..8]);
        let t = d.text();
        assert!(!t.iter().any(|l| l.contains("ほか")), "{t:?}");
        assert_eq!(d.rows().len, len_top - 2);
    }

    #[test]
    fn deciding_a_row_sends_that_row_and_leaves_the_list() {
        let rows = [
            wrow(1, "Alice", "Bob", 3, 10),
            wrow(2, "Carol", "Dave", 0, 10),
        ];
        let mut d = ListDriver::new(&rows);
        d.tap(DOWN);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::SpectateListed(rows[1])]);
        assert!(!d.m.wants_watch_list() && d.m.is_active());
        let mut d = ListDriver::new(&rows);
        d.tap(Z);
        assert_eq!(
            d.cmds,
            vec![Cmd::SpectateListed(rows[0])],
            "最初に決めれば最初の行"
        );
    }

    #[test]
    fn full_rows_are_greyed_skipped_and_never_chosen() {
        let rows = [
            wrow(1, "Full-A", "Full-B", 10, 10),
            wrow(2, "Open-A", "Open-B", 9, 10),
            wrow(3, "Full-C", "Full-D", 1, 1),
            wrow(4, "Open-C", "Open-D", 0, 10),
        ];
        let mut d = ListDriver::new(&rows);
        assert_eq!(
            d.marked(),
            [format!("> {}", row_line(&rows[1]))],
            "最初は選べる最初の行"
        );
        d.tap(DOWN);
        assert_eq!(
            d.marked(),
            [format!("> {}", row_line(&rows[3]))],
            "満員の行を飛ばす"
        );
        d.tap(DOWN);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.tap(DOWN);
        assert_eq!(
            d.marked(),
            [format!("> {}", row_line(&rows[1]))],
            "折り返しでも満員の先頭を飛ばす"
        );
        d.tap(UP);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.tap(UP);
        assert_eq!(d.marked(), [format!("> {}", row_line(&rows[3]))]);
        d.tap(UP);
        assert_eq!(
            d.marked(),
            [format!("> {}", row_line(&rows[1]))],
            "満員の行を飛ばして戻る"
        );
        assert_eq!(d.cmds, vec![]);
        // 全部満員なら、カーソルは「もどる」で、決定は一覧を出るだけ。
        let all_full = [wrow(1, "A", "B", 10, 10), wrow(2, "C", "D", 5, 5)];
        let mut d = ListDriver::new(&all_full);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.tap(DOWN);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)], "動ける所が無い");
        d.tap(Z);
        assert_eq!(d.cmds, vec![], "満員の行を観戦する指示が出た");
        assert!(!d.m.wants_watch_list());
    }

    #[test]
    fn a_row_that_fills_up_under_the_cursor_cannot_be_decided() {
        let rows = [
            wrow(1, "Alice", "Bob", 9, 10),
            wrow(2, "Carol", "Dave", 0, 10),
        ];
        let mut d = ListDriver::new(&rows);
        // 動かした後の（行を覚えた）カーソルは、その行が満員になっても動かない。
        d.tap(DOWN);
        d.tap(UP);
        assert_eq!(d.marked(), [format!("> {}", row_line(&rows[0]))]);
        let mut full = rows;
        full[0].watchers = 10;
        d.w.set(ListStatus::Ready, &full);
        assert_eq!(d.marked(), [format!("> {}", row_line(&full[0]))]);
        d.tap(Z);
        assert_eq!(d.cmds, vec![], "満員になった行を観戦する指示が出た");
        assert!(d.m.wants_watch_list(), "一覧を出てしまった");
        // 満員の行の上からも、↓ で次の選べる行へ動ける。
        d.tap(DOWN);
        assert_eq!(d.marked(), [format!("> {}", row_line(&full[1]))]);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::SpectateListed(full[1])]);
    }

    #[test]
    fn the_cursor_keeps_its_row_when_the_list_changes_and_never_slides_onto_another() {
        let rows = [
            wrow(1, "Alice", "Bob", 0, 10),
            wrow(2, "Carol", "Dave", 0, 10),
            wrow(3, "Erin", "Frank", 0, 10),
        ];
        let mut d = ListDriver::new(&rows);
        d.tap(DOWN);
        d.tap(DOWN);
        assert_eq!(d.marked(), [format!("> {}", row_line(&rows[2]))]);
        // 上の行が消えても、カーソルは同じ行（行の番号が 2 → 1 に変わる）。
        d.w.set(ListStatus::Ready, &rows[1..]);
        assert_eq!(d.marked(), [format!("> {}", row_line(&rows[2]))]);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::SpectateListed(rows[2])]);
        // 行の入れ替わり（先頭に新しい行が割り込む）でも同じ。
        let mut d = ListDriver::new(&rows);
        d.tap(DOWN);
        let fresh = wrow(9, "New", "One", 0, 10);
        d.w.set(ListStatus::Ready, &[fresh, rows[0], rows[1], rows[2]]);
        assert_eq!(d.marked(), [format!("> {}", row_line(&rows[1]))]);
        // 選んでいた行そのものが消えたら、別の行を指したままにせず、選べる最初の行へ戻る（動きが見える）。
        d.w.set(ListStatus::Ready, &[fresh, rows[0], rows[2]]);
        assert_eq!(d.marked(), [format!("> {}", row_line(&fresh))]);
        d.tap(Z);
        assert_eq!(d.cmds, vec![Cmd::SpectateListed(fresh)]);
        // 動かしていない間は、最初に来た行ではなく「選べる最初の行」を指し続ける。
        let mut d = ListDriver::with(Lang::Ja, snap(ListStatus::Loading, &[]));
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.w.set(ListStatus::Ready, &rows);
        assert_eq!(d.marked(), [format!("> {}", row_line(&rows[0]))]);
        // 「もどる」へ動かしたら、一覧が変わっても「もどる」に留まる。
        d.tap(UP);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.w.set(ListStatus::Ready, &[fresh, rows[0]]);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
    }

    #[test]
    fn an_empty_loading_and_failed_list_say_so_and_offer_only_back() {
        for (status, text, style) in [
            (ListStatus::Ready, T_WATCH_EMPTY, RowStyle::Hint),
            (ListStatus::Loading, T_WATCH_LOADING, RowStyle::Hint),
            (ListStatus::Failed, T_WATCH_FAILED, RowStyle::Warn),
        ] {
            for lang in Lang::ALL {
                let d = ListDriver::with(lang, snap(status, &[]));
                let t = d.text();
                assert_eq!(t[1], format!("  {}", text.get(lang)), "{status:?} {lang:?}");
                assert_eq!(d.rows().styles[1], style);
                assert_eq!(d.marked(), [format!("> {}", T_BACK.get(lang))]);
                assert_eq!(*t.last().unwrap(), T_KEYS_WATCH.get(lang));
            }
        }
        // 空の一覧で ↑↓ しても「もどる」のまま。決定すると出る。
        let mut d = ListDriver::new(&[]);
        d.tap(DOWN);
        d.tap(UP);
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.tap(Z);
        assert!(!d.m.wants_watch_list());
        assert_eq!(d.cmds, vec![]);
    }

    #[test]
    fn the_english_list_screen_uses_the_english_texts() {
        let rows: Vec<WatchRow> = (0..10)
            .map(|i| wrow(i + 1, "Alice", "Bob", 1, 10))
            .collect();
        let d = ListDriver::with(Lang::En, snap(ListStatus::Ready, &rows));
        let t = d.text();
        assert_eq!(t[0], "Watch ─ Random match");
        assert!(t.contains(&"  ▼ 2 more".to_string()), "{t:?}");
        assert_eq!(
            *t.last().unwrap(),
            "↑↓ Move   Confirm: Watch   Cancel: Back"
        );
        assert!(d.marked()[0].starts_with("> Alice"));
        let e = ListDriver::with(Lang::En, snap(ListStatus::Ready, &[]));
        assert_eq!(e.text()[1], "  There are no random matches to watch");
    }

    #[test]
    fn the_list_screen_ends_when_the_link_is_made_and_a_hundred_rows_scroll_to_the_end() {
        let rows = [wrow(1, "Alice", "Bob", 0, 10)];
        let mut d = ListDriver::new(&rows);
        d.conn = connected();
        d.tap(Z);
        assert_eq!(d.cmds, vec![], "つながった後に観戦の指示が出た");
        assert!(!d.m.wants_watch_list());
        let many: Vec<WatchRow> = (0..100)
            .map(|i| wrow(i + 1, "Alice", "Bob", i % 10, 10))
            .collect();
        let mut d = ListDriver::new(&many);
        // 満員でない行（10 行に 1 行は 0/10…9/10 のうち 9/10 まで。満員なのは「1/1」ではなく 10/10 だけ）を全部たどって「もどる」へ。
        let selectable = many.iter().filter(|r| !r.is_full()).count();
        for _ in 0..selectable {
            d.tap(DOWN);
            assert_eq!(d.marked().len(), 1);
        }
        assert_eq!(d.marked(), [format!("> {}", T_BACK.ja)]);
        d.tap(DOWN);
        assert_eq!(d.marked(), [format!("> {}", row_line(&many[0]))]);
    }

    #[test]
    fn every_list_screen_row_fits_the_box_and_the_screen() {
        let spec = AtlasSpec::fit(7, 14, &ui_glyphs()).unwrap();
        let game = game_font_spec("");
        let limit = 640
            - 2 * crate::ui_layout::SCREEN_MARGIN
            - 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        let longest = "W".repeat(32);
        let rows: Vec<WatchRow> = (0..40)
            .map(|i| wrow(i + 1, &longest, &longest, 10, 10))
            .collect();
        let mut tallest = 0;
        for lang in Lang::ALL {
            for (status, list) in [
                (ListStatus::Ready, &rows[..]),
                (ListStatus::Ready, &rows[..3]),
                (ListStatus::Ready, &rows[..0]),
                (ListStatus::Loading, &rows[..0]),
                (ListStatus::Failed, &rows[..0]),
            ] {
                let mut d = ListDriver::with(lang, snap(status, list));
                for step in 0..6 {
                    let r = d.rows();
                    assert!(r.len <= MENU_ROWS);
                    tallest = tallest.max(crate::ui_layout::rows_height(&spec, &r.styles[..r.len]));
                    for l in &r.lines[..r.len] {
                        assert!(l.as_str().chars().count() <= crate::overlay::MAX_TEXT);
                        for s in [&spec, &game] {
                            let w = crate::ui_layout::text_width(s, l.as_str());
                            assert!(
                                w <= limit,
                                "{lang:?} {step}: {w} 画素 > {limit}: {}",
                                l.as_str()
                            );
                        }
                    }
                    d.tap(DOWN);
                }
            }
        }
        let h = tallest + 2 * (crate::ui_layout::BOX_MARGIN + crate::ui_layout::BORDER);
        assert!(
            h <= 480 - 2 * crate::ui_layout::SCREEN_MARGIN,
            "枠の高さ {h} 画素"
        );
    }
}
