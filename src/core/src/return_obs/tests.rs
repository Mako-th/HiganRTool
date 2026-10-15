use super::*;
use crate::input_model::keys::{DOWN, ESC, UP, Z};

/// DLL の `menu::MENU_CSV_HEADER` は、これを再公開している（DLL 側の検査が、DLL の行をこの読み手で読めることを見る）。
const HEADER: &str = MENU_CSV_HEADER;

const Q: u16 = 0x0200;

/// 見出しと同じ 25 欄の行。使わない欄は空（DLL の行も、読めなかった欄は空）。
fn csv_line(seq: u32, screen: i32, cursor: i32, k: [u32; 6]) -> String {
    let mut f = vec![String::new(); 25];
    f[0] = seq.to_string();
    f[1] = seq.to_string();
    f[2] = "0".to_string();
    f[3] = "0x00000000".to_string();
    f[4] = screen.to_string();
    f[5] = "1".to_string();
    f[6] = cursor.to_string();
    for (slot, v) in f[19..].iter_mut().zip(k) {
        *slot = v.to_string();
    }
    f.join(",")
}

/// ゲームの模型。一時停止（`0x00434740`）と終わりのメニュー（`0x004355F0`）を逆アセンブルの順で 1 呼びずつ進める。
/// 入力は押した瞬間（`[2]+0x06`）。数えは呼びの終わりに 1 足す（`0x00434B44`）。
#[derive(Clone)]
struct Sim {
    seq: u32,
    pause_flag: u32,
    pause: (u32, u32),
    end_flag: u32,
    end: (u32, u32),
    screen: i32,
    cursor: i32,
    /// 入力を読み始める数え（実物は `MENU_READ_FRAMES`）。変異用に変えられる。
    read_frames: u32,
    /// 決定で数えをリセットするか（実物は する）。
    reset_on_decide: bool,
    /// 閉じるまでの数え（実物は `CLOSE_FRAMES`）。
    close_frames: u32,
    lines: Vec<String>,
    last_key: Option<[u32; 6]>,
}

impl Sim {
    fn new() -> Sim {
        Sim {
            seq: 0,
            pause_flag: 0,
            pause: (0, 0),
            end_flag: 0,
            end: (0, 0),
            screen: 6,
            cursor: 4,
            read_frames: MENU_READ_FRAMES,
            reset_on_decide: true,
            close_frames: CLOSE_FRAMES,
            lines: vec![HEADER.to_string()],
            last_key: None,
        }
    }

    fn pause_call(&mut self, edge: u16) {
        let (mut s, mut f) = self.pause;
        if edge & ESC != 0 && s != 3 {
            (s, f) = (3, 0);
        }
        if edge & Q != 0 && s != 8 {
            (s, f) = (8, 0);
        }
        let ready = f >= self.read_frames;
        let ud = edge & (UP | DOWN) != 0;
        let decide = edge & Z != 0;
        let reset = |f: &mut u32, on: bool| {
            if on {
                *f = 0;
            }
        };
        match s {
            0 => (s, f) = (1, 0),
            1 if ready => {
                if ud {
                    s = 2;
                } else if decide {
                    s = 3;
                    reset(&mut f, self.reset_on_decide);
                }
            }
            2 if ready => {
                if ud {
                    s = 1;
                } else if decide {
                    s = 5;
                    reset(&mut f, self.reset_on_decide);
                }
            }
            5 if ready => {
                if ud {
                    s = 4;
                } else if decide {
                    s = 2;
                    reset(&mut f, self.reset_on_decide);
                }
            }
            4 if ready => {
                if ud {
                    s = 5;
                } else if decide {
                    s = 8;
                    reset(&mut f, self.reset_on_decide);
                }
            }
            3 | 8 if f >= self.close_frames => {
                s = 0;
                self.pause_flag = 0;
            }
            _ => {}
        }
        self.pause = (s, f + 1);
    }

    fn end_call(&mut self, edge: u16) {
        let (mut s, mut f) = self.end;
        let ready = f >= self.read_frames;
        let (up, down, decide) = (edge & UP != 0, edge & DOWN != 0, edge & Z != 0);
        match s {
            0 => (s, f) = (1, 0),
            1..=3 if ready => {
                if up {
                    s = [3, 1, 2][(s - 1) as usize];
                } else if down {
                    s = [2, 3, 1][(s - 1) as usize];
                } else if decide {
                    s += 3;
                    f = 0;
                }
            }
            4..=6 if f >= self.close_frames && s == 6 => self.end_flag = 0,
            _ => {}
        }
        self.end = (s, f + 1);
    }

    fn record(&mut self) {
        let key = [
            self.pause_flag,
            self.pause.0,
            self.pause.1,
            self.end_flag,
            self.end.0,
            self.end.1,
        ];
        if self.last_key == Some(key) {
            return;
        }
        self.last_key = Some(key);
        self.lines
            .push(csv_line(self.seq, self.screen, self.cursor, key));
    }

    /// 1 tick。入力の地点（feed）の後、メニューの関数が旗の立っているものだけ走る。
    fn tick(&mut self, edge: u16) {
        self.seq += 1;
        if self.pause_flag != 0 {
            self.pause_call(edge);
        }
        if self.end_flag != 0 {
            self.end_call(edge);
        }
        self.record();
    }

    fn idle(&mut self, n: u32) {
        for _ in 0..n {
            self.tick(0);
        }
    }

    /// Esc の tick: GameManager（優先度 2）が旗を立てる。メニューの関数（優先度 1）はこの tick では走らない。
    fn open_pause(&mut self) {
        self.seq += 1;
        self.pause_flag = 1;
        self.record();
    }

    /// 試合の終わり: 終わりのメニューの印が立つ。
    fn open_end(&mut self) {
        self.seq += 1;
        self.end_flag = 1;
        self.record();
    }

    fn press(&mut self, edge: u16, then_idle: u32) {
        self.tick(edge);
        self.idle(then_idle);
    }

    fn text(&self) -> String {
        self.lines.join("\n") + "\n"
    }
}

/// 台本どおりの一時停止 2 回（開いた直後の下は効かず、再開と「タイトルへ戻る」の往復で再開 / 確かめの枠を一周して「いいえ」の決定 → 再開）。
fn pause_plan(s: &mut Sim) {
    s.idle(40);
    s.open_pause();
    s.idle(1);
    s.press(DOWN, 30);
    s.press(DOWN, 10);
    s.press(UP, 10);
    s.press(ESC, 30);
    s.idle(20);
    s.open_pause();
    s.idle(30);
    s.press(DOWN, 10);
    s.press(Z, 10);
    s.press(UP, 10);
    s.press(DOWN, 10);
    s.press(Z, 10);
    s.press(UP, 10);
    s.press(Z, 30);
}

/// 台本どおりの終わりのメニュー: 下を一周・上を一周して、再戦を決める。
fn end_plan(s: &mut Sim) {
    s.idle(20);
    s.open_end();
    s.idle(30);
    for k in [DOWN, DOWN, DOWN, UP, UP, UP] {
        s.press(k, 10);
    }
    s.press(Z, 30);
}

fn judge_text(text: &str, want: &Expect) -> Report {
    judge(&parse_menu_csv(text).expect("読める"), want)
}

const BOTH: Expect = Expect {
    pause: true,
    end: true,
    pause_first: true,
    final_screen: Some((6, 4)),
};

fn full_trace() -> Sim {
    let mut s = Sim::new();
    pause_plan(&mut s);
    end_plan(&mut s);
    s
}

fn status(r: &Report, check: &str) -> Status {
    r.find(check)
        .unwrap_or_else(|| panic!("{check} の結果が無い:\n{}", r.render()))
        .status
}

/// 壊れたら落ちる: 逆アセンブルの順の模型が出した台本どおりの記録が、全部の照らしを通らない（表・ゲート・リセット・閉じるまでの tick・求めた移り）。
#[test]
fn a_model_trace_of_the_planned_script_passes_every_check() {
    let t = full_trace().text();
    let r = judge_text(&t, &BOTH);
    assert!(!r.not_all_passed(), "{}", r.render());
    assert_eq!(r.episodes, (2, 1));
    for c in [
        "pause.state_range",
        "pause.transitions",
        "pause.gate",
        "pause.frames_reset",
        "pause.frames_vs_ticks",
        "pause.close_delay",
        "pause.coverage",
        "pause.first_moves",
        "end.state_range",
        "end.transitions",
        "end.gate",
        "end.frames_reset",
        "end.coverage",
        "final_screen",
    ] {
        assert_eq!(status(&r, c), Status::Pass, "{c}\n{}", r.render());
    }
}

/// 壊れたら落ちる: 表に無い移り（再開から確かめの「はい」へ飛ぶ）を見逃す。
#[test]
fn a_move_outside_the_table_fails() {
    let mut s = Sim::new();
    s.idle(5);
    s.open_pause();
    s.idle(10);
    s.pause = (4, 10);
    s.pause_flag = 1;
    s.last_key = None;
    s.idle(3);
    let r = judge_text(&s.text(), &Expect::default());
    assert_eq!(
        status(&r, "pause.transitions"),
        Status::Fail,
        "{}",
        r.render()
    );
}

/// 壊れたら落ちる: 範囲の外の状態（STATES 以上）を見逃す。
#[test]
fn a_state_out_of_range_fails() {
    let mut t = full_trace().text();
    // 最初の「状態 2」の行を 12 に書き換える。
    let line = t
        .lines()
        .find(|l| l.split(',').nth(20) == Some("2"))
        .unwrap()
        .to_string();
    let mut f: Vec<&str> = line.split(',').collect();
    f[20] = "12";
    t = t.replacen(&line, &f.join(","), 1);
    let r = judge_text(&t, &BOTH);
    assert_eq!(
        status(&r, "pause.state_range"),
        Status::Fail,
        "{}",
        r.render()
    );
}

/// 壊れたら落ちる: 読み始めの数え（`MENU_READ_FRAMES`）より前に入力が効いた移りを見逃す。実物が 4 でなく 2 から読む模型で起こす。
#[test]
fn an_input_taken_before_the_read_frames_fails_the_gate() {
    let mut s = Sim::new();
    s.read_frames = 2;
    s.idle(5);
    s.open_pause();
    s.idle(3);
    s.press(DOWN, 10);
    s.press(ESC, 30);
    let r = judge_text(&s.text(), &Expect::default());
    assert_eq!(status(&r, "pause.gate"), Status::Fail, "{}", r.render());
    // 同じ入力でも、実物の模型は数えが足りない間は受けない（状態が動かない）ので、ゲートは通る。
    let mut ok = Sim::new();
    ok.idle(5);
    ok.open_pause();
    ok.idle(1);
    ok.press(DOWN, 2);
    ok.idle(10);
    ok.press(DOWN, 10);
    ok.press(ESC, 30);
    let r = judge_text(&ok.text(), &Expect::default());
    assert_eq!(status(&r, "pause.gate"), Status::Pass, "{}", r.render());
    assert_eq!(
        status(&r, "pause.transitions"),
        Status::Pass,
        "{}",
        r.render()
    );
}

/// 壊れたら落ちる: 決定で数えがリセットされない（確かめの枠が出てすぐ読まれる）のを見逃す。
#[test]
fn a_decide_that_does_not_reset_the_frames_fails() {
    let mut s = Sim::new();
    s.reset_on_decide = false;
    s.idle(5);
    s.open_pause();
    s.idle(30);
    s.press(DOWN, 10);
    s.press(Z, 10);
    let r = judge_text(&s.text(), &Expect::default());
    assert_eq!(
        status(&r, "pause.frames_reset"),
        Status::Fail,
        "{}",
        r.render()
    );
}

/// 壊れたら落ちる: 確定から旗が下りるまでが 20 tick でない（長い・短い）のを見逃す。±1 は許す。
#[test]
fn the_close_delay_must_be_twenty_ticks() {
    for (frames, ok) in [(20, true), (19, true), (21, true), (30, false), (10, false)] {
        let mut s = Sim::new();
        s.close_frames = frames;
        s.idle(5);
        s.open_pause();
        s.idle(30);
        s.press(ESC, 40);
        let r = judge_text(&s.text(), &Expect::default());
        assert_eq!(
            status(&r, "pause.close_delay"),
            if ok { Status::Pass } else { Status::Fail },
            "閉じるまでの数え {frames}\n{}",
            r.render()
        );
    }
}

/// 壊れたら落ちる: 閉じるところまで読めていない（旗が立ったまま終わる）のに、一時停止を求めた回を合格にする。求めていない回の「測れていない」で
/// 終了コードを落とす（求めた検査だけで合否を決める）。
#[test]
fn an_unclosed_pause_is_not_measured_for_the_close_only_when_the_pause_was_asked_for() {
    let mut s = Sim::new();
    s.idle(5);
    s.open_pause();
    s.idle(30);
    let asked = Expect {
        pause: true,
        ..Expect::default()
    };
    let r = judge_text(&s.text(), &asked);
    assert_eq!(status(&r, "pause.close_delay"), Status::NotMeasured);
    assert!(r.not_all_passed());
    let r = judge_text(&s.text(), &Expect::default());
    assert_eq!(status(&r, "pause.close_delay"), Status::NotMeasured);
    assert!(!r.not_all_passed(), "{}", r.render());
    assert!(
        r.render()
            .contains("[測れていない・求めていない] pause.close_delay")
    );
}

/// 壊れたら落ちる: 求めていない区間の本物の不合格（表に無い移り）を黙って消す、または求めていない区間の不合格で求めたものの合否を決める。
/// 結果は出すが数えず、同じ記録を一時停止を求めて読めば落ちる。
#[test]
fn a_failure_in_a_section_nobody_asked_for_is_shown_but_does_not_decide() {
    let mut s = Sim::new();
    s.idle(5);
    s.open_pause();
    s.idle(10);
    s.pause = (4, 10);
    s.last_key = None;
    s.idle(3);
    let only_screen = Expect {
        final_screen: Some((6, 4)),
        ..Expect::default()
    };
    let r = judge_text(&s.text(), &only_screen);
    assert_eq!(status(&r, "pause.transitions"), Status::Fail);
    assert!(!r.failed() && !r.not_all_passed(), "{}", r.render());
    let asked = Expect {
        pause: true,
        ..only_screen
    };
    assert!(judge_text(&s.text(), &asked).failed());
    // 求めたものが落ちるときは、求めていない区間が合格でも落ちる。
    let wrong = Expect {
        final_screen: Some((7, 0)),
        ..Expect::default()
    };
    let r = judge_text(&full_trace().text(), &wrong);
    assert!(r.failed(), "{}", r.render());
}

/// 壊れたら落ちる: 開いた直後の下が効いた（読み始めの数えより前に入力を読んだ）のに、1 回目の移りの列が台本どおりだと言う。
/// 効いた版は 1→2 の後に 2→1・1→2・2→3 と続いて列が変わる。効かなかった版は、下を押さない記録と同じ列になる。
#[test]
fn an_early_input_that_took_effect_changes_the_first_pause_moves() {
    let first = Expect {
        pause_first: true,
        ..Expect::default()
    };
    let run = |read_frames: u32, early: bool, wait: u32| {
        let mut s = Sim::new();
        s.read_frames = read_frames;
        s.idle(5);
        s.open_pause();
        s.idle(wait);
        if early {
            s.press(DOWN, 30);
        } else {
            s.idle(30);
        }
        s.press(DOWN, 10);
        s.press(UP, 10);
        s.press(ESC, 30);
        judge_text(&s.text(), &first)
    };
    let ignored = run(MENU_READ_FRAMES, true, 2);
    assert_eq!(
        status(&ignored, "pause.first_moves"),
        Status::Pass,
        "{}",
        ignored.render()
    );
    assert_eq!(
        status(&run(MENU_READ_FRAMES, false, 2), "pause.first_moves"),
        Status::Pass
    );
    let took = run(2, true, 2);
    let f = took.find("pause.first_moves").unwrap();
    assert_eq!(f.status, Status::Fail, "{}", took.render());
    assert!(f.detail.contains("2→3"), "{}", f.detail);
    assert!(took.not_all_passed());
    // 読み始めを過ぎてから当たった「開いた直後の下」: 入力で動いた移りの前の数えは足りている（gate は通る）が、台本の想定（読み始め前に当たる）とは違うので列が変わる。
    let late = run(MENU_READ_FRAMES, true, 6);
    assert_eq!(
        status(&late, "pause.first_moves"),
        Status::Fail,
        "{}",
        late.render()
    );
    assert_eq!(
        status(&late, "pause.gate"),
        Status::Pass,
        "{}",
        late.render()
    );
}

/// 壊れたら落ちる: 1 回目の区間が無い（開かなかった）のに「1 回目の移りが台本どおり」を通す。閉じる呼びの途中で読んだ 3→0 の行は列に混ぜない。
#[test]
fn the_first_pause_moves_need_an_episode_and_ignore_a_closing_race_row() {
    let first = Expect {
        pause_first: true,
        ..Expect::default()
    };
    let mut none = Sim::new();
    none.idle(10);
    assert_eq!(
        status(&judge_text(&none.text(), &first), "pause.first_moves"),
        Status::NotMeasured
    );
    let mut s = Sim::new();
    s.idle(5);
    s.open_pause();
    s.idle(1);
    s.press(DOWN, 30);
    s.press(DOWN, 10);
    s.press(UP, 10);
    s.press(ESC, 19);
    let saved = s.pause;
    s.pause = (0, saved.1);
    s.last_key = None;
    s.record();
    s.pause = saved;
    s.tick(0);
    let r = judge_text(&s.text(), &first);
    assert_eq!(
        status(&r, "pause.first_moves"),
        Status::Pass,
        "{}",
        r.render()
    );
}

/// 壊れたら落ちる: 終わりのメニューの決定（4・5・6）の後、閉じる呼びの途中で読んだ行（旗が立ったまま状態が 0）を表の外の移りとして落とす。
#[test]
fn a_closing_race_row_of_the_end_menu_is_not_a_failure() {
    for state in [4u32, 5, 6] {
        let mut s = Sim::new();
        s.idle(3);
        s.open_end();
        s.idle(30);
        match state {
            4 => {}
            5 => s.press(DOWN, 10),
            _ => s.press(UP, 10),
        }
        s.press(Z, 10);
        assert_eq!(s.end.0, state, "模型が状態 {state} に入っていない");
        let saved = s.end;
        s.end = (0, saved.1);
        s.last_key = None;
        s.record();
        s.end = saved;
        s.end_flag = 0;
        s.last_key = None;
        s.record();
        let r = judge_text(&s.text(), &Expect::default());
        assert_eq!(
            status(&r, "end.transitions"),
            Status::Pass,
            "状態 {state}\n{}",
            r.render()
        );
    }
}

/// 壊れたら落ちる: 求めた移りが 1 つ欠けても通る（台本の押しが届かなかったのを見逃す）。確かめの枠で「はい」へ行かない記録で起こす。
#[test]
fn a_missing_required_move_fails_the_coverage_and_names_it() {
    // 2 回目の一時停止の「はい」の往復を省いた記録。
    let mut s = Sim::new();
    s.idle(40);
    s.open_pause();
    s.idle(30);
    s.press(DOWN, 10);
    s.press(UP, 10);
    s.press(ESC, 30);
    s.idle(20);
    s.open_pause();
    s.idle(30);
    s.press(DOWN, 10);
    s.press(Z, 10);
    s.press(Z, 10);
    s.idle(5);
    let r = judge_text(
        &s.text(),
        &Expect {
            pause: true,
            ..Expect::default()
        },
    );
    let f = r.find("pause.coverage").unwrap();
    assert_eq!(f.status, Status::Fail, "{}", r.render());
    assert!(f.detail.contains("5→4"), "{}", f.detail);
    assert!(f.detail.contains("4→5"), "{}", f.detail);
    assert!(!f.detail.contains("1→2"), "{}", f.detail);
}

/// 壊れたら落ちる: 終わりのメニューの上下が循環しない記録（下を 1 周しか通っていない）を、全部見えたことにする。
#[test]
fn the_end_menu_needs_both_cycles() {
    let mut s = Sim::new();
    s.idle(5);
    s.open_end();
    s.idle(30);
    for k in [DOWN, DOWN, DOWN] {
        s.press(k, 10);
    }
    s.press(Z, 30);
    let r = judge_text(
        &s.text(),
        &Expect {
            end: true,
            ..Expect::default()
        },
    );
    let f = r.find("end.coverage").unwrap();
    assert_eq!(f.status, Status::Fail, "{}", r.render());
    assert!(f.detail.contains("1→3"), "{}", f.detail);
    assert!(!f.detail.contains("1→2"), "{}", f.detail);
}

/// 壊れたら落ちる: 区間が 1 つも無いのに「求めた」を通す（母数 0 の合格）。
#[test]
fn a_trace_without_any_menu_is_not_measured() {
    let mut s = Sim::new();
    s.idle(10);
    let r = judge_text(&s.text(), &BOTH);
    assert_eq!(status(&r, "pause.coverage"), Status::NotMeasured);
    assert_eq!(status(&r, "end.coverage"), Status::NotMeasured);
    assert!(r.not_all_passed());
    // 求めなければ落とさない。
    let r = judge_text(&s.text(), &Expect::default());
    assert!(!r.not_all_passed(), "{}", r.render());
}

/// 壊れたら落ちる: 数えが tick より速く進む（1 tick に 2 回の更新）のを見逃す。
#[test]
fn frames_faster_than_ticks_fail() {
    let mut s = Sim::new();
    s.idle(5);
    s.open_pause();
    s.idle(5);
    // 1 tick の間に数えが 10 進んだ行を足す。
    let last = s.lines.last().unwrap().clone();
    let mut f: Vec<String> = last.split(',').map(str::to_string).collect();
    f[1] = (s.seq + 1).to_string();
    f[0] = f[1].clone();
    let frames: u32 = f[21].parse().unwrap();
    f[21] = (frames + 10).to_string();
    s.lines.push(f.join(","));
    let r = judge_text(&s.text(), &Expect::default());
    assert_eq!(
        status(&r, "pause.frames_vs_ticks"),
        Status::Fail,
        "{}",
        r.render()
    );
}

/// 壊れたら落ちる: 最後の画面・カーソルが違う（戻り道が着いていない）のを通す。
#[test]
fn the_final_screen_must_match() {
    let mut s = full_trace();
    s.cursor = 3;
    s.last_key = None;
    s.idle(1);
    let r = judge_text(&s.text(), &BOTH);
    assert_eq!(status(&r, "final_screen"), Status::Fail, "{}", r.render());
}

/// 壊れたら落ちる: 確かめの枠の「はい」（状態 4 → 8）からタイトルへ出るまでの閉じ方（旗が下りるまで 20 tick・状態 0）を見ない。
#[test]
fn leaving_by_the_confirm_box_closes_after_twenty_ticks_too() {
    let mut s = Sim::new();
    s.idle(5);
    s.open_pause();
    s.idle(30);
    s.press(DOWN, 10);
    s.press(Z, 10);
    s.press(UP, 10);
    s.press(Z, 40);
    assert_eq!(s.pause_flag, 0, "模型が閉じていない");
    let r = judge_text(&s.text(), &Expect::default());
    assert_eq!(
        status(&r, "pause.close_delay"),
        Status::Pass,
        "{}",
        r.render()
    );
    assert_eq!(
        status(&r, "pause.transitions"),
        Status::Pass,
        "{}",
        r.render()
    );
    assert_eq!(
        status(&r, "pause.frames_reset"),
        Status::Pass,
        "{}",
        r.render()
    );
    assert!(
        s.lines.iter().any(|l| l.split(',').nth(20) == Some("8")),
        "状態 8 の行が無い"
    );
}

/// 壊れたら落ちる: 閉じる呼びの途中で読んだ行（旗が立ったまま状態が 0）や、旗が立った直後の前の値が残る行を、表の外の移りとして落とす。
#[test]
fn a_race_row_while_closing_and_a_stale_first_row_are_not_failures() {
    let mut s = Sim::new();
    s.idle(3);
    // 旗が立った最初の行は前の区間の値が残る（状態 4・数え 21）。メニューの関数が走ると状態 0 から始まる。
    s.pause = (4, 21);
    s.open_pause();
    s.pause = (0, 0);
    s.idle(30);
    s.press(ESC, 19);
    // 閉じる呼びの途中: 状態は 0 にしたが旗はまだ 1。
    let saved = s.pause;
    s.pause = (0, saved.1);
    s.last_key = None;
    s.record();
    s.pause = saved;
    s.tick(0);
    assert_eq!(s.pause_flag, 0, "模型が閉じていない");
    let r = judge_text(&s.text(), &Expect::default());
    assert_eq!(
        status(&r, "pause.transitions"),
        Status::Pass,
        "{}",
        r.render()
    );
    assert_eq!(
        status(&r, "pause.frames_reset"),
        Status::Pass,
        "{}",
        r.render()
    );
}

/// 壊れたら落ちる: 読めない行（空の欄）を無視して合格にする。
#[test]
fn unreadable_rows_fail() {
    let t = full_trace().text();
    let line = t.lines().nth(5).unwrap().to_string();
    let mut f: Vec<&str> = line.split(',').collect();
    f[20] = "";
    let t = t.replacen(&line, &f.join(","), 1);
    let r = judge_text(&t, &BOTH);
    assert_eq!(status(&r, "pause.readable"), Status::Fail, "{}", r.render());
}

/// 壊れたら落ちる: 見出しから照らすのに要る列が消える、見出しの欄の数とこの検査の模型の行の欄の数（25）がずれる。
#[test]
fn the_header_names_every_column_the_reader_needs() {
    let names: Vec<&str> = MENU_CSV_HEADER.split(',').collect();
    assert_eq!(names.len(), 25);
    for c in COLUMNS {
        assert!(names.contains(&c), "{c}");
    }
    assert!(
        parse_menu_csv(&format!("{MENU_CSV_HEADER}\n"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_reader_refuses_old_or_broken_files() {
    assert_eq!(parse_menu_csv("").unwrap_err(), ParseError::NoHeader);
    let old = "seq_before,seq_after,screen_id\n1,1,6\n";
    assert!(matches!(
        parse_menu_csv(old).unwrap_err(),
        ParseError::MissingColumn(_)
    ));
    let t = full_trace().text();
    let broken = t.replacen(",0,0x00000000,", ",0,", 1);
    assert!(matches!(
        parse_menu_csv(&broken).unwrap_err(),
        ParseError::Width { .. }
    ));
    let first = t
        .lines()
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_string();
    let text_in_number = t.replacen(&format!("\n{first},{first},"), &format!("\nx,{first},"), 1);
    assert!(
        matches!(
            parse_menu_csv(&text_in_number).unwrap_err(),
            ParseError::BadNumber {
                column: "seq_before",
                ..
            }
        ),
        "{:?}",
        parse_menu_csv(&text_in_number).err()
    );
    // 見出しの前に別の行があっても、見出しの列の並びで読む。
    let swapped = HEADER.replace("pause_flag,pause_state", "pause_state,pause_flag");
    let row = csv_line(1, 6, 4, [2, 1, 3, 0, 0, 0]);
    let rows = parse_menu_csv(&format!("; 注\n{swapped}\n{row}\n")).unwrap();
    // 見出しで 19 番目が pause_state、20 番目が pause_flag になったので、値は入れ替わって読まれる。
    assert_eq!(rows[0].pause.state, Some(2));
    assert_eq!(rows[0].pause.flag, Some(1));
}

#[test]
fn the_report_renders_every_finding() {
    let r = judge_text(&full_trace().text(), &BOTH);
    let text = r.render();
    assert_eq!(text.lines().count(), r.findings.len());
    assert!(text.contains("[合格] pause.close_delay"));
}
