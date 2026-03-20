use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table};

use super::{App, Focus, JobStatus, Screen};
use crate::util::format_sol_compact;

const INPUT_BYTE_LIMIT: usize = 1024;

/// Truncate a string at the last char boundary on or before `max_bytes`.
/// Returns the truncated slice (guaranteed valid UTF-8).
fn truncate_input(s: &str) -> &str {
    if s.len() <= INPUT_BYTE_LIMIT {
        return s;
    }
    tracing::warn!(
        len = s.len(),
        "job input exceeds {} bytes, truncating for display",
        INPUT_BYTE_LIMIT,
    );
    let mut end = INPUT_BYTE_LIMIT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── Theme helpers ──
// Use only ANSI colors + modifiers so the terminal palette handles light/dark.
// `Color::Reset` = terminal's default fg/bg — always readable.

/// Default foreground: inherits terminal color.
const FG: Color = Color::Reset;
/// Muted/secondary text: uses Modifier::DIM on default fg.
fn muted() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}
/// Accent: cyan from terminal palette.
const ACCENT: Color = Color::Cyan;
/// Success: green from terminal palette.
const OK: Color = Color::Green;
/// Warning/money: yellow from terminal palette.
const WARN: Color = Color::Yellow;
/// Error: red from terminal palette.
const ERR: Color = Color::Red;

pub fn render(f: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::Main => render_main(f, app),
        Screen::JobDetail(idx) => render_detail(f, app, idx),
        Screen::Recovery => render_recovery(f, app),
    }
}

fn render_main(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header
            Constraint::Percentage(50), // table
            Constraint::Min(4),   // logs
            Constraint::Length(1), // help bar
        ])
        .split(area);

    // ── Header ──
    let price_str = if app.price == 0 {
        "FREE".to_string()
    } else {
        format!("{} SOL", format_sol_compact(app.price))
    };
    let header_line1 = Line::from(vec![
        Span::styled("  ⚡ ELISYM", Style::default().fg(WARN).bold()),
        Span::styled("  agent: ", muted()),
        Span::styled(&app.agent_name, Style::default().fg(FG).bold()),
        Span::styled("  skill: ", muted()),
        Span::styled(&app.skill_name, Style::default().fg(ACCENT).bold()),
    ]);
    let header_line2 = Line::from(vec![
        Span::styled("     price: ", muted()),
        Span::styled(
            &price_str,
            if app.price == 0 {
                Style::default().fg(WARN).bold()
            } else {
                Style::default().fg(OK).bold()
            },
        ),
        Span::styled("  wallet: ", muted()),
        Span::styled(
            format!("{} SOL", format_sol_compact(app.wallet_balance)),
            Style::default().fg(OK),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(&app.network, Style::default().fg(ACCENT)),
    ]);
    let header = Paragraph::new(vec![header_line1, header_line2]);
    f.render_widget(header, chunks[0]);

    // ── Job table ──
    let table_focus = matches!(app.focus, Focus::Table);
    let table_border_style = if table_focus {
        Style::default().fg(ACCENT)
    } else {
        muted()
    };

    let job_count = app.jobs.len();
    let title = if job_count == 0 {
        " Jobs ".to_string()
    } else {
        let running = app.jobs.iter().filter(|j| matches!(j.status, JobStatus::Processing)).count();
        let done = app.jobs.iter().filter(|j| matches!(j.status, JobStatus::Completed)).count();
        let failed = app.jobs.iter().filter(|j| matches!(j.status, JobStatus::Failed(_))).count();
        format!(" Jobs ({}) — {} running, {} done, {} failed ", job_count, running, done, failed)
    };

    let header_row = Row::new(vec![
        Cell::from(" # "),
        Cell::from("Job ID"),
        Cell::from("From"),
        Cell::from("Status"),
        Cell::from("Skill"),
        Cell::from("  Time"),
        Cell::from("    SOL"),
    ])
    .style(Style::default().bold().fg(FG))
    .bottom_margin(0);

    let rows: Vec<Row> = app
        .jobs
        .iter()
        .enumerate()
        .map(|(i, job)| {
            let short_id = if job.job_id.len() > 10 {
                format!("{}…", &job.job_id[..10])
            } else {
                job.job_id.clone()
            };
            let short_customer = if job.customer_id.len() > 10 {
                format!("{}…", &job.customer_id[..10])
            } else {
                job.customer_id.clone()
            };

            let elapsed = job
                .completed_at
                .unwrap_or_else(std::time::Instant::now)
                .duration_since(job.started_at);
            let secs = elapsed.as_secs();
            let time_str = if secs >= 60 {
                format!("{:>2}m{:02}s", secs / 60, secs % 60)
            } else {
                format!("{:>4}s", secs)
            };

            let sol_str = job
                .price
                .map(format_sol_compact)
                .unwrap_or_else(|| "   --".into());

            let (status_text, status_style) = match &job.status {
                JobStatus::PaymentPending => ("$ Awaiting", Style::default().fg(WARN)),
                JobStatus::Processing => ("⚙ Running", Style::default().fg(ACCENT)),
                JobStatus::Completed => ("✓ Done", Style::default().fg(OK)),
                JobStatus::Failed(_) => ("✗ Failed", Style::default().fg(ERR)),
            };

            let skill_str = job.skill_name.as_deref().unwrap_or("—");

            // Alternate rows: dim modifier instead of hardcoded RGB background
            let row_style = if i % 2 == 1 {
                muted()
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(format!("{:>2}", i + 1)).style(muted()),
                Cell::from(short_id).style(Style::default().fg(FG)),
                Cell::from(short_customer).style(muted()),
                Cell::from(status_text).style(status_style),
                Cell::from(skill_str).style(Style::default().fg(ACCENT)),
                Cell::from(time_str).style(muted()),
                Cell::from(format!("{:>7}", sol_str)).style(Style::default().fg(WARN)),
            ])
            .style(row_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),       // #
            Constraint::Min(14),         // Job ID (expands)
            Constraint::Min(14),         // From (expands)
            Constraint::Length(11),      // Status
            Constraint::Length(18),      // Skill
            Constraint::Length(7),       // Time
            Constraint::Length(9),       // SOL
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(table_border_style)
            .title(title),
    )
    .row_highlight_style(
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD),
    );

    f.render_stateful_widget(table, chunks[1], &mut app.table_state);

    // Empty state message
    if app.jobs.is_empty() {
        let inner = chunks[1].inner(Margin::new(1, 1));
        let empty = Paragraph::new("  Waiting for jobs…")
            .style(muted().add_modifier(Modifier::ITALIC))
            .alignment(Alignment::Left);
        // Render below the header row
        if inner.height > 2 {
            let empty_area = Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            };
            f.render_widget(empty, empty_area);
        }
    }

    // ── Logs ──
    let log_focus = matches!(app.focus, Focus::Log);
    let log_border_style = if log_focus {
        Style::default().fg(ACCENT)
    } else {
        muted()
    };

    let log_lines: Vec<Line> = app
        .global_logs
        .iter()
        .map(|l| {
            Line::from(vec![
                Span::styled(format!("  {} ", l.time), muted()),
                Span::styled(format!("{} ", l.icon), icon_style(l.icon)),
                Span::styled(&l.message, Style::default().fg(FG)),
            ])
        })
        .collect();

    let log_height = chunks[2].height.saturating_sub(2) as usize;
    let total_lines = log_lines.len();
    let max_scroll = total_lines.saturating_sub(log_height) as u16;

    // Auto-scroll to bottom only when NOT focused on log pane
    if !log_focus {
        app.log_scroll = max_scroll;
    }
    // Clamp scroll to valid range
    if app.log_scroll > max_scroll {
        app.log_scroll = max_scroll;
    }

    let log_paragraph = Paragraph::new(log_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(log_border_style)
                .title(" Log "),
        )
        .scroll((app.log_scroll, 0));

    f.render_widget(log_paragraph, chunks[2]);

    if total_lines > log_height {
        let mut scrollbar_state = ScrollbarState::new(max_scroll as usize)
            .position(app.log_scroll as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            chunks[2],
            &mut scrollbar_state,
        );
    }

    // ── Help bar ──
    let sound_label = if app.sound_enabled { "sound:on" } else { "sound:off" };
    let help = Line::from(vec![
        Span::styled("  ↑↓", Style::default().fg(FG).bold()),
        Span::styled(" select  ", muted()),
        Span::styled("Enter", Style::default().fg(FG).bold()),
        Span::styled(" detail  ", muted()),
        Span::styled("Tab", Style::default().fg(FG).bold()),
        Span::styled(" switch pane  ", muted()),
        Span::styled("r", Style::default().fg(FG).bold()),
        Span::styled(" recovery  ", muted()),
        Span::styled("s", Style::default().fg(FG).bold()),
        Span::styled(
            format!(" {}  ", sound_label),
            if app.sound_enabled {
                Style::default().fg(OK)
            } else {
                muted()
            },
        ),
        Span::styled("q", Style::default().fg(FG).bold()),
        Span::styled(" quit", muted()),
    ]);
    f.render_widget(Paragraph::new(help), chunks[3]);
}

fn render_detail(f: &mut Frame, app: &mut App, job_idx: usize) {
    let area = f.area();

    let job = match app.jobs.get(job_idx) {
        Some(j) => j,
        None => {
            app.screen = Screen::Main;
            return;
        }
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // info
            Constraint::Min(5),   // logs
            Constraint::Length(1), // help
        ])
        .split(area);

    // ── Info block ──
    let elapsed = job
        .completed_at
        .unwrap_or_else(std::time::Instant::now)
        .duration_since(job.started_at);

    let price_str = job
        .price
        .map(|p| format!("{} SOL", format_sol_compact(p)))
        .unwrap_or_else(|| "—".into());
    let net_str = job
        .net_amount
        .map(|n| format!(" (net: {} SOL)", format_sol_compact(n)))
        .unwrap_or_default();

    let trimmed = truncate_input(&job.input);
    let input_preview = if trimmed.len() < job.input.len() {
        format!("{}…", trimmed)
    } else {
        trimmed.to_string()
    };
    let input_preview = input_preview.replace('\n', " ");

    let secs = elapsed.as_secs();
    let duration_str = if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    };

    let info_text = vec![
        Line::from(vec![
            Span::styled("  From:     ", muted()),
            Span::styled(&job.customer_id, Style::default().fg(FG)),
        ]),
        Line::from(vec![
            Span::styled("  Status:   ", muted()),
            Span::styled(
                job.status.to_string(),
                match &job.status {
                    JobStatus::PaymentPending => Style::default().fg(WARN),
                    JobStatus::Processing => Style::default().fg(ACCENT),
                    JobStatus::Completed => Style::default().fg(OK),
                    JobStatus::Failed(_) => Style::default().fg(ERR),
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("  Skill:    ", muted()),
            Span::styled(
                job.skill_name.as_deref().unwrap_or("—"),
                Style::default().fg(ACCENT),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Input:    ", muted()),
            Span::styled(input_preview, Style::default().fg(FG)),
        ]),
        Line::from(vec![
            Span::styled("  Price:    ", muted()),
            Span::styled(
                format!("{}{}", price_str, net_str),
                Style::default().fg(WARN),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Duration: ", muted()),
            Span::styled(duration_str, Style::default().fg(FG)),
        ]),
    ];

    let short_id = if job.job_id.len() > 16 {
        format!("{}…", &job.job_id[..16])
    } else {
        job.job_id.clone()
    };

    let info = Paragraph::new(info_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(format!(" Job {} ", short_id))
            .title_bottom(" Esc to back "),
    );
    f.render_widget(info, chunks[0]);

    // ── Detail logs ──
    let detail_lines: Vec<Line> = job
        .logs
        .iter()
        .map(|l| {
            Line::from(vec![
                Span::styled(format!("  {} ", l.time), muted()),
                Span::styled(format!("{} ", l.icon), icon_style(l.icon)),
                Span::styled(&l.message, Style::default().fg(FG)),
            ])
        })
        .collect();

    let log_height = chunks[1].height.saturating_sub(2) as usize;
    let total_lines = detail_lines.len();
    let max_scroll = total_lines.saturating_sub(log_height) as u16;
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }

    let detail_log = Paragraph::new(detail_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" Events "),
        )
        .scroll((app.detail_scroll, 0));

    f.render_widget(detail_log, chunks[1]);

    if total_lines > log_height {
        let mut scrollbar_state = ScrollbarState::new(max_scroll as usize)
            .position(app.detail_scroll as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            chunks[1],
            &mut scrollbar_state,
        );
    }

    // ── Help ──
    let help = Line::from(vec![
        Span::styled("  Esc", Style::default().fg(FG).bold()),
        Span::styled(" back  ", muted()),
        Span::styled("↑↓", Style::default().fg(FG).bold()),
        Span::styled(" scroll", muted()),
    ]);
    f.render_widget(Paragraph::new(help), chunks[2]);
}

fn render_recovery(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Percentage(40), // table
            Constraint::Min(5),   // detail
            Constraint::Length(1), // help
        ])
        .split(area);

    // ── Title ──
    let pending = app.recovery_entries.iter()
        .filter(|e| e.status == crate::ledger::LedgerStatus::Paid || e.status == crate::ledger::LedgerStatus::Executed)
        .count();
    let title = Line::from(vec![
        Span::styled("  ⚡RECOVERY LEDGER", Style::default().fg(WARN).bold()),
        Span::styled(format!("  {} total, {} pending", app.recovery_entries.len(), pending), muted()),
    ]);
    f.render_widget(Paragraph::new(title), chunks[0]);

    // ── Table ──
    let header_row = Row::new(vec![
        Cell::from(" # "),
        Cell::from("Job ID"),
        Cell::from("Status"),
        Cell::from("Retries"),
        Cell::from("Customer"),
        Cell::from("Age"),
    ])
    .style(Style::default().bold().fg(FG))
    .bottom_margin(0);

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let rows: Vec<Row> = app
        .recovery_entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let short_id = if entry.job_id.len() > 12 {
                format!("{}…", &entry.job_id[..12])
            } else {
                entry.job_id.clone()
            };
            let short_customer = if entry.customer_id.len() > 12 {
                format!("{}…", &entry.customer_id[..12])
            } else {
                entry.customer_id.clone()
            };

            let (status_text, status_style) = match &entry.status {
                crate::ledger::LedgerStatus::Paid => ("$ Paid", Style::default().fg(WARN)),
                crate::ledger::LedgerStatus::Executed => ("⚙ Executed", Style::default().fg(ACCENT)),
                crate::ledger::LedgerStatus::Delivered => ("✓ Delivered", Style::default().fg(OK)),
                crate::ledger::LedgerStatus::Failed => ("✗ Failed", Style::default().fg(ERR)),
            };

            let age_secs = now_secs.saturating_sub(entry.created_at);
            let age_str = if age_secs >= 86400 {
                format!("{}d", age_secs / 86400)
            } else if age_secs >= 3600 {
                format!("{}h", age_secs / 3600)
            } else if age_secs >= 60 {
                format!("{}m", age_secs / 60)
            } else {
                format!("{}s", age_secs)
            };

            let row_style = if i % 2 == 1 { muted() } else { Style::default() };

            Row::new(vec![
                Cell::from(format!("{:>2}", i + 1)).style(muted()),
                Cell::from(short_id).style(Style::default().fg(FG)),
                Cell::from(status_text).style(status_style),
                Cell::from(format!("{:>4}", entry.retry_count)).style(
                    if entry.retry_count > 0 { Style::default().fg(WARN) } else { muted() }
                ),
                Cell::from(short_customer).style(muted()),
                Cell::from(format!("{:>5}", age_str)).style(muted()),
            ])
            .style(row_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),       // #
            Constraint::Min(16),         // Job ID
            Constraint::Length(12),      // Status
            Constraint::Length(8),       // Retries
            Constraint::Min(16),         // Customer
            Constraint::Length(7),       // Age
        ],
    )
    .header(header_row)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(" Ledger entries "),
    )
    .row_highlight_style(
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD),
    );

    f.render_stateful_widget(table, chunks[1], &mut app.recovery_table_state);

    // Empty state
    if app.recovery_entries.is_empty() {
        let inner = chunks[1].inner(Margin::new(1, 1));
        let empty = Paragraph::new("  No ledger entries — no paid jobs recorded yet.")
            .style(muted().add_modifier(Modifier::ITALIC))
            .alignment(Alignment::Left);
        if inner.height > 2 {
            let empty_area = Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            };
            f.render_widget(empty, empty_area);
        }
    }

    // ── Detail pane (selected entry) ──
    let detail_lines: Vec<Line> = if let Some(idx) = app.recovery_table_state.selected() {
        if let Some(entry) = app.recovery_entries.get(idx) {
            let status_str = match &entry.status {
                crate::ledger::LedgerStatus::Paid => "Paid (awaiting execution)",
                crate::ledger::LedgerStatus::Executed => "Executed (awaiting delivery)",
                crate::ledger::LedgerStatus::Delivered => "Delivered (completed)",
                crate::ledger::LedgerStatus::Failed => "Failed (gave up)",
            };

            let age_secs = now_secs.saturating_sub(entry.created_at);
            let age_str = if age_secs >= 86400 {
                format!("{}d {}h ago", age_secs / 86400, (age_secs % 86400) / 3600)
            } else if age_secs >= 3600 {
                format!("{}h {}m ago", age_secs / 3600, (age_secs % 3600) / 60)
            } else {
                format!("{}m ago", age_secs / 60)
            };

            let net_sol = crate::util::format_sol_compact(entry.net_amount);

            let trimmed = truncate_input(&entry.input);
            let input_preview = if trimmed.len() < entry.input.len() {
                format!("{}…", trimmed)
            } else {
                trimmed.to_string()
            };
            let input_preview = input_preview.replace('\n', " ");

            let has_result = entry.result.as_ref().map(|r| format!("yes ({} chars)", r.len())).unwrap_or_else(|| "no".into());

            let mut lines = vec![
                Line::from(vec![
                    Span::styled("  Job ID:    ", muted()),
                    Span::styled(&entry.job_id, Style::default().fg(FG)),
                ]),
                Line::from(vec![
                    Span::styled("  Customer:  ", muted()),
                    Span::styled(&entry.customer_id, Style::default().fg(FG)),
                ]),
                Line::from(vec![
                    Span::styled("  Status:    ", muted()),
                    Span::styled(status_str, match &entry.status {
                        crate::ledger::LedgerStatus::Paid => Style::default().fg(WARN),
                        crate::ledger::LedgerStatus::Executed => Style::default().fg(ACCENT),
                        crate::ledger::LedgerStatus::Delivered => Style::default().fg(OK),
                        crate::ledger::LedgerStatus::Failed => Style::default().fg(ERR),
                    }),
                ]),
                Line::from(vec![
                    Span::styled("  Net SOL:   ", muted()),
                    Span::styled(format!("{} SOL", net_sol), Style::default().fg(WARN)),
                ]),
                Line::from(vec![
                    Span::styled("  Retries:   ", muted()),
                    Span::styled(format!("{}/5", entry.retry_count), if entry.retry_count >= 5 {
                        Style::default().fg(ERR)
                    } else {
                        Style::default().fg(FG)
                    }),
                ]),
                Line::from(vec![
                    Span::styled("  Created:   ", muted()),
                    Span::styled(age_str, Style::default().fg(FG)),
                ]),
                Line::from(vec![
                    Span::styled("  Result:    ", muted()),
                    Span::styled(has_result, Style::default().fg(FG)),
                ]),
                Line::from(vec![
                    Span::styled("  Input:     ", muted()),
                    Span::styled(input_preview, Style::default().fg(FG)),
                ]),
            ];

            if !entry.tags.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  Tags:      ", muted()),
                    Span::styled(entry.tags.join(", "), Style::default().fg(ACCENT)),
                ]));
            }

            lines
        } else {
            vec![Line::from(Span::styled("  Select an entry above", muted()))]
        }
    } else {
        vec![Line::from(Span::styled("  Select an entry above", muted()))]
    };

    let detail = Paragraph::new(detail_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" Detail "),
        )
        .scroll((app.recovery_detail_scroll, 0));
    f.render_widget(detail, chunks[2]);

    // ── Help ──
    let help = Line::from(vec![
        Span::styled("  Esc", Style::default().fg(FG).bold()),
        Span::styled(" back  ", muted()),
        Span::styled("↑↓", Style::default().fg(FG).bold()),
        Span::styled(" select  ", muted()),
        Span::styled("Enter", Style::default().fg(FG).bold()),
        Span::styled(" retry  ", muted()),
        Span::styled("r", Style::default().fg(FG).bold()),
        Span::styled(" refresh", muted()),
    ]);
    f.render_widget(Paragraph::new(help), chunks[3]);
}

fn icon_style(icon: &str) -> Style {
    match icon {
        "▶" => Style::default().fg(ACCENT),
        "$" => Style::default().fg(WARN),
        "✓" => Style::default().fg(OK),
        "✗" => Style::default().fg(ERR),
        "⚙" => Style::default().fg(ACCENT),
        "↻" => Style::default().fg(WARN),
        "→" | "←" | "↔" => muted(),
        _ => Style::default(),
    }
}
