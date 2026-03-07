use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use std::collections::HashMap;
use nostr_sdk::{Filter, Kind, ToBech32};
use tokio::sync::watch;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Terminal;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;
use tracing::info;

use super::error::Result;

// ── Types ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AgentEntry {
    name: String,
    pubkey: String,
    capabilities: Vec<String>,
    description: String,
    price: u64,
    chain: String,
    network: String,
    solana_address: Option<String>,
    sol_balance: Option<u64>,
    payment_address: Option<String>,
    metadata: Option<serde_json::Value>,
    supported_kinds: Vec<u16>,
}

enum DashboardEvent {
    Tick,
    Key(KeyEvent),
    AgentDiscovered(Box<AgentEntry>),
    JobResultSeen { provider_npub: String, amount: u64 },
    BalanceUpdates(Vec<(String, u64)>),
    #[allow(dead_code)]
    Error(String),
}

// ── State ────────────────────────────────────────────────────────────

struct DashboardState {
    chain: String,
    network: String,
    started_at: Instant,

    // data
    discovered_agents: Vec<AgentEntry>,
    earnings: HashMap<String, u64>, // npub -> total earned (independent of discovery)
    last_update: Option<u64>,       // unix timestamp of last data update

    // navigation
    cursor: usize,
    detail_open: bool,
    detail_scroll: usize,

    quit: bool,
}

impl DashboardState {
    fn new(chain: String, network: String) -> Self {
        Self {
            chain,
            network,
            started_at: Instant::now(),
            discovered_agents: Vec::new(),
            earnings: HashMap::new(),
            last_update: None,
            cursor: 0,
            detail_open: false,
            detail_scroll: 0,
            quit: false,
        }
    }

    fn uptime_str(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        let m = secs / 60;
        let s = secs % 60;
        if m >= 60 {
            format!("{}h{:02}m{:02}s", m / 60, m % 60, s)
        } else {
            format!("{}m{:02}s", m, s)
        }
    }

    fn total_earned(&self) -> u64 {
        // Only sum earnings for agents in the discovered list
        self.discovered_agents
            .iter()
            .filter_map(|a| self.earnings.get(&a.pubkey))
            .sum()
    }

    fn agent_earned(&self, npub: &str) -> u64 {
        self.earnings.get(npub).copied().unwrap_or(0)
    }

    fn touch_update(&mut self) {
        self.last_update = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }

    fn last_update_str(&self) -> String {
        match self.last_update {
            Some(ts) => format_utc_time(ts),
            None => "syncing...".to_string(),
        }
    }

    fn handle_event(&mut self, ev: DashboardEvent) {
        match ev {
            DashboardEvent::Tick => {}
            DashboardEvent::Key(key) => self.handle_key(key),
            DashboardEvent::AgentDiscovered(boxed) => {
                let entry = *boxed;
                if let Some(existing) = self
                    .discovered_agents
                    .iter_mut()
                    .find(|a| a.pubkey == entry.pubkey)
                {
                    existing.price = entry.price;
                    existing.capabilities = entry.capabilities.clone();
                    existing.solana_address = entry.solana_address.clone();
                    existing.description = entry.description.clone();
                    existing.metadata = entry.metadata.clone();
                    existing.payment_address = entry.payment_address.clone();
                } else {
                    self.discovered_agents.push(entry);
                }
                self.touch_update();
            }
            DashboardEvent::JobResultSeen { provider_npub, amount } => {
                *self.earnings.entry(provider_npub).or_insert(0) += amount;
                // Sort: highest earned first
                self.discovered_agents.sort_by(|a, b| {
                    let ea = self.earnings.get(&a.pubkey).copied().unwrap_or(0);
                    let eb = self.earnings.get(&b.pubkey).copied().unwrap_or(0);
                    eb.cmp(&ea)
                });
                self.touch_update();
            }
            DashboardEvent::BalanceUpdates(updates) => {
                for (addr, balance) in updates {
                    if let Some(agent) = self
                        .discovered_agents
                        .iter_mut()
                        .find(|a| a.solana_address.as_deref() == Some(&addr))
                    {
                        agent.sol_balance = Some(balance);
                    }
                }
                self.touch_update();
            }
            DashboardEvent::Error(_) => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyModifiers;

        if key.kind != KeyEventKind::Press {
            return;
        }

        // Quit: q or Ctrl+C — from any screen
        if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.quit = true;
            return;
        }

        if self.detail_open {
            match key.code {
                KeyCode::Esc | KeyCode::Backspace => {
                    self.detail_open = false;
                    self.detail_scroll = 0;
                }
                KeyCode::Up => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                if !self.discovered_agents.is_empty() {
                    self.cursor = (self.cursor + 1).min(self.discovered_agents.len() - 1);
                }
            }
            KeyCode::Enter => {
                if !self.discovered_agents.is_empty() {
                    self.detail_open = true;
                    self.detail_scroll = 0;
                }
            }
            _ => {}
        }
    }
}

// ── Rendering ────────────────────────────────────────────────────────

fn render(frame: &mut ratatui::Frame, state: &DashboardState) {
    let area = frame.area();

    // Force-clear the entire frame with terminal default colors.
    // Without this, ratatui may leave stale fg/bg on light terminal themes.
    let reset_style = Style::default().fg(Color::Reset).bg(Color::Reset);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            frame.buffer_mut()[(x, y)].set_style(reset_style);
        }
    }

    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(3),   // content
        Constraint::Length(1), // footer
    ])
    .split(area);

    render_header(frame, chunks[0], state);

    if state.detail_open {
        render_detail(frame, chunks[1], state);
    } else {
        render_agents_list(frame, chunks[1], state);
    }

    render_footer(frame, chunks[2], state);
}

/// Cycle through a set of colors based on elapsed time.
fn cycle_color(elapsed_ms: u128, period_ms: u128) -> Color {
    let palette = [
        Color::Cyan,
        Color::Blue,
        Color::Magenta,
        Color::LightMagenta,
        Color::Cyan,
    ];
    let phase = (elapsed_ms % (period_ms * palette.len() as u128)) / period_ms;
    palette[phase as usize % palette.len()]
}

fn render_header(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let elapsed = state.started_at.elapsed().as_millis();

    // Braille spinner
    let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spin_idx = (elapsed / 80) as usize % spinner.len();

    // LIVE indicator — gray while syncing, red once agents discovered
    let is_live = !state.discovered_agents.is_empty();
    let live_dot = "●";
    let live_color = if is_live { Color::Red } else { Color::DarkGray };

    // Earned amount pulse — green/yellow when > 0
    let total_earned = state.total_earned();
    let earned_color = if total_earned > 0 {
        if (elapsed / 800).is_multiple_of(2) {
            Color::Green
        } else {
            Color::Yellow
        }
    } else {
        Color::DarkGray
    };

    let line = Line::from(vec![
        Span::styled(
            " ELISYM PROTOCOL DASHBOARD ",
            Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::styled(" ", Style::default()),
        Span::styled(live_dot, Style::default().fg(live_color)),
        Span::styled(" LIVE  ", Style::default().fg(live_color).bold()),
        Span::styled(&state.chain, Style::default().fg(Color::Magenta).bold()),
        Span::styled("/", Style::default().fg(Color::DarkGray)),
        Span::styled(&state.network, Style::default().fg(Color::Yellow).bold()),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{} agents", state.discovered_agents.len()),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("Total earned: {} SOL", super::format_sol_compact(total_earned)),
            Style::default().fg(earned_color).bold(),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{} {}", spinner[spin_idx], state.uptime_str()),
            Style::default().fg(Color::Green),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("updated: {}", state.last_update_str()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_agents_list(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let elapsed = state.started_at.elapsed().as_millis();
    let border_color = cycle_color(elapsed, 2000);

    if state.discovered_agents.is_empty() {
        // Animated scanning text
        let dots_count = ((elapsed / 400) % 4) as usize;
        let dots: String = ".".repeat(dots_count);
        let scan_frames = ["◜", "◠", "◝", "◞", "◡", "◟"];
        let scan_idx = (elapsed / 150) as usize % scan_frames.len();
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", scan_frames[scan_idx]),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled("Scanning network for agents", Style::default().fg(Color::Reset)),
            Span::styled(dots, Style::default().fg(Color::Cyan)),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Agents ")
                .border_style(Style::default().fg(border_color)),
        );
        frame.render_widget(msg, area);
        return;
    }

    let header = Row::new(vec![
        "Name",
        "Pubkey",
        "Capabilities",
        "Price",
        "Earned SOL",
    ])
    .style(Style::default().bold().fg(Color::Cyan).bg(Color::Reset))
    .bottom_margin(1);

    let content_height = area.height.saturating_sub(4) as usize;
    let agent_count = state.discovered_agents.len();

    let viewport_start = if state.cursor >= content_height {
        state.cursor - content_height + 1
    } else {
        0
    };

    let rows: Vec<Row> = state
        .discovered_agents
        .iter()
        .enumerate()
        .skip(viewport_start)
        .take(content_height)
        .map(|(i, a)| {
            let price_str = format!("{} SOL", super::format_sol_compact(a.price));
            let agent_earned = state.agent_earned(&a.pubkey);
            let earned_str = if agent_earned > 0 {
                super::format_sol_compact(agent_earned)
            } else {
                "—".to_string()
            };
            let caps = truncate(&a.capabilities.join(", "), 28);

            let style = if i == state.cursor {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Reset).bg(Color::Reset)
            };

            let is_cursor = i == state.cursor;
            let cell_bg = if is_cursor { Color::Blue } else { Color::Reset };

            // Earned column with color based on amount
            let earned_fg = if is_cursor {
                Color::White
            } else if agent_earned > 0 {
                Color::Green
            } else {
                Color::DarkGray
            };
            let earned_cell = Cell::from(earned_str).style(Style::default().fg(earned_fg).bg(cell_bg));

            let pubkey_fg = if is_cursor { Color::White } else { Color::DarkGray };

            Row::new(vec![
                Cell::from(truncate(&a.name, 20)),
                Cell::from(truncate(&a.pubkey, 16)).style(Style::default().fg(pubkey_fg).bg(cell_bg)),
                Cell::from(caps),
                Cell::from(price_str),
                earned_cell,
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(22),
        Constraint::Length(18),
        Constraint::Length(30),
        Constraint::Length(16),
        Constraint::Length(12),
    ];

    let title = format!(" Agents ({}) ", agent_count);

    let table = Table::new(rows, widths)
        .header(header)
        .style(Style::default().fg(Color::Reset).bg(Color::Reset))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, Style::default().fg(Color::Cyan).bold()))
                .border_style(Style::default().fg(border_color)),
        );

    frame.render_widget(table, area);
}

fn render_detail(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let agent = match state.discovered_agents.get(state.cursor) {
        Some(a) => a,
        None => return,
    };

    let mut lines: Vec<Line> = vec![
        // Name
        Line::from(vec![
            Span::styled("  Name          ", Style::default().fg(Color::DarkGray)),
            Span::styled(&agent.name, Style::default().fg(Color::Cyan).bold()),
        ]),
        Line::raw(""),
        // Description
        Line::from(vec![
            Span::styled("  Description   ", Style::default().fg(Color::DarkGray)),
            Span::styled(&agent.description, Style::default().fg(Color::Reset)),
        ]),
        Line::raw(""),
        // Pubkey (full)
        Line::from(vec![
            Span::styled("  Pubkey        ", Style::default().fg(Color::DarkGray)),
            Span::styled(&agent.pubkey, Style::default().fg(Color::Reset)),
        ]),
        Line::raw(""),
        // Capabilities header
        Line::from(Span::styled(
            "  Capabilities",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if agent.capabilities.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (none)",
            Style::default().fg(Color::DarkGray).italic(),
        )));
    } else {
        for cap in &agent.capabilities {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled("• ", Style::default().fg(Color::Cyan)),
                Span::styled(cap.as_str(), Style::default().fg(Color::Reset)),
            ]));
        }
    }
    lines.push(Line::raw(""));

    // Price
    let price_str = format!("{} SOL", super::format_sol_compact(agent.price));
    lines.push(Line::from(vec![
        Span::styled("  Price         ", Style::default().fg(Color::DarkGray)),
        Span::styled(price_str, Style::default().fg(Color::Green)),
    ]));
    lines.push(Line::raw(""));

    // Chain / Network
    lines.push(Line::from(vec![
        Span::styled("  Chain         ", Style::default().fg(Color::DarkGray)),
        Span::styled(&agent.chain, Style::default().fg(Color::Magenta)),
        Span::styled(" / ", Style::default().fg(Color::DarkGray)),
        Span::styled(&agent.network, Style::default().fg(Color::Yellow)),
    ]));
    lines.push(Line::raw(""));

    // Payment address
    if let Some(ref addr) = agent.payment_address {
        lines.push(Line::from(vec![
            Span::styled("  Pay Address   ", Style::default().fg(Color::DarkGray)),
            Span::styled(addr.as_str(), Style::default().fg(Color::Reset)),
        ]));
        lines.push(Line::raw(""));
    }

    // SOL Balance
    if let Some(balance) = agent.sol_balance {
        lines.push(Line::from(vec![
            Span::styled("  SOL Balance   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} SOL", super::format_sol(balance)),
                Style::default().fg(Color::Green),
            ),
        ]));
        lines.push(Line::raw(""));
    }

    // Total Earned
    let agent_earned = state.agent_earned(&agent.pubkey);
    if agent_earned > 0 {
        lines.push(Line::from(vec![
            Span::styled("  Total Earned  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} SOL", super::format_sol(agent_earned)),
                Style::default().fg(Color::Yellow).bold(),
            ),
        ]));
        lines.push(Line::raw(""));
    }

    // Supported job kinds
    if !agent.supported_kinds.is_empty() {
        let kinds_str: Vec<String> = agent.supported_kinds.iter().map(|k| k.to_string()).collect();
        lines.push(Line::from(vec![
            Span::styled("  Job Kinds     ", Style::default().fg(Color::DarkGray)),
            Span::styled(kinds_str.join(", "), Style::default().fg(Color::Reset)),
        ]));
        lines.push(Line::raw(""));
    }

    // Metadata (raw JSON, if present)
    if let Some(ref meta) = agent.metadata {
        lines.push(Line::from(Span::styled(
            "  Metadata",
            Style::default().fg(Color::DarkGray),
        )));
        if let Ok(pretty) = serde_json::to_string_pretty(meta) {
            for json_line in pretty.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {}", json_line),
                    Style::default().fg(Color::Reset),
                )));
            }
        }
        lines.push(Line::raw(""));
    }

    // Apply scroll
    let visible_height = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(visible_height);
    let scroll = state.detail_scroll.min(max_scroll);

    let elapsed = state.started_at.elapsed().as_millis();
    let border_color = cycle_color(elapsed, 2000);

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    format!(" {} ", agent.name),
                    Style::default().fg(Color::Cyan).bold(),
                ))
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));

    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let controls = if state.detail_open {
        vec![
            Span::styled(" esc", Style::default().fg(Color::Cyan).bold()),
            Span::styled(":back  ", Style::default().fg(Color::DarkGray)),
            Span::styled("↑↓", Style::default().fg(Color::Cyan).bold()),
            Span::styled(":scroll  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q/ctrl+c", Style::default().fg(Color::Cyan).bold()),
            Span::styled(":quit", Style::default().fg(Color::DarkGray)),
        ]
    } else {
        vec![
            Span::styled(" ↑↓", Style::default().fg(Color::Cyan).bold()),
            Span::styled(":navigate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("enter", Style::default().fg(Color::Cyan).bold()),
            Span::styled(":details  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q/ctrl+c", Style::default().fg(Color::Cyan).bold()),
            Span::styled(":quit", Style::default().fg(Color::DarkGray)),
        ]
    };

    let mut spans = controls;
    spans.push(Span::styled("  | ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        "Elisym protocol - Decentralized AI agent economy",
        Style::default().fg(Color::DarkGray),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Terminal guard ───────────────────────────────────────────────────

struct TerminalGuard;

impl TerminalGuard {
    fn init() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(terminal)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

// ── Event collectors ─────────────────────────────────────────────────

fn spawn_tick(tx: mpsc::Sender<DashboardEvent>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        loop {
            interval.tick().await;
            if tx.send(DashboardEvent::Tick).await.is_err() {
                break;
            }
        }
    });
}

fn spawn_input(tx: mpsc::Sender<DashboardEvent>) {
    tokio::task::spawn_blocking(move || {
        let mut consecutive_errors: u32 = 0;
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {
                    consecutive_errors = 0;
                    if let Ok(CEvent::Key(key)) = event::read() {
                        if tx.blocking_send(DashboardEvent::Key(key)).is_err() {
                            break;
                        }
                    }
                }
                Ok(false) => {
                    consecutive_errors = 0;
                    if tx.is_closed() {
                        break;
                    }
                }
                Err(_) => {
                    consecutive_errors += 1;
                    // Persistent terminal errors — back off to avoid CPU spin
                    if consecutive_errors > 50 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
}

fn spawn_discovery(
    agent: Arc<elisym_core::AgentNode>,
    chain: String,
    network: String,
    tx: mpsc::Sender<DashboardEvent>,
    addr_tx: watch::Sender<Vec<String>>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let filter = elisym_core::AgentFilter::default();
            match agent.discovery.search_agents(&filter).await {
                Ok(agents) => {
                    // Share discovered payment addresses with balance poller
                    // (collected before the consuming loop below)
                    let addrs: Vec<String> = agents
                        .iter()
                        .filter_map(|a| a.card.payment_address.clone())
                        .collect();
                    let _ = addr_tx.send(addrs);

                    for a in agents {
                        // Filter by chain + network
                        let agent_chain = super::extract_chain(&a);
                        let agent_network = super::extract_network(&a);
                        if !agent_chain.eq_ignore_ascii_case(&chain)
                            || !agent_network.eq_ignore_ascii_case(&network)
                        {
                            continue;
                        }

                        // Skip agents without capabilities (e.g. observer nodes)
                        if a.card.capabilities.is_empty() {
                            continue;
                        }

                        let price = super::extract_job_price(&a);
                        let npub_str = a.pubkey.to_bech32().unwrap_or_default();
                        let entry = AgentEntry {
                            name: a.card.name.clone(),
                            pubkey: npub_str,
                            capabilities: a.card.capabilities.clone(),
                            description: a.card.description.clone(),
                            price,
                            chain: agent_chain,
                            network: agent_network,
                            solana_address: a.card.payment_address.clone(),
                            sol_balance: None,
                            payment_address: a.card.payment_address.clone(),
                            metadata: a.card.metadata.clone(),
                            supported_kinds: a.supported_kinds.clone(),
                        };
                        if tx.send(DashboardEvent::AgentDiscovered(Box::new(entry))).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(DashboardEvent::Error(format!("discovery: {}", e)))
                        .await;
                }
            }
        }
    });
}

fn spawn_balance_poller(
    rpc: Arc<RpcClient>,
    addr_rx: watch::Receiver<Vec<String>>,
    tx: mpsc::Sender<DashboardEvent>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;

            // Read addresses shared by the discovery task (no extra relay calls)
            let addresses = addr_rx.borrow().clone();

            if addresses.is_empty() {
                continue;
            }

            let rpc = Arc::clone(&rpc);
            let result = tokio::task::spawn_blocking(move || {
                let mut updates = Vec::new();
                for addr_str in &addresses {
                    if let Ok(pubkey) = addr_str.parse::<Pubkey>() {
                        if let Ok(balance) = rpc.get_balance(&pubkey) {
                            updates.push((addr_str.clone(), balance));
                        }
                    }
                }
                updates
            })
            .await;

            if let Ok(updates) = result {
                if !updates.is_empty()
                    && tx.send(DashboardEvent::BalanceUpdates(updates)).await.is_err()
                {
                    break;
                }
            }
        }
    });
}

fn spawn_job_results(
    agent: Arc<elisym_core::AgentNode>,
    tx: mpsc::Sender<DashboardEvent>,
) {
    const MAX_SEEN: usize = 10_000;

    tokio::spawn(async move {
        let result_kind = Kind::from(elisym_core::KIND_JOB_RESULT_BASE + elisym_core::DEFAULT_KIND_OFFSET);
        let mut seen = std::collections::HashSet::new();
        let mut since_ts = nostr_sdk::Timestamp::now();
        let mut interval = tokio::time::interval(Duration::from_secs(15));

        loop {
            interval.tick().await;

            // Fetch only events newer than last poll
            let filter = Filter::new().kind(result_kind).since(since_ts);
            let events = match agent
                .client
                .fetch_events(vec![filter], Some(Duration::from_secs(10)))
                .await
            {
                Ok(events) => events,
                Err(_) => continue,
            };

            for event in events {
                // Track latest timestamp for next poll
                if event.created_at > since_ts {
                    since_ts = event.created_at;
                }
                // Deduplicate within the current window
                if !seen.insert(event.id) {
                    continue;
                }
                // Prune seen set to cap memory growth. Clear-all is acceptable
                // because `since_ts` advances each poll, so old events won't
                // reappear from relays.
                if seen.len() > MAX_SEEN {
                    seen.clear();
                    seen.insert(event.id);
                }

                // Extract amount from "amount" tag
                let amount = event.tags.iter().find_map(|tag| {
                    let s = tag.as_slice();
                    if s.first().map(|v| v.as_str()) == Some("amount") {
                        s.get(1).and_then(|v| v.parse::<u64>().ok())
                    } else {
                        None
                    }
                });

                let amount = match amount {
                    Some(a) => a,
                    None => continue,
                };

                let provider_npub = event.pubkey.to_bech32().unwrap_or_default();

                if tx
                    .send(DashboardEvent::JobResultSeen {
                        provider_npub,
                        amount,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    });
}

// ── Entry point ──────────────────────────────────────────────────────

pub async fn run_dashboard(chain: String, network: String, rpc_url: Option<String>) -> Result<()> {
    info!(chain = %chain, network = %network, "launching protocol dashboard (observer mode)");

    // Build ephemeral observer node — no identity, no wallet, no capabilities
    let agent = elisym_core::AgentNodeBuilder::new("elisym-observer", "Protocol dashboard observer")
        .capabilities(vec![])
        .build()
        .await?;
    let agent = Arc::new(agent);

    // Build Solana RPC client for balance queries
    let resolved_rpc = super::resolve_rpc_url(&network, rpc_url.as_deref());
    let rpc = Arc::new(RpcClient::new_with_commitment(
        resolved_rpc,
        CommitmentConfig::confirmed(),
    ));

    // Init state
    let mut state = DashboardState::new(chain.clone(), network.clone());

    // Channel
    let (tx, mut rx) = mpsc::channel::<DashboardEvent>(256);

    // Shared address list: discovery writes, balance poller reads (no duplicate relay calls)
    let (addr_tx, addr_rx) = watch::channel::<Vec<String>>(vec![]);

    // Spawn collectors
    spawn_tick(tx.clone());
    spawn_input(tx.clone());
    spawn_discovery(Arc::clone(&agent), chain, network, tx.clone(), addr_tx);
    spawn_balance_poller(Arc::clone(&rpc), addr_rx, tx.clone());
    spawn_job_results(Arc::clone(&agent), tx.clone());
    drop(tx);

    // Terminal
    let _guard = TerminalGuard;
    let mut terminal = TerminalGuard::init()?;

    // Render loop
    loop {
        terminal.draw(|frame| render(frame, &state))?;

        if let Some(ev) = rx.recv().await {
            state.handle_event(ev);
        } else {
            break;
        }

        while let Ok(ev) = rx.try_recv() {
            state.handle_event(ev);
        }

        if state.quit {
            break;
        }
    }

    // Cleanup: restore terminal, drop receiver to signal tasks to stop
    drop(terminal);
    drop(_guard);
    drop(rx);

    // Brief grace period for spawned tasks to notice closed channel and exit
    tokio::time::sleep(Duration::from_millis(200)).await;

    info!("dashboard closed");
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let end: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{}…", end)
    }
}

/// Format a unix timestamp as UTC time HH:MM:SS.
fn format_utc_time(unix_ts: u64) -> String {
    let secs_of_day = unix_ts % 86400;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    format!("{:02}:{:02}:{:02} UTC", h, m, s)
}
