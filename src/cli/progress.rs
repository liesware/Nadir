use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use crate::exploration::RunProgress;
use crate::storage::{CRASHES_DIR, TRANSPORT_ERRORS_DIR};

const REFRESH_INTERVAL: Duration = Duration::from_millis(200);
/// Size used when the terminal does not report one.
const DEFAULT_SIZE: (usize, usize) = (100, 40);

#[derive(Clone, Copy)]
pub(super) enum EvidenceKind {
    Crash,
    Transport,
}

pub(super) struct ProgressDisplay {
    interactive: bool,
    last_draw: Option<Instant>,
    rows: usize,
    latest: RunProgress,
    /// `print` templates: drawn below the counters, unchanged during the run.
    print_rows: Vec<String>,
    /// The `debug` panel: a request, its response, the keys.
    debug_rows: Vec<String>,
    /// One row per crash file written, in order.
    crashes: Vec<String>,
    transport: Vec<String>,
    /// When the cases started running.
    started: Option<Instant>,
}

impl ProgressDisplay {
    pub(super) fn new() -> Self {
        Self {
            interactive: io::stderr().is_terminal()
                && std::env::var_os("TERM")
                    .as_deref()
                    .is_some_and(|term| term != "dumb"),
            last_draw: None,
            rows: 0,
            latest: RunProgress::default(),
            print_rows: Vec::new(),
            debug_rows: Vec::new(),
            crashes: Vec::new(),
            transport: Vec::new(),
            started: None,
        }
    }

    /// Whether the display redraws in place (stderr is a terminal).
    pub(super) fn interactive(&self) -> bool {
        self.interactive
    }

    /// Columns and rows of the terminal.
    pub(super) fn size(&self) -> (usize, usize) {
        match crossterm::terminal::size() {
            Ok((columns, rows)) if columns > 0 && rows > 0 => (usize::from(columns), usize::from(rows)),
            _ => DEFAULT_SIZE,
        }
    }

    pub(super) fn update(&mut self, progress: &RunProgress) {
        self.latest = progress.clone();
        self.draw(false);
    }

    pub(super) fn set_print(&mut self, rows: Vec<String>) {
        self.print_rows = rows;
    }

    pub(super) fn set_debug(&mut self, rows: Vec<String>) {
        self.debug_rows = rows;
    }

    /// Start the `Duration:` clock: the cases begin now.
    pub(super) fn start_clock(&mut self) {
        self.started = Some(Instant::now());
    }

    /// `Duration: 23s` while running.
    fn duration_line(&self) -> Option<String> {
        self.started.map(|started| format!("Duration: {}s", started.elapsed().as_secs()))
    }

    /// `Duration: 55.3s` for the final report.
    pub(super) fn final_duration(&self) -> Option<String> {
        self.started.map(|started| format!("Duration: {:.1}s", started.elapsed().as_secs_f64()))
    }

    pub(super) fn add_evidence(&mut self, kind: EvidenceKind, row: String) {
        let (prefix, rows) = match kind {
            EvidenceKind::Crash => ("crash", &mut self.crashes),
            EvidenceKind::Transport => ("transport", &mut self.transport),
        };
        if !self.interactive {
            eprintln!("{prefix}: {row}");
        }
        rows.push(row);
        self.draw(true);
    }

    /// Draw now, without waiting for the refresh interval.
    pub(super) fn redraw(&mut self) {
        self.draw(true);
    }

    /// `case 65/101`: the case the next request belongs to.
    pub(super) fn current_case(&self) -> String {
        let current = self.latest.completed() + 1;
        match self.latest.total_limit {
            Some(limit) => format!("case {current}/{limit}"),
            None => format!("case {current}"),
        }
    }

    /// The `print`, `debug` and `crashes` sections for the final report.
    /// The key help is dropped: the run is over. Without a terminal only
    /// the crash list is repeated; the rest was already printed.
    pub(super) fn final_sections(&self) -> Vec<String> {
        let incidents = self.evidence_rows(FINAL_CRASH_ROWS);
        if !self.interactive {
            return incidents;
        }
        let debug = &self.debug_rows[..self.debug_rows.len().saturating_sub(1)];
        let mut rows = self.print_rows.to_vec();
        if !debug.is_empty() {
            rows.extend(debug.iter().cloned());
            rows.push(String::new());
        }
        rows.extend(incidents);
        rows
    }

    fn evidence_rows(&self, limit: usize) -> Vec<String> {
        let mut rows = incident_rows("transport errors", TRANSPORT_ERRORS_DIR, &self.transport, limit);
        rows.extend(incident_rows("crashes", CRASHES_DIR, &self.crashes, limit));
        rows
    }

    fn draw(&mut self, force: bool) {
        let now = Instant::now();
        if !self.interactive
            || (!force
                && self
                    .last_draw
                    .is_some_and(|last| now.duration_since(last) < REFRESH_INTERVAL))
        {
            return;
        }
        let progress = &self.latest;
        let mut lines = vec![format!(
            "Enumeration cases: {}",
            progress.deterministic_completed.saturating_sub(1),
        )];
        if let (Some(random), Some(limit)) = (&progress.random, progress.random_limit) {
            lines.push(format!(
                "Random cases: {}/{} (kept {}, seed {})",
                random.executed, limit, random.kept, random.seed,
            ));
        }
        lines.push(self.cases_line());
        lines.push(format!("Findings: {}", progress.findings));
        lines.extend(self.duration_line());
        let (_, height) = self.size();
        fit_live_frame(
            &mut lines,
            &self.print_rows,
            &self.debug_rows,
            &self.evidence_rows(LIVE_CRASH_ROWS),
            height,
        );

        // `\r\n` keeps rows aligned while `debug` has the terminal in raw mode.
        let frame: String = lines.iter().map(|line| format!("{line}\r\n")).collect();
        let mut stderr = io::stderr().lock();
        let clear = self.clear_sequence();
        // Disable wrapping for the frame so a narrow terminal still uses
        // exactly one row per line. Restore wrapping before other output.
        if write!(stderr, "{clear}\x1b[?7l{frame}\x1b[?7h")
            .and_then(|()| stderr.flush())
            .is_ok()
        {
            self.rows = lines.len();
            self.last_draw = Some(now);
        } else {
            self.interactive = false;
        }
    }

    pub(super) fn cases_line(&self) -> String {
        let completed = self.latest.completed();
        match self.latest.total_limit {
            Some(limit) => format!("Cases: {completed}/{limit}"),
            None => format!("Cases: {completed}"),
        }
    }

    fn clear_sequence(&self) -> String {
        if self.rows == 0 {
            String::new()
        } else {
            format!("\x1b[{}A\r\x1b[J", self.rows)
        }
    }

    pub(super) fn clear(&mut self) {
        if self.rows != 0 {
            let mut stderr = io::stderr().lock();
            let _ = write!(stderr, "{}", self.clear_sequence()).and_then(|()| stderr.flush());
            self.rows = 0;
        }
    }
}

/// Crash rows shown while running and in the final report.
const LIVE_CRASH_ROWS: usize = 5;
const FINAL_CRASH_ROWS: usize = 20;

fn fit_live_frame(
    lines: &mut Vec<String>,
    print: &[String],
    debug: &[String],
    incidents: &[String],
    terminal_height: usize,
) {
    let row_limit = terminal_height.saturating_sub(1);
    lines.truncate(row_limit);
    let mut remaining = row_limit - lines.len();
    if remaining == 0 || (print.is_empty() && debug.is_empty() && incidents.is_empty()) {
        return;
    }
    if remaining > 1 {
        lines.push(String::new());
        remaining -= 1;
    }
    let visible_incidents = &incidents[incidents.len().saturating_sub(remaining)..];
    remaining -= visible_incidents.len();
    let separate = !visible_incidents.is_empty() && !debug.is_empty() && remaining >= 2;
    lines.extend(fit(print, debug, remaining - usize::from(separate)));
    if separate {
        lines.push(String::new());
    }
    lines.extend_from_slice(visible_incidents);
}

fn incident_rows(title: &str, directory: &str, incidents: &[String], limit: usize) -> Vec<String> {
    if incidents.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![format!("── {title} {}", "─".repeat(37))];
    let hidden = incidents.len().saturating_sub(limit);
    if hidden > 0 {
        rows.push(format!("… {hidden} more in {directory}/"));
    }
    rows.extend(incidents[hidden..].iter().cloned());
    rows
}

/// Fit the sections into `available` rows. The last two `debug` rows (the
/// response and the keys) always stay; other rows are cut from the end.
fn fit(print: &[String], debug: &[String], available: usize) -> Vec<String> {
    let total = print.len() + debug.len();
    if total <= available {
        return print.iter().chain(debug).cloned().collect();
    }
    let kept_tail = debug.len().min(2).min(available.saturating_sub(1));
    let head_rows = available.saturating_sub(kept_tail + 1);
    let mut rows: Vec<String> = print
        .iter()
        .chain(&debug[..debug.len() - kept_tail])
        .take(head_rows)
        .cloned()
        .collect();
    if available > 0 {
        rows.push(format!("… {} rows hidden (enlarge the terminal)", total - head_rows - kept_tail));
    }
    rows.extend(debug[debug.len() - kept_tail..].iter().cloned());
    rows
}

impl Drop for ProgressDisplay {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{fit, fit_live_frame, incident_rows, CRASHES_DIR};

    #[test]
    fn crash_section_keeps_the_latest_rows() {
        let crashes = rows("c", 7);
        let shown = incident_rows("crashes", CRASHES_DIR, &crashes, 5);
        assert!(shown[0].starts_with("── crashes "));
        assert_eq!(shown[1], "… 2 more in artifacts/crashes/");
        assert_eq!(&shown[2..], &crashes[2..]);
        assert!(incident_rows("crashes", CRASHES_DIR, &[], 5).is_empty());
    }

    fn rows(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|index| format!("{prefix}{index}")).collect()
    }

    #[test]
    fn sections_that_fit_are_unchanged() {
        assert_eq!(fit(&rows("p", 2), &rows("d", 2), 4), vec!["p0", "p1", "d0", "d1"]);
    }

    #[test]
    fn long_sections_keep_the_response_and_keys() {
        let fitted = fit(&rows("p", 10), &rows("d", 10), 6);
        assert_eq!(fitted, vec!["p0", "p1", "p2", "… 15 rows hidden (enlarge the terminal)", "d8", "d9"]);
        assert_eq!(fit(&rows("p", 10), &rows("d", 10), 0), Vec::<String>::new());
    }

    #[test]
    fn combined_panels_reserve_a_row_for_the_cursor() {
        for height in 1..=30 {
            let mut frame = rows("progress", 5);
            fit_live_frame(&mut frame, &rows("p", 10), &rows("d", 10), &rows("incident", 7), height);
            assert!(frame.len() < height, "height {height}: {frame:?}");
            if height >= 8 {
                assert_eq!(frame.last().map(String::as_str), Some("incident6"));
            }
        }
    }
}
