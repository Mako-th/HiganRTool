//! fps表示の測り（pure）。
//! 描画のたびに数を足し、フレームを始めた時刻（QPC）の間隔が500 ms以上になったら「数 ÷ 間隔」を出す。
//! ゲーム自身の測りは描画した時刻をmsで読む。そのため描画までの長さの揺れがそのまま表示に出る。始めた時刻なら格子の刻みのまま測れる。
//! 状態はゲームのスレッドだけが持つ。呼ぶ側が時刻・描画を飛ばす設定・QPCの周波数を渡す。
#![forbid(unsafe_code)]

/// 表示を作る間隔（ms）。ゲームの`0x43171D cmp $0x1f4`と同じ。
pub const WINDOW_MS: u64 = 500;

/// 基点（前に表示を作ったフレームを始めた時刻）と、その後の描画の数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FpsMeter {
    /// QPC。0は基点が無い（呼ぶ側が時刻0を渡さないので値としては現れない）。
    base: u64,
    count: u32,
}

impl FpsMeter {
    pub const fn new() -> Self {
        FpsMeter { base: 0, count: 0 }
    }

    /// 保存した状態から戻す（`state`の逆）。アトミック変数に保存した状態を読み出すのに使う。
    pub const fn restore(base: u64, count: u32) -> Self {
        FpsMeter { base, count }
    }

    pub const fn state(&self) -> (u64, u32) {
        (self.base, self.count)
    }

    /// 描画1回ぶん。`start`はこのフレームを始めた時刻（QPC）、`skip`は描画を飛ばす設定（ゲームは1 + `skip`を足す）、`qpf`はQPCの周波数。
    /// 表示を作るならfpsを返し、基点を`start`に・数を0にする。
    /// 時刻0・周波数0は測れないので状態を変えない。基点が無いか`start`が基点より前なら基点を置き直すだけ（数は基点より後の描画から数える）。
    pub fn on_render(&mut self, start: u64, skip: u32, qpf: u64) -> Option<f32> {
        if start == 0 || qpf == 0 {
            return None;
        }
        if self.base == 0 || start < self.base {
            self.base = start;
            self.count = 0;
            return None;
        }
        self.count = self.count.saturating_add(1).saturating_add(skip);
        let elapsed = start - self.base;
        // elapsed / qpf >= WINDOW_MS / 1000を割らずにu128で比べる。
        if u128::from(elapsed) * 1000 < u128::from(WINDOW_MS) * u128::from(qpf) {
            return None;
        }
        let fps = (f64::from(self.count) * qpf as f64 / elapsed as f64) as f32;
        self.base = start;
        self.count = 0;
        Some(fps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QPF: u64 = 10_000_000;
    const T0: u64 = 123_456_789;

    /// 60 fpsの格子のi番目のフレームの開始（QPC。端数は切り捨て）。
    fn grid(i: u64) -> u64 {
        T0 + i * QPF / 60
    }

    /// 開始の列を渡して、表示を作ったフレームの番号とfpsを返す。
    fn run(starts: impl Iterator<Item = u64>, skip: u32) -> Vec<(usize, f32)> {
        let mut m = FpsMeter::new();
        starts
            .enumerate()
            .filter_map(|(i, s)| m.on_render(s, skip, QPF).map(|f| (i, f)))
            .collect()
    }

    #[test]
    fn a_steady_60_fps_grid_shows_60_every_30_frames() {
        let shown = run((0..=300).map(grid), 0);
        // 0番目は基点を置くだけ。30番目で500 msに届き、以後30フレームごと。
        let at: Vec<usize> = shown.iter().map(|s| s.0).collect();
        assert_eq!(at, (1..=10).map(|k| k * 30).collect::<Vec<_>>());
        for (i, f) in shown {
            assert_eq!(format!("{f:.2}"), "60.00", "フレーム{i}: {f}");
        }
    }

    #[test]
    fn start_jitter_of_twelve_microseconds_still_reads_60() {
        // 開始が最大12 µs（120 tick）揺れても500 msの間隔の誤差は24 µs以下。60.00の表示に収まる。
        let jittered = (0..=300u64).map(|i| grid(i) + i % 7 * 20);
        for (i, f) in run(jittered, 0) {
            assert_eq!(format!("{f:.2}"), "60.00", "フレーム{i}: {f}");
        }
    }

    #[test]
    fn one_late_frame_at_the_end_of_a_window_shows_below_60() {
        // 実際に始めた時刻で測る。5 msの遅れは遅れた500 msの区間の表示に出る（30 ÷ 0.505 s）。
        let late = (0..=30u64).map(|i| if i == 30 { grid(i) + 50_000 } else { grid(i) });
        let shown = run(late, 0);
        assert_eq!(shown.len(), 1);
        assert_eq!(format!("{:.2}", shown[0].1), "59.41");
    }

    #[test]
    fn the_skip_setting_adds_to_the_count_like_the_game() {
        // 描画を1つ飛ばす設定で2フレームに1回だけ描く。1回の描画が2フレームを表すので60 fps。
        let every_other = (0..=150u64).map(|i| grid(i * 2));
        for (i, f) in run(every_other, 1) {
            assert_eq!(format!("{f:.2}"), "60.00", "描画{i}: {f}");
        }
        // 設定を足さなければ半分に読める（足し算が効いている）。
        let every_other = (0..=150u64).map(|i| grid(i * 2));
        for (_, f) in run(every_other, 0) {
            assert_eq!(format!("{f:.2}"), "30.00");
        }
    }

    #[test]
    fn the_window_opens_at_exactly_500_ms() {
        let half = QPF / 2;
        let mut m = FpsMeter::new();
        assert_eq!(m.on_render(T0, 0, QPF), None);
        assert_eq!(m.on_render(T0 + half - 1, 0, QPF), None, "1 tick 手前");
        assert_eq!(m.state().1, 1, "手前では数を足すだけ");
        let f = m.on_render(T0 + half, 0, QPF).expect("ちょうど500 ms");
        assert_eq!(f, 4.0, "2回 ÷ 0.5 s");
    }

    #[test]
    fn showing_resets_the_count_and_the_base() {
        let mut m = FpsMeter::new();
        m.on_render(T0, 0, QPF);
        for i in 1..30 {
            assert_eq!(m.on_render(grid(i), 0, QPF), None);
        }
        assert!(m.on_render(grid(30), 0, QPF).is_some());
        assert_eq!(m.state(), (grid(30), 0));
    }

    #[test]
    fn the_first_frame_and_a_start_before_the_base_only_set_the_base() {
        let mut m = FpsMeter::new();
        assert_eq!(m.on_render(T0, 5, QPF), None);
        assert_eq!(m.state(), (T0, 0), "最初のフレームの数は足さない");
        m.on_render(T0 + 10, 0, QPF);
        assert_eq!(m.on_render(T0 - 1, 0, QPF), None);
        assert_eq!(m.state(), (T0 - 1, 0), "時刻が戻ったら基点を置き直す");
    }

    #[test]
    fn zero_time_or_zero_frequency_changes_nothing() {
        let mut m = FpsMeter::restore(T0, 7);
        assert_eq!(m.on_render(0, 0, QPF), None);
        assert_eq!(m.on_render(T0 + QPF, 0, 0), None);
        assert_eq!(m.state(), (T0, 7));
    }

    #[test]
    fn state_round_trips_and_the_count_saturates() {
        let m = FpsMeter::restore(9, u32::MAX);
        assert_eq!(FpsMeter::restore(m.state().0, m.state().1), m);
        let mut m = FpsMeter::restore(T0, u32::MAX);
        let f = m.on_render(T0 + QPF, u32::MAX, QPF).expect("1 s");
        assert!(f.is_finite() && f > 4.0e9, "{f}");
    }

    #[test]
    fn a_long_gap_averages_over_the_gap() {
        // 読み込みなどで2 s止まった後は2 sの間の数で割る（ゲームの測りと同じ）。
        let mut m = FpsMeter::new();
        m.on_render(T0, 0, QPF);
        m.on_render(T0 + 1_000, 0, QPF);
        let f = m.on_render(T0 + 2 * QPF, 0, QPF).expect("2 s");
        assert_eq!(f, 1.0, "2回 ÷ 2 s");
    }
}
