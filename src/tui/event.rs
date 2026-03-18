use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use futures::StreamExt;
use ratatui::prelude::*;
use tokio::sync::mpsc;

use super::{App, AppEvent, Focus, Screen};
use super::ui;
use crate::cli::error::Result;
use crate::runtime::AgentRuntime;
use crate::transport::Transport;

pub async fn run_tui(
    mut app: App,
    mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    runtime: AgentRuntime,
    transport: Box<dyn Transport>,
) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Spawn runtime in background
    let mut runtime_handle = tokio::spawn(async move {
        runtime.run(transport).await
    });

    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    let result = loop {
        // Draw
        terminal.draw(|f| ui::render(f, &mut app))?;

        tokio::select! {
            // Keyboard events
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        if handle_key(&mut app, key) {
                            break Ok(());
                        }
                    }
                    Some(Err(e)) => {
                        break Err(crate::cli::error::CliError::Io(e));
                    }
                    _ => {}
                }
            }
            // App events from runtime/transport
            Some(event) = event_rx.recv() => {
                app.update(event);
            }
            // Tick for time updates
            _ = tick.tick() => {}
            // Runtime finished
            result = &mut runtime_handle => {
                match result {
                    Ok(inner) => break inner,
                    Err(e) => break Err(crate::cli::error::CliError::Other(format!("runtime panic: {}", e))),
                }
            }
        }
    };

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Normalize a KeyCode::Char to lowercase.
fn normalize_key(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    }
}

/// Returns true if the app should quit.
fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) -> bool {
    // Ctrl+C always quits
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }

    let code = normalize_key(key.code);

    match &app.screen {
        Screen::Main => match code {
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => match app.focus {
                Focus::Table => app.select_prev(),
                Focus::Log => app.log_scroll = app.log_scroll.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match app.focus {
                Focus::Table => app.select_next(),
                Focus::Log => app.log_scroll = app.log_scroll.saturating_add(1),
            },
            KeyCode::Enter => {
                if let Some(idx) = app.table_state.selected() {
                    if idx < app.jobs.len() {
                        app.detail_scroll = 0;
                        app.screen = Screen::JobDetail(idx);
                    }
                }
            }
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::Table => Focus::Log,
                    Focus::Log => Focus::Table,
                };
            }
            KeyCode::Char('s') => {
                app.toggle_sound();
            }
            KeyCode::Char('r') => {
                app.refresh_recovery();
                app.recovery_detail_scroll = 0;
                if app.recovery_table_state.selected().is_none() && !app.recovery_entries.is_empty() {
                    app.recovery_table_state.select(Some(0));
                }
                app.screen = Screen::Recovery;
            }
            _ => {}
        },
        Screen::JobDetail(_) => match code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                app.screen = Screen::Main;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.detail_scroll = app.detail_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.detail_scroll = app.detail_scroll.saturating_add(1);
            }
            _ => {}
        },
        Screen::Recovery => match code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                app.screen = Screen::Main;
            }
            KeyCode::Char('r') => {
                app.refresh_recovery();
            }
            KeyCode::Enter => {
                app.retry_selected();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let len = app.recovery_entries.len();
                if len > 0 {
                    let i = app.recovery_table_state.selected()
                        .map(|i| if i == 0 { len - 1 } else { i - 1 })
                        .unwrap_or(0);
                    app.recovery_table_state.select(Some(i));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = app.recovery_entries.len();
                if len > 0 {
                    let i = app.recovery_table_state.selected()
                        .map(|i| if i + 1 >= len { 0 } else { i + 1 })
                        .unwrap_or(0);
                    app.recovery_table_state.select(Some(i));
                }
            }
            _ => {}
        },
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn new_app() -> App {
        App::new(
            "test".into(),
            "skill".into(),
            100_000_000,
            0,
            "devnet".into(),
            false,
            0.15,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn handle_key_q_main_quits() {
        let mut app = new_app();
        assert!(handle_key(&mut app, key(KeyCode::Char('q'))));
    }

    #[test]
    fn handle_key_ctrl_c_quits() {
        let mut app = new_app();
        assert!(handle_key(&mut app, ctrl_key(KeyCode::Char('c'))));
    }

    #[test]
    fn handle_key_enter_opens_job_detail() {
        let mut app = new_app();
        // Add a job so there's something to select
        app.update(AppEvent::JobReceived {
            job_id: "job123456789abc".into(),
            customer_id: "cust123456789abc".into(),
            input: "x".into(),
        });
        assert!(!handle_key(&mut app, key(KeyCode::Enter)));
        assert!(matches!(app.screen, Screen::JobDetail(0)));
    }

    #[test]
    fn handle_key_esc_returns_to_main() {
        let mut app = new_app();
        app.screen = Screen::JobDetail(0);
        assert!(!handle_key(&mut app, key(KeyCode::Esc)));
        assert!(matches!(app.screen, Screen::Main));
    }
}
