//! The `debug` and `print` sections of the run display.
//!
//! # Design notes
//!
//! **Chosen:** `debug` samples, it never waits. Cases run at full speed and
//! the panel shows one request with its own response until the space key
//! (or the `auto` interval) asks for the request in flight. Keys are read on
//! a separate thread while the terminal is in raw mode for the whole run; a
//! guard restores the terminal on every exit path. Ctrl+C arrives as a key
//! and stops the run like `q`.

use std::cell::RefCell;
use std::io::{self, IsTerminal};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use super::progress::ProgressDisplay;
use crate::experiment::types::{DebugMode, Step};
use crate::inspect::{
    render_request, render_response, Control, RenderOptions, RequestInspector, RequestTemplate,
    RequestView, ResponseView,
};

/// How often the key thread checks whether the run is over.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Width used for plain-text output.
const PLAIN_WIDTH: usize = 100;
/// Body rows shown in a `print` template.
const TEMPLATE_BODY_ROWS: usize = 10;

/// Render the `print` templates as display rows.
pub(super) fn template_rows(templates: &[RequestTemplate], width: usize) -> Vec<String> {
    let options = RenderOptions {
        secrets: &[],
        template: true,
        width,
        max_body_rows: TEMPLATE_BODY_ROWS,
    };
    let mut rows = Vec::new();
    for template in templates {
        rows.push(title("print", &template.step_id));
        rows.extend(render_request(&template.request, &options));
        rows.extend(template.notes.iter().map(|note| format!("  {note}")));
        rows.push(String::new());
    }
    rows
}

fn title(kind: &str, label: &str) -> String {
    let text = format!("── {kind}: {label} ");
    let fill = 48usize.saturating_sub(text.chars().count()).max(3);
    format!("{text}{}", "─".repeat(fill))
}

/// Flags the key thread sets.
#[derive(Clone, Default)]
pub(super) struct Signals {
    refresh: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

/// Raw mode and the thread that reads keys, for as long as this lives.
pub(super) struct Keys {
    signals: Signals,
    done: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _raw: RawMode,
}

impl Keys {
    /// Start reading keys when both the display and the input are terminals.
    pub(super) fn start(display: &ProgressDisplay) -> Option<Self> {
        if !display.interactive() || !io::stdin().is_terminal() {
            return None;
        }
        let raw = RawMode::enable().ok()?;
        let signals = Signals::default();
        let done = Arc::new(AtomicBool::new(false));
        let worker = {
            let (signals, done) = (signals.clone(), done.clone());
            std::thread::spawn(move || read_keys(&signals, &done))
        };
        Some(Self { signals, done, worker: Some(worker), _raw: raw })
    }

    pub(super) fn signals(&self) -> Signals {
        self.signals.clone()
    }
}

impl Drop for Keys {
    fn drop(&mut self) {
        // Stop the thread before raw mode is restored (field order).
        self.done.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_keys(signals: &Signals, done: &AtomicBool) {
    while !done.load(Ordering::Relaxed) {
        match event::poll(POLL_INTERVAL) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return,
        }
        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Char(' ') => signals.refresh.store(true, Ordering::Relaxed),
            KeyCode::Char('q' | 'Q') | KeyCode::Esc => signals.quit.store(true, Ordering::Relaxed),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                signals.quit.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// Decides which requests `debug` shows.
#[derive(Debug, Default)]
pub(super) struct Sampler {
    last_capture: Option<Instant>,
}

impl Sampler {
    /// The first request is always shown; later ones after a refresh or,
    /// in `auto`, once the interval has passed.
    pub(super) fn should_capture(&mut self, mode: DebugMode, now: Instant, refresh: bool) -> bool {
        let due = match (self.last_capture, mode) {
            (None, _) => true,
            (Some(_), _) if refresh => true,
            (Some(last), DebugMode::Auto(interval)) => now.duration_since(last) >= interval,
            (Some(_), DebugMode::Manual) => false,
        };
        if due {
            self.last_capture = Some(now);
        }
        due
    }

    fn next_in(&self, mode: DebugMode, now: Instant) -> Option<Duration> {
        match (self.last_capture, mode) {
            (Some(last), DebugMode::Auto(interval)) => {
                Some(interval.saturating_sub(now.duration_since(last)))
            }
            _ => None,
        }
    }
}

pub(super) struct Debugger {
    display: Rc<RefCell<ProgressDisplay>>,
    /// `None` without a terminal: nothing to read keys from.
    signals: Option<Signals>,
    sampler: Sampler,
    /// The request being captured, with its case label, until its response.
    captured: Option<(String, RequestView)>,
    /// The panel without its key help.
    panel: Vec<String>,
    footer: String,
}

impl Debugger {
    pub(super) fn new(display: Rc<RefCell<ProgressDisplay>>, signals: Option<Signals>) -> Self {
        Self {
            display,
            signals,
            sampler: Sampler::default(),
            captured: None,
            panel: Vec::new(),
            footer: String::new(),
        }
    }

    fn footer(&self, mode: DebugMode, now: Instant) -> String {
        match self.sampler.next_in(mode, now) {
            Some(left) => format!(
                "refresh in {} s · [space] now · [q] quit",
                left.as_secs_f64().ceil() as u64
            ),
            None => "[space] refresh · [q] quit".to_owned(),
        }
    }

    fn show_footer(&mut self, display: &mut ProgressDisplay, mode: DebugMode, now: Instant) {
        let footer = self.footer(mode, now);
        if footer != self.footer && !self.panel.is_empty() {
            let mut rows = self.panel.clone();
            rows.push(footer.clone());
            display.set_debug(rows);
            self.footer = footer;
        }
    }
}

impl RequestInspector for Debugger {
    fn before_send(&mut self, step: &Step, request: &RequestView, _: &[String]) -> Control {
        let Some(mode) = step.debug else {
            return Control::Send;
        };
        let refresh = match &self.signals {
            Some(signals) if signals.quit.load(Ordering::Relaxed) => return Control::Stop,
            Some(signals) => signals.refresh.swap(false, Ordering::Relaxed),
            None => false,
        };
        let now = Instant::now();
        let display = self.display.clone();
        let mut display = display.borrow_mut();
        if self.sampler.should_capture(mode, now, refresh) {
            self.captured = Some((display.current_case(), request.clone()));
        } else if display.interactive() {
            self.show_footer(&mut display, mode, now);
        }
        Control::Send
    }

    fn after_send(&mut self, step: &Step, response: &ResponseView, secrets: &[String]) {
        let (Some(mode), Some((case, request))) = (step.debug, self.captured.take()) else {
            return;
        };
        let display = self.display.clone();
        let mut display = display.borrow_mut();
        let interactive = display.interactive();
        let (width, height) = if interactive { display.size() } else { (PLAIN_WIDTH, usize::MAX) };
        let options = RenderOptions {
            secrets,
            template: false,
            width,
            max_body_rows: (height / 3).clamp(3, 40),
        };
        let mut rows = vec![title("debug", &format!("{} · {case}", step.id))];
        rows.extend(render_request(&request, &options));
        rows.push(String::new());
        rows.push(render_response(response, secrets, width));
        if interactive {
            self.panel = rows;
            self.footer.clear();
            self.show_footer(&mut display, mode, Instant::now());
            display.redraw();
        } else {
            eprintln!("{}\n", rows.join("\n"));
        }
    }
}

/// Raw mode for as long as the guard lives.
struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_mode_shows_the_first_request_then_waits_for_a_refresh() {
        let mut sampler = Sampler::default();
        let start = Instant::now();
        assert!(sampler.should_capture(DebugMode::Manual, start, false));
        assert!(!sampler.should_capture(DebugMode::Manual, start + Duration::from_secs(60), false));
        assert!(sampler.should_capture(DebugMode::Manual, start + Duration::from_secs(61), true));
        assert!(!sampler.should_capture(DebugMode::Manual, start + Duration::from_secs(62), false));
    }

    #[test]
    fn auto_mode_shows_a_request_per_interval_or_on_refresh() {
        let mode = DebugMode::Auto(Duration::from_secs(10));
        let mut sampler = Sampler::default();
        let start = Instant::now();
        assert!(sampler.should_capture(mode, start, false));
        assert!(!sampler.should_capture(mode, start + Duration::from_secs(9), false));
        assert_eq!(sampler.next_in(mode, start + Duration::from_millis(2500)), Some(Duration::from_millis(7500)));
        assert!(sampler.should_capture(mode, start + Duration::from_secs(10), false));
        assert!(sampler.should_capture(mode, start + Duration::from_secs(11), true));
        assert!(!sampler.should_capture(mode, start + Duration::from_secs(20), false));
        assert!(sampler.should_capture(mode, start + Duration::from_secs(21), false));
    }
}
