//! Terminal progress rendering for task waits (`mf repo load`, `mf reconcile`).
//!
//! Mirrors the GUI's task display (spec-tasks "Display"): a determinate bar
//! when both counts are known, an indeterminate spinner otherwise. The caller
//! redraws the line in place (`\r` + clear-to-end) on every poll.

const BAR_WIDTH: usize = 20;
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

/// Renders one progress line for a polled task. `tick` advances the spinner
/// one step per poll (ignored when the bar is determinate).
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
}
