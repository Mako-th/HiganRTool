//! ローダーのウィンドウ（`status.txt` の「状態」の行）に出す文の日本語・英語（pure）。
//!
//! 接続コードを受け取った人が読む文で、内部の語を使わず、次にすることを 1 つ添える。言語は呼び手が `ui_lang::current()` で渡す。
//! 本ログの文はここに無い（日本語のまま）。数や名前を挟む文は関数にして、語順を言語ごとに組む。
#![forbid(unsafe_code)]

use higanr_core::code::span_text_in;
use higanr_core::lang::{Lang, Tx};

/// 準備の中の誤り（こちらの作りの誤りで、人の手では直らない）。詳しくは本ログ。
pub const INTERNAL: Tx = Tx {
    ja: "接続の準備に失敗しました（詳細はログを確認してください）。ゲームを閉じて起動し直してください。",
    en: "Failed to prepare the connection (details in the log). Close the game and start it again.",
};

/// 相手のコードの文字の誤り（途中で切れた・コピー違えた）。
pub const BROKEN: Tx = Tx {
    ja: "相手のコードが欠けているか、間違っているようです。全部をコピーし直して、このウィンドウに貼り直してください。",
    en: "The opponent's code is incomplete or wrong. Copy the whole code again and paste it into this window.",
};

/// ネットワークを開けなかった（見張りを起こす前に断った・見張りが止められた）ときの文。
pub const NOT_STARTED: Tx = Tx {
    ja: "通信を始められませんでした（理由はログにあります）。logsフォルダのファイルを添えて、GitHubのIssuesで知らせてください。",
    en: "Could not start networking (the reason is in the log). Please report this on GitHub Issues with the files in the logs folder.",
};

pub const PEER_UNREADABLE: Tx = Tx {
    ja: "相手のコードを読めませんでした（文字の形式が違います）。このウィンドウに貼り直してください。",
    en: "Could not read the opponent's code (wrong text format). Paste it into this window again.",
};

pub const PEER_EMPTY: Tx = Tx {
    ja: "相手のコードが空でした。相手から届いたコードを貼ってください。",
    en: "The opponent's code was empty. Paste the code from your opponent.",
};

pub const OWN_CODE: Tx = Tx {
    ja: "貼られたのは自分の接続コードです。相手から届いたコードを貼ってください。",
    en: "You pasted your own connection code. Paste the code from your opponent.",
};

pub const BOTH_OFFER: Tx = Tx {
    ja: "2人とも「始める側」の設定です。どちらか一方がHiganRTool.iniのcode_sideをanswerにして、起動し直してください。",
    en: "Both players are set to the offer side. One of you: set code_side to answer in HiganRTool.ini and start the game again.",
};

pub const BOTH_ANSWER: Tx = Tx {
    ja: "2人とも「返す側」の設定です。どちらか一方がHiganRTool.iniのcode_sideをofferにして、起動し直してください。",
    en: "Both players are set to the answer side. One of you: set code_side to offer in HiganRTool.ini and start the game again.",
};

pub const WRONG_SESSION: Tx = Tx {
    ja: "これは前回使用した返答コードです。上に出ている今回の自分のコードを相手に送り直してください。",
    en: "This is the reply code from last time. Send your current code (shown above) to your opponent again.",
};

pub const WRONG_KIND: Tx = Tx {
    ja: "相手のコードが今回のものと合いません。相手に今のコードを送り直してもらってください。",
    en: "The opponent's code does not match this session. Ask your opponent to send the current code again.",
};

pub const ANSWER_WAITING: Tx = Tx {
    ja: "相手から届くコードを待っています。",
    en: "Waiting for the code from your opponent.",
};

pub const ENDED_UNCONNECTED: Tx = Tx {
    ja: "相手と接続できないまま終わりました。もう一度試すときは、2人ともゲームを閉じて起動し直してください。",
    en: "Ended without connecting. To try again, both players close the game and start it again.",
};

pub const WATCH_UNREADABLE: Tx = Tx {
    ja: "観戦用のコードを読めませんでした（文字の形式が違います）。このウィンドウに貼り直してください。",
    en: "Could not read the watch code (wrong text format). Paste it into this window again.",
};

pub const WATCH_BROKEN: Tx = Tx {
    ja: "観戦用のコードが欠けているか、間違っているようです。全部をコピーし直して、このウィンドウに貼り直してください。",
    en: "The watch code is incomplete or wrong. Copy the whole code again and paste it into this window.",
};

pub const WATCH_EMPTY: Tx = Tx {
    ja: "観戦用のコードが空でした。ホストから届いたコードを貼ってください。",
    en: "The watch code was empty. Paste the code from the host.",
};

pub const WATCH_NOT_MATCH_CODE: Tx = Tx {
    ja: "これは対戦用のコードです。観戦するには、ホストから届いた「観戦用」のコードを貼ってください。",
    en: "This is a match code. To watch, paste the watch code from the host.",
};

pub const WATCH_WAITING: Tx = Tx {
    ja: "ホストから届いた観戦用のコードを待っています。",
    en: "Waiting for the watch code from the host.",
};

pub const WATCH_CONNECTED: Tx = Tx {
    ja: "接続しました。ホストの試合を観戦します。",
    en: "Connected. Watching the host's match.",
};

pub const WATCH_ENDED_UNCONNECTED: Tx = Tx {
    ja: "ホストと接続できないまま終わりました。もう一度試すときは、ゲームを閉じて起動し直してください。",
    en: "Ended without connecting to the host. To try again, close the game and start it again.",
};

/// ウィンドウで選んだ rollback を採れなかったときに「状態」の行へ足す文（`NetSettings` の ini が delay 方式）。
pub const ROLLBACK_INI_DELAY: Tx = Tx {
    ja: "ウィンドウで選んだrollbackは使えません（設定ファイルがdelay方式になっているため）。delay方式で接続します。",
    en: "The rollback you chose in the window cannot be used (the setting file says delay mode). Connecting in delay mode.",
};

/// 同じく、この起動で rollback の準備ができなかったとき。
pub const ROLLBACK_NOT_READY: Tx = Tx {
    ja: "ウィンドウで選んだrollbackは使えません（この起動ではrollbackの準備ができなかったため。理由はログにあります）。delay方式で接続します。",
    en: "The rollback you chose in the window cannot be used (rollback was not ready in this start; the reason is in the log). Connecting in delay mode.",
};

/// 相手のコードの期限切れ。長さは `ttl_secs` から作る（数字を字に写すと、期限を変えたときにウィンドウの文だけ古いままになる）。
pub fn expired(lang: Lang, ttl_secs: u64) -> String {
    let span = span_text_in(lang, ttl_secs);
    match lang {
        Lang::Ja => format!(
            "相手のコードは期限（{span}）が切れています。相手に新しいコードを生成してもらってください。"
        ),
        Lang::En => {
            format!("The opponent's code has expired ({span}). Ask your opponent for a new code.")
        }
    }
}

/// 観戦用のコードの期限切れ。
pub fn watch_expired(lang: Lang, ttl_secs: u64) -> String {
    let span = span_text_in(lang, ttl_secs);
    match lang {
        Lang::Ja => format!(
            "観戦用のコードは期限（{span}）が切れています。ホストに新しいコードを生成してもらってください。"
        ),
        Lang::En => format!("The watch code has expired ({span}). Ask the host for a new code."),
    }
}

/// 始める側が返しのコードを待つ間。
pub fn offer_waiting(lang: Lang, ttl_secs: u64) -> String {
    let span = span_text_in(lang, ttl_secs);
    match lang {
        Lang::Ja => format!(
            "相手から返ってくるコードを待っています（自分のコードは{span}で使えなくなります）。"
        ),
        Lang::En => {
            format!("Waiting for the reply code from your opponent (your code expires in {span}).")
        }
    }
}

/// 待つ長さが切れた。
pub fn gave_up(lang: Lang, secs: u64) -> String {
    let span = span_text_in(lang, secs);
    match lang {
        Lang::Ja => format!(
            "{span}待っても使えるコードが届かなかったため、待機をやめました。もう一度試すときは、ゲームを閉じて起動し直してください。"
        ),
        Lang::En => format!(
            "Stopped waiting: no usable code arrived in {span}. To try again, close the game and start it again."
        ),
    }
}

/// 始める側が返しのコードを読んだ。
pub fn reply_read(lang: Lang, who: &str) -> String {
    match lang {
        Lang::Ja => format!("相手「{who}」の返答コードを読みました。接続しています。"),
        Lang::En => format!("Read the reply code from \"{who}\". Connecting."),
    }
}

/// 返す側が始める側のコードを読んだ。
pub fn offer_read(lang: Lang, who: &str) -> String {
    match lang {
        Lang::Ja => {
            format!("相手「{who}」のコードを読みました。上に出た返答コードを相手に送ってください。")
        }
        Lang::En => format!(
            "Read the code from \"{who}\". Send the reply code shown above to your opponent."
        ),
    }
}

/// 観戦者が観戦用のコードを読んだ。
pub fn watch_read(lang: Lang, who: &str) -> String {
    match lang {
        Lang::Ja => format!("ホスト「{who}」の観戦用のコードを読みました。接続しています。"),
        Lang::En => format!("Read the watch code from host \"{who}\". Connecting."),
    }
}

/// 今の設定では相手へつなぎに行けない（ネットワークの範囲 `scope` の外）。アドレスはウィンドウに出さない。
pub fn no_route(lang: Lang, scope: &str) -> String {
    match lang {
        Lang::Ja => format!(
            "今の設定（scope = {scope}）では相手に接続できません。同じ家の中の相手ならHiganRTool.iniのscopeをlanに、インターネットの向こうの相手ならinternetにして、起動し直してください。"
        ),
        Lang::En => format!(
            "Cannot reach the opponent with the current settings (scope = {scope}). For an opponent on your LAN, set scope to lan in HiganRTool.ini; for one over the internet, set it to internet. Then start the game again."
        ),
    }
}

/// つながった。席は画面から読めない値なので出す。形は繋がったセッションの AGREE で決まった形（モードと遅れ）。
pub fn connected(
    lang: Lang,
    seat: Option<higanr_core::wire::Seat>,
    shape: Option<(higanr_core::wire::Mode, u8)>,
) -> String {
    use higanr_core::wire::{Mode, Seat};
    let who = match (lang, seat) {
        (Lang::Ja, Some(Seat::P1)) => "接続しました。あなたは1Pです。",
        (Lang::Ja, Some(Seat::P2)) => "接続しました。あなたは2Pです。",
        (Lang::Ja, None) => "接続しました。",
        (Lang::En, Some(Seat::P1)) => "Connected. You are 1P.",
        (Lang::En, Some(Seat::P2)) => "Connected. You are 2P.",
        (Lang::En, None) => "Connected.",
    };
    match (lang, shape) {
        (Lang::Ja, Some((Mode::Rollback, d))) => {
            format!("{who}（rollback・ディレイ {d} フレーム）")
        }
        (Lang::Ja, Some((Mode::Delay, d))) => format!("{who}（delay方式・ディレイ {d} フレーム）"),
        (Lang::En, Some((Mode::Rollback, d))) => format!("{who} (rollback, delay {d})"),
        (Lang::En, Some((Mode::Delay, d))) => format!("{who} (delay mode, delay {d})"),
        (_, None) => who.to_string(),
    }
}

/// 相手のコードを受け取るファイルを読めなかった。`what` は本ログと同じ日本語の文（パスと OS の誤り）で、
/// 英語のウィンドウには出さない（日本語が混ざるため。詳細は本ログで読める）。
pub fn peer_file_unreadable(lang: Lang, what: &str) -> String {
    match lang {
        Lang::Ja => format!(
            "相手のコードを受け取るファイルを読めませんでした（{what}）。このウィンドウに貼り直してください。"
        ),
        Lang::En => "Could not read the file for the opponent's code (details in the log). Paste it into this window again.".to_string(),
    }
}

/// `what` の扱いは `peer_file_unreadable` と同じ。
pub fn watch_file_unreadable(lang: Lang, what: &str) -> String {
    match lang {
        Lang::Ja => format!(
            "観戦用のコードを受け取るファイルを読めませんでした（{what}）。このウィンドウに貼り直してください。"
        ),
        Lang::En => "Could not read the file for the watch code (details in the log). Paste it into this window again.".to_string(),
    }
}

/// 返すコードをファイルに書けなかった。`what` の扱いは `peer_file_unreadable` と同じ。
pub fn reply_not_written(lang: Lang, what: &str) -> String {
    match lang {
        Lang::Ja => format!(
            "返答コードをファイルに書けませんでした（{what}）。ゲームを閉じて起動し直してください。"
        ),
        Lang::En => "Could not write the reply code to the file (details in the log). Close the game and start it again.".to_string(),
    }
}

/// 終わりの文に足す、自分の接続コードのファイルを消せなかった知らせ。`what` の扱いは `peer_file_unreadable` と同じ。
pub fn my_code_not_deleted(lang: Lang, shown: &str, what: &str) -> String {
    match lang {
        Lang::Ja => format!("{shown}（自分の接続コードのファイルを消せませんでした: {what}）"),
        Lang::En => {
            format!("{shown} (could not delete your connection code file; details in the log)")
        }
    }
}

/// `status.txt` の見出しの行。ローダーは最後の行だけを「状態」に出す。
pub fn status_header(lang: Lang, side: Side, secs: f32) -> String {
    match lang {
        Lang::Ja => {
            let side = match side {
                Side::Offer => "始める側（offer）",
                Side::Answer => "返す側（answer）",
                Side::Spectate => "見るだけの側（spectate）",
                Side::Game => "ゲームの中のメニュー（game）",
            };
            format!("HiganRToolの接続コード：{side}。開いてから{secs:.1}秒の時点:")
        }
        Lang::En => {
            let side = match side {
                Side::Offer => "offer side",
                Side::Answer => "answer side",
                Side::Spectate => "watch side (spectate)",
                Side::Game => "in-game menu (game)",
            };
            format!("HiganRTool connection code ({side}). {secs:.1} s after start:")
        }
    }
}

/// `status_header` の側（`net_config::CodeSide` を、この module が持たなくてよいようにコピーしたもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Offer,
    Answer,
    Spectate,
    Game,
}

#[cfg(test)]
mod tests {
    use super::*;
    use higanr_core::lang::has_japanese;

    /// 呼び元（`net_code` / `spectate_watch_code`）が渡す `what` の実物の形: パスと OS の誤りの文（日本語）。
    const WHAT: &str = "peer_code.txt を開けない: アクセスが拒否されました。 (os error 5)";

    const ALL: [Tx; 21] = [
        INTERNAL,
        BROKEN,
        NOT_STARTED,
        PEER_UNREADABLE,
        PEER_EMPTY,
        OWN_CODE,
        BOTH_OFFER,
        BOTH_ANSWER,
        WRONG_SESSION,
        WRONG_KIND,
        ANSWER_WAITING,
        ENDED_UNCONNECTED,
        WATCH_UNREADABLE,
        WATCH_EMPTY,
        WATCH_NOT_MATCH_CODE,
        WATCH_WAITING,
        WATCH_CONNECTED,
        WATCH_ENDED_UNCONNECTED,
        ROLLBACK_INI_DELAY,
        ROLLBACK_NOT_READY,
        WATCH_BROKEN,
    ];

    #[test]
    fn every_fixed_text_has_an_english_without_japanese() {
        for t in ALL {
            assert!(has_japanese(t.ja), "日本語に日本語が無い: {}", t.ja);
            assert!(!t.en.is_empty() && !has_japanese(t.en), "{}", t.en);
        }
    }

    /// 壊れたら落ちる: 公開した後の利用者に「この道具を渡してくれた人」へ送るよう案内する（報告先は GitHub の Issues）。
    #[test]
    fn the_failure_text_points_at_github_issues_not_at_whoever_handed_over_the_tool() {
        assert!(
            NOT_STARTED.ja.contains("GitHubのIssues") && NOT_STARTED.ja.contains("logsフォルダ")
        );
        assert!(NOT_STARTED.en.contains("GitHub Issues") && NOT_STARTED.en.contains("logs folder"));
        for t in [NOT_STARTED.ja, NOT_STARTED.en] {
            assert!(!t.contains("渡して") && !t.contains("gave you"), "{t}");
        }
    }

    #[test]
    fn formatted_texts_differ_by_language_and_keep_the_values() {
        let who = "Bob";
        let list: [(String, String); 13] = [
            (expired(Lang::Ja, 1800), expired(Lang::En, 1800)),
            (watch_expired(Lang::Ja, 1800), watch_expired(Lang::En, 1800)),
            (offer_waiting(Lang::Ja, 1800), offer_waiting(Lang::En, 1800)),
            (gave_up(Lang::Ja, 600), gave_up(Lang::En, 600)),
            (reply_read(Lang::Ja, who), reply_read(Lang::En, who)),
            (offer_read(Lang::Ja, who), offer_read(Lang::En, who)),
            (watch_read(Lang::Ja, who), watch_read(Lang::En, who)),
            (no_route(Lang::Ja, "lan"), no_route(Lang::En, "lan")),
            (
                connected(Lang::Ja, None, None),
                connected(Lang::En, None, None),
            ),
            (
                peer_file_unreadable(Lang::Ja, WHAT),
                peer_file_unreadable(Lang::En, WHAT),
            ),
            (
                watch_file_unreadable(Lang::Ja, WHAT),
                watch_file_unreadable(Lang::En, WHAT),
            ),
            (
                reply_not_written(Lang::Ja, WHAT),
                reply_not_written(Lang::En, WHAT),
            ),
            (
                my_code_not_deleted(Lang::Ja, "文", WHAT),
                my_code_not_deleted(Lang::En, "text", WHAT),
            ),
        ];
        for (ja, en) in &list {
            assert!(has_japanese(ja), "{ja}");
            assert!(!has_japanese(en), "{en}");
        }
        assert!(expired(Lang::En, 1800).contains("30 min"));
        assert!(expired(Lang::Ja, 1800).contains("30分"));
        assert!(gave_up(Lang::En, 45).contains("45 s"));
        assert!(reply_read(Lang::En, who).contains("\"Bob\""));
        assert!(no_route(Lang::En, "lan").contains("scope = lan"));
    }

    #[test]
    fn the_os_error_text_is_shown_in_japanese_only_and_the_log_holds_it_for_english() {
        let ja = [
            peer_file_unreadable(Lang::Ja, WHAT),
            watch_file_unreadable(Lang::Ja, WHAT),
            reply_not_written(Lang::Ja, WHAT),
            my_code_not_deleted(Lang::Ja, "文", WHAT),
        ];
        let en = [
            peer_file_unreadable(Lang::En, WHAT),
            watch_file_unreadable(Lang::En, WHAT),
            reply_not_written(Lang::En, WHAT),
            my_code_not_deleted(Lang::En, "text", WHAT),
        ];
        for s in &ja {
            assert!(s.contains(WHAT), "{s}");
        }
        for s in &en {
            assert!(!s.contains("os error") && !s.contains("peer_code"), "{s}");
            assert!(s.contains("details in the log"), "{s}");
        }
    }

    #[test]
    fn connected_names_the_seat_and_the_agreed_shape_in_each_language() {
        use higanr_core::wire::{Mode, Seat};
        assert_eq!(
            connected(Lang::En, Some(Seat::P1), Some((Mode::Rollback, 2))),
            "Connected. You are 1P. (rollback, delay 2)"
        );
        assert_eq!(
            connected(Lang::En, Some(Seat::P2), Some((Mode::Delay, 1))),
            "Connected. You are 2P. (delay mode, delay 1)"
        );
        assert_eq!(connected(Lang::En, None, None), "Connected.");
        assert_eq!(
            connected(Lang::Ja, Some(Seat::P1), Some((Mode::Rollback, 2))),
            "接続しました。あなたは1Pです。（rollback・ディレイ 2 フレーム）"
        );
    }

    #[test]
    fn the_status_header_is_localised_and_keeps_the_seconds() {
        for lang in Lang::ALL {
            for side in [Side::Offer, Side::Answer, Side::Spectate, Side::Game] {
                let h = status_header(lang, side, 1.25);
                assert!(h.contains("1.2") || h.contains("1.3"), "{h}");
                assert_eq!(has_japanese(&h), lang == Lang::Ja, "{h}");
            }
        }
    }
}
