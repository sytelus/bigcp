//! Terminal interfaces for live copy runs and saved reports.
//!
//! Rendering consumes immutable snapshots only. The copy worker never touches
//! terminal state, and terminal refresh cannot block an I/O engine.

#![deny(missing_docs, unsafe_code)]

use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use bigcp_core::{BigcpError, CopyOptions, RunObserver, RunReport, RunSnapshot, run_copy};
use crossterm::event::{self, Event, KeyCode};
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
const REPORT_TABS: [&str; 5] = ["Errors", "Devices", "Performance", "Hints", "Audit"];

/// Returns true when stdout can host the interactive dashboard.
#[must_use]
pub fn stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

/// Plain, log-friendly observer for redirected output and scripts.
pub struct PlainObserver {
    quiet: bool,
}

impl PlainObserver {
    /// Creates a plain observer.
    #[must_use]
    pub const fn new(quiet: bool) -> Self {
        Self { quiet }
    }
}

impl RunObserver for PlainObserver {
    fn on_snapshot(&self, snapshot: &RunSnapshot) {
        if self.quiet {
            return;
        }
        println!(
            "state={:?} discovered={} copied={} replaced={} skipped={} failed={} read={} written={}",
            snapshot.state,
            snapshot.counters.files_discovered,
            snapshot.counters.copied_new,
            snapshot.counters.copied_replaced,
            snapshot.counters.skipped_same,
            snapshot.counters.failed,
            snapshot.counters.bytes_read_source,
            snapshot.counters.bytes_written_destination
        );
    }

    fn on_message(&self, message: &str) {
        if !self.quiet {
            println!("{message}");
        }
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
        self.canceled.load(Ordering::Relaxed)
    }
}

/// Runs a copy worker beside the full-screen live dashboard.
pub fn run_dashboard(options: CopyOptions) -> Result<RunReport, BigcpError> {
    let state = Arc::new(Mutex::new(LiveState::default()));
    let canceled = Arc::new(AtomicBool::new(false));
    let observer = DashboardObserver {
        state: Arc::clone(&state),
        canceled: Arc::clone(&canceled),
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let result = run_copy(&options, &observer);
            let _ = sender.send(result);
        });
        let terminal_result = dashboard_loop(&state, &receiver, &canceled);
        match terminal_result {
            Ok(Some(result)) => result,
            Ok(None) => receiver.recv().map_err(|error| {
                BigcpError::Invariant(format!("copy worker disappeared: {error}"))
            })?,
            Err(error) => receiver.recv().map_err(|recv| {
                BigcpError::Invariant(format!(
                    "dashboard failed ({error}) and copy worker disappeared ({recv})"
                ))
            })?,
        }
    })
}

/// Opens the saved-report browser or prints a plain summary when redirected.
pub fn show_report(report: &RunReport) -> io::Result<()> {
    if !stdout_is_terminal() {
        print_report_summary(report);
        return Ok(());
    }
    let mut session = TerminalSession::enter()?;
    let mut tab = 0_usize;
    loop {
        session
            .terminal
            .draw(|frame| draw_report(frame, report, tab))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Tab | KeyCode::Right => tab = (tab + 1) % 5,
                    KeyCode::BackTab | KeyCode::Left => tab = (tab + 4) % 5,
                    KeyCode::Char(value @ '1'..='5') => {
                        tab = usize::from(value as u8 - b'1');
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Prints the durable final summary used by both UI modes.
pub fn print_report_summary(report: &RunReport) {
    println!("bigcp {} -> {}", report.run.source, report.run.destination);
    if report.run.dry_run {
        println!(
            "dry-run forecast: new={} replacements={} metadata-fixes={} (destination tree unchanged)",
            report.counters.would_copy_new,
            report.counters.would_copy_replaced,
            report.counters.would_meta_fix
        );
    }
    println!(
        "copied={} replaced={} skipped={} meta-fixed={} failed={} extras={}",
        report.counters.copied_new,
        report.counters.copied_replaced,
        report.counters.skipped_same,
        report.counters.meta_fixed,
        report.counters.failed,
        report.counters.extra
    );
    println!(
        "logical-bytes={} duration={:.2}s average={:.1} MB/s observed-peak={:.1} MB/s",
        report.counters.bytes_logical_copied,
        report.run.duration_seconds,
        report.bottleneck.average_mbps,
        report.bottleneck.observed_peak_mbps
    );
    println!(
        "durability={} audit={} integrity={} log={}",
        report.run.durability,
        report.run.audit,
        report.integrity,
        report.run.log_path.display()
    );
}

fn dashboard_loop(
    state: &Arc<Mutex<LiveState>>,
    receiver: &mpsc::Receiver<Result<RunReport, BigcpError>>,
    canceled: &AtomicBool,
) -> io::Result<Option<Result<RunReport, BigcpError>>> {
    let mut session = TerminalSession::enter()?;
    let mut tab = 0_usize;
    loop {
        match receiver.try_recv() {
            Ok(result) => {
                session
                    .terminal
                    .draw(|frame| draw_live(frame, state, tab))?;
                return Ok(Some(result));
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(None),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        session
            .terminal
            .draw(|frame| draw_live(frame, state, tab))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        canceled.store(true, Ordering::Relaxed);
                    }
                    KeyCode::Tab | KeyCode::Right => tab = (tab + 1) % 6,
                    KeyCode::BackTab | KeyCode::Left => tab = (tab + 5) % 6,
                    KeyCode::Char(value @ '1'..='6') => {
                        tab = usize::from(value as u8 - b'1');
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw_live(frame: &mut ratatui::Frame<'_>, state: &Arc<Mutex<LiveState>>, tab: usize) {
    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_tabs(frame, chunks[0], tab, &LIVE_TABS);
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
                    .gauge_style(Style::default().fg(Color::Cyan))
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label(format!(
                        "{total_terminal} / {} discovered",
                        snapshot.counters.files_discovered
                    )),
                body[0],
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("State: {:?}", snapshot.state)),
                    Line::from(format!(
                        "Copied: {} new + {} replaced  Skipped: {}  Failed: {}",
                        snapshot.counters.copied_new,
                        snapshot.counters.copied_replaced,
                        snapshot.counters.skipped_same,
                        snapshot.counters.failed
                    )),
                    Line::from(format!(
                        "Read: {} bytes  Written: {} bytes",
                        snapshot.counters.bytes_read_source,
                        snapshot.counters.bytes_written_destination
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
                "Current application read: {:.1} MiB/s\nCurrent application write: {:.1} MiB/s\n\nStatic device classes, queue depths, chunk size, and confidence are persisted in the final report.",
                snapshot.read_bytes_per_second / (1024.0 * 1024.0),
                snapshot.write_bytes_per_second / (1024.0 * 1024.0)
            ))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Devices")),
            chunks[1],
        ),
        3 => frame.render_widget(
            Paragraph::new(format!(
                "Read: {:.1} MiB/s\nWrite: {:.1} MiB/s\nLogical bytes copied: {}\nFiles discovered: {}",
                snapshot.read_bytes_per_second / (1024.0 * 1024.0),
                snapshot.write_bytes_per_second / (1024.0 * 1024.0),
                snapshot.counters.bytes_logical_copied,
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
        Paragraph::new("1–6 tabs  Tab/Shift-Tab navigate  q/Esc cancel gracefully")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_live_errors(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    snapshot: &RunSnapshot,
) {
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

fn draw_report(frame: &mut ratatui::Frame<'_>, report: &RunReport, tab: usize) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_tabs(frame, chunks[0], tab, &REPORT_TABS);
    match tab {
        0 => {
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
        1 => frame.render_widget(
            Paragraph::new(format!(
                "Source: {}\nDestination: {}\nDurability: {}\nAudit: {}",
                report.run.source, report.run.destination, report.run.durability, report.run.audit
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Devices / Run"),
            ),
            chunks[1],
        ),
        2 => frame.render_widget(
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
        3 => frame.render_widget(
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
                "Log: {}\nIntegrity: {}\nExit: {}",
                report.run.log_path.display(),
                report.integrity,
                report.run.exit
            ))
            .block(Block::default().borders(Borders::ALL).title("Audit")),
            chunks[1],
        ),
    }
    frame.render_widget(
        Paragraph::new("1–5 tabs  Tab/Shift-Tab navigate  q quit")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_tabs(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    selected: usize,
    titles: &[&str],
) {
    let titles = titles
        .iter()
        .map(|title| Line::from(Span::raw(*title)))
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(Block::default().borders(Borders::ALL).title("bigcp"))
            .highlight_style(Style::default().fg(Color::Cyan).bold()),
        area,
    );
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
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

/// Returns the human-readable product name.
#[must_use]
pub const fn product_name() -> &'static str {
    "bigcp"
}

#[cfg(test)]
mod tests {
    use super::{LIVE_TABS, LiveState, draw_live};
    use bigcp_core::{Counters, RunSnapshot, RunState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

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
        for (tab, expected) in LIVE_TABS.iter().enumerate() {
            let backend = TestBackend::new(100, 30);
            let terminal = Terminal::new(backend);
            assert!(terminal.is_ok());
            let Some(mut terminal) = terminal.ok() else {
                return;
            };
            assert!(terminal.draw(|frame| draw_live(frame, &state, tab)).is_ok());
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
        }
    }
}
