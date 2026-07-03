//! Terminal progress rendering for long waits (`mf repo load`, `mf reconcile`,
//! the daemon's startup auto-load). Std-only, like the rest of the crate.
//!
//! Mirrors the GUI's task display (spec-tasks "Display"): a determinate bar
//! when both counts are known, an indeterminate spinner otherwise. The line is
//! redrawn in place (`\r` + clear-to-end) on every update.

const BAR_WIDTH: usize = 20;
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

/// Renders one progress line for a polled/reported task. `tick` advances the
/// spinner one step per update (ignored when the bar is determinate).
pub fn render_progress(
    label: &str,
    phase: &str,
    done: Option<u64>,
    total: Option<u64>,
    tick: usize,
) -> String {
    let mut line = format!("{label}:");
    if !phase.is_empty() {
        line.push(' ');
        line.push_str(phase);
    }
    match (done, total) {
        // A zero total cannot place the cursor: fall through to the spinner.
        (Some(done), Some(total)) if total > 0 => {
            let done = done.min(total);
            let filled = (done as usize * BAR_WIDTH) / total as usize;
            line.push_str(" [");
            for i in 0..BAR_WIDTH {
                line.push(if i < filled { '#' } else { '-' });
            }
            line.push_str(&format!("] {done}/{total}"));
        }
        _ => {
            line.push(' ');
            line.push(SPINNER[tick % SPINNER.len()]);
        }
    }
    line
}

/// A single in-place progress line over any writer: each [`update`] redraws
/// the line (`\r<line>\x1b[K`), [`clear`] erases it. Does nothing when
/// `enabled` is false (stderr is not a terminal: pipes and logs stay clean) —
/// callers create it unconditionally and never branch themselves.
///
/// [`update`]: ProgressLine::update
/// [`clear`]: ProgressLine::clear
pub struct ProgressLine<W: std::io::Write> {
    out: W,
    enabled: bool,
    tick: usize,
    drawn: bool,
}

impl<W: std::io::Write> ProgressLine<W> {
    pub fn new(out: W, enabled: bool) -> Self {
        Self { out, enabled, tick: 0, drawn: false }
    }

    /// Redraws the line in place. Write errors are ignored (progress is
    /// best-effort decoration; the work it reports must not fail on a closed
    /// stderr).
    pub fn update(&mut self, label: &str, phase: &str, done: Option<u64>, total: Option<u64>) {
        if !self.enabled {
            return;
        }
        let line = render_progress(label, phase, done, total, self.tick);
        self.tick += 1;
        self.drawn = true;
        let _ = write!(self.out, "\r{line}\x1b[K");
        let _ = self.out.flush();
    }

    /// Erases the line, if one was drawn. Call before printing regular output.
    pub fn clear(&mut self) {
        if self.drawn {
            let _ = write!(self.out, "\r\x1b[K");
            let _ = self.out.flush();
            self.drawn = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determinate_bar_half_filled() {
        assert_eq!(
            render_progress("load", "index", Some(5), Some(10), 0),
            "load: index [##########----------] 5/10"
        );
    }

    #[test]
    fn determinate_bar_empty_and_full() {
        assert_eq!(
            render_progress("load", "tree cache", Some(0), Some(4), 0),
            "load: tree cache [--------------------] 0/4"
        );
        assert_eq!(
            render_progress("load", "tree cache", Some(4), Some(4), 0),
            "load: tree cache [####################] 4/4"
        );
    }

    #[test]
    fn done_beyond_total_is_clamped() {
        assert_eq!(
            render_progress("load", "index", Some(7), Some(4), 0),
            "load: index [####################] 4/4"
        );
    }

    #[test]
    fn missing_or_zero_counts_render_a_spinner() {
        assert_eq!(render_progress("load", "index", None, None, 0), "load: index |");
        assert_eq!(render_progress("load", "index", Some(3), None, 1), "load: index /");
        assert_eq!(render_progress("load", "index", Some(0), Some(0), 2), "load: index -");
    }

    #[test]
    fn spinner_cycles_with_tick() {
        let frames: Vec<String> =
            (0..5).map(|t| render_progress("load", "", None, None, t)).collect();
        assert_eq!(frames, ["load: |", "load: /", "load: -", "load: \\", "load: |"]);
    }

    #[test]
    fn empty_phase_omits_the_phase_segment() {
        assert_eq!(
            render_progress("reconcile", "", Some(1), Some(2), 0),
            "reconcile: [##########----------] 1/2"
        );
    }

    #[test]
    fn progress_line_redraws_in_place_and_advances_the_spinner() {
        let mut buf = Vec::new();
        let mut line = ProgressLine::new(&mut buf, true);
        line.update("load", "tree cache", None, None);
        line.update("load", "tree cache", None, None);
        line.clear();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "\rload: tree cache |\x1b[K\rload: tree cache /\x1b[K\r\x1b[K"
        );
    }

    #[test]
    fn disabled_progress_line_writes_nothing() {
        let mut buf = Vec::new();
        let mut line = ProgressLine::new(&mut buf, false);
        line.update("load", "index", Some(1), Some(2));
        line.clear();
        assert!(buf.is_empty(), "disabled: no bytes, got {:?}", String::from_utf8_lossy(&buf));
    }

    #[test]
    fn clear_without_a_drawn_line_writes_nothing() {
        let mut buf = Vec::new();
        let mut line = ProgressLine::new(&mut buf, true);
        line.clear();
        assert!(buf.is_empty());
    }
}
