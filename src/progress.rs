//! A one-line progress display for dstore transfers.
//!
//! It is drawn only where jj draws its own progress (`Ui::progress_output`: stderr is a terminal,
//! progress is enabled, and `--quiet` is off), overwritten in place and erased when the transfer
//! ends; the commands print their summaries through `ui.status()` either way.

use std::io::{self, Stderr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use dstore_client::{Progress, ProgressReport, human_bytes, rate};
use jj_cli::ui::{ProgressOutput, Ui};

/// Redraws at most this often; the last report of a phase is always drawn.
const INTERVAL: Duration = Duration::from_millis(100);

pub struct Meter {
    state: Mutex<State>,
}

struct State {
    out: Option<ProgressOutput<Stderr>>,
    label: String,
    phase_start: Instant,
    last_draw: Option<Instant>,
    drawn: bool,
}

impl Meter {
    pub fn new(ui: &Ui) -> Arc<Meter> {
        Arc::new(Meter {
            state: Mutex::new(State {
                out: ui.progress_output(),
                label: String::new(),
                phase_start: Instant::now(),
                last_draw: None,
                drawn: false,
            }),
        })
    }

    /// Starts a phase: `label` alone until the first report, e.g. "Connecting to dstore".
    pub fn phase(&self, label: impl Into<String>) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.label = label.into();
        st.phase_start = Instant::now();
        st.last_draw = None;
        let text = format!("{}…", st.label);
        st.draw(&text);
    }

    /// The callback dstore's transfers report to, drawing `label: counts`; `None` without a display.
    pub fn callback(self: &Arc<Self>) -> Option<Progress> {
        let has_output = self.state.lock().unwrap_or_else(PoisonError::into_inner).out.is_some();
        has_output.then(|| {
            let me = Arc::clone(self);
            Arc::new(move |r: &ProgressReport| me.report(r)) as Progress
        })
    }

    fn report(&self, r: &ProgressReport) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let finished = r.total_objects > 0 && r.objects >= r.total_objects;
        if !finished && st.last_draw.is_some_and(|t| now - t < INTERVAL) {
            return;
        }
        st.last_draw = Some(now);
        let text = format!("{}: {}", st.label, describe(r, now - st.phase_start));
        st.draw(&text);
    }

    /// Erases the line.
    pub fn clear(&self) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if st.drawn
            && let Some(out) = &mut st.out
        {
            let _ = write!(out, "\r\x1b[K");
            let _ = out.flush();
        }
        st.drawn = false;
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.clear();
    }
}

impl State {
    fn draw(&mut self, text: &str) {
        let Some(out) = &mut self.out else { return };
        // A pty without a size (script, some CI runners) reports 0 columns.
        let width = out.term_width().map(usize::from).filter(|w| *w >= 20).unwrap_or(80) - 1;
        let text: String = text.chars().take(width).collect();
        let r: io::Result<()> = (|| {
            write!(out, "\r{text}\x1b[K")?;
            out.flush()
        })();
        self.drawn = r.is_ok();
    }
}

/// `12/40 objects, 1.2 MiB/3.4 MiB (35%), 2.1 MiB/s`; totals appear once the transfer knows them.
pub fn describe(r: &ProgressReport, took: Duration) -> String {
    let mut s = if r.total_objects > 0 {
        format!("{}/{} objects", r.objects, r.total_objects)
    } else {
        format!("{} objects", r.objects)
    };
    if r.total_bytes > 0 {
        let pct = (r.bytes as f64 / r.total_bytes as f64 * 100.0).clamp(0.0, 100.0);
        s += &format!(", {}/{} ({pct:.0}%)", human_bytes(r.bytes), human_bytes(r.total_bytes));
    } else {
        s += &format!(", {}", human_bytes(r.bytes));
    }
    if r.bytes > 0 && took >= Duration::from_millis(500) {
        s += &format!(", {}", rate(r.bytes, took));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(objects: i64, total_objects: i64, bytes: i64, total_bytes: i64) -> ProgressReport {
        ProgressReport { objects, total_objects, bytes, total_bytes, nodes: Vec::new() }
    }

    #[test]
    fn describes_what_is_known() {
        let fast = Duration::from_millis(10);
        assert_eq!(describe(&report(3, 0, 2048, 0), fast), "3 objects, 2.0 KiB");
        assert_eq!(describe(&report(12, 40, 1 << 20, 4 << 20), fast), "12/40 objects, 1.0 MiB/4.0 MiB (25%)");
        assert_eq!(
            describe(&report(40, 40, 4 << 20, 4 << 20), Duration::from_secs(2)),
            "40/40 objects, 4.0 MiB/4.0 MiB (100%), 2.0 MiB/s"
        );
    }
}
