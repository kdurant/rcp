//! 进度渲染。三种形态：
//!   * 终端：三行动态进度条（当前文件 / 当前文件字节 / 总量字节），画到 stderr
//!   * 管道 + `--progress=always`：退化成每秒一行的快照（不写 ANSI 转义）
//!   * 关闭：所有方法都是空操作

use crate::cli::ProgressWhen;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::cell::{Cell, RefCell};
use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

const SPINNER_TICK: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
const CUR_TEMPLATE: &str = "{spinner:.cyan} {msg}";
const BAR_TEMPLATE: &str =
    "    {bytes:>12}/{total_bytes:<12} [{bar:32.cyan/blue}] {percent:>3}% {binary_bytes_per_sec:>14} ETA {eta}";
const TOTAL_TEMPLATE: &str =
    "total {bytes:>12}/{total_bytes:<12} [{bar:32.cyan/blue}] {percent:>3}% {binary_bytes_per_sec:>14} ETA {eta}";
const PLAIN_TEMPLATE: &str = "    {msg:.dim}";

/// 渲染模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 动态进度条
    Bars,
    /// 每秒一行文本（非 TTY 下被 --progress=always 强制打开时）
    Lines,
    Off,
}

pub fn mode(when: ProgressWhen, quiet: bool) -> Mode {
    if quiet {
        return Mode::Off;
    }
    match when {
        ProgressWhen::Never => Mode::Off,
        ProgressWhen::Always => {
            if std::io::stderr().is_terminal() {
                Mode::Bars
            } else {
                Mode::Lines
            }
        }
        ProgressWhen::Auto => {
            if std::io::stderr().is_terminal() {
                Mode::Bars
            } else {
                Mode::Off
            }
        }
    }
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|e| panic!("invalid progress template: {e}"))
        .tick_chars(SPINNER_TICK)
}

fn spinner_style() -> ProgressStyle {
    style(CUR_TEMPLATE).progress_chars("█░")
}

fn draw_target() -> ProgressDrawTarget {
    ProgressDrawTarget::stderr_with_hz(20)
}

extern "C" fn on_sigint(_sig: libc::c_int) {
    // 只做 async-signal-safe 的事：补一个换行，别让光标停在进度条中间。
    unsafe {
        let nl = b"\n";
        libc::write(2, nl.as_ptr() as *const libc::c_void, 1);
        libc::_exit(130);
    }
}

extern "C" fn on_sigterm(_sig: libc::c_int) {
    unsafe {
        let nl = b"\n";
        libc::write(2, nl.as_ptr() as *const libc::c_void, 1);
        libc::_exit(143);
    }
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }
}

/// 简单的节流器。
struct Throttle {
    last: Cell<Option<Instant>>,
    interval: Duration,
}

impl Throttle {
    fn new(interval: Duration) -> Self {
        Throttle {
            last: Cell::new(None),
            interval,
        }
    }

    fn ready(&self) -> bool {
        match self.last.get() {
            None => true,
            Some(t) => t.elapsed() >= self.interval,
        }
    }

    fn reset(&self) {
        self.last.set(Some(Instant::now()));
    }
}

pub struct Progress {
    bars: Option<Bars>,
    lines: Option<Lines>,
}

struct Bars {
    multi: MultiProgress,
    /// 当前文件名
    cur: ProgressBar,
    /// 当前文件的字节进度
    bar: ProgressBar,
    /// 总字节进度
    total: ProgressBar,
    total_len: Cell<u64>,
}

struct Lines {
    total_len: Cell<u64>,
    pos: Cell<u64>,
    window_bytes: Cell<u64>,
    window_start: Cell<Instant>,
    throttle: Throttle,
    cur: RefCell<String>,
}

impl Lines {
    fn new(total_len: u64) -> Self {
        Lines {
            total_len: Cell::new(total_len.max(1)),
            pos: Cell::new(0),
            window_bytes: Cell::new(0),
            window_start: Cell::new(Instant::now()),
            throttle: Throttle::new(Duration::from_millis(1000)),
            cur: RefCell::new(String::new()),
        }
    }

    fn rate(&self) -> f64 {
        let secs = self.window_start.get().elapsed().as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.window_bytes.get() as f64 / secs
        }
    }

    fn render(&self) {
        let pos = self.pos.get();
        let len = self.total_len.get();
        // len 始终 >= 1（构造与 skip 都做了 max(1)）
        let pct = (pos.saturating_mul(100) / len).min(100);
        let rate = self.rate();
        let eta = if rate > 0.0 {
            let remain = len.saturating_sub(pos) as f64 / rate;
            human_duration(remain)
        } else {
            "--".to_string()
        };
        eprintln!(
            "rcp: [{pct:>3}%] {:>12} / {:<12} {:>12}  ETA {:<6} {}",
            human_bytes(pos),
            human_bytes(len),
            human_rate(rate),
            eta,
            self.cur.borrow()
        );
        // 重新开一个速率窗口
        self.window_start.set(Instant::now());
        self.window_bytes.set(0);
    }

    fn inc(&self, n: u64) {
        self.pos.set(self.pos.get() + n);
        self.window_bytes.set(self.window_bytes.get() + n);
        if self.throttle.ready() {
            self.throttle.reset();
            self.render();
        }
    }

    fn skip(&self, n: u64) {
        self.total_len
            .set(self.total_len.get().saturating_sub(n).max(1));
        if self.pos.get() > self.total_len.get() {
            self.pos.set(self.total_len.get());
        }
    }
}

impl Progress {
    pub fn new(mode: Mode, total_bytes: u64) -> Self {
        match mode {
            Mode::Off => Progress {
                bars: None,
                lines: None,
            },
            Mode::Lines => Progress {
                bars: None,
                lines: Some(Lines::new(total_bytes)),
            },
            Mode::Bars => {
                install_signal_handlers();
                let multi = MultiProgress::with_draw_target(draw_target());
                let cur = multi.add(ProgressBar::new(0).with_style(spinner_style()));
                let bar = multi.add(ProgressBar::new(0).with_style(style(PLAIN_TEMPLATE)));
                let total_len = total_bytes.max(1);
                let total =
                    multi.add(ProgressBar::new(total_len).with_style(style(TOTAL_TEMPLATE)));
                for b in [&cur, &bar, &total] {
                    b.enable_steady_tick(Duration::from_millis(100));
                }
                bar.set_message("starting...");
                Progress {
                    bars: Some(Bars {
                        multi,
                        cur,
                        bar,
                        total,
                        total_len: Cell::new(total_len),
                    }),
                    lines: None,
                }
            }
        }
    }

    pub fn is_on(&self) -> bool {
        self.bars.is_some() || self.lines.is_some()
    }

    /// 条目开始时更新"当前条目"文案。
    pub fn set_current(&self, idx: u64, total: u64, src: &Path) {
        let text = format!("{}   ({}/{})", src.display(), idx, total);
        if let Some(b) = &self.bars {
            b.cur.set_message(text);
        } else if let Some(l) = &self.lines {
            *l.cur.borrow_mut() = text;
        }
    }

    /// 当前条目有确定字节数。
    pub fn begin_bytes(&self, len: u64) {
        if let Some(b) = &self.bars {
            b.bar.set_style(style(BAR_TEMPLATE));
            b.bar.set_length(len.max(1));
            b.bar.set_position(0);
        }
    }

    /// 当前条目没有字节数（目录 / 符号链接 / 硬链接 / 特殊文件）。
    pub fn begin_plain(&self, msg: impl Into<String>) {
        if let Some(b) = &self.bars {
            b.bar.set_style(style(PLAIN_TEMPLATE));
            b.bar.set_length(1);
            b.bar.set_position(0);
            b.bar.set_message(msg.into());
        }
    }

    pub fn inc(&self, n: u64) {
        if let Some(b) = &self.bars {
            b.bar.inc(n);
            let pos = b.total.position() + n;
            b.total.set_position(pos.min(b.total_len.get()));
        } else if let Some(l) = &self.lines {
            l.inc(n);
        }
    }

    pub fn end_bytes(&self) {
        if let Some(b) = &self.bars {
            if let Some(len) = b.bar.length() {
                b.bar.set_position(len);
            }
        }
    }

    /// 条目被跳过：把总量相应缩小，避免进度条永远到不了 100%。
    pub fn skip_bytes(&self, n: u64) {
        if let Some(b) = &self.bars {
            let new = b.total_len.get().saturating_sub(n).max(1);
            b.total_len.set(new);
            b.total.set_length(new);
            if b.total.position() > new {
                b.total.set_position(new);
            }
        } else if let Some(l) = &self.lines {
            l.skip(n);
        }
    }

    /// 在进度条上方安全打印一行。
    pub fn println(&self, msg: impl AsRef<str>) {
        match &self.bars {
            Some(b) => {
                let _ = b.multi.println(msg.as_ref());
            }
            None => eprintln!("{}", msg.as_ref()),
        }
    }

    /// 暂停刷新执行交互（`-i` 询问用）。
    pub fn suspend<R>(&self, f: impl FnOnce() -> R) -> R {
        match &self.bars {
            Some(b) => b.multi.suspend(f),
            None => f(),
        }
    }

    /// 复制结束：收尾进度显示，并按需打印汇总。
    ///
    /// 注意必须停掉 steady tick 线程（`disable_steady_tick`）并丢弃进度条，
    /// 否则后台线程会在汇总行之后把进度条又画回屏幕上。
    pub fn finish(&mut self, summary: Option<&str>) {
        if let Some(b) = self.bars.take() {
            b.cur.disable_steady_tick();
            b.bar.disable_steady_tick();
            b.total.disable_steady_tick();
            b.total.set_position(b.total_len.get());
            let _ = b.multi.clear();
        } else if let Some(l) = self.lines.take() {
            l.pos.set(l.total_len.get());
            l.render();
        }
        if let Some(s) = summary {
            eprintln!("rcp: {s}");
        }
    }
}

/// 扫描阶段的进度提示。
pub struct ScanProgress {
    bars: Option<(MultiProgress, ProgressBar)>,
    lines: bool,
    throttle: Throttle,
}

impl ScanProgress {
    pub fn new(mode: Mode) -> Self {
        match mode {
            Mode::Off => ScanProgress {
                bars: None,
                lines: false,
                throttle: Throttle::new(Duration::from_secs(1)),
            },
            Mode::Lines => ScanProgress {
                bars: None,
                lines: true,
                throttle: Throttle::new(Duration::from_secs(1)),
            },
            Mode::Bars => {
                let multi = MultiProgress::with_draw_target(draw_target());
                let pb = multi.add(ProgressBar::new(0).with_style(spinner_style()));
                pb.enable_steady_tick(Duration::from_millis(100));
                pb.set_message("scanning...");
                ScanProgress {
                    bars: Some((multi, pb)),
                    lines: false,
                    throttle: Throttle::new(Duration::from_secs(1)),
                }
            }
        }
    }

    pub fn tick(&self, items: u64, bytes: u64) {
        if let Some((_, pb)) = &self.bars {
            pb.set_message(format!(
                "scanning... {} entries, {}",
                items,
                human_bytes(bytes)
            ));
        } else if self.lines && self.throttle.ready() {
            self.throttle.reset();
            eprintln!("rcp: scanning... {items} entries, {}", human_bytes(bytes));
        }
    }

    pub fn finish(&self) {
        if let Some((multi, pb)) = &self.bars {
            pb.disable_steady_tick();
            pb.finish_and_clear();
            let _ = multi.clear();
        }
    }
}

/// 二进制单位的人类可读字节数。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// 传输速率文本。
pub fn human_rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec <= 0.0 {
        return "--".to_string();
    }
    format!("{}/s", human_bytes(bytes_per_sec as u64))
}

/// 时长文本。
pub fn human_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--".to_string();
    }
    let s = secs as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(4 * 1024 * 1024 * 1024), "4.00 GiB");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(human_duration(3.0), "3s");
        assert_eq!(human_duration(65.0), "1m05s");
        assert_eq!(human_duration(3700.0), "1h01m");
    }

    #[test]
    fn templates_are_valid() {
        for t in [CUR_TEMPLATE, BAR_TEMPLATE, TOTAL_TEMPLATE, PLAIN_TEMPLATE] {
            ProgressStyle::with_template(t).expect("template");
        }
    }
}
