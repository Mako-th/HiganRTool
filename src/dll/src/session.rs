//! 読み取りのスレッドの 1 回の実行（pure。unsafe 無し）: 照合（`probe::run`）→ 設置の条件 → ウィンドウのアドレスとコピーの表の確かめ →
//! 1 地点目のフックの設置（`hook::install_call_hook`）→ 回数と tick ごとの記録の計測（`observe::run`）。
//!
//! 同定・image base・表の検査・14 地点の照合が**全部**通ったときだけ設置する（1 地点でも合わなければ何も書かない）。
//! 条件の判定を Win32 の外に置き、偽物で「欠けたら設置の呼び出しが 0 回」を見る。
#![forbid(unsafe_code)]

use std::io::Write;

use higanr_core::lang::LangSetting;
use higanr_core::patch::AddrRange;
use higanr_core::script::{Script, parse};

use crate::alloc_count::AllocStep;
use crate::copies::{check_copy_table, row_text};
use crate::detour::{self, DetourEnv};
use crate::hook;
use crate::log_mode::detail_out;
use crate::net_config::{self, AfterScript, INI_FILE, NetSettings, SECTION};
use crate::observe::{self, RecordEnv, RunLen, SCRIPT_MAX_SECONDS, SCRIPT_TAIL_SECONDS};
use crate::pacing::Installed;
use crate::probe::{self, Env, Summary};
use crate::sites::{SiteDef, SiteOutcome, hex_bytes, judge};
use crate::th09_addr::{
    FEED_SITE, LOCAL_INPUT_SITE, MATCH_START_SITE, POLL_SITE, POLL_SITES, PORTRAIT_MARKER_SITE,
    PORTRAIT_SITES, PORTRAIT_SLIDE_BACK_SITE, PORTRAIT_SLIDE_SITE, SET_STATE_SITE, WINDOW_DEADLINE,
    WINDOW_NOW, WINDOW_SPAN,
};
use crate::ticks::CopyRow;
use crate::trial_summary::{self, GameFacts, PaceEnd, PaceKind, Pacing, Setup, Stage, Wrote};

/// 台本のインターフェース（台本のファイルと、ゲームのスレッドが使う静的変数）。本物は `script_win::RealScriptHost`。呼ぶ順は `run` が決める:
/// 読む → 移す → 置き場を確保 → 台本の状態と関数のアドレスを置く → 2 つのフックを書く → 撮影を有効に → 分岐を開く。
pub trait ScriptHost {
    fn read_script(&mut self) -> Result<Option<Vec<u8>>, String>;
    /// `script.txt` を本ログの隣の `<回の名前>_script.txt` へ上書きせずに移す。移した先の名前。
    fn move_script(&mut self) -> Result<String, String>;
    /// 撮影の置き場 4 枚を確保し、デバイスと present parameters のアドレスを置く（撮影はまだ有効にしない）。
    fn prepare_shots(&mut self) -> Result<(), String>;
    fn arm(&mut self, script: Script, feed_fn: u32, local_fn: u32) -> Result<(), String>;
    /// 台本の無いネットワーク実行（M2-5）: アドレスと関数のアドレスだけを置き、台本の状態は空にする（分かれ道はまだ開かない）。
    fn arm_io(&mut self, _feed_fn: u32, _local_fn: u32) -> Result<(), String> {
        Err("台本の無い走りの口が無い".to_string())
    }
    fn shots_on(&mut self);
    fn start(&mut self) -> Result<(), String>;

    // ---- ネットワーク（既定は「ネットワークなし」。本物は `script_win::RealScriptHost`）----
    /// DLL のフォルダの `HiganRTool.ini`（ローダーの `--ini` が渡したパスがあればそれ）を読む（移さない）。無ければ `Ok(None)`。
    fn read_net_ini(&mut self) -> Result<Option<String>, String> {
        Ok(None)
    }
    /// ini の置き場の文（本ログの「ini: …」の行）。
    fn ini_origin(&self) -> String {
        format!("DLL のフォルダの {INI_FILE}")
    }
    /// ローダーの `--script` が渡した台本なら、その置き場の文（`Some` なら読んだ後に移さない）。
    fn explicit_script(&self) -> Option<String> {
        None
    }
    /// ネットワークを開いてセッションを置く（ソケット・受信スレッド・入力の地点の判断。まだ動かさない）。開いた受信ソケットの文。
    fn arm_net(&mut self, _s: &NetSettings, _set_state_fn: u32) -> Result<String, String> {
        Err("ネットワークの実装が無い".to_string())
    }
    fn start_net(&mut self) -> Result<(), String> {
        Err("ネットワークの実装が無い".to_string())
    }
    /// 立ち絵の 3 本の関数の入口（`th09_addr::PORTRAIT_SITES` の順）を渡す。3 本のバイト列を全部照合して合ったときだけ呼ばれる。
    /// 既定は「置かない」（立ち絵は追わないだけで、選択は動く）。
    fn arm_portrait(&mut self, _fns: [u32; 3]) {}
    /// 素の poll `0x0042B850` の入口（`th09_addr::POLL_SITES`）を渡す。先頭 8 バイトを照合して合ったときだけ呼ばれる。
    /// 既定は「置かない」（ローカル入力は今までの `0x0042BE40` で読む）。
    fn arm_poll(&mut self, _addr: u32) {}
    /// rollback のモードの入口（確保・解放・calloc・走査の call・Present の後の入口）を書く。ini で rollback を望む実行だけ
    /// （望まなければ 1 バイトも書かない）。**ネットワークのセッションを置く前に**呼ぶ: HELLO で rollback を名乗るかは入口を全部書けたかで決まる
    /// （書けなかったのに名乗ると、相手が予測で進めるのにこちらは待つ）。書けたら true。既定は「入れない」。
    fn arm_rollback(
        &mut self,
        _s: &NetSettings,
        _hook_env: &mut dyn DetourEnv,
        _forbidden: &[AddrRange],
        _out: &mut dyn Write,
    ) -> bool {
        false
    }

    // ---- 確保・解放の数え（既定は「数えない」）----
    /// `Arm` は入力のフックを書いた後・台本の分岐を開く前（入口がゲームのスレッドの ID を tick の記録から取るため）、
    /// `Finish` は計測の後。
    fn alloc_count(&mut self, _step: AllocStep<'_>, _out: &mut dyn Write) {}

    // ---- ペースと時刻のインターフェース（既定は「無し」）----
    /// ペースと時刻のインターフェース（`pacing::install` から `pacing::finish` まで `run` が持つ）。
    fn pacing_host(&mut self) -> Option<Box<dyn crate::pacing::PacingHost>> {
        None
    }

    // ---- 文字の描画（既定は「無し」）----
    /// 文字の描画のインターフェース（`overlay::install` から `overlay::finish` まで `run` が持つ）。
    fn overlay_host(&mut self) -> Option<Box<dyn crate::overlay::OverlayHost>> {
        None
    }

    // ---- リプレイの自動保存（既定は「入れない」）----
    /// ini の `[replay]` を読み、`enable = 1` なら自動保存を入れる。入れたらその文、設定に無ければ `Ok(None)`。
    fn arm_replay(&mut self, _net: Option<&NetSettings>) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// メニューで決めて覚えた名前（ゲーム内のメニューの実行だけ）。あれば `load_net` が ini の `name` より優先して使う
    /// （自動保存の自分の名前にも同じ値が渡る）。
    fn saved_name(&mut self, _s: &NetSettings) -> Option<String> {
        None
    }

    /// OS の UI の言語（LANGID）。既定は日本語。
    fn os_langid(&mut self) -> u16 {
        0x0411
    }
}

/// 画面の言語を決めて置く（ini の `[ui] lang`、`auto` なら OS の UI の言語）。ini の `[ui]` の誤りは本ログに出して自動で続ける。
fn decide_lang(host: &mut dyn ScriptHost, text: &str, out: &mut dyn Write) {
    let setting = match higanr_core::ini::parse(text)
        .map_err(|e| e.to_string())
        .and_then(|i| higanr_core::lang::setting_in_ini(&i))
    {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(
                out,
                "言語: {INI_FILE} の [ui] を読めない（{e}）。OS の言語に従う"
            );
            LangSetting::Auto
        }
    };
    let lang = higanr_core::lang::resolve(setting, host.os_langid());
    crate::ui_lang::set(lang);
    let from = match setting {
        LangSetting::Fixed(_) => "ini の [ui] lang",
        LangSetting::Auto => "OS の UI の言語",
    };
    let _ = writeln!(detail_out(out), "言語: {}（{from}）", lang.word());
}

/// ネットワークの設定を読む（ini が無い・`[netplay]` が無い・`enable = 0` なら `None`）。読めなければログに書いて `None`
/// （ネットワークなしで走る）。
fn load_net(host: &mut dyn ScriptHost, out: &mut dyn Write) -> Option<NetSettings> {
    let _ = writeln!(detail_out(out), "ini: {}", host.ini_origin());
    let text = match host.read_net_ini() {
        Ok(None) => {
            let _ = writeln!(
                out,
                "ネットワーク: {INI_FILE} が無い（ネットワークなしで走る）"
            );
            return None;
        }
        Ok(Some(t)) => t,
        Err(why) => {
            let _ = writeln!(
                out,
                "ネットワーク: {INI_FILE} を読めない（{why}）。ネットワークなしで走る"
            );
            return None;
        }
    };
    match net_config::from_text(&text) {
        Ok(None) => {
            let _ = writeln!(
                out,
                "ネットワーク: {INI_FILE} に [{SECTION}] が無いか enable = 1 でない（ネットワークなしで走る）"
            );
            None
        }
        Ok(Some(mut s)) => {
            for note in s.old_key_notes() {
                let _ = writeln!(out, "ネットワーク: {note}");
            }
            // メニューで決めて覚えた名前は ini の `name` より優先する（ネットワーク・自動保存の自分の名前の全部が同じ値を使う）。
            if let Some(saved) = host.saved_name(&s) {
                let _ = writeln!(
                    detail_out(out),
                    "ネットワーク: 名前は覚えたファイルの値を使う（ini の name より優先）"
                );
                s.name = saved;
            }
            decide_lang(host, &text, out);
            let _ = writeln!(
                detail_out(out),
                "ネットワーク: 設定を読んだ（{}）",
                s.text()
            );
            Some(s)
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "ネットワーク: {INI_FILE} の [{SECTION}] を読めない（{e}）。ネットワークなしで走る"
            );
            None
        }
    }
}

/// 開始のフックを書き、ネットワークのセッションを置く（まだ動かさない）。どちらかができなければ Err（ネットワークなしで走る）。開始のフックが書けていれば
/// 残るが、`NET_ON` が 0 の間は回数を数えて元の飛び先へ飛ぶだけ。書けたかは `start_hook` へ置く。
fn install_net(
    hook_env: &mut dyn DetourEnv,
    host: &mut dyn ScriptHost,
    sites: &[SiteDef],
    forbidden: &[AddrRange],
    s: &NetSettings,
    start_hook: &mut Wrote,
    out: &mut dyn Write,
) -> Result<(), String> {
    let find = |name: &str| sites.iter().find(|d| d.name == name);
    let (Some(mdef), Some(sdef)) = (find(MATCH_START_SITE), find(SET_STATE_SITE)) else {
        let _ = writeln!(
            out,
            "ネットワーク: 表に {MATCH_START_SITE} か {SET_STATE_SITE} の行が無いので開かない"
        );
        return Err(format!(
            "表に {MATCH_START_SITE} か {SET_STATE_SITE} の行が無い"
        ));
    };
    let r = hook::install_match_hook(hook_env, mdef);
    hook::write_report(out, mdef, &r);
    if !r.written() {
        *start_hook = Wrote::Refused;
        let _ = writeln!(out, "ネットワーク: 開始のフックが書けなかったので開かない");
        return Err("開始のフックを書けなかった".to_string());
    }
    *start_hook = Wrote::Written;
    // rollback のモードの入口。ネットワークのセッション（HELLO で名乗るモード）より先に置く。
    host.arm_rollback(s, hook_env, forbidden, out);
    match host.arm_net(s, sdef.addr) {
        Ok(desc) => {
            // 観戦の準備の失敗が混ざっているときは簡潔にも残す。
            let o: &mut dyn Write = if crate::spectate_host::note_has_problem(&desc) {
                out
            } else {
                &mut detail_out(out)
            };
            let _ = writeln!(
                o,
                "ネットワーク: {desc}。set_state 0x{:08X}。動かすのは分かれ道を開く直前",
                sdef.addr
            );
            Ok(())
        }
        Err(why) => {
            let _ = writeln!(
                out,
                "ネットワーク: 開けない（{why}）。ネットワークなしで走る"
            );
            Err(why)
        }
    }
}

/// 立ち絵の 3 本の関数の入口を照合し、全部合ったときだけ入口へ渡す（ネットワークのセッションを置いた後、ネットワークを動かす前）。1 本でも合わない・
/// 読めないときは何も渡さず理由をログに書く（立ち絵を追わないだけで、選択は今までどおり動く）。別の表（`PORTRAIT_SITES`）に
/// するのは、`SITES` は 1 行でも合わなければ何も書かない表で、ここで止めたいのは立ち絵だけだから。
fn arm_portrait(env: &mut dyn Env, host: &mut dyn ScriptHost, out: &mut dyn Write) {
    let mut fns = [0u32; 3];
    for (slot, name) in fns.iter_mut().zip([
        PORTRAIT_SLIDE_SITE,
        PORTRAIT_SLIDE_BACK_SITE,
        PORTRAIT_MARKER_SITE,
    ]) {
        let Some(def) = PORTRAIT_SITES.iter().find(|d| d.name == name) else {
            let _ = writeln!(out, "立ち絵の追随: 入れない（表に {name} の行が無い）");
            return;
        };
        let actual = env.read(def.addr, def.expected.len());
        match judge(def, actual.as_deref()) {
            SiteOutcome::Match => *slot = def.addr,
            SiteOutcome::Mismatch { actual } => {
                let _ = writeln!(
                    out,
                    "立ち絵の追随: 入れない（{name} 0x{:08X} が不一致 実際 = {} / 期待 = {}）。選択そのものは今までどおり",
                    def.addr,
                    hex_bytes(&actual),
                    hex_bytes(def.expected)
                );
                return;
            }
            SiteOutcome::Unreadable => {
                let _ = writeln!(
                    out,
                    "立ち絵の追随: 入れない（{name} 0x{:08X} が読めない）。選択そのものは今までどおり",
                    def.addr
                );
                return;
            }
        }
    }
    host.arm_portrait(fns);
    let _ = writeln!(
        detail_out(out),
        "立ち絵の追随: 入れた（決定の tick にカーソルを書いたとき、立ち絵 0x{:08X} / 0x{:08X} と席の目印 0x{:08X} を呼ぶ。3 本の入口のバイト列が一致）",
        fns[0],
        fns[1],
        fns[2]
    );
}

/// 素の poll の入口を照合し、合ったときだけ入口へ渡す（ネットワークのセッションを置いた後、ネットワークを動かす前）。合わない・読めないときは何も渡さず
/// 理由をログに書く（ローカル入力は今までの `0x0042BE40` で読む。ネットワークは開いたまま: 各自が自分の入力ワードを読んで送るので同期には効かない）。
/// 別の表（`POLL_SITES`）にする理由は `th09_addr::POLL_SITES` の注。
fn arm_poll(env: &mut dyn Env, host: &mut dyn ScriptHost, out: &mut dyn Write) {
    let Some(def) = POLL_SITES.iter().find(|d| d.name == POLL_SITE) else {
        let _ = writeln!(
            out,
            "poll: 使わない（表に {POLL_SITE} の行が無い）。ローカル入力は 0x0042BE40"
        );
        return;
    };
    let actual = env.read(def.addr, def.expected.len());
    match judge(def, actual.as_deref()) {
        SiteOutcome::Match => {
            host.arm_poll(def.addr);
            let _ = writeln!(
                detail_out(out),
                "poll: 使う（0x{:08X} の先頭 {} バイトが一致）。自分の入力は 1P の設定どおりに、席 0 の poll で読む",
                def.addr,
                def.expected.len()
            );
        }
        SiteOutcome::Mismatch { actual } => {
            let _ = writeln!(
                out,
                "poll: 使わない（0x{:08X} が不一致 実際 = {} / 期待 = {}）。ローカル入力は 0x0042BE40",
                def.addr,
                hex_bytes(&actual),
                hex_bytes(def.expected)
            );
        }
        SiteOutcome::Unreadable => {
            let _ = writeln!(
                out,
                "poll: 使わない（0x{:08X} が読めない）。ローカル入力は 0x0042BE40",
                def.addr
            );
        }
    }
}

/// 台本を読み、移し、読めたら返す。移せなければ使わない。UTF-8 でない・誤りなら使わない（台本なしで走る）。本文と読んだ結果を
/// ログに書く。移すのは中身を確かめる前（読めた台本は、誤りでも 1 回で片付け、残った台本が後の実行で押さないようにする）。
/// ローダーの `--script` が渡した台本は移さない。
pub fn load_script(host: &mut dyn ScriptHost, out: &mut dyn Write) -> Option<Script> {
    let bytes = match host.read_script() {
        Ok(None) => {
            let _ = writeln!(detail_out(out), "台本: script.txt が無い（台本なしで走る）");
            return None;
        }
        Ok(Some(b)) => b,
        Err(why) => {
            let _ = writeln!(out, "台本: script.txt を読めない（{why}）。台本なしで走る");
            return None;
        }
    };
    // ローダーが渡した台本は移さない（2 つのゲームがそれぞれのコピーを持てるように）。
    if let Some(from) = host.explicit_script() {
        let _ = writeln!(
            detail_out(out),
            "台本: {from}（{} バイト）を読んだ（ローダーが渡した台本なので移さない）",
            bytes.len()
        );
    } else {
        match host.move_script() {
            Ok(name) => {
                let _ = writeln!(
                    detail_out(out),
                    "台本: script.txt（{} バイト）を読み、{name} へ移した",
                    bytes.len()
                );
            }
            Err(why) => {
                let _ = writeln!(
                    out,
                    "台本: script.txt を移せない（{why}）ので使わない（台本なしで走る）"
                );
                return None;
            }
        }
    }
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(
                out,
                "台本: UTF-8 でない（先頭から {} バイトの後）ので使わない（台本なしで走る）",
                e.utf8_error().valid_up_to()
            );
            return None;
        }
    };
    // 先頭の BOM（メモ帳が付けることがある）は読まない。
    let body = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    let _ = writeln!(
        detail_out(out),
        "台本の本文（{} 行）:",
        body.lines().count()
    );
    for (i, l) in body.lines().enumerate() {
        let _ = writeln!(detail_out(out), "  {:>4}| {l}", i + 1);
    }
    match parse(body) {
        Ok(s) => {
            let _ = writeln!(
                detail_out(out),
                "台本: 読めた（{} 段、settle = {} tick）",
                s.steps.len(),
                s.settle
            );
            Some(s)
        }
        Err(e) => {
            let _ = writeln!(out, "台本: 読めない（{e}）。台本なしで走る");
            None
        }
    }
}

/// 台本を置く（設置の前）。置けたら台本の段階の数と、撮影の置き場を確保できたかを返す。
fn arm_script(
    host: &mut dyn ScriptHost,
    sites: &[SiteDef],
    out: &mut dyn Write,
) -> Option<(usize, bool)> {
    let script = load_script(host, out)?;
    let steps = script.steps.len();
    let addr_of = |name: &str| sites.iter().find(|d| d.name == name).map(|d| d.addr);
    let (Some(feed), Some(local)) = (addr_of(FEED_SITE), addr_of(LOCAL_INPUT_SITE)) else {
        let _ = writeln!(
            out,
            "台本: 表に {FEED_SITE} か {LOCAL_INPUT_SITE} の行が無いので使わない"
        );
        return None;
    };
    let shots = match host.prepare_shots() {
        Ok(()) => {
            let _ = writeln!(detail_out(out), "撮影: 置き場 4 枚を確保した");
            true
        }
        Err(why) => {
            let _ = writeln!(
                out,
                "撮影: 置き場を用意できない（{why}）。台本は動かすが撮影はしない"
            );
            false
        }
    };
    if let Err(why) = host.arm(script, feed, local) {
        let _ = writeln!(out, "台本: 置けない（{why}）ので使わない");
        return None;
    }
    let _ = writeln!(
        detail_out(out),
        "台本: 箱とアドレスを置いた（feed 0x{feed:08X} / ローカル入力 0x{local:08X}）。分かれ道は 2 つのフックを書いた後に開く"
    );
    Some((steps, shots))
}

/// 台本の無いネットワーク実行: 入力の入口が読むアドレスと呼ぶ関数のアドレスだけを置く。置けなければ Err（ネットワークを開かない）。
fn arm_io(host: &mut dyn ScriptHost, sites: &[SiteDef], out: &mut dyn Write) -> Result<(), String> {
    let addr_of = |name: &str| sites.iter().find(|d| d.name == name).map(|d| d.addr);
    let (Some(feed), Some(local)) = (addr_of(FEED_SITE), addr_of(LOCAL_INPUT_SITE)) else {
        let _ = writeln!(
            out,
            "ネットワーク: 表に {FEED_SITE} か {LOCAL_INPUT_SITE} の行が無いので開かない"
        );
        return Err(format!("表に {FEED_SITE} か {LOCAL_INPUT_SITE} の行が無い"));
    };
    match host.arm_io(feed, local) {
        Ok(()) => {
            let _ = writeln!(
                detail_out(out),
                "ネットワーク: 台本なしで開く（M2-5。自分の語はローカル入力 0x{local:08X}、選択の画面は人の押下を送る）。アドレスを置いた（feed 0x{feed:08X}）"
            );
            Ok(())
        }
        Err(why) => {
            let _ = writeln!(out, "ネットワーク: アドレスを置けない（{why}）ので開かない");
            Err(format!("アドレスを置けない（{why}）"))
        }
    }
}

/// 分岐を開く直前の、ネットワークと自動保存（台本の有無によらず同じ順）。ネットワークを動かせたら true。動かせなかったこと・自動保存の入り方は、
/// 試験のまとめの材料 `game` へ置く。
fn start_net_and_replay(
    host: &mut dyn ScriptHost,
    net: Option<&NetSettings>,
    game: &mut GameFacts,
    out: &mut dyn Write,
) -> bool {
    // ネットワークは分岐を開く直前に動かす（開始のフックは `NET_ON` を、入力の入口は `SCRIPT_ON` を見る）。
    let mut net_on = false;
    if game.net == Setup::On {
        match host.start_net() {
            Ok(()) => {
                net_on = true;
                let _ = writeln!(
                    detail_out(out),
                    "ネットワーク: 動かした（NET_ON = 1）。繋がったら止める地点で SELECT を送り合い、試合は lockstep で進む"
                );
            }
            Err(why) => {
                let _ = writeln!(out, "ネットワーク: 動かせない（{why}）");
                game.net = Setup::Failed(format!("動かせなかった（{why}）"));
            }
        }
    }
    // リプレイの自動保存（ini の `[replay]` に `enable = 1` があるときだけ。分岐を開く前に入れる）。
    match host.arm_replay(net) {
        Ok(Some(desc)) => {
            let _ = writeln!(detail_out(out), "自動保存: {desc}");
            game.replay = Setup::On;
        }
        Ok(None) => {
            let _ = writeln!(
                detail_out(out),
                "自動保存: 入れない（ini の [replay] に enable = 1 が無い）"
            );
            game.replay = Setup::NotConfigured;
        }
        Err(why) => {
            let _ = writeln!(out, "自動保存: 入れられない（{why}）");
            game.replay = Setup::Failed(why);
        }
    }
    net_on
}

/// 試験のまとめの終わりの節を本ログへ書く（`session::run` の全部の出口で、本ログの最後の行の前に 1 回）。
fn write_final_trial(
    out: &mut dyn Write,
    rec: &mut dyn RecordEnv,
    game: &GameFacts,
    pace_end: Option<PaceEnd>,
) {
    let net = rec.net_status().map(|(s, _)| s);
    let facts = observe::trial_facts(game, rec, net, pace_end, Stage::Final);
    let _ = write!(out, "{}", trial_summary::render(&facts));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// 何も書いていない。
    NotInstalled,
    /// rel32 を書いた（フックは残る）。
    Installed,
}

/// 設置の条件。照合の集計から判定する: 同定・image base・表の検査が通ったときだけ `checked_sites` が true になり、そのうえで
/// 表の全行を読んで全部一致したか。
pub fn ready_to_install(s: &Summary, sites: &[SiteDef]) -> Result<(), &'static str> {
    if !s.checked_sites {
        return Err("同定・image base・表の検査のどれかが通っていない");
    }
    if sites.is_empty() || s.total != sites.len() {
        return Err("表の全行を照合していない");
    }
    if s.matched != s.total || s.mismatched != 0 || s.unreadable != 0 {
        return Err("一致しない地点がある");
    }
    Ok(())
}

/// ウィンドウのアドレスを、入口に読ませてよいかを確かめる（入口は `+0x2C` と `+0x3C` の 8 バイトずつを読む）。
/// 読めなければ 0 を返し（入口はウィンドウを読まずに 0 を書く）、その旨をログに書く。読めれば今の値も書く。
fn checked_window(env: &mut dyn Env, window: u32, out: &mut dyn Write) -> u32 {
    if window == 0 {
        let _ = writeln!(
            out,
            "ウィンドウ: アドレスが 0 なので、ウィンドウの 2 欄は 0 を書く"
        );
        return 0;
    }
    let Some(b) = env.read(window, WINDOW_SPAN as usize) else {
        let _ = writeln!(
            out,
            "ウィンドウ: 0x{window:08X} から 0x{WINDOW_SPAN:X} バイトが読めないので、入口にはウィンドウを読ませない（ウィンドウの 2 欄は 0 を書く）"
        );
        return 0;
    };
    let f = |off: u32| {
        let o = off as usize;
        b.get(o..o + 8)
            .and_then(|s| s.try_into().ok())
            .map(f64::from_le_bytes)
    };
    let _ = writeln!(
        detail_out(out),
        "ウィンドウ: 0x{window:08X}（設置の前の値: 今の時刻 +0x{WINDOW_NOW:X} = {:?} / 期限 +0x{WINDOW_DEADLINE:X} = {:?}）",
        f(WINDOW_NOW),
        f(WINDOW_DEADLINE)
    );
    window
}

/// コピーの表を、入口に使わせてよいかを確かめる。合わなければ 0 行を返し（入口はコピーしない）、ログに大きく書く。フックの設置そのものは
/// 続ける（tick ごとの始まりはコピーが無くても取れる）。
fn checked_copies(env: &mut dyn Env, rows: &[CopyRow], out: &mut dyn Write) -> Vec<CopyRow> {
    match check_copy_table(rows, |a, len| env.read(a, len as usize).is_some()) {
        Ok(()) => {
            let _ = writeln!(
                detail_out(out),
                "コピーの表: {} 行（合計 {} バイト）が枠に収まり、元が読めることを設置の前に確かめた: {}",
                rows.len(),
                rows.iter().map(|r| r.len).sum::<u32>(),
                rows.iter().map(row_text).collect::<Vec<_>>().join(" / ")
            );
            rows.to_vec()
        }
        Err(e) => {
            let _ = writeln!(
                out,
                "★★★ コピーの表を置かない（{}）。入口は何も写さない（件数 0）。_ticks.csv のコピーの列と _changes.csv は空になる。フックの設置は続ける ★★★",
                e.text()
            );
            Vec::new()
        }
    }
}

/// 実行に渡す地点の名前。
#[derive(Debug, Clone, Copy)]
pub struct Sites<'a> {
    /// call 先の差し替えの行（入力と待ち。`th09_addr::HOOK_SITE`）。
    pub hook_site: &'a str,
    /// inline detour で書く描画の行（`draw_before_endscene`）。
    pub draw_site: &'a str,
    /// 描画だけのフレームの出口の行（照合だけ。`render_only_exit`）。一致していればそのアドレスを入口に置く。
    pub exit_site: &'a str,
}

/// 描画だけのフレームの出口のアドレスを決める（`exit_site` 行があり、照合が全部通っていれば そのアドレス。無ければ 0）。
/// ここに来る前に `ready_to_install` が全地点の一致を確かめているので、行があれば一致している。
fn render_only_exit(sites: &[SiteDef], exit_site: &str, out: &mut dyn Write) -> u32 {
    match sites.iter().find(|d| d.name == exit_site) {
        Some(d) => {
            let _ = writeln!(
                detail_out(out),
                "描画だけのフレームの出口: 0x{:08X}（{} が照合で一致。入口に置く）",
                d.addr,
                exit_site
            );
            d.addr
        }
        None => {
            let _ = writeln!(
                out,
                "描画だけのフレームの出口: 置かない（表に {exit_site} の行が無い。入口は分かれ道を通らない）"
            );
            0
        }
    }
}

/// 1 回の実行。ログの行を `out` に書く（書けなくても続ける）。`sites_used` は使う地点の名前、`window` は入口が tick ごとの記録で
/// 読むウィンドウのアドレス、`copies` は入口が枠へコピーする表、`seconds` は計測の長さ。
///
/// call 先の差し替え（入力と待ち）を書いた後に、描画の地点へ inline detour を書く。どちらも書けたら計測（`observe::run`）へ進む。
/// 描画の detour が書けなくても call のフックは残る（描画のリングバッファが空になるだけ）。設置の条件が通ったら台本（`script`）を読み、置く。
/// 2 つのフックを書いた**後で**撮影を有効にし（detour が書けて置き場を確保できたときだけ）、分岐を開く。call 先の差し替えが
/// 断られたら開かない。台本のある実行は台本が終わるか止まってから 10 秒で計測を終える（上限 `SCRIPT_MAX_SECONDS`）。
/// 設置の条件が通らない実行（花映塚でない・自己検査）では `script.txt` に触らない。
///
/// ini に `[netplay] enable = 1` があれば、台本が無くてもネットワークを開く（`arm_io` → 開始のフック → ネットワークのセッション → ネットワークを動かす → 自動保存 →
/// 分かれ道）。長さは `RunLen::Net`（ネットワークが終わってから 10 秒。上限は無い: 人が長く遊ぶと本ログが途中で止まるため）。台本があって
/// `after_script = keep` なら、台本の後もネットワークを閉じず、長さも `RunLen::Net`（こちらは台本の実行と同じ上限のまま）。
#[allow(clippy::too_many_arguments)] // 実行の入力を 1 か所で渡す
pub fn run(
    env: &mut dyn Env,
    hook_env: &mut dyn DetourEnv,
    rec: &mut dyn RecordEnv,
    script: &mut dyn ScriptHost,
    sites: &[SiteDef],
    forbidden: &[AddrRange],
    expected_image_base: u32,
    sites_used: Sites<'_>,
    window: u32,
    copies: &[CopyRow],
    seconds: u32,
    out: &mut dyn Write,
) -> End {
    let summary = probe::run(env, sites, forbidden, expected_image_base, out);
    // 試験のまとめの材料（設置までに分かったこと）。
    let mut game = GameFacts {
        ident: summary.ident,
        sites_checked: summary.checked_sites,
        sites_total: summary.total,
        sites_matched: summary.matched,
        sites_bad: summary.bad_sites.clone(),
        ..GameFacts::default()
    };
    if let Err(why) = ready_to_install(&summary, sites) {
        let _ = writeln!(out, "フックの設置: しない（{why}）");
        let _ = writeln!(
            detail_out(out),
            "台本: 設置しないので script.txt には触らない"
        );
        write_final_trial(out, rec, &game, None);
        return End::NotInstalled;
    }
    let Some(def) = sites.iter().find(|d| d.name == sites_used.hook_site) else {
        let _ = writeln!(
            out,
            "フックの設置: しない（表に {} の行が無い）",
            sites_used.hook_site
        );
        // 書き換える行が表に無い（ビルドの誤り）。書いていないので、断ったのと同じく「フックの設置」は ×。
        game.input_hook = Wrote::Refused;
        write_final_trial(out, rec, &game, None);
        return End::NotInstalled;
    };
    let window = checked_window(env, window, out);
    let copies = checked_copies(env, copies, out);
    let exit = render_only_exit(sites, sites_used.exit_site, out);
    let armed = arm_script(script, sites, out);
    let net_settings = load_net(script, out);
    // 設置の前の番号から読む（本物では 0。設置と計測の始まりの間の件も落とさない）。
    let first_seq = rec.next_seq();
    let report = hook::install_call_hook(hook_env, def, window, &copies, exit);
    hook::write_report(out, def, &report);
    if !report.written() {
        if armed.is_some() {
            let _ = writeln!(
                out,
                "台本: フックの設置が断られたので動かさない（分かれ道は開かない）"
            );
        }
        game.input_hook = Wrote::Refused;
        write_final_trial(out, rec, &game, None);
        return End::NotInstalled;
    }
    game.input_hook = Wrote::Written;
    let mut draw_written = false;
    match sites.iter().find(|d| d.name == sites_used.draw_site) {
        Some(draw_def) => {
            let dr = detour::install_detour(hook_env, draw_def, forbidden);
            detour::write_report(out, draw_def, &dr);
            draw_written = dr.written();
            game.draw_hook = if draw_written {
                Wrote::Written
            } else {
                Wrote::Refused
            };
            if !dr.written() {
                let _ = writeln!(
                    out,
                    "inline detour: 描画の地点は書けなかった（描画のリングバッファは空・描画だけのフレームの集計は 0 件になる。計測は続ける）"
                );
            }
        }
        None => {
            let _ = writeln!(
                out,
                "inline detour: しない（表に {} の行が無い。描画のリングバッファは空のまま）",
                sites_used.draw_site
            );
        }
    }
    // 文字の描画（ini の [overlay] に enable = 1 があり、描画の detour が書けたときだけ）。
    let mut overlay = script.overlay_host();
    let overlay_on = overlay
        .as_deref_mut()
        .is_some_and(|o| crate::overlay::install(o, draw_written, out));
    // ペースと時刻のインターフェース（ini や [pacing] が無ければ既定の auto —— ウィンドウなら b-spin・全画面なら game。`mode = game` なら頭は書かない。
    // 書けなくても計測は続ける）。
    let mut pacing = script.pacing_host();
    if let Some(p) = pacing.as_deref_mut() {
        let pi = crate::pacing::install(hook_env, p, out);
        game.pacing = Pacing {
            kind: match pi.installed {
                Installed::Nothing => PaceKind::Nothing,
                Installed::Stamps => PaceKind::Stamps,
                Installed::Paced => PaceKind::Paced,
            },
            requested: pi.requested.map(|m| m.name()),
            used: pi.used.map(|m| m.name()),
        };
    }
    // ネットワーク: 開始のフックを書き、セッションを置く。台本があれば台本の状態の後に、無ければ入力の関数のアドレスだけを置いてから（台本の無い実行は、
    // 人が選択を押し、マッチの自分の入力ワードはローカル入力）。
    game.net = match &net_settings {
        None => Setup::NotConfigured,
        Some(s) => {
            let placed = if armed.is_some() {
                Ok(())
            } else {
                arm_io(script, sites, out)
            };
            match placed.and_then(|()| {
                install_net(
                    hook_env,
                    script,
                    sites,
                    forbidden,
                    s,
                    &mut game.start_hook,
                    out,
                )
            }) {
                Ok(()) => Setup::On,
                Err(why) => Setup::Failed(why),
            }
        }
    };
    // ネットワークのセッションを置けたら、立ち絵の 3 本の関数の入口と素の poll の入口を照合して渡す（ネットワークを動かす前。合わなければ立ち絵だけ追わない・
    // ローカル入力は今までの読み方）。台本だけの実行（ネットワークなし）のローカル入力は変えない。
    if game.net == Setup::On {
        arm_portrait(env, script, out);
        arm_poll(env, script, out);
    }
    // ini で有効なときだけ、確保・解放の入口 5 つに detour を書いて数え始める。
    script.alloc_count(
        AllocStep::Arm {
            hook_env: &mut *hook_env,
            forbidden,
        },
        out,
    );
    // ネットワークの設定がある実行の長さ: ネットワークが終わってから（台本があれば台本も終わってから）`SCRIPT_TAIL_SECONDS` 秒で終える。描画だけの
    // フレームの実験（`RunLen::Fixed`）は、ネットワークを開けなかったときもしない（人が遊ぶつもりの実行で、更新を飛ばすフレームを作らない）。
    // 上限（`max`）は台本のあるネットワーク実行（検査・実機の回）だけ。台本の無いネットワーク実行はネットワークが開いている間は上限で止めない。
    let net_len = |steps: usize, feed_first: u32, max: Option<u32>| RunLen::Net {
        tail: SCRIPT_TAIL_SECONDS,
        max,
        steps,
        feed_first,
    };
    let keep = net_settings
        .as_ref()
        .is_some_and(|s| s.after_script == AfterScript::Keep);
    let len = match armed {
        None if net_settings.is_none() => RunLen::Fixed(seconds),
        None => {
            // 台本の無いネットワーク実行: ネットワークを動かせたときだけ分岐を開く。
            let feed_first = rec.feed_next_seq();
            if start_net_and_replay(script, net_settings.as_ref(), &mut game, out) {
                match script.start() {
                    Ok(()) => {
                        let _ = writeln!(
                            detail_out(out),
                            "ネットワーク: 分かれ道を開いた（SCRIPT_ON = 1）。台本は無い。人の入力で選択を押し、試合をする"
                        );
                    }
                    Err(why) => {
                        let _ = writeln!(out, "ネットワーク: 分かれ道を開けない（{why}）");
                    }
                }
            } else {
                let _ = writeln!(
                    out,
                    "ネットワーク: 開けなかったので分かれ道は開かない（描画だけのフレームの実験もしない）"
                );
            }
            net_len(0, feed_first, None)
        }
        Some((steps, shots_ready)) => {
            if draw_written && shots_ready {
                script.shots_on();
                let _ = writeln!(detail_out(out), "撮影: 有効にした（描画の入口で写す）");
            } else {
                let _ = writeln!(
                    detail_out(out),
                    "撮影: しない（描画の detour を{}・置き場を{}）。台本は動かす",
                    if draw_written {
                        "書けた"
                    } else {
                        "書けなかった"
                    },
                    if shots_ready {
                        "確保した"
                    } else {
                        "確保できなかった"
                    }
                );
            }
            let net_on = start_net_and_replay(script, net_settings.as_ref(), &mut game, out);
            // 分岐を開く前の feed のリングバッファの番号（読み取りのスレッドはそこから読む）。
            let feed_first = rec.feed_next_seq();
            match script.start() {
                Ok(()) => {
                    let _ = writeln!(
                        detail_out(out),
                        "台本: 分かれ道を開いた（SCRIPT_ON = 1）。ここから台本で押し、人の入力はゲームに届かない{}",
                        if net_on && keep {
                            "（after_script = keep: 台本が最後の段まで押した後はネットワークを閉じず、人の入力で続く）"
                        } else {
                            ""
                        }
                    );
                    if net_on && keep {
                        net_len(steps, feed_first, Some(SCRIPT_MAX_SECONDS))
                    } else {
                        RunLen::Script {
                            tail: SCRIPT_TAIL_SECONDS,
                            max: SCRIPT_MAX_SECONDS,
                            steps,
                            feed_first,
                        }
                    }
                }
                Err(why) => {
                    let _ = writeln!(out, "台本: 分かれ道を開けない（{why}）。台本なしで計測する");
                    if net_settings.is_some() {
                        net_len(steps, feed_first, Some(SCRIPT_MAX_SECONDS))
                    } else {
                        RunLen::Fixed(seconds)
                    }
                }
            }
        }
    };
    observe::run_len(hook_env, rec, first_seq, out, len, &game);
    script.alloc_count(AllocStep::Finish, out);
    let pace_end = pacing
        .as_deref_mut()
        .and_then(|p| crate::pacing::finish(p, out));
    if let (true, Some(o)) = (overlay_on, overlay.as_deref_mut()) {
        crate::overlay::finish(o, out);
    }
    write_final_trial(out, rec, &game, pace_end);
    End::Installed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::tests::FakeHook;
    use crate::observe::tests::FakeRecord;
    use crate::probe::IdentOutcome;
    use crate::probe::tests::Fake;
    use crate::th09_addr::{FORBIDDEN, HOOK_SITE, IMAGE_BASE, SITES, WINDOW_ADDR};
    use crate::ticks::COPY_TABLE;
    use higanr_core::lang::Lang;
    use higanr_core::pe::{IdentField, IdentMismatch};

    fn with_copies(env: &mut Fake) {
        for r in COPY_TABLE {
            env.mem.insert(r.src, vec![0u8; r.len as usize]);
        }
    }

    fn with_window(env: &mut Fake) {
        let mut b = vec![0u8; WINDOW_SPAN as usize];
        b[WINDOW_NOW as usize..WINDOW_NOW as usize + 8].copy_from_slice(&1.5f64.to_le_bytes());
        b[WINDOW_DEADLINE as usize..WINDOW_DEADLINE as usize + 8]
            .copy_from_slice(&(1.5f64 + 1.0 / 60.0).to_le_bytes());
        env.mem.insert(WINDOW_ADDR, b);
    }

    fn fake_hook_for_real_row() -> FakeHook {
        let def = SITES.iter().find(|d| d.name == HOOK_SITE).unwrap();
        let mut h = FakeHook::new(2);
        h.mem_base = def.addr;
        h.mem[..5].copy_from_slice(def.expected);
        h.counter_fn = |now| (now / 1000 * 60) as u32;
        h
    }

    fn run_both(env: &mut Fake, h: &mut FakeHook, sites: &[SiteDef]) -> (End, String) {
        run_with(env, h, sites, &mut FakeScript::default())
    }

    /// 偽の台本のインターフェース。呼ばれた順を `events` に残す。
    #[derive(Default)]
    pub(crate) struct FakeScript {
        pub file: Option<Vec<u8>>,
        pub read_fail: bool,
        pub move_fail: bool,
        pub shots_fail: bool,
        pub arm_fail: bool,
        pub events: Vec<String>,
        pub armed: Option<(usize, u32, u32)>,
        pub ini: Option<String>,
        pub ini_reads: u32,
        pub net_arm_fail: bool,
        /// `arm_net` が返す文（None なら既定の合成の文）。
        pub net_desc: Option<String>,
        /// ネットワークのセッションを置いたときの設定と set_state のアドレス。
        pub net_armed: Option<(NetSettings, u32)>,
        /// ローダーが渡した台本・ini の置き場の文（2 台の走らせ方）。None なら既定（DLL のフォルダ）。
        pub explicit: Option<String>,
        pub ini_from: Option<String>,
        pub io_fail: bool,
        /// 台本なしで置いた関数のアドレス（feed, ローカル入力）。
        pub io_armed: Option<(u32, u32)>,
        pub net_start_fail: bool,
        pub portrait_armed: Option<[u32; 3]>,
        /// 素の poll の入口を渡されたとき（`events` には入れない）。
        pub poll_armed: Option<u32>,
        pub saved_name: Option<String>,
        /// OS の UI の言語（LANGID）。None なら日本語（0x0411）。
        pub os_langid: Option<u16>,
    }

    impl ScriptHost for FakeScript {
        fn saved_name(&mut self, _s: &NetSettings) -> Option<String> {
            self.saved_name.clone()
        }
        fn os_langid(&mut self) -> u16 {
            self.os_langid.unwrap_or(0x0411)
        }
        fn read_script(&mut self) -> Result<Option<Vec<u8>>, String> {
            self.events.push("read".to_string());
            if self.read_fail {
                return Err("合成の失敗".to_string());
            }
            Ok(self.file.clone())
        }
        fn move_script(&mut self) -> Result<String, String> {
            self.events.push("move".to_string());
            if self.move_fail {
                return Err("合成の失敗".to_string());
            }
            Ok("synthetic_script.txt".to_string())
        }
        fn prepare_shots(&mut self) -> Result<(), String> {
            self.events.push("prepare".to_string());
            if self.shots_fail {
                return Err("合成の失敗".to_string());
            }
            Ok(())
        }
        fn arm(&mut self, script: Script, feed_fn: u32, local_fn: u32) -> Result<(), String> {
            self.events.push("arm".to_string());
            if self.arm_fail {
                return Err("合成の失敗".to_string());
            }
            self.armed = Some((script.steps.len(), feed_fn, local_fn));
            Ok(())
        }
        fn arm_io(&mut self, feed_fn: u32, local_fn: u32) -> Result<(), String> {
            self.events.push("arm_io".to_string());
            if self.io_fail {
                return Err("合成の失敗".to_string());
            }
            self.io_armed = Some((feed_fn, local_fn));
            Ok(())
        }
        fn shots_on(&mut self) {
            self.events.push("shots_on".to_string());
        }
        fn start(&mut self) -> Result<(), String> {
            self.events.push("start".to_string());
            Ok(())
        }
        fn read_net_ini(&mut self) -> Result<Option<String>, String> {
            self.ini_reads += 1;
            Ok(self.ini.clone())
        }
        fn arm_net(&mut self, s: &NetSettings, set_state_fn: u32) -> Result<String, String> {
            self.events.push("arm_net".to_string());
            if self.net_arm_fail {
                return Err("合成の失敗".to_string());
            }
            self.net_armed = Some((s.clone(), set_state_fn));
            Ok(self.net_desc.clone().unwrap_or_else(|| {
                "受信ソケット 127.0.0.1:1 を開き、箱を置いた（合成）".to_string()
            }))
        }
        fn start_net(&mut self) -> Result<(), String> {
            self.events.push("start_net".to_string());
            if self.net_start_fail {
                return Err("合成の失敗".to_string());
            }
            Ok(())
        }
        fn arm_portrait(&mut self, fns: [u32; 3]) {
            self.events.push("arm_portrait".to_string());
            self.portrait_armed = Some(fns);
        }
        fn arm_poll(&mut self, addr: u32) {
            self.poll_armed = Some(addr);
        }
        fn ini_origin(&self) -> String {
            self.ini_from
                .clone()
                .unwrap_or_else(|| "DLL のフォルダの HiganRTool.ini（合成）".to_string())
        }
        fn explicit_script(&self) -> Option<String> {
            self.explicit.clone()
        }
    }

    fn run_with(
        env: &mut Fake,
        h: &mut FakeHook,
        sites: &[SiteDef],
        script: &mut FakeScript,
    ) -> (End, String) {
        let mut rec = FakeRecord::new(Vec::new(), 0);
        let mut out = Vec::new();
        let end = run(
            env,
            h,
            &mut rec,
            script,
            sites,
            FORBIDDEN,
            IMAGE_BASE,
            Sites {
                hook_site: HOOK_SITE,
                draw_site: "draw_before_endscene",
                exit_site: "render_only_exit",
            },
            WINDOW_ADDR,
            &COPY_TABLE,
            60,
            &mut out,
        );
        (end, String::from_utf8(out).unwrap())
    }

    #[test]
    fn installs_and_observes_when_everything_matches() {
        let def = SITES.iter().find(|d| d.name == HOOK_SITE).unwrap();
        let crate::sites::Use::CallRel32 { original_target } = def.kind else {
            panic!("{HOOK_SITE} が call 先の差し替えの行でない");
        };
        let mut env = Fake::all_match();
        with_window(&mut env);
        with_copies(&mut env);
        let mut h = fake_hook_for_real_row();
        let (end, log) = run_both(&mut env, &mut h, SITES);
        assert_eq!(end, End::Installed);
        assert_eq!(h.writes, 1);
        assert_eq!(h.set_original, Some(original_target));
        assert_eq!(h.set_window, Some(WINDOW_ADDR));
        assert_eq!(h.set_copies, Some(COPY_TABLE.to_vec()));
        assert!(
            log.contains(
                "コピーの表: 6 行（合計 548 バイト）が枠に収まり、元が読めることを設置の前に確かめた"
            ),
            "{log}"
        );
        assert_eq!(h.writes_before_setup, 0);
        assert_eq!(h.written_at, Some(def.addr + 1));
        assert!(
            log.contains(&format!(
                "ウィンドウ: 0x{WINDOW_ADDR:08X}（設置の前の値: 今の時刻 +0x2C = Some(1.5) / 期限 +0x3C = Some({:?})）",
                1.5f64 + 1.0 / 60.0
            )),
            "{log}"
        );
        let want = format!(
            "フックの設置: 地点 {HOOK_SITE} 0x{:08X}（call 先の差し替えの予定）の call の飛び先を 0x{original_target:08X} から入口 0x{:08X} へ",
            def.addr,
            crate::hook::tests::ENTRY
        );
        assert!(log.contains(&want), "{log}");
        assert!(log.contains("計測の集計: 60 回、合計の増分 3600"), "{log}");
        assert!(log.contains("tick の集計: 取れた 0 件"), "{log}");
        let head = log.rfind("==== 試験のまとめ ====\n").expect(&log);
        assert_eq!(log[head..].lines().count(), 15, "{}", &log[head..]);
        assert!(log.find("計測の集計: 60 回").unwrap() < head, "{log}");
        let block = &log[head..];
        assert!(
            block.contains("〇 花映塚のバージョン: 花映塚 1.50a と一致した"),
            "{block}"
        );
        assert!(block.contains("〇 フックの設置: "), "{block}");
        assert!(
            block.contains("－ ネットワークを開けたか: 対戦の通信を使わない設定"),
            "{block}"
        );
        assert!(
            block.contains("－ 自動保存: この実行では設定を読んでいない"),
            "{block}"
        );
        assert!(block.contains("〇 パニック: 受けていない"), "{block}");
        assert!(
            log.contains("==== 試験のまとめ（途中経過。計測 1 秒の時点） ===="),
            "1 秒目の途中経過が無い: {log}"
        );
    }

    #[test]
    fn the_summary_names_why_the_net_did_not_open() {
        let go = |hook: FakeHook, s: FakeScript| {
            let mut env = Fake::all_match();
            with_window(&mut env);
            with_copies(&mut env);
            let (mut h, mut s) = (hook, s);
            let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
            let head = log.rfind("==== 試験のまとめ ====\n").expect(&log);
            log[head..].to_string()
        };
        let block = go(
            fake_hook_both_writable(),
            FakeScript {
                ini: Some(NET_INI.to_string()),
                ..FakeScript::default()
            },
        );
        assert!(
            block.contains("× フックの設置: 14 地点が全部一致し、入力のフックは書いたが、試合の開始のフックを書けなかった"),
            "{block}"
        );
        assert!(
            block.contains("× ネットワークを開けたか: 開けなかった: 開始のフックを書けなかった"),
            "{block}"
        );
        assert!(
            block.contains("－ STUN の答え: 対戦の通信を開いていない"),
            "{block}"
        );
        assert!(
            block.contains("－ 繋がったか: 対戦の通信を使っていない"),
            "{block}"
        );
        let block = go(
            fake_hook_three_writable(),
            FakeScript {
                ini: Some(NET_INI.to_string()),
                net_arm_fail: true,
                ..FakeScript::default()
            },
        );
        assert!(block.contains("〇 フックの設置: "), "{block}");
        assert!(
            block.contains("× ネットワークを開けたか: 開けなかった: 合成の失敗"),
            "{block}"
        );
        let block = go(
            fake_hook_three_writable(),
            FakeScript {
                ini: Some(NET_INI.to_string()),
                net_start_fail: true,
                ..FakeScript::default()
            },
        );
        assert!(
            block.contains("× ネットワークを開けたか: 開けなかった: 動かせなかった（合成の失敗）"),
            "{block}"
        );
        let block = go(
            fake_hook_three_writable(),
            FakeScript {
                ini: Some(NET_INI.to_string()),
                ..FakeScript::default()
            },
        );
        assert!(
            block.contains("〇 ネットワークを開けたか: 受信ソケットを開いた"),
            "{block}"
        );
        assert!(
            block.contains("－ 繋がったか: 対戦の通信を使っていない"),
            "{block}"
        );
    }

    #[test]
    fn unreadable_copy_source_means_zero_rows() {
        let mut env = Fake::all_match();
        with_window(&mut env);
        with_copies(&mut env);
        env.mem.remove(&COPY_TABLE[4].src); // InputManager の 2 本目（4 バイト境界に無い元）
        let mut h = fake_hook_for_real_row();
        let (end, log) = run_both(&mut env, &mut h, SITES);
        assert_eq!(end, End::Installed);
        assert_eq!(h.set_copies, Some(Vec::new()));
        assert!(
            log.contains("★★★ コピーの表を置かない（5 行目の元の範囲が読めない）"),
            "{log}"
        );
    }

    #[test]
    fn unreadable_window_is_replaced_by_zero() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_for_real_row();
        let (end, log) = run_both(&mut env, &mut h, SITES);
        assert_eq!(end, End::Installed);
        assert_eq!(h.set_window, Some(0));
        assert!(
            log.contains(&format!(
                "ウィンドウ: 0x{WINDOW_ADDR:08X} から 0x44 バイトが読めないので、入口にはウィンドウを読ませない"
            )),
            "{log}"
        );
    }

    #[test]
    fn any_missing_condition_means_zero_install_calls() {
        type Breaker = fn(&mut Fake) -> Vec<SiteDef>;
        let cases: [(&str, Breaker, &str); 7] = [
            (
                "同定の不一致",
                |e| {
                    e.ident = IdentOutcome::Mismatch(IdentMismatch {
                        fields: vec![IdentField::TimeDateStamp],
                    });
                    SITES.to_vec()
                },
                "同定・image base・表の検査のどれかが通っていない",
            ),
            (
                "exe が読めない",
                |e| {
                    e.ident = IdentOutcome::Unreadable("合成".to_string());
                    SITES.to_vec()
                },
                "同定・image base・表の検査のどれかが通っていない",
            ),
            (
                "image base の違い",
                |e| {
                    e.base = 0x0100_0000;
                    SITES.to_vec()
                },
                "同定・image base・表の検査のどれかが通っていない",
            ),
            (
                "表の破損",
                |_| {
                    let mut t = SITES.to_vec();
                    t[1].addr = t[2].addr;
                    t
                },
                "同定・image base・表の検査のどれかが通っていない",
            ),
            (
                "1 地点の不一致",
                |e| {
                    let d = SITES.iter().find(|d| d.name == "fn_set_state").unwrap();
                    let mut b = d.expected.to_vec();
                    b[0] ^= 0xFF;
                    e.mem.insert(d.addr, b);
                    SITES.to_vec()
                },
                "一致しない地点がある",
            ),
            (
                "1 地点が読めない",
                |e| {
                    let d = SITES.iter().find(|d| d.name == "replay_saved").unwrap();
                    e.mem.remove(&d.addr);
                    SITES.to_vec()
                },
                "一致しない地点がある",
            ),
            (
                "差し替える行が表に無い",
                |_| {
                    SITES
                        .iter()
                        .filter(|d| d.name != HOOK_SITE)
                        .copied()
                        .collect()
                },
                "表に input_wait_call の行が無い",
            ),
        ];
        for (label, breaker, why) in cases {
            let mut env = Fake::all_match();
            let table = breaker(&mut env);
            let mut h = fake_hook_for_real_row();
            let (end, log) = run_both(&mut env, &mut h, &table);
            assert_eq!(end, End::NotInstalled, "{label}");
            assert_eq!(h.called_any, 0, "{label}: 設置の呼び出しがあった");
            assert_eq!(h.set_original, None, "{label}");
            assert!(
                log.contains(&format!("フックの設置: しない（{why}）")),
                "{label}: {log}"
            );
            let head = log
                .find("==== 試験のまとめ ====\n")
                .unwrap_or_else(|| panic!("{label}: まとめが無い: {log}"));
            let block = &log[head..];
            assert_eq!(
                block.lines().count(),
                15,
                "{label}: まとめが本ログの最後でない: {block}"
            );
            let line = |name: &str| {
                block
                    .lines()
                    .find(|l| l[l.find(' ').unwrap_or(0)..].trim_start().starts_with(name))
                    .unwrap_or_else(|| panic!("{label}: {name} の行が無い: {block}"))
            };
            match label {
                "同定の不一致" | "exe が読めない" => {
                    assert!(
                        line("花映塚のバージョン").starts_with("× "),
                        "{label}: {block}"
                    );
                }
                "1 地点の不一致" => assert!(
                    line("フックの設置").starts_with("× ")
                        && line("フックの設置").contains("fn_set_state"),
                    "{label}: {block}"
                ),
                "1 地点が読めない" => assert!(
                    line("フックの設置").starts_with("× ")
                        && line("フックの設置").contains("replay_saved"),
                    "{label}: {block}"
                ),
                _ => assert!(line("フックの設置").starts_with("× "), "{label}: {block}"),
            }
            if !label.starts_with("同定") && label != "exe が読めない" {
                assert!(
                    line("花映塚のバージョン").starts_with("〇 "),
                    "{label}: {block}"
                );
            }
        }
    }

    #[test]
    fn ready_to_install_needs_every_row() {
        let full = Summary {
            checked_sites: true,
            total: SITES.len(),
            matched: SITES.len(),
            ..Summary::default()
        };
        assert_eq!(ready_to_install(&full, SITES), Ok(()));
        let short = Summary {
            total: SITES.len() - 1,
            matched: SITES.len() - 1,
            ..full
        };
        assert!(ready_to_install(&short, SITES).is_err());
        assert!(ready_to_install(&Summary::default(), &[]).is_err());
    }

    #[test]
    fn refused_install_does_not_observe() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_for_real_row();
        h.eip = Box::new(|_, _| None);
        let (end, log) = run_both(&mut env, &mut h, SITES);
        assert_eq!(end, End::NotInstalled);
        assert_eq!(h.sleeps.len() as u32, crate::hook::RETRY_LIMIT - 1);
        assert!(h.sleeps.iter().all(|&ms| ms == crate::hook::RETRY_WAIT_MS));
        assert!(!log.contains("計測"));
        let block = &log[log.find("==== 試験のまとめ ====\n").expect(&log)..];
        assert!(
            block.contains(
                "× フックの設置: 14 地点は全部一致したが、入力のフックの書き込みを断った"
            ),
            "{block}"
        );
        assert!(
            block.contains("－ パニック: フックを入れていないので見ていない"),
            "{block}"
        );
    }

    const GOOD_SCRIPT: &[u8] = b"settle=5\nscene=1 wait=3 p1=Z note=a\nwait=2 note=b\n";

    fn site_addr(name: &str) -> u32 {
        SITES.iter().find(|d| d.name == name).unwrap().addr
    }

    fn fake_hook_both_writable() -> FakeHook {
        let mut h = fake_hook_for_real_row();
        let d = SITES
            .iter()
            .find(|d| d.name == "draw_before_endscene")
            .unwrap();
        h.mem2_base = d.addr;
        h.mem2[..d.expected.len()].copy_from_slice(d.expected);
        h
    }

    /// 配るバージョンのビルドでは走らせない（配るバージョンは描画だけのフレームを作らない —— `observe.rs` の `makes_render_only`）。
    #[cfg(not(feature = "dist"))]
    #[test]
    fn no_script_runs_as_before() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_both_writable();
        let mut s = FakeScript::default();
        let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(end, End::Installed);
        assert_eq!(s.events, vec!["read"]);
        assert!(
            log.contains("台本: script.txt が無い（台本なしで走る）"),
            "{log}"
        );
        assert!(
            log.contains("描画だけのフレームの予定: 設置の後 20 秒"),
            "{log}"
        );
        assert!(log.contains("計測の集計: 60 回"), "{log}");
        assert!(!log.contains("==== 台本"), "{log}");
        assert_eq!(h.writes, 2, "call 先の差し替えと描画の detour");
    }

    #[test]
    fn script_starts_after_both_hooks() {
        for draw_ok in [true, false] {
            let mut env = Fake::all_match();
            let mut h = if draw_ok {
                fake_hook_both_writable()
            } else {
                fake_hook_for_real_row()
            };
            let mut s = FakeScript {
                file: Some(GOOD_SCRIPT.to_vec()),
                ..Default::default()
            };
            let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
            assert_eq!(end, End::Installed);
            let want: Vec<&str> = if draw_ok {
                vec!["read", "move", "prepare", "arm", "shots_on", "start"]
            } else {
                vec!["read", "move", "prepare", "arm", "start"]
            };
            assert_eq!(s.events, want, "draw_ok={draw_ok}");
            assert_eq!(
                s.armed,
                Some((2, site_addr(FEED_SITE), site_addr(LOCAL_INPUT_SITE)))
            );
            assert_eq!(h.writes, if draw_ok { 2 } else { 1 });
            assert!(
                log.contains("台本: 読めた（2 段、settle = 5 tick）"),
                "{log}"
            );
            assert!(log.contains("     2| scene=1 wait=3 p1=Z note=a"), "{log}");
            assert!(
                log.contains("描画だけのフレームの予定: 作らない（台本のある走り）"),
                "{log}"
            );
            assert!(!log.contains("描画だけのフレーム: "), "{log}");
            assert!(log.contains("==== 台本（M1d） ===="), "{log}");
            assert!(log.contains("台本: 1 秒の時点で進まなくなった"), "{log}");
            assert!(log.contains("計測の集計: 11 回"), "{log}");
            if !draw_ok {
                assert!(
                    log.contains("撮影: しない（描画の detour を書けなかった・置き場を確保した）"),
                    "{log}"
                );
            }
        }
    }

    const NET_INI: &str = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 0x48475254\ncheck = digest\n";

    fn fake_hook_three_writable() -> FakeHook {
        let mut h = fake_hook_both_writable();
        let d = SITES.iter().find(|d| d.name == MATCH_START_SITE).unwrap();
        h.mem3_base = d.addr;
        h.mem3[..d.expected.len()].copy_from_slice(d.expected);
        h
    }

    fn net_script() -> FakeScript {
        FakeScript {
            file: Some(GOOD_SCRIPT.to_vec()),
            ini: Some(NET_INI.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn net_opens_after_the_three_hooks_and_starts_before_the_gate() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_three_writable();
        let mut s = net_script();
        let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(end, End::Installed);
        assert_eq!(
            s.events,
            vec![
                "read",
                "move",
                "prepare",
                "arm",
                "arm_net",
                "shots_on",
                "start_net",
                "start"
            ]
        );
        assert_eq!(h.writes, 3, "入力・描画・開始の 3 つ");
        let m = SITES.iter().find(|d| d.name == MATCH_START_SITE).unwrap();
        assert_eq!(
            h.written_at,
            Some(m.addr + 1),
            "最後に書いたのが開始のフックの rel32"
        );
        assert_eq!(h.set_match, Some(0x0043_BF30), "開始のフックの元の飛び先");
        assert_eq!(h.writes_before_setup, 0);
        let (settings, set_state) = s
            .net_armed
            .clone()
            .expect("ネットワークのセッションを置いていない");
        assert_eq!(set_state, site_addr(SET_STATE_SITE));
        assert_eq!(settings.peer, Some("127.0.0.1:7002".parse().unwrap()));
        assert_eq!(settings.check, crate::netplay::CheckSource::InputDigest);
        for w in [
            "ネットワーク: 設定を読んだ",
            "ネットワーク: 受信ソケット",
            "ネットワーク: 動かした（NET_ON = 1）",
        ] {
            assert!(log.contains(w), "{w}: {log}");
        }
    }

    #[test]
    fn a_remembered_name_beats_the_ini_name() {
        let ini = format!("{NET_INI}name = ini-name\n");
        for (saved, want) in [(Some("saved-name"), "saved-name"), (None, "ini-name")] {
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                ini: Some(ini.clone()),
                saved_name: saved.map(str::to_string),
                ..net_script()
            };
            let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
            let (settings, _) = s
                .net_armed
                .clone()
                .expect("ネットワークのセッションを置いていない");
            assert_eq!(settings.name, want, "{log}");
            assert_eq!(
                log.contains("名前は覚えたファイルの値を使う"),
                saved.is_some(),
                "{log}"
            );
        }
    }

    #[test]
    fn an_old_delay_key_is_one_brief_log_line_and_the_net_still_opens() {
        for (extra, lines) in [
            ("latency = 2\n", vec!["latency"]),
            ("rollback_delay = 1\ndelay = 3\n", vec!["rollback_delay"]),
            ("delay = 3\n", vec![]),
        ] {
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                ini: Some(format!("{NET_INI}{extra}")),
                ..net_script()
            };
            let (_, log) = brief(|| run_with(&mut env, &mut h, SITES, &mut s));
            assert!(s.net_armed.is_some(), "{extra:?}: 節を断った: {log}");
            assert_eq!(
                log.matches("ネットワーク: 古いキー ").count(),
                lines.len(),
                "{extra:?}: {log}"
            );
            for k in lines {
                assert!(
                    log.contains(&format!(
                        "ネットワーク: 古いキー {k} は読まない。delay を使う\n"
                    )),
                    "{extra:?}: {log}"
                );
            }
        }
    }

    #[test]
    fn the_screen_language_comes_from_the_ini_and_auto_follows_the_os() {
        const JA_OS: u16 = 0x0411;
        const EN_OS: u16 = 0x0409;
        for (ini_lang, os, want) in [
            (None, JA_OS, Lang::Ja),
            (None, EN_OS, Lang::En),
            (Some("auto"), EN_OS, Lang::En),
            (Some("auto"), JA_OS, Lang::Ja),
            (Some("ja"), EN_OS, Lang::Ja),
            (Some("en"), JA_OS, Lang::En),
        ] {
            let ini = match ini_lang {
                Some(l) => format!("{NET_INI}\n[ui]\nlang = {l}\n"),
                None => NET_INI.to_string(),
            };
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                ini: Some(ini),
                os_langid: Some(os),
                ..net_script()
            };
            crate::ui_lang::set(Lang::Ja);
            let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
            assert_eq!(
                crate::ui_lang::current(),
                want,
                "{ini_lang:?} {os:#x}: {log}"
            );
            assert!(log.contains(&format!("言語: {}（", want.word())), "{log}");
            assert!(
                s.net_armed.is_some(),
                "言語の決め方がネットワークを止めた: {log}"
            );
        }
    }

    #[test]
    fn a_broken_ui_section_is_reported_and_the_run_goes_on_with_the_os_language() {
        for bad in ["lang = klingon", "language = en", "lang ="] {
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                ini: Some(format!("{NET_INI}\n[ui]\n{bad}\n")),
                os_langid: Some(0x0409),
                ..net_script()
            };
            crate::ui_lang::set(Lang::Ja);
            let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
            assert!(
                log.contains("言語: HiganRTool.ini の [ui] を読めない"),
                "{bad}: {log}"
            );
            assert_eq!(
                crate::ui_lang::current(),
                Lang::En,
                "{bad}: OS の言語に従う"
            );
            assert!(s.net_armed.is_some(), "{bad}: ネットワークは開く: {log}");
        }
    }

    #[test]
    fn net_stays_closed_unless_everything_is_in_place() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_both_writable();
        let mut s = net_script();
        let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(
            s.events,
            vec!["read", "move", "prepare", "arm", "shots_on", "start"]
        );
        assert!(
            log.contains("ネットワーク: 開始のフックが書けなかったので開かない"),
            "{log}"
        );
        let mut env = Fake::all_match();
        let mut h = fake_hook_three_writable();
        let mut s = FakeScript {
            net_arm_fail: true,
            ..net_script()
        };
        let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(
            s.events,
            vec![
                "read", "move", "prepare", "arm", "arm_net", "shots_on", "start"
            ]
        );
        assert!(
            log.contains("ネットワーク: 開けない（合成の失敗）"),
            "{log}"
        );
        for (ini, want) in [
            (
                Some(format!("{NET_INI}latancy = 3\n")),
                "ネットワーク: HiganRTool.ini の [netplay] を読めない",
            ),
            (
                None,
                "ネットワーク: HiganRTool.ini が無い（ネットワークなしで走る）",
            ),
            (
                Some("[netplay]\nenable = 0\n".to_string()),
                "enable = 1 でない",
            ),
        ] {
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                ini,
                ..net_script()
            };
            let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
            assert_eq!(
                s.events,
                vec!["read", "move", "prepare", "arm", "shots_on", "start"]
            );
            assert_eq!(s.ini_reads, 1);
            assert_eq!(h.writes, 2);
            assert!(log.contains(want), "{want}: {log}");
        }
    }

    fn with_portrait_fns(env: &mut Fake) {
        for d in PORTRAIT_SITES {
            env.mem.insert(d.addr, d.expected.to_vec());
        }
    }

    #[test]
    fn portrait_fns_are_armed_only_when_all_three_match() {
        let addr_of = |name: &str| PORTRAIT_SITES.iter().find(|d| d.name == name).unwrap().addr;
        let run_net = |env: &mut Fake, s: &mut FakeScript| {
            let mut h = fake_hook_three_writable();
            run_with(env, &mut h, SITES, s)
        };
        let fresh = || FakeScript {
            ini: Some(NET_INI.to_string()),
            ..Default::default()
        };
        let mut env = Fake::all_match();
        with_portrait_fns(&mut env);
        let mut s = fresh();
        let (end, log) = run_net(&mut env, &mut s);
        assert_eq!(end, End::Installed);
        assert_eq!(
            s.events,
            vec![
                "read",
                "arm_io",
                "arm_net",
                "arm_portrait",
                "start_net",
                "start"
            ]
        );
        assert_eq!(
            s.portrait_armed,
            Some([
                addr_of(PORTRAIT_SLIDE_SITE),
                addr_of(PORTRAIT_SLIDE_BACK_SITE),
                addr_of(PORTRAIT_MARKER_SITE)
            ])
        );
        assert!(log.contains("立ち絵の追随: 入れた"), "{log}");
        for d in PORTRAIT_SITES {
            for unreadable in [false, true] {
                let mut env = Fake::all_match();
                with_portrait_fns(&mut env);
                if unreadable {
                    env.mem.remove(&d.addr);
                } else {
                    let mut b = d.expected.to_vec();
                    let last = b.len() - 1;
                    b[last] ^= 0xFF;
                    env.mem.insert(d.addr, b);
                }
                let mut s = fresh();
                let (end, log) = run_net(&mut env, &mut s);
                let why = format!(
                    "{} {}",
                    d.name,
                    if unreadable {
                        "読めない"
                    } else {
                        "不一致"
                    }
                );
                assert_eq!(end, End::Installed, "{why}");
                assert_eq!(s.portrait_armed, None, "{why}: 渡した");
                assert_eq!(
                    s.events,
                    vec!["read", "arm_io", "arm_net", "start_net", "start"],
                    "{why}"
                );
                assert!(log.contains("立ち絵の追随: 入れない"), "{why}: {log}");
                assert!(log.contains(d.name), "{why}: {log}");
            }
        }
        let mut env = Fake::all_match();
        with_portrait_fns(&mut env);
        let mut s = FakeScript {
            net_arm_fail: true,
            ..fresh()
        };
        let (_, log) = run_net(&mut env, &mut s);
        assert_eq!(s.portrait_armed, None);
        assert!(!log.contains("立ち絵の追随"), "{log}");
    }

    #[test]
    fn poll_fn_is_armed_only_when_the_head_matches() {
        let d = POLL_SITES[0];
        let run_net = |env: &mut Fake, s: &mut FakeScript| {
            let mut h = fake_hook_three_writable();
            run_with(env, &mut h, SITES, s)
        };
        let fresh = || FakeScript {
            ini: Some(NET_INI.to_string()),
            ..Default::default()
        };
        let mut env = Fake::all_match();
        env.mem.insert(d.addr, d.expected.to_vec());
        let mut s = fresh();
        let (end, log) = run_net(&mut env, &mut s);
        assert_eq!(end, End::Installed);
        assert_eq!(s.poll_armed, Some(0x0042_B850));
        assert_eq!(
            s.events,
            vec!["read", "arm_io", "arm_net", "start_net", "start"]
        );
        assert!(log.contains("poll: 使う"), "{log}");
        for i in 0..d.expected.len() {
            let mut env = Fake::all_match();
            let mut b = d.expected.to_vec();
            b[i] ^= 0xFF;
            env.mem.insert(d.addr, b);
            let mut s = fresh();
            let (end, log) = run_net(&mut env, &mut s);
            assert_eq!(end, End::Installed, "{i} 番目");
            assert_eq!(s.poll_armed, None, "{i} 番目: 不一致なのに渡した");
            assert_eq!(
                s.events,
                vec!["read", "arm_io", "arm_net", "start_net", "start"],
                "{i} 番目"
            );
            assert!(log.contains("poll: 使わない（"), "{i} 番目: {log}");
            assert!(log.contains("不一致"), "{i} 番目: {log}");
            assert!(log.contains("0x0042BE40"), "{i} 番目: {log}");
        }
        let mut env = Fake::all_match();
        env.mem.remove(&d.addr);
        let mut s = fresh();
        let (end, log) = run_net(&mut env, &mut s);
        assert_eq!(end, End::Installed);
        assert_eq!(s.poll_armed, None);
        assert!(log.contains("読めない"), "{log}");
        let mut env = Fake::all_match();
        env.mem.insert(d.addr, d.expected.to_vec());
        let mut s = FakeScript {
            net_arm_fail: true,
            ..fresh()
        };
        let (_, log) = run_net(&mut env, &mut s);
        assert_eq!(s.poll_armed, None);
        assert!(!log.contains("poll:"), "{log}");
        let mut env = Fake::all_match();
        env.mem.insert(d.addr, d.expected.to_vec());
        let mut s = FakeScript::default();
        let (_, log) = run_net(&mut env, &mut s);
        assert_eq!(s.poll_armed, None);
        assert!(!log.contains("poll:"), "{log}");
    }

    const NET_INI_KEEP: &str = "[netplay]\nenable = 1\nbind = 127.0.0.1:0\npeer = 127.0.0.1:7002\nsession = 0x48475254\ncheck = digest\nafter_script = keep\n";

    #[test]
    fn net_opens_without_a_script() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_three_writable();
        let mut s = FakeScript {
            ini: Some(NET_INI.to_string()),
            ..Default::default()
        };
        let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(end, End::Installed);
        assert_eq!(
            s.events,
            vec!["read", "arm_io", "arm_net", "start_net", "start"]
        );
        assert_eq!(
            s.io_armed,
            Some((site_addr(FEED_SITE), site_addr(LOCAL_INPUT_SITE)))
        );
        assert_eq!(h.writes, 3, "入力・描画・開始の 3 つ");
        let (settings, set_state) = s
            .net_armed
            .clone()
            .expect("ネットワークのセッションを置いていない");
        assert_eq!(set_state, site_addr(SET_STATE_SITE));
        assert_eq!(settings.session_id, Some(0x4847_5254));
        for w in [
            "ネットワーク: 台本なしで開く（M2-5",
            "ネットワーク: 動かした（NET_ON = 1）",
            "ネットワーク: 分かれ道を開いた（SCRIPT_ON = 1）。台本は無い",
            "描画だけのフレームの予定: 作らない（ネットワークの実行）",
            "ネットワーク: 1 秒の時点で終わっていた（ネットワークのセッションを置いていない）",
            "計測の集計: 11 回",
            "ネットワークが開いている間は計測の上限で止めない",
            "計測 1/上限なし: ",
        ] {
            assert!(log.contains(w), "{w}: {log}");
        }
        assert!(!log.contains("描画だけのフレーム: "), "{log}");
        assert!(!log.contains("秒。ネットワークは計測の後も続く"), "{log}");
    }

    #[test]
    fn a_scriptless_net_that_cannot_open_keeps_the_gate_closed() {
        type Setup = fn(&mut FakeScript, &mut FakeHook);
        let cases: [(&str, Setup, &[&str], &str); 4] = [
            (
                "アドレスを置けない",
                |s, _| s.io_fail = true,
                &["read", "arm_io"],
                "ネットワーク: アドレスを置けない（合成の失敗）ので開かない",
            ),
            (
                "開始のフックが書けない",
                |_, h| *h = fake_hook_both_writable(),
                &["read", "arm_io"],
                "ネットワーク: 開始のフックが書けなかったので開かない",
            ),
            (
                "ネットワークのセッションを置けない",
                |s, _| s.net_arm_fail = true,
                &["read", "arm_io", "arm_net"],
                "ネットワーク: 開けない（合成の失敗）",
            ),
            (
                "ネットワークを動かせない",
                |s, _| s.net_start_fail = true,
                &["read", "arm_io", "arm_net", "start_net"],
                "ネットワーク: 動かせない（合成の失敗）",
            ),
        ];
        for (label, setup, events, why) in cases {
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                ini: Some(NET_INI.to_string()),
                ..Default::default()
            };
            setup(&mut s, &mut h);
            let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
            assert_eq!(end, End::Installed, "{label}");
            assert_eq!(s.events, events.to_vec(), "{label}");
            assert!(log.contains(why), "{label}: {log}");
            assert!(
                log.contains("ネットワーク: 開けなかったので分かれ道は開かない"),
                "{label}: {log}"
            );
            assert!(!log.contains("描画だけのフレーム: "), "{label}: {log}");
            assert!(log.contains("計測の集計: 11 回"), "{label}: {log}");
        }
    }

    #[test]
    fn keep_after_script_waits_for_the_net() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_three_writable();
        let mut s = FakeScript {
            ini: Some(NET_INI_KEEP.to_string()),
            ..net_script()
        };
        let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(
            s.events,
            vec![
                "read",
                "move",
                "prepare",
                "arm",
                "arm_net",
                "shots_on",
                "start_net",
                "start"
            ]
        );
        let (settings, _) = s.net_armed.clone().unwrap();
        assert_eq!(settings.after_script, AfterScript::Keep);
        for w in [
            "after_script = keep: 台本が最後の段まで押した後はネットワークを閉じず",
            "台本（あれば）とネットワークが終わってから 10 秒で終える",
            &format!("（上限 {SCRIPT_MAX_SECONDS} 秒。ネットワークは計測の後も続く）"),
            &format!("計測 1/{SCRIPT_MAX_SECONDS}: "),
        ] {
            assert!(log.contains(w), "{w}: {log}");
        }
        assert!(!log.contains("上限なし"), "{log}");
        assert!(
            !log.contains("台本が終わるか止まってから 10 秒で終える"),
            "{log}"
        );
        let mut env = Fake::all_match();
        let mut h = fake_hook_three_writable();
        let mut s = net_script();
        let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert!(
            log.contains(&format!(
                "台本が終わるか止まってから 10 秒で終える（上限 {SCRIPT_MAX_SECONDS} 秒）"
            )),
            "{log}"
        );
        assert!(!log.contains("上限なし"), "{log}");
        assert!(!log.contains("after_script = keep"), "{log}");
    }

    #[test]
    fn mismatched_run_does_not_read_the_ini() {
        let mut env = Fake::all_match();
        env.ident = crate::probe::IdentOutcome::Unreadable("合成".to_string());
        let mut h = fake_hook_three_writable();
        let mut s = net_script();
        let (end, _) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(end, End::NotInstalled);
        assert_eq!(s.ini_reads, 0);
        assert!(s.events.is_empty());
    }

    #[test]
    fn refused_install_keeps_the_script_closed() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_for_real_row();
        h.eip = Box::new(|_, _| None);
        let mut s = FakeScript {
            file: Some(GOOD_SCRIPT.to_vec()),
            ..Default::default()
        };
        let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(end, End::NotInstalled);
        assert_eq!(s.events, vec!["read", "move", "prepare", "arm"]);
        assert!(
            log.contains("フックの設置が断られたので動かさない"),
            "{log}"
        );
    }

    #[test]
    fn bad_scripts_are_not_used_and_install_goes_on() {
        type Setup = fn(&mut FakeScript);
        let cases: [(&str, Setup, &[&str], &str); 5] = [
            (
                "読めない行",
                |s| s.file = Some(b"scene=1\nscene=1 p1=ENTER\n".to_vec()),
                &["read", "move"],
                "台本: 読めない（2行目: p1 に知らないキー名があります: ENTER）",
            ),
            (
                "移せない",
                |s| {
                    s.file = Some(GOOD_SCRIPT.to_vec());
                    s.move_fail = true;
                },
                &["read", "move"],
                "台本: script.txt を移せない（合成の失敗）ので使わない",
            ),
            (
                "読めないファイル",
                |s| s.read_fail = true,
                &["read"],
                "台本: script.txt を読めない（合成の失敗）",
            ),
            (
                "UTF-8 でない",
                |s| s.file = Some(vec![b's', 0xFF, b'\n']),
                &["read", "move"],
                "台本: UTF-8 でない（先頭から 1 バイトの後）",
            ),
            (
                "置けない",
                |s| {
                    s.file = Some(GOOD_SCRIPT.to_vec());
                    s.arm_fail = true;
                },
                &["read", "move", "prepare", "arm"],
                "台本: 置けない（合成の失敗）ので使わない",
            ),
        ];
        for (label, setup, events, why) in cases {
            let mut env = Fake::all_match();
            let mut h = fake_hook_both_writable();
            let mut s = FakeScript::default();
            setup(&mut s);
            let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
            assert_eq!(end, End::Installed, "{label}");
            assert_eq!(s.events, events.to_vec(), "{label}");
            assert!(log.contains(why), "{label}: {log}");
            assert!(log.contains("計測の集計: 60 回"), "{label}: 台本なしの長さ");
        }
    }

    #[test]
    fn mismatched_run_does_not_touch_the_script() {
        let mut env = Fake::all_match();
        env.ident = crate::probe::IdentOutcome::Unreadable("合成".to_string());
        let mut h = fake_hook_for_real_row();
        let mut s = FakeScript {
            file: Some(GOOD_SCRIPT.to_vec()),
            ..Default::default()
        };
        let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(end, End::NotInstalled);
        assert!(s.events.is_empty(), "{:?}", s.events);
        assert!(
            log.contains("台本: 設置しないので script.txt には触らない"),
            "{log}"
        );
    }

    #[test]
    fn explicit_script_is_read_but_not_moved_and_ini_origin_is_logged() {
        let mut env = Fake::all_match();
        let mut h = fake_hook_three_writable();
        let mut s = FakeScript {
            explicit: Some(
                r"環境の変数 HIGANRTOOL_SCRIPT で渡された C:\s\two_01_host.txt".to_string(),
            ),
            ini_from: Some(r"環境の変数 HIGANRTOOL_INI で渡された C:\s\two_host.ini".to_string()),
            ..net_script()
        };
        let (end, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(end, End::Installed);
        assert_eq!(
            s.events,
            vec![
                "read",
                "prepare",
                "arm",
                "arm_net",
                "shots_on",
                "start_net",
                "start"
            ],
            "渡された台本を移した"
        );
        for w in [
            r"台本: 環境の変数 HIGANRTOOL_SCRIPT で渡された C:\s\two_01_host.txt（",
            "を読んだ（ローダーが渡した台本なので移さない）",
            r"ini: 環境の変数 HIGANRTOOL_INI で渡された C:\s\two_host.ini",
            "ネットワーク: 設定を読んだ",
        ] {
            assert!(log.contains(w), "{w}: {log}");
        }
        let mut env = Fake::all_match();
        let mut h = fake_hook_three_writable();
        let mut s = net_script();
        let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
        assert_eq!(s.events.get(1).map(String::as_str), Some("move"));
        assert!(
            log.contains("ini: DLL のフォルダの HiganRTool.ini"),
            "{log}"
        );
    }

    fn brief<T>(f: impl FnOnce() -> T) -> T {
        crate::log_mode::with_detail(false, f)
    }

    fn detailed<T>(f: impl FnOnce() -> T) -> T {
        crate::log_mode::with_detail(true, f)
    }

    /// 壊れたら落ちる: 簡潔なログが、起動の節目（ビルド・時刻・同定・照合の集計・終わりのまとめ）を落とす。または、地点ごとの照合・
    /// ウィンドウの値・コピーの表・設置の細目を残して簡潔になっていない。詳細が今までの行を失う。
    #[test]
    fn a_brief_run_keeps_the_start_lines_and_the_summary_but_not_the_detail_lines() {
        let run = || {
            let mut env = Fake::all_match();
            with_window(&mut env);
            with_copies(&mut env);
            let mut h = fake_hook_both_writable();
            let (end, log) = run_both(&mut env, &mut h, SITES);
            assert_eq!(end, End::Installed);
            log
        };
        let (b, d) = (brief(run), detailed(run));
        for want in [
            "HiganRTool: ビルド",
            "時刻: ",
            "同定: 一致（花映塚 1.50a）",
            "集計: 14 地点のうち 一致 14 / 不一致 0 / 読めない 0",
            "==== 状態の変化（計測 1 秒の時点） ====",
            "〇 フックの設置: ",
            "==== 試験のまとめ ====",
        ] {
            assert!(b.contains(want), "簡潔に {want:?} が無い:\n{b}");
        }
        for detail_only in [
            "PID: ",
            "exe: ",
            "image base: ",
            "表の検査: 通った",
            "地点 input_wait_call",
            "コピーの表: 6 行",
            "ウィンドウ: 0x",
            "フックの設置: 地点 ",
            "フックの設置の試み: ",
            "読み返し: ",
            "inline detour: ",
            "TSC: invariant",
            "計測 1/",
            "==== 集計",
        ] {
            assert!(
                !b.contains(detail_only),
                "簡潔に {detail_only:?} が出た:\n{b}"
            );
            assert!(
                d.contains(detail_only),
                "詳細に {detail_only:?} が無い:\n{d}"
            );
        }
        assert!(
            b.lines().count() * 4 < d.lines().count(),
            "簡潔 {} 行 / 詳細 {} 行",
            b.lines().count(),
            d.lines().count()
        );
        assert!(!b.contains("==== 試験のまとめ（途中経過"));
    }

    /// 壊れたら落ちる: 簡潔なログが、断られた・合わなかった・開けなかった理由の行を落とす。
    #[test]
    fn a_brief_run_still_says_why_something_was_refused_or_failed() {
        // 照合が合わない地点の行と、設置しない理由。
        let log = brief(|| {
            let mut env = Fake::all_match();
            let d = SITES.iter().find(|d| d.name == "fn_set_state").unwrap();
            let mut b = d.expected.to_vec();
            b[0] ^= 0xFF;
            env.mem.insert(d.addr, b);
            let mut h = fake_hook_for_real_row();
            run_both(&mut env, &mut h, SITES).1
        });
        for want in [
            "地点 fn_set_state",
            "不一致 実際 = ",
            "集計: 14 地点のうち 一致 13 / 不一致 1 / 読めない 0",
            "フックの設置: しない（一致しない地点がある）",
            "× フックの設置: 照合が合わなかった地点: fn_set_state",
        ] {
            assert!(log.contains(want), "{want:?}:\n{log}");
        }
        // 同定が合わない。
        let log = brief(|| {
            let mut env = Fake::all_match();
            env.ident = IdentOutcome::Mismatch(IdentMismatch {
                fields: vec![IdentField::TimeDateStamp],
            });
            let mut h = fake_hook_for_real_row();
            run_both(&mut env, &mut h, SITES).1
        });
        assert!(log.contains("同定: 不一致（食い違った欄: "), "{log}");
        assert!(
            log.contains("地点の照合: しない（同定が合わないため）"),
            "{log}"
        );
        // 書き込みを断った。
        let log = brief(|| {
            let mut env = Fake::all_match();
            let mut h = fake_hook_for_real_row();
            h.eip = Box::new(|_, _| None);
            run_both(&mut env, &mut h, SITES).1
        });
        for want in [
            "フックの設置: 地点 input_wait_call",
            "フックの設置: 書かない（",
            "フックの設置の試み: ",
        ] {
            assert!(log.contains(want), "{want:?}:\n{log}");
        }
        assert!(log.contains("× フックの設置: "), "{log}");
        // ネットワークを開けない・ini が読めない・開始のフックが書けない。
        let log = brief(|| {
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                net_arm_fail: true,
                ..net_script()
            };
            run_with(&mut env, &mut h, SITES, &mut s).1
        });
        assert!(
            log.contains("ネットワーク: 開けない（合成の失敗）"),
            "{log}"
        );
        for (ini, want) in [
            (
                Some(format!("{NET_INI}latancy = 3\n")),
                "ネットワーク: HiganRTool.ini の [netplay] を読めない",
            ),
            (
                None,
                "ネットワーク: HiganRTool.ini が無い（ネットワークなしで走る）",
            ),
        ] {
            let log = brief(|| {
                let mut env = Fake::all_match();
                let mut h = fake_hook_three_writable();
                let mut s = FakeScript {
                    ini,
                    ..net_script()
                };
                run_with(&mut env, &mut h, SITES, &mut s).1
            });
            assert!(log.contains(want), "{want}:\n{log}");
        }
        let log = brief(|| {
            let mut env = Fake::all_match();
            let mut h = fake_hook_both_writable();
            run_with(&mut env, &mut h, SITES, &mut net_script()).1
        });
        assert!(
            log.contains("ネットワーク: 開始のフックが書けなかったので開かない"),
            "{log}"
        );
        // 壊れた台本。
        let log = brief(|| {
            let mut env = Fake::all_match();
            let mut h = fake_hook_both_writable();
            let mut s = FakeScript {
                file: Some(b"scene=1 nonsense\n".to_vec()),
                ..Default::default()
            };
            run_with(&mut env, &mut h, SITES, &mut s).1
        });
        assert!(log.contains("台本: 読めない（"), "{log}");
    }

    /// 壊れたら落ちる: ネットワークの名前・IP のアドレス・ユーザー名入りのパスが、簡潔でも詳細でも本ログに出る。
    #[test]
    fn neither_mode_writes_the_net_name_an_ip_address_or_a_user_folder() {
        const INI: &str = "[netplay]\nenable = 1\nscope = internet\nbind = 127.0.0.1:0\npeer = 198.51.100.9:7002\nsession = 0x48475254\ncheck = digest\nname = SecretPlayer\n";
        // 公開用の走査が拾わないよう、ユーザーのフォルダのパスは組み立てて作る。
        let home = format!("C:{0}Users{0}bob", '\\');
        let run = || {
            let mut env = Fake::all_match();
            let mut h = fake_hook_three_writable();
            let mut s = FakeScript {
                ini: Some(INI.to_string()),
                saved_name: Some("SecretSaved".to_string()),
                ini_from: Some(format!(
                    "環境の変数 HIGANRTOOL_INI で渡された {home}\\Downloads\\HiganRTool\\HiganRTool.ini"
                )),
                ..Default::default()
            };
            let (_, log) = run_with(&mut env, &mut h, SITES, &mut s);
            // 本物の書き込み関数と同じに、1 行ずつ伏せる処理を通す。
            let mut out = Vec::new();
            {
                use higanr_core::log_scrub::{Scrub, ScrubWrite};
                use std::io::Write;
                let mut w = ScrubWrite::new(&mut out, Scrub::new());
                w.write_all(log.as_bytes()).unwrap();
            }
            String::from_utf8(out).unwrap()
        };
        for (mode, log) in [("簡潔", brief(run)), ("詳細", detailed(run))] {
            for secret in ["SecretPlayer", "SecretSaved", "198.51.100.9", "bob"] {
                assert!(!log.contains(secret), "{mode}: {secret} が出た:\n{log}");
            }
        }
        // 詳細には、伏せた形の設定の行が残る。
        let d = detailed(run);
        assert!(d.contains("名前は記録しない"), "{d}");
        assert!(d.contains("外:7002"), "{d}");
        assert!(
            d.contains(&format!("C:{0}Users{0}<user>{0}Downloads", '\\')),
            "{d}"
        );
    }

    /// 壊れたら落ちる: ネットワークを開いた文に混ざった観戦の準備の失敗が、簡潔なログから消える。成功の文が簡潔に出る。
    #[test]
    fn a_spectate_setup_failure_in_the_net_note_reaches_the_brief_log() {
        let run = |desc: &str| {
            brief(|| {
                let mut env = Fake::all_match();
                let mut h = fake_hook_three_writable();
                let mut s = FakeScript {
                    net_desc: Some(desc.to_string()),
                    ..net_script()
                };
                run_with(&mut env, &mut h, SITES, &mut s).1
            })
        };
        let ok = run("受信ソケットを開いた / 観戦の配る側を起こした");
        assert!(!ok.contains("観戦の配る側を起こした"), "{ok}");
        for bad in [
            " / 観戦は使わない: 部屋サーバーが無い",
            " / 観戦: 受信スレッドに置けなかった（もう置いてある）",
            " / 観戦用の部屋。ただしコードを書けない: 書けない",
        ] {
            let log = run(&format!("受信ソケットを開いた{bad}"));
            assert!(log.contains(bad), "{bad}:\n{log}");
        }
    }

    #[test]
    fn bom_is_skipped() {
        let mut s = FakeScript {
            file: Some("\u{FEFF}scene=1 p1=Z\n".as_bytes().to_vec()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let got = load_script(&mut s, &mut out);
        assert_eq!(
            got.map(|x| x.steps.len()),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out)
        );
    }
}
