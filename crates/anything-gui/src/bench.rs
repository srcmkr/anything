//! Scripted measurement run (`ANYTHING_BENCH=1`).
//!
//! The view calls [`Bench::next_action`] once per frame before drawing and
//! [`Bench::painted`] after every drawn frame. A latency runs from the moment
//! an action is handed out to the end of the first frame that shows its
//! result.

use anything_core::SortKey;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Action {
    SetQuery(String),
    Sort(SortKey, bool),
    /// Scroll down by this many pixels.
    ScrollBy(f32),
    /// Jump to this row, like dragging the scroll bar.
    JumpTo(usize),
    /// Hide and show the window this many times, measuring each show.
    ShowCycles(usize),
    Quit,
}

enum Step {
    Measure(&'static str, Action),
    Scroll { label: &'static str, frames: usize, pixels: f32 },
    Jumps { label: &'static str, frames: usize },
    Pause(usize),
}

fn script() -> Vec<Step> {
    use Step::*;
    let mut steps = vec![Pause(30)];
    for typed in ["rechnung *.pdf", "Rechnung*.pdf"] {
        for end in typed.char_indices().map(|(i, c)| i + c.len_utf8()) {
            steps.push(Measure("type", Action::SetQuery(typed[..end].to_owned())));
            steps.push(Pause(3));
        }
        for end in (0..typed.len()).rev() {
            steps.push(Measure("delete", Action::SetQuery(typed[..end].to_owned())));
            steps.push(Pause(3));
        }
    }
    for (key, desc) in [
        (SortKey::Size, true),
        (SortKey::Modified, false),
        (SortKey::Path, false),
        (SortKey::Name, false),
    ] {
        steps.push(Measure("sort 5M", Action::Sort(key, desc)));
        steps.push(Pause(3));
    }
    // Optionally scroll through a different result, e.g. one with previews.
    if let Ok(query) = std::env::var("ANYTHING_BENCH_QUERY") {
        steps.push(Measure("type", Action::SetQuery(query)));
        steps.push(Pause(60));
    }
    steps.push(Scroll { label: "scroll 120 px/frame", frames: 600, pixels: 120.0 });
    steps.push(Pause(10));
    steps.push(Jumps { label: "scroll bar jumps", frames: 240 });
    steps.push(Pause(10));
    steps.push(Measure("show", Action::ShowCycles(20)));
    steps.push(Pause(10));
    steps
}

pub struct Bench {
    rss_before_ui: u64,
    rss_first_paint: u64,
    ui_start: Instant,
    first_paint: Option<Duration>,
    steps: std::vec::IntoIter<Step>,
    current: Option<Step>,
    progress: usize,
    /// Label, start, and whether the result is on its way to the screen.
    pending: Option<(&'static str, Instant, bool)>,
    busy: bool,
    latencies: Vec<(&'static str, Duration)>,
    last_paint: Option<Instant>,
    frame_label: Option<&'static str>,
    frames: Vec<(&'static str, Duration)>,
    jump_seed: u64,
    rows: usize,
}

impl Bench {
    pub fn enabled() -> bool {
        std::env::var_os("ANYTHING_BENCH").is_some()
    }

    /// Call right before the toolkit is initialised.
    pub fn new() -> Self {
        Self {
            rss_before_ui: rss_kib(),
            rss_first_paint: 0,
            ui_start: Instant::now(),
            first_paint: None,
            steps: script().into_iter(),
            current: None,
            progress: 0,
            pending: None,
            busy: false,
            latencies: Vec::new(),
            last_paint: None,
            frame_label: None,
            frames: Vec::new(),
            jump_seed: 7,
            rows: 0,
        }
    }

    pub fn set_rows(&mut self, rows: usize) {
        self.rows = rows;
    }

    /// The result of the pending action has been applied and will show in
    /// the next frame.
    pub fn result_applied(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.2 = true;
        }
    }

    /// Records a latency measured elsewhere, e.g. a show cycle.
    pub fn record(&mut self, label: &'static str, latency: Duration) {
        self.latencies.push((label, latency));
    }

    /// An asynchronous step (show cycles) finished.
    pub fn step_done(&mut self) {
        self.busy = false;
    }

    pub fn next_action(&mut self) -> Option<Action> {
        if self.first_paint.is_none() || self.pending.is_some() || self.busy {
            return None;
        }
        loop {
            if self.current.is_none() {
                self.current = Some(self.steps.next().unwrap_or(Step::Measure("", Action::Quit)));
                self.progress = 0;
            }
            match self.current.as_mut().unwrap() {
                Step::Pause(frames) => {
                    if self.progress < *frames {
                        self.progress += 1;
                        self.frame_label = None;
                        return None;
                    }
                }
                Step::Measure(label, action) => {
                    let (label, action) = (*label, action.clone());
                    self.current = None;
                    match action {
                        Action::Quit => {}
                        Action::ShowCycles(_) => self.busy = true,
                        // Searches (also re-sorting) run in the background and
                        // report back.
                        Action::SetQuery(_) | Action::Sort(..) => self.pending = Some((label, Instant::now(), false)),
                        _ => self.pending = Some((label, Instant::now(), true)),
                    }
                    return Some(action);
                }
                Step::Scroll { label, frames, pixels } => {
                    if self.progress < *frames {
                        self.progress += 1;
                        self.frame_label = Some(label);
                        return Some(Action::ScrollBy(*pixels));
                    }
                }
                Step::Jumps { label, frames } => {
                    if self.progress < *frames {
                        self.progress += 1;
                        self.frame_label = Some(label);
                        self.jump_seed =
                            self.jump_seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let row = (self.jump_seed >> 33) as usize % self.rows.max(1);
                        return Some(Action::JumpTo(row));
                    }
                }
            }
            self.current = None;
            self.frame_label = None;
        }
    }

    /// Call at the end of every drawn frame.
    pub fn painted(&mut self) {
        let now = Instant::now();
        if self.first_paint.is_none() {
            self.first_paint = Some(now - self.ui_start);
            self.rss_first_paint = rss_kib();
        }
        if let Some((label, start, true)) = self.pending {
            self.latencies.push((label, now - start));
            self.pending = None;
        }
        if let (Some(label), Some(last)) = (self.frame_label, self.last_paint) {
            self.frames.push((label, now - last));
        }
        self.last_paint = Some(now);
    }

    pub fn report(&self) -> String {
        let mut out = String::from("== anything ==\n");
        out += &format!(
            "first frame after toolkit start: {:.1} ms\n",
            ms(self.first_paint.unwrap_or_default())
        );
        out += &format!(
            "RSS: before UI {} MB, first frame {} MB, end {} MB\n",
            self.rss_before_ui >> 10,
            self.rss_first_paint >> 10,
            rss_kib() >> 10
        );
        for label in ["type", "delete", "sort 5M", "show"] {
            let mut v: Vec<f64> =
                self.latencies.iter().filter(|(l, _)| *l == label).map(|(_, d)| ms(*d)).collect();
            if v.is_empty() {
                continue;
            }
            v.sort_by(f64::total_cmp);
            out += &format!(
                "{label:>8}: n={:>2} median {:>6.1} ms  max {:>6.1} ms\n",
                v.len(),
                v[v.len() / 2],
                v[v.len() - 1]
            );
        }
        for label in ["scroll 120 px/frame", "scroll bar jumps"] {
            let mut v: Vec<f64> =
                self.frames.iter().filter(|(l, _)| *l == label).map(|(_, d)| ms(*d)).collect();
            if v.is_empty() {
                continue;
            }
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            v.sort_by(f64::total_cmp);
            let median = v[v.len() / 2];
            let late = v.iter().filter(|&&d| d > median * 1.5).count();
            out += &format!(
                "{label}: {} frames, {:.0} fps avg, median {:.2} ms, p99 {:.2} ms, max {:.2} ms, {} late (>1.5x median)\n",
                v.len(),
                1000.0 / mean,
                median,
                v[(v.len() * 99) / 100],
                v[v.len() - 1],
                late
            );
        }
        out
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}
