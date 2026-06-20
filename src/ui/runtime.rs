use std::cell::Cell;
use std::io;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{Config, load_report};

use super::draw::draw;
use super::input::handle_key;
use super::state::UiState;

/// Render one provider tab to plain text at the given size. Used by the
/// hidden `--render` flag and snapshot tests; keeps the TUI inspectable
/// without a terminal session.
pub fn render_text(config: &Config, width: u16, height: u16, tab_index: usize) -> Result<String> {
    let report = load_report(config)?;
    let state = UiState {
        config: config.clone(),
        tab_index: tab_index.min(report.providers.len()),
        report,
        status: String::new(),
        scroll: 0,
        max_scroll: Cell::new(0),
        share: None,
    };
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).context("create test terminal backend")?;
    terminal
        .draw(|frame| draw(frame, &state))
        .context("draw Agent Walker UI to test backend")?;

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut row = String::new();
        for x in 0..buffer.area.width {
            row.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    Ok(out)
}

pub fn run(config: Config) -> Result<()> {
    let report = load_report(&config)?;
    // First-run guidance: an empty dashboard should explain itself.
    let status = if report.combined.scan_stats.files_seen == 0 {
        "no agent logs found · point me at them with --claude-dir / --codex-dir / --agy-dir"
            .to_owned()
    } else {
        String::new()
    };
    let mut state = UiState {
        config,
        report,
        tab_index: 0,
        status,
        scroll: 0,
        max_scroll: Cell::new(0),
        share: None,
    };

    let mut terminal = setup_terminal()?;
    install_panic_hook();
    let run_result = run_loop(&mut terminal, &mut state);
    let restore_result = restore_terminal(&mut terminal);
    run_result.and(restore_result)
}

/// Restore the terminal on panic before the message is printed.
///
/// crossterm's raw mode and alternate screen survive an unwind, so a panic
/// inside the draw/event loop would otherwise drop the user back to a dead
/// shell — no echo, no prompt. The hook leaves the alternate screen first, then
/// defers to the previous hook so the panic message prints on the normal screen
/// instead of the discarded alternate buffer.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
        original(info);
    }));
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = io::stdout();
    let setup_result = execute!(stdout, EnterAlternateScreen, Hide)
        .context("enter alternate terminal screen")
        .and_then(|()| {
            let backend = CrosstermBackend::new(stdout);
            Terminal::new(backend).context("create terminal backend")
        });
    if setup_result.is_err() {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
    setup_result
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let raw_result = disable_raw_mode().context("disable terminal raw mode");
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)
        .context("leave alternate terminal screen");
    let cursor_result = terminal.show_cursor().context("restore terminal cursor");

    raw_result.and(screen_result).and(cursor_result)
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut UiState,
) -> Result<()> {
    loop {
        terminal
            .draw(|frame| draw(frame, state))
            .context("draw Agent Walker UI")?;
        if !event::poll(StdDuration::from_millis(250)).context("poll terminal events")? {
            continue;
        }

        let Event::Key(key) = event::read().context("read terminal event")? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if handle_key(state, key.code, key.modifiers) {
            return Ok(());
        }
    }
}
