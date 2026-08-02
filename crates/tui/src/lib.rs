//! Terminal interfaces for live copy runs and saved reports.
//!
//! Rendering consumes immutable snapshots only. The copy worker never touches
//! terminal state, and terminal refresh cannot block an I/O engine.

#![deny(missing_docs, unsafe_code)]

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use bigcp_core::report::VerificationSummary;
use bigcp_core::{
    BigcpError, CopyOptions, RunObserver, RunReport, RunSnapshot, RunState, run_copy,
};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Row, Table, Tabs, Wrap};

const LIVE_TABS: [&str; 6] = [
    "Dashboard",
    "Errors",
    "Devices",
    "Performance",
    "Hints",
    "Log",
];
const REPORT_TABS: [&str; 6] = [
    "Summary",
    "Errors",
    "Devices",
    "Performance",
    "Hints",
    "Audit",
];

/// Returns true when stdout can host the interactive dashboard.
#[must_use]
pub fn stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

/// Plain, log-friendly observer for redirected output and scripts.
pub struct PlainObserver {
    quiet: bool,
    /// External cancellation source (typically a console Ctrl+C flag). The
    /// TUI crate forbids unsafe code, so the caller supplies the probe as a
    /// plain function instead of this crate touching Win32 itself.
    cancel: Option<fn() -> bool>,
}

impl PlainObserver {
    /// Creates a plain observer.
    ///
    /// `cancel`, when provided, is polled by the copy engine between bounded
    /// work units; returning true requests a graceful stop (exit 3).
    #[must_use]
    pub const fn new(quiet: bool, cancel: Option<fn() -> bool>) -> Self {
        Self { quiet, cancel }
    }
}

impl RunObserver for PlainObserver {
    fn on_snapshot(&self, snapshot: &RunSnapshot) {
        if self.quiet {
            return;
        }
        // Progress is advisory. In particular, a consumer closing a pipe
        // must not panic (and abort a release build) while the copy engine is
        // still mutating the destination. The durable log/report remain the
        // authoritative output channels.
        let _ = writeln!(
            io::stdout().lock(),
            "state={:?} discovered={} copied={} replaced={} {} failed={} read={} written={} {}",
            snapshot.state,
            snapshot.counters.files_discovered,
            snapshot.counters.copied_new,
            snapshot.counters.copied_replaced,
            skipped_counts(&snapshot.counters),
            snapshot.counters.failed,
            snapshot.counters.bytes_read_source,
            snapshot.counters.bytes_written_destination,
            eta_line(snapshot)
        );
    }

    fn on_message(&self, message: &str) {
        if !self.quiet {
            let _ = writeln!(io::stdout().lock(), "{message}");
        }
    }

    fn cancellation_requested(&self) -> bool {
        self.cancel.is_some_and(|cancel| cancel())
    }
}

#[derive(Default)]
struct LiveState {
    snapshot: Option<RunSnapshot>,
    message: String,
}

struct DashboardObserver {
    state: Arc<Mutex<LiveState>>,
    canceled: Arc<AtomicBool>,
    /// Console Ctrl+C flag. While the dashboard owns the terminal, raw mode
    /// turns Ctrl+C into a key event handled by the event loop; this probe
    /// matters when the dashboard has died and the copy continues headless.
    console_cancel: Option<fn() -> bool>,
}

impl RunObserver for DashboardObserver {
    fn on_snapshot(&self, snapshot: &RunSnapshot) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.snapshot = Some(snapshot.clone());
    }

    fn on_message(&self, message: &str) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        message.clone_into(&mut state.message);
    }

    fn cancellation_requested(&self) -> bool {
        self.canceled.load(Ordering::Relaxed) || self.console_cancel.is_some_and(|cancel| cancel())
    }
}

/// Runs a copy worker beside the dashboard, optionally without color styling.
///
/// Disabling color does not select line-oriented output; callers use
/// [`PlainObserver`] explicitly when they need a script-friendly stream.
/// `console_cancel`, when provided, lets a console Ctrl+C flag request a
/// graceful stop even if the dashboard itself has failed.
pub fn run_dashboard_with_color(
    options: CopyOptions,
    color_enabled: bool,
    console_cancel: Option<fn() -> bool>,
) -> Result<RunReport, BigcpError> {
    let state = Arc::new(Mutex::new(LiveState::default()));
    let canceled = Arc::new(AtomicBool::new(false));
    let observer = DashboardObserver {
        state: Arc::clone(&state),
        canceled: Arc::clone(&canceled),
        console_cancel,
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = run_copy(&options, &observer);
            let _ = sender.send(result);
        });
        let terminal_result = dashboard_loop(&state, &receiver, &canceled, color_enabled);
        match terminal_result {
            Ok(Some(result)) => result,
            Ok(None) => receiver.recv().map_err(|error| {
                BigcpError::Invariant(format!("copy worker disappeared: {error}"))
            })?,
            Err(error) => {
                // The TerminalSession was restored when dashboard_loop
                // returned, so stderr reaches the user. The copy worker is
                // unaffected; without this notice the terminal would sit
                // silent for the rest of the run and the user would have no
                // way to know the copy is still progressing.
                let _ = writeln!(
                    io::stderr(),
                    "bigcp: live dashboard failed ({error}); the copy continues without display. \
                     Ctrl+C still cancels between chunks; a second Ctrl+C force-quits (rerun resumes)."
                );
                receiver.recv().map_err(|recv| {
                    BigcpError::Invariant(format!(
                        "dashboard failed ({error}) and copy worker disappeared ({recv})"
                    ))
                })?
            }
        }
    })
}

/// Opens the saved-report browser or prints a plain summary when redirected.
pub fn show_report(report: &RunReport, color_enabled: bool) -> io::Result<()> {
    if !stdout_is_terminal() {
        return print_report_summary(report);
    }
    let mut session = TerminalSession::enter()?;
    let mut tab = 0_usize;
    loop {
        session
            .terminal
            .draw(|frame| draw_report(frame, report, tab, color_enabled))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                match key_action(&key, tab, REPORT_TABS.len()) {
                    KeyAction::Quit => break,
                    KeyAction::SelectTab(next) => tab = next,
                    KeyAction::None => {}
                }
            }
        }
    }
    Ok(())
}

/// One decoded navigation action shared by both full-screen event loops.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyAction {
    None,
    Quit,
    SelectTab(usize),
}

/// Maps a key event to a navigation action for a `tab_count`-tab screen.
///
/// Windows consoles deliver both press and release key events; acting on
/// anything but `Press` fires every key twice, and with tab-cycling that made
/// the odd-numbered tabs unreachable (each Tab advanced two positions).
/// Ctrl+C is an explicit quit/cancel gesture here because raw mode disables
/// the console's default Ctrl+C processing.
fn key_action(key: &KeyEvent, current_tab: usize, tab_count: usize) -> KeyAction {
    if key.kind != KeyEventKind::Press {
        return KeyAction::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
    {
        return KeyAction::Quit;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
        KeyCode::Tab | KeyCode::Right => KeyAction::SelectTab((current_tab + 1) % tab_count),
        KeyCode::BackTab | KeyCode::Left => {
            KeyAction::SelectTab((current_tab + tab_count - 1) % tab_count)
        }
        KeyCode::Char(digit @ '1'..='9') => {
            let index = usize::from(digit as u8 - b'1');
            if index < tab_count {
                KeyAction::SelectTab(index)
            } else {
                KeyAction::None
            }
        }
        _ => KeyAction::None,
    }
}

/// Prints the durable final summary used by both UI modes.
///
/// Output failures are returned instead of panicking so a closed pipe cannot
/// abort the process after a copy has completed.
pub fn print_report_summary(report: &RunReport) -> io::Result<()> {
    write_report_summary(&mut io::stdout().lock(), report)
}

fn write_report_summary(output: &mut impl Write, report: &RunReport) -> io::Result<()> {
    writeln!(output, "{}", report_headline(report))?;
    writeln!(output, "  Source:      {}", report.run.source)?;
    writeln!(output, "  Destination: {}", report.run.destination)?;
    writeln!(
        output,
        "  Run:         {} to {} ({})",
        report.run.started,
        report.run.ended,
        format_duration(report.run.duration_seconds)
    )?;
    if report.run.dry_run {
        writeln!(
            output,
            "  Forecast:    {} new, {} replacements, {} metadata repairs; destination tree unchanged",
            report.counters.would_copy_new,
            report.counters.would_copy_replaced,
            report.counters.would_meta_fix
        )?;
        writeln!(
            output,
            "  Existing:    {} unchanged, {} withheld",
            report.counters.skipped_same, report.counters.skipped_diff
        )?;
        writeln!(
            output,
            "  Exceptions:  {} failed, {} excluded, {} destination-only",
            report.counters.failed, report.counters.excluded, report.counters.extra
        )?;
    } else {
        writeln!(
            output,
            "  Files:       {} new, {} replaced, {} unchanged, {} withheld, {} metadata-repaired",
            report.counters.copied_new,
            report.counters.copied_replaced,
            report.counters.skipped_same,
            report.counters.skipped_diff,
            report.counters.meta_fixed
        )?;
        writeln!(
            output,
            "  Exceptions:  {} failed, {} not attempted, {} excluded, {} destination-only",
            report.counters.failed,
            report.counters.not_attempted,
            report.counters.excluded,
            report.counters.extra
        )?;
    }
    writeln!(
        output,
        "  Data:        {} copied; {} read, {} written",
        format_bytes(report.counters.bytes_logical_copied),
        format_bytes(report.counters.bytes_read_source),
        format_bytes(report.counters.bytes_written_destination)
    )?;
    writeln!(
        output,
        "  Performance: {:.1} MB/s average, {:.1} MB/s observed peak ({:.0}% efficiency)",
        report.bottleneck.average_mbps,
        report.bottleneck.observed_peak_mbps,
        report.bottleneck.efficiency_vs_observed_peak * 100.0
    )?;
    if report.phases.fastest.is_some() || report.phases.slowest.is_some() {
        writeln!(
            output,
            "  Phases:      fastest {}; slowest {}",
            report
                .phases
                .fastest
                .as_ref()
                .map_or_else(|| "not observed".to_owned(), phase_summary),
            report
                .phases
                .slowest
                .as_ref()
                .map_or_else(|| "not observed".to_owned(), phase_summary)
        )?;
    }
    writeln!(
        output,
        "  Replaced:    {} files ({} where destination was newer)",
        report.replacements.count, report.replacements.destination_newer
    )?;
    writeln!(
        output,
        "  Objects:     {} directories, {} links",
        report.counters.dirs_discovered, report.counters.links_discovered
    )?;
    if let Some(verification) = &report.verify {
        writeln!(
            output,
            "  Verification: {}",
            verification_summary(verification)
        )?;
    }
    writeln!(
        output,
        "  Assurance:   durability={}, audit={}, counter-integrity={}",
        report.run.durability, report.run.audit, report.integrity
    )?;
    if !report.warnings.is_empty() {
        let warnings = report
            .warnings
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(output, "  Warnings:    {warnings}")?;
    }
    if !report.errors.is_empty() {
        writeln!(output, "Issues:")?;
        for error in &report.errors {
            let folders = busiest_folders(&error.by_folder);
            writeln!(
                output,
                "  - {}: {}. {}{}",
                category_label(error.category),
                error.count,
                error.hint,
                if folders.is_empty() {
                    String::new()
                } else {
                    format!(" (most affected: {folders})")
                }
            )?;
        }
    }
    if !report.hints.is_empty() {
        writeln!(output, "Helpful notes:")?;
        for hint in report.hints.iter().take(3) {
            writeln!(output, "  - [{} confidence] {}", hint.confidence, hint.text)?;
        }
    }
    writeln!(
        output,
        "  Log:         {}",
        friendly_path(&report.run.log_path)
    )?;
    writeln!(
        output,
        "  Report:      {}",
        friendly_path(&report.run.report_path)
    )?;
    writeln!(output, "{}", report_next_step(report))
}

fn skipped_counts(counters: &bigcp_core::Counters) -> String {
    format!(
        "skipped-same={} skipped-different={}",
        counters.skipped_same, counters.skipped_diff
    )
}

fn verification_counts(summary: &VerificationSummary) -> String {
    format!(
        "verification={} passed={} failed={} projected={} last-access-differences={}",
        summary.mode,
        summary.passed,
        summary.failed,
        summary.projected,
        summary.last_access_differences
    )
}

fn verification_summary(summary: &VerificationSummary) -> String {
    let semantics = if summary.projected {
        "destination-filesystem semantics"
    } else {
        "strict semantics"
    };
    format!(
        "{} passed, {} failed ({semantics}; {} last-access differences)",
        summary.passed, summary.failed, summary.last_access_differences
    )
}

fn phase_summary(point: &bigcp_core::stats::TimelinePoint) -> String {
    format!(
        "{:.1} MB/s write at {} ({})",
        point.write_mbps,
        format_duration(point.seconds),
        point.hypothesis
    )
}

fn report_headline(report: &RunReport) -> &'static str {
    if report.integrity != "ok" {
        return "INTERNAL ERROR - counters did not reconcile";
    }
    match (report.run.exit, report.run.dry_run) {
        (0, true) => "DRY RUN COMPLETE - no destination-tree changes were made",
        (0, false) => "COPY COMPLETE",
        (2, true) => "DRY RUN COMPLETE WITH ISSUES - no destination-tree changes were made",
        (2, false) => "COPY COMPLETE WITH ISSUES",
        (3, _) => "COPY CANCELED - completed work is resumable",
        (4, _) => "COPY STOPPED - destination recovery is required",
        (5, _) => "COPY DID NOT START - preflight failed",
        _ => "INTERNAL OR AUDIT ERROR - retain the audit artifacts",
    }
}

fn report_next_step(report: &RunReport) -> String {
    next_step(
        report.run.exit,
        report.run.dry_run,
        &report.integrity,
        &report.counters,
        report.verify.as_ref(),
        &report.run.source,
        &report.run.destination,
    )
}

fn next_step(
    exit: i32,
    dry_run: bool,
    integrity: &str,
    counters: &bigcp_core::Counters,
    verification: Option<&VerificationSummary>,
    source: &str,
    destination: &str,
) -> String {
    if integrity != "ok" || exit == 6 {
        return "Next: trust the JSONL log over this summary, retain both artifacts, and file a bigcp bug."
            .to_owned();
    }
    match exit {
        3 => return "Next: re-run the same command to continue safely.".to_owned(),
        4 => {
            return "Next: reconnect the endpoint or free destination space, then re-run the same command."
                .to_owned();
        }
        5 => return "Next: resolve the preflight error, then run the command again.".to_owned(),
        _ => {}
    }
    if dry_run {
        return if counters.failed > 0 {
            "Next: resolve the grouped dry-run issues above, then preview again.".to_owned()
        } else {
            "Next: review the forecast, then re-run without --dry-run to copy.".to_owned()
        };
    }
    if counters.failed > 0 || counters.not_attempted > 0 {
        return "Next: resolve the grouped issues above, then re-run the same command; completed files will skip."
            .to_owned();
    }
    if counters.skipped_diff > 0 {
        return "Next: review the withheld files; re-run with --replace=true only if you want to replace them."
            .to_owned();
    }
    match verification {
        Some(summary) if summary.failed == 0 => {
            "Next: verification passed; keep the report with your audit records.".to_owned()
        }
        Some(_) => "Next: review the verification mismatches and re-run the copy.".to_owned(),
        None => {
            format!("Next: for important data, run: bigcp verify \"{source}\" \"{destination}\"")
        }
    }
}

fn busiest_folders(folders: &std::collections::BTreeMap<String, u64>) -> String {
    let mut folders = folders.iter().collect::<Vec<_>>();
    folders.sort_by(|(left_name, left_count), (right_name, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_name.cmp(right_name))
    });
    folders
        .into_iter()
        .take(3)
        .map(|(folder, count)| format!("{folder}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn category_label(category: bigcp_core::ErrorCategory) -> &'static str {
    use bigcp_core::ErrorCategory;

    match category {
        ErrorCategory::Permissions => "permissions",
        ErrorCategory::Locked => "locked",
        ErrorCategory::Path => "path",
        ErrorCategory::Space => "space",
        ErrorCategory::Media => "media",
        ErrorCategory::DeviceGone => "device gone",
        ErrorCategory::FsLimit => "filesystem limit",
        ErrorCategory::SourceChanged => "source changed",
        ErrorCategory::DestinationChanged => "destination changed",
        ErrorCategory::UnsupportedReparse => "unsupported reparse point",
        ErrorCategory::ParentDirFailed => "parent directory failed",
        ErrorCategory::TypeConflict => "type conflict",
        ErrorCategory::Cloud => "cloud provider",
        ErrorCategory::Internal => "internal",
    }
}

fn state_label(state: RunState) -> &'static str {
    match state {
        RunState::Preflight => "Checking paths and capabilities",
        RunState::Copying => "Copying",
        RunState::Verifying => "Verifying copied files",
        RunState::Canceling => "Canceling safely; finishing in-flight work",
        RunState::Complete => "Complete",
        RunState::Failed => "Failed",
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;
    match bytes {
        value if value >= TIB => format!("{:.2} TiB", value as f64 / TIB as f64),
        value if value >= GIB => format!("{:.2} GiB", value as f64 / GIB as f64),
        value if value >= MIB => format!("{:.1} MiB", value as f64 / MIB as f64),
        value if value >= KIB => format!("{:.1} KiB", value as f64 / KIB as f64),
        value => format!("{value} B"),
    }
}

fn friendly_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = path.strip_prefix(r"\\?\") {
        local.to_owned()
    } else {
        path.into_owned()
    }
}

fn format_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "0s".to_owned();
    }
    if seconds < 1.0 {
        return format!("{:.0}ms", (seconds * 1000.0).round().max(1.0));
    }
    let total = seconds.floor();
    let hours = (total / 3600.0).floor();
    let minutes = ((total % 3600.0) / 60.0).floor();
    let secs = total % 60.0;
    if hours >= 1.0 {
        format!("{hours:.0}h {minutes:02.0}m {secs:02.0}s")
    } else if minutes >= 1.0 {
        format!("{minutes:.0}m {secs:02.0}s")
    } else {
        format!("{secs:.0}s")
    }
}

fn dashboard_loop(
    state: &Arc<Mutex<LiveState>>,
    receiver: &mpsc::Receiver<Result<RunReport, BigcpError>>,
    canceled: &AtomicBool,
    color_enabled: bool,
) -> io::Result<Option<Result<RunReport, BigcpError>>> {
    let mut session = TerminalSession::enter()?;
    let mut tab = 0_usize;
    loop {
        match receiver.try_recv() {
            Ok(result) => {
                session
                    .terminal
                    .draw(|frame| draw_live(frame, state, tab, color_enabled))?;
                return Ok(Some(result));
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(None),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        session
            .terminal
            .draw(|frame| draw_live(frame, state, tab, color_enabled))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key_action(&key, tab, LIVE_TABS.len()) {
                    // Quit is a graceful-cancel request, not an exit: the
                    // loop keeps rendering until the worker acknowledges and
                    // sends its final result.
                    KeyAction::Quit => canceled.store(true, Ordering::Relaxed),
                    KeyAction::SelectTab(next) => tab = next,
                    KeyAction::None => {}
                }
            }
        }
    }
}

/// Formats the live remaining-time estimate (VISION's `/ETA` default).
///
/// Discovery streams alongside copying, so the outstanding figure covers the
/// work found *so far* and grows as enumeration continues. It may include
/// bytes of files a rerun will end up skipping, so the estimate is an upper
/// bound and advisory only. It is suppressed while writes are idle — a
/// skip-heavy rerun settles files without writing, and dividing by a
/// near-zero write rate would display a meaningless figure.
fn eta_line(snapshot: &RunSnapshot) -> String {
    let outstanding = snapshot
        .counters
        .bytes_enumerated
        .saturating_sub(snapshot.counters.bytes_logical_discovered);
    if outstanding == 0 || snapshot.write_bytes_per_second < 64.0 * 1024.0 {
        return "ETA: —".to_owned();
    }
    let seconds = outstanding as f64 / snapshot.write_bytes_per_second;
    format!(
        "ETA: {} for {:.1} MiB outstanding",
        format_eta(seconds),
        mib(outstanding as f64)
    )
}

/// Converts a byte quantity (or bytes-per-second rate) to MiB for display.
fn mib(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to [0, 99h] immediately before the cast"
)]
fn format_eta(seconds: f64) -> String {
    let total = seconds.clamp(0.0, 99.0 * 3600.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

fn draw_live(
    frame: &mut ratatui::Frame<'_>,
    state: &Arc<Mutex<LiveState>>,
    tab: usize,
    color_enabled: bool,
) {
    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_tabs(frame, chunks[0], tab, &LIVE_TABS, color_enabled);
    let Some(snapshot) = &state.snapshot else {
        frame.render_widget(
            Paragraph::new(format!(
                "{}\nWaiting for first state snapshot…",
                state.message
            ))
            .block(Block::default().borders(Borders::ALL).title("Starting")),
            chunks[1],
        );
        return;
    };
    let total_terminal = snapshot
        .counters
        .copied_new
        .saturating_add(snapshot.counters.copied_replaced)
        .saturating_add(snapshot.counters.skipped_same)
        .saturating_add(snapshot.counters.skipped_diff)
        .saturating_add(snapshot.counters.meta_fixed)
        .saturating_add(snapshot.counters.failed)
        .saturating_add(snapshot.counters.excluded)
        .saturating_add(snapshot.counters.not_attempted);
    let ratio = if snapshot.counters.files_discovered == 0 {
        0.0
    } else {
        total_terminal as f64 / snapshot.counters.files_discovered as f64
    };
    match tab {
        0 => {
            let body = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(5)])
                .split(chunks[1]);
            frame.render_widget(
                Gauge::default()
                    .block(Block::default().borders(Borders::ALL).title("Files"))
                    .gauge_style(accent_style(color_enabled))
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(format!(
                        "{total_terminal} / {} discovered",
                        snapshot.counters.files_discovered
                    )),
                body[0],
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("State: {}", state_label(snapshot.state))),
                    Line::from(format!(
                        "Files: {} new, {} replaced, {} unchanged, {} withheld, {} failed",
                        snapshot.counters.copied_new,
                        snapshot.counters.copied_replaced,
                        snapshot.counters.skipped_same,
                        snapshot.counters.skipped_diff,
                        snapshot.counters.failed
                    )),
                    Line::from(format!(
                        "Data: {} read, {} written  Rate: {:.1} MiB/s read, {:.1} MiB/s write",
                        format_bytes(snapshot.counters.bytes_read_source),
                        format_bytes(snapshot.counters.bytes_written_destination),
                        mib(snapshot.read_bytes_per_second),
                        mib(snapshot.write_bytes_per_second)
                    )),
                    Line::from(eta_line(snapshot)),
                    Line::from(snapshot.active_paths.last().map_or_else(
                        || "Active: -".to_owned(),
                        |path| format!("Active: {}", path.display()),
                    )),
                    Line::from(state.message.clone()),
                ])
                .block(Block::default().borders(Borders::ALL).title("Dashboard")),
                body[1],
            );
        }
        1 => draw_live_errors(frame, chunks[1], snapshot),
        2 => frame.render_widget(
            Paragraph::new(format!(
                "Current application read: {:.1} MiB/s\nCurrent application write: {:.1} MiB/s\n\nEndpoint kinds, static device classes, topology-selected transport, chunk/burst sizes, worker count, and confidence are persisted in the final report.",
                mib(snapshot.read_bytes_per_second),
                mib(snapshot.write_bytes_per_second)
            ))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Devices")),
            chunks[1],
        ),
        3 => frame.render_widget(
            Paragraph::new(format!(
                "Read: {:.1} MiB/s\nWrite: {:.1} MiB/s\nLogical data copied: {}\nFiles discovered: {}",
                mib(snapshot.read_bytes_per_second),
                mib(snapshot.write_bytes_per_second),
                format_bytes(snapshot.counters.bytes_logical_copied),
                snapshot.counters.files_discovered
            ))
            .block(Block::default().borders(Borders::ALL).title("Performance")),
            chunks[1],
        ),
        4 => frame.render_widget(
            Paragraph::new(if snapshot.counters.failed == 0 {
                "No failure-derived hints yet. Hardware and throughput hints are finalized in the report."
            } else {
                "Failures are present. Open the Errors tab now; the final report groups repair hints by category and top-level folder."
            })
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Hints")),
            chunks[1],
        ),
        _ => frame.render_widget(
            Paragraph::new(format!(
                "{}\n\nEvery terminal outcome is being written to JSONL. The exact log path is printed and stored in the final report.",
                state.message
            ))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Log")),
            chunks[1],
        ),
    }
    frame.render_widget(
        Paragraph::new("1–6 tabs  Tab/Shift-Tab navigate  q/Esc/Ctrl+C cancel gracefully")
            .style(muted_style(color_enabled)),
        chunks[2],
    );
}

fn draw_live_errors(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    snapshot: &RunSnapshot,
) {
    if snapshot.failures_by_category.is_empty() {
        frame.render_widget(
            Paragraph::new("No object failures have been reported.")
                .block(Block::default().borders(Borders::ALL).title("Errors")),
            area,
        );
        return;
    }
    let rows = snapshot
        .failures_by_category
        .iter()
        .map(|(category, count)| Row::new(vec![format!("{category:?}"), count.to_string()]));
    frame.render_widget(
        Table::new(
            rows,
            [Constraint::Percentage(70), Constraint::Percentage(30)],
        )
        .header(Row::new(vec!["Category", "Count"]).style(Style::default().bold()))
        .block(Block::default().borders(Borders::ALL).title("Errors")),
        area,
    );
}

fn draw_report(
    frame: &mut ratatui::Frame<'_>,
    report: &RunReport,
    tab: usize,
    color_enabled: bool,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_tabs(frame, chunks[0], tab, &REPORT_TABS, color_enabled);
    match tab {
        0 => frame.render_widget(
            Paragraph::new(format!(
                "{}\n\nSource: {}\nDestination: {}\nDuration: {}\n{}\nData copied: {}\nPerformance: {:.1} MB/s average, {:.1} MB/s observed peak\n{}",
                report_headline(report),
                report.run.source,
                report.run.destination,
                format_duration(report.run.duration_seconds),
                report_file_overview(report),
                format_bytes(report.counters.bytes_logical_copied),
                report.bottleneck.average_mbps,
                report.bottleneck.observed_peak_mbps,
                report_next_step(report)
            ))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Summary")),
            chunks[1],
        ),
        1 if report.errors.is_empty() => frame.render_widget(
            Paragraph::new("No object failures were reported.")
                .block(Block::default().borders(Borders::ALL).title("Errors")),
            chunks[1],
        ),
        1 => {
            let rows = report.errors.iter().map(|error| {
                Row::new(vec![
                    format!("{:?}", error.category),
                    error.count.to_string(),
                    error.hint.clone(),
                ])
            });
            frame.render_widget(
                Table::new(
                    rows,
                    [
                        Constraint::Length(20),
                        Constraint::Length(10),
                        Constraint::Min(20),
                    ],
                )
                .header(
                    Row::new(vec!["Category", "Count", "Resolution"])
                        .style(Style::default().add_modifier(Modifier::BOLD)),
                )
                .block(Block::default().borders(Borders::ALL).title("Errors")),
                chunks[1],
            );
        }
        2 => frame.render_widget(
            Paragraph::new(format!(
                "Source: {} ({} / {:?})\nDestination: {} ({} / {:?})\nTransport: {:?}  Same physical disk: {}\nChunk: {} bytes  Request/burst: {} bytes  Workers: {}\nDurability: {}  Audit: {}",
                report.run.source,
                report.devices.source.endpoint.name(),
                report.devices.source.class,
                report.run.destination,
                report.devices.destination.endpoint.name(),
                report.devices.destination.class,
                report.devices.transport.kind,
                report.devices.same_physical_disk,
                report.devices.chunk_bytes,
                report.devices.transport.burst_bytes,
                report.devices.workers,
                report.run.durability,
                report.run.audit
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Devices / Run"),
            ),
            chunks[1],
        ),
        3 => frame.render_widget(
            Paragraph::new(format!(
                "Average: {:.1} MB/s\nObserved peak: {:.1} MB/s\nHypothesis: {} ({})\n{}",
                report.bottleneck.average_mbps,
                report.bottleneck.observed_peak_mbps,
                report.bottleneck.hypothesis,
                report.bottleneck.confidence,
                report.bottleneck.evidence
            ))
            .block(Block::default().borders(Borders::ALL).title("Performance")),
            chunks[1],
        ),
        // An empty view states so explicitly; a silent blank pane reads as a
        // rendering failure (README "Command output" promises this).
        4 if report.hints.is_empty() => frame.render_widget(
            Paragraph::new("No hints were recorded for this run.")
                .block(Block::default().borders(Borders::ALL).title("Hints")),
            chunks[1],
        ),
        4 => frame.render_widget(
            Paragraph::new(
                report
                    .hints
                    .iter()
                    .map(|hint| format!("[{}] {}", hint.confidence, hint.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .block(Block::default().borders(Borders::ALL).title("Hints")),
            chunks[1],
        ),
        _ => frame.render_widget(
            Paragraph::new(format!(
                "Log: {}\nReport: {}\n{}\nIntegrity: {}\nExit: {}",
                friendly_path(&report.run.log_path),
                friendly_path(&report.run.report_path),
                report.verify.as_ref().map_or_else(
                    || "verification=not-requested".to_owned(),
                    verification_counts
                ),
                report.integrity,
                report.run.exit
            ))
            .block(Block::default().borders(Borders::ALL).title("Audit")),
            chunks[1],
        ),
    }
    frame.render_widget(
        Paragraph::new("1–6 tabs  Tab/Shift-Tab navigate  q quit")
            .style(muted_style(color_enabled)),
        chunks[2],
    );
}

fn report_file_overview(report: &RunReport) -> String {
    if report.run.dry_run {
        format!(
            "Forecast: {} new, {} replacements, {} metadata repairs, {} failures",
            report.counters.would_copy_new,
            report.counters.would_copy_replaced,
            report.counters.would_meta_fix,
            report.counters.failed
        )
    } else {
        format!(
            "Files: {} new, {} replaced, {} unchanged, {} withheld, {} failed",
            report.counters.copied_new,
            report.counters.copied_replaced,
            report.counters.skipped_same,
            report.counters.skipped_diff,
            report.counters.failed
        )
    }
}

fn draw_tabs(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    selected: usize,
    titles: &[&str],
    color_enabled: bool,
) {
    let titles = titles
        .iter()
        .map(|title| Line::from(Span::raw(*title)))
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(Block::default().borders(Borders::ALL).title("bigcp"))
            .highlight_style(accent_style(color_enabled)),
        area,
    );
}

fn accent_style(color_enabled: bool) -> Style {
    if color_enabled {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default().bold()
    }
}

fn muted_style(color_enabled: bool) -> Style {
    if color_enabled {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => {
                // Release builds abort on panic (workspace `panic = "abort"`),
                // so Drop never runs on that path — without this hook a panic
                // anywhere in the process (including the copy worker thread)
                // would strand the user's console in raw mode inside the
                // alternate screen. Panic hooks still run before an abort.
                // bigcp installs no other hooks, so restoring the default on
                // Drop is sufficient.
                let previous = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    let _ = disable_raw_mode();
                    let _ = execute!(io::stdout(), LeaveAlternateScreen);
                    previous(info);
                }));
                Ok(Self { terminal })
            }
            Err(error) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        drop(std::panic::take_hook());
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KeyAction, LIVE_TABS, LiveState, REPORT_TABS, draw_live, draw_report, format_bytes,
        format_duration, format_eta, friendly_path, key_action, next_step, skipped_counts,
        verification_counts, write_report_summary,
    };
    use bigcp_core::report::{
        BottleneckSummary, PhaseSummary, RunInfo, RunReport, VerificationSummary,
    };
    use bigcp_core::{Counters, RunSnapshot, RunState};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// A minimal completed-run report for pure formatting/rendering tests.
    #[allow(
        clippy::default_trait_access,
        reason = "several field types (serde_json::Value, bigcp_win::EndpointKind) are not \
                  nameable here without adding test-only dependencies"
    )]
    fn sample_report() -> RunReport {
        RunReport {
            v: 1,
            run: RunInfo {
                id: "test-run".to_owned(),
                started: "2026-08-01T00:00:00Z".to_owned(),
                ended: "2026-08-01T00:01:05Z".to_owned(),
                duration_seconds: 65.0,
                exit: 0,
                dry_run: false,
                durability: "logical".to_owned(),
                audit: "ok".to_owned(),
                source: r"C:\source".to_owned(),
                destination: r"D:\destination".to_owned(),
                source_filesystem: "NTFS".to_owned(),
                destination_filesystem: "NTFS".to_owned(),
                log_path: PathBuf::from(r"\\?\C:\audit\run.log.jsonl"),
                report_path: PathBuf::from(r"\\?\C:\audit\run.report.json"),
            },
            config: Default::default(),
            devices: sample_profile(),
            counters: Counters::default(),
            replacements: Default::default(),
            errors: Vec::new(),
            warnings: BTreeMap::new(),
            extras: Default::default(),
            folders: BTreeMap::new(),
            timeline: Vec::new(),
            phases: PhaseSummary::default(),
            bottleneck: BottleneckSummary {
                hypothesis: "balanced".to_owned(),
                confidence: "low".to_owned(),
                evidence: "test".to_owned(),
                observed_peak_mbps: 100.0,
                average_mbps: 50.0,
                efficiency_vs_observed_peak: 0.5,
                provenance: "test".to_owned(),
            },
            hints: Vec::new(),
            analysis: None,
            verify: None,
            integrity: "ok".to_owned(),
        }
    }

    #[allow(
        clippy::default_trait_access,
        reason = "TransportProfile lives in a core module this test does not otherwise use"
    )]
    fn sample_profile() -> bigcp_core::devprofile::CopyProfile {
        bigcp_core::devprofile::CopyProfile {
            source: sample_side(),
            destination: sample_side(),
            chunk_bytes: 1024 * 1024,
            workers: 4,
            same_physical_disk: false,
            transport: Default::default(),
        }
    }

    #[allow(
        clippy::default_trait_access,
        reason = "EndpointKind is a bigcp-win type not nameable without a new dependency"
    )]
    fn sample_side() -> bigcp_core::devprofile::SideProfile {
        bigcp_core::devprofile::SideProfile {
            endpoint: Default::default(),
            class: bigcp_core::DeviceClass::Unknown,
            confidence: "low".to_owned(),
            chunk_bytes: 1024 * 1024,
            workers: 4,
        }
    }

    #[test]
    fn every_live_tab_renders_in_a_bounded_terminal() {
        let state = Arc::new(Mutex::new(LiveState {
            snapshot: Some(RunSnapshot {
                state: RunState::Copying,
                counters: Counters::default(),
                read_bytes_per_second: 1024.0,
                write_bytes_per_second: 2048.0,
                failures_by_category: BTreeMap::new(),
                active_paths: Vec::new(),
            }),
            message: "test audit message".to_owned(),
        }));
        for color_enabled in [true, false] {
            for (tab, expected) in LIVE_TABS.iter().enumerate() {
                let backend = TestBackend::new(100, 30);
                let terminal = Terminal::new(backend);
                assert!(terminal.is_ok());
                let Some(mut terminal) = terminal.ok() else {
                    return;
                };
                assert!(
                    terminal
                        .draw(|frame| draw_live(frame, &state, tab, color_enabled))
                        .is_ok()
                );
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>();
                assert!(
                    rendered.contains(expected),
                    "tab {tab} did not render {expected}"
                );
                if !color_enabled {
                    assert!(terminal.backend().buffer().content().iter().all(|cell| {
                        cell.fg == ratatui::style::Color::Reset
                            && cell.bg == ratatui::style::Color::Reset
                    }));
                }
            }
        }
    }

    #[test]
    fn human_units_are_compact_and_unambiguous() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(format_duration(0.125), "125ms");
        assert_eq!(format_duration(65.0), "1m 05s");
        assert_eq!(format_duration(3_661.0), "1h 01m 01s");
        assert_eq!(
            friendly_path(Path::new(r"\\?\C:\audit\run.json")),
            r"C:\audit\run.json"
        );
        assert_eq!(
            friendly_path(Path::new(r"\\?\UNC\server\share\run.json")),
            r"\\server\share\run.json"
        );
    }

    #[test]
    fn dry_run_next_step_treats_modeled_writes_as_forecast_not_failure() {
        let counters = Counters {
            not_attempted: 6,
            would_copy_new: 6,
            ..Counters::default()
        };
        assert_eq!(
            next_step(0, true, "ok", &counters, None, "source", "destination"),
            "Next: review the forecast, then re-run without --dry-run to copy."
        );
    }

    #[test]
    fn eta_estimates_known_work_and_suppresses_idle_writes() {
        let mut snapshot = RunSnapshot {
            state: RunState::Copying,
            counters: Counters::default(),
            read_bytes_per_second: 0.0,
            write_bytes_per_second: 0.0,
            failures_by_category: BTreeMap::new(),
            active_paths: Vec::new(),
        };
        // No write rate (skip-heavy phase): the figure is suppressed.
        snapshot.counters.bytes_enumerated = 100 * 1024 * 1024;
        assert_eq!(super::eta_line(&snapshot), "ETA: —");
        // 90 MiB outstanding at 1 MiB/s: a 90-second estimate.
        snapshot.counters.bytes_logical_discovered = 10 * 1024 * 1024;
        snapshot.write_bytes_per_second = 1024.0 * 1024.0;
        assert_eq!(
            super::eta_line(&snapshot),
            "ETA: 1m 30s for 90.0 MiB outstanding"
        );
        // Nothing outstanding: suppressed again.
        snapshot.counters.bytes_logical_discovered = snapshot.counters.bytes_enumerated;
        assert_eq!(super::eta_line(&snapshot), "ETA: —");
    }

    #[test]
    fn plain_summary_distinguishes_identical_and_withheld_files() {
        let counters = Counters {
            skipped_same: 2,
            skipped_diff: 3,
            ..Counters::default()
        };
        assert_eq!(
            skipped_counts(&counters),
            "skipped-same=2 skipped-different=3"
        );
    }

    #[test]
    fn key_actions_fire_once_per_press_and_never_on_release() {
        // Windows consoles deliver Press and Release for every key; only
        // Press may act, or each keystroke advances tabs twice and the
        // odd-numbered tabs become unreachable.
        let press = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(key_action(&press, 0, 6), KeyAction::SelectTab(1));
        let release =
            KeyEvent::new_with_kind(KeyCode::Tab, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(key_action(&release, 0, 6), KeyAction::None);

        let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(key_action(&back, 0, 6), KeyAction::SelectTab(5));
        let digit = KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE);
        assert_eq!(key_action(&digit, 0, 6), KeyAction::SelectTab(2));
        let out_of_range = KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE);
        assert_eq!(key_action(&out_of_range, 0, 6), KeyAction::None);

        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(key_action(&quit, 0, 6), KeyAction::Quit);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_action(&ctrl_c, 0, 6), KeyAction::Quit);
        let plain_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(key_action(&plain_c, 0, 6), KeyAction::None);
    }

    #[test]
    fn eta_formatting_covers_each_unit_and_clamps() {
        assert_eq!(format_eta(45.0), "45s");
        assert_eq!(format_eta(125.0), "2m 05s");
        assert_eq!(format_eta(7_290.0), "2h 01m");
        // Absurd inputs clamp to the 99-hour ceiling instead of overflowing.
        assert_eq!(format_eta(1.0e12), "99h 00m");
        assert_eq!(format_eta(-5.0), "0s");
    }

    #[test]
    fn byte_units_scale_through_tebibytes() {
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 * 1024), "3.00 TiB");
    }

    #[test]
    fn busiest_folders_orders_by_count_then_name_and_caps_at_three() {
        let mut folders = BTreeMap::new();
        folders.insert("delta".to_owned(), 2_u64);
        folders.insert("alpha".to_owned(), 5);
        folders.insert("bravo".to_owned(), 5);
        folders.insert("echo".to_owned(), 1);
        assert_eq!(
            super::busiest_folders(&folders),
            "alpha=5, bravo=5, delta=2"
        );
    }

    #[test]
    fn plain_state_tokens_are_a_stable_output_contract() {
        // The --plain progress stream prints `state={:?}`; renaming RunState
        // variants would silently change a documented machine-readable format.
        assert_eq!(format!("{:?}", RunState::Preflight), "Preflight");
        assert_eq!(format!("{:?}", RunState::Copying), "Copying");
        assert_eq!(format!("{:?}", RunState::Verifying), "Verifying");
        assert_eq!(format!("{:?}", RunState::Canceling), "Canceling");
        assert_eq!(format!("{:?}", RunState::Complete), "Complete");
        assert_eq!(format!("{:?}", RunState::Failed), "Failed");
    }

    #[test]
    fn report_summary_lists_outcomes_audit_paths_and_next_step() {
        let report = sample_report();
        let mut output = Vec::new();
        assert!(write_report_summary(&mut output, &report).is_ok());
        let text = String::from_utf8_lossy(&output);
        assert!(text.starts_with("COPY COMPLETE"));
        assert!(text.contains("Files:       0 new"));
        assert!(text.contains(r"C:\audit\run.log.jsonl"));
        assert!(text.contains("Next: for important data, run: bigcp verify"));
        // No hints were recorded, so the hints section must be absent.
        assert!(!text.contains("Helpful notes:"));
    }

    #[test]
    fn dry_run_summary_reports_forecast_existing_and_exceptions() {
        let mut report = sample_report();
        report.run.dry_run = true;
        report.counters.would_copy_new = 4;
        report.counters.skipped_same = 2;
        let mut output = Vec::new();
        assert!(write_report_summary(&mut output, &report).is_ok());
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("Forecast:    4 new"));
        assert!(text.contains("Existing:    2 unchanged"));
        assert!(text.contains("Exceptions:  0 failed"));
    }

    #[test]
    fn next_step_matches_each_terminal_outcome() {
        let counters = Counters::default();
        assert!(next_step(3, false, "ok", &counters, None, "s", "d").contains("re-run the same"));
        assert!(next_step(4, false, "ok", &counters, None, "s", "d").contains("reconnect"));
        assert!(next_step(5, false, "ok", &counters, None, "s", "d").contains("preflight"));
        assert!(next_step(6, false, "ok", &counters, None, "s", "d").contains("file a bigcp bug"));
        assert!(
            next_step(0, false, "failed", &counters, None, "s", "d").contains("file a bigcp bug")
        );
        let failed_verify = VerificationSummary {
            failed: 1,
            ..VerificationSummary::default()
        };
        assert!(
            next_step(2, false, "ok", &counters, Some(&failed_verify), "s", "d")
                .contains("verification mismatches")
        );
    }

    #[test]
    fn every_report_tab_renders_including_the_empty_hints_view() {
        let report = sample_report();
        for (tab, expected) in REPORT_TABS.iter().enumerate() {
            let backend = TestBackend::new(100, 30);
            let terminal = Terminal::new(backend);
            assert!(terminal.is_ok());
            let Some(mut terminal) = terminal.ok() else {
                return;
            };
            assert!(
                terminal
                    .draw(|frame| draw_report(frame, &report, tab, true))
                    .is_ok()
            );
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(
                rendered.contains(expected),
                "tab {tab} did not render {expected}"
            );
            if tab == 4 {
                // An empty view states so explicitly rather than rendering an
                // unexplained blank pane.
                assert!(rendered.contains("No hints were recorded"));
            }
        }
    }

    #[test]
    fn plain_summary_reports_verification_projection_and_counts() {
        let summary = VerificationSummary {
            mode: "copied".to_owned(),
            projected: true,
            passed: 7,
            failed: 2,
            last_access_differences: 3,
            ..VerificationSummary::default()
        };
        assert_eq!(
            verification_counts(&summary),
            "verification=copied passed=7 failed=2 projected=true last-access-differences=3"
        );
    }
}
