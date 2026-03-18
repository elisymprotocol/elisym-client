pub mod event;
pub mod ui;

use std::collections::VecDeque;
use std::time::Instant;

use ratatui::widgets::TableState;
use tokio::sync::mpsc;

/// Maximum global log lines retained.
const MAX_GLOBAL_LOGS: usize = 500;

#[derive(Debug)]
pub enum AppEvent {
    JobReceived {
        job_id: String,
        customer_id: String,
        input: String,
    },
    PaymentRequested {
        job_id: String,
        price: u64,
        fee: u64,
    },
    PaymentReceived {
        job_id: String,
        net_amount: u64,
    },
    PaymentTimeout {
        job_id: String,
    },
    SkillStarted {
        job_id: String,
        skill_name: String,
    },
    LlmRound {
        job_id: String,
        round: usize,
        max_rounds: usize,
    },
    ToolStarted {
        job_id: String,
        tool_name: String,
    },
    ToolCompleted {
        job_id: String,
        tool_name: String,
        output_len: usize,
    },
    ToolFailed {
        job_id: String,
        tool_name: String,
        error: String,
    },
    JobCompleted {
        job_id: String,
        result_len: usize,
    },
    JobFailed {
        job_id: String,
        error: String,
    },
    WalletBalance(u64),
    Ping {
        from: String,
    },
}

#[derive(Debug, Clone)]
pub enum JobStatus {
    PaymentPending,
    Processing,
    Completed,
    Failed(String),
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::PaymentPending => write!(f, "$ Payment"),
            JobStatus::Processing => write!(f, "⚙ Running"),
            JobStatus::Completed => write!(f, "✓ Done"),
            JobStatus::Failed(_) => write!(f, "✗ Failed"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub time: String,
    pub icon: &'static str,
    pub message: String,
}

pub struct JobEntry {
    pub job_id: String,
    pub customer_id: String,
    pub input: String,
    pub status: JobStatus,
    pub skill_name: Option<String>,
    pub price: Option<u64>,
    pub fee: Option<u64>,
    pub net_amount: Option<u64>,
    pub started_at: Instant,
    pub completed_at: Option<Instant>,
    pub logs: Vec<LogLine>,
}

pub enum Screen {
    Main,
    JobDetail(usize),
    Recovery,
}

pub enum Focus {
    Table,
    Log,
}

pub struct App {
    pub screen: Screen,
    pub focus: Focus,
    pub jobs: Vec<JobEntry>,
    pub global_logs: VecDeque<LogLine>,
    pub table_state: TableState,
    pub log_scroll: u16,
    pub detail_scroll: u16,
    // Header info
    pub agent_name: String,
    pub skill_name: String,
    pub price: u64,
    pub wallet_balance: u64,
    pub network: String,
    // Sound
    pub sound_enabled: bool,
    pub sound_volume: f32,
    // Recovery
    pub ledger: Option<std::sync::Arc<tokio::sync::Mutex<crate::ledger::JobLedger>>>,
    pub recovery_entries: Vec<crate::ledger::LedgerEntry>,
    pub recovery_table_state: TableState,
    pub recovery_detail_scroll: u16,
    /// Channel to trigger immediate recovery sweep in the runtime.
    pub retry_tx: Option<mpsc::UnboundedSender<String>>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_name: String,
        skill_name: String,
        price: u64,
        wallet_balance: u64,
        network: String,
        sound_enabled: bool,
        sound_volume: f32,
    ) -> Self {
        Self {
            screen: Screen::Main,
            focus: Focus::Table,
            jobs: Vec::new(),
            global_logs: VecDeque::new(),
            table_state: TableState::default(),
            log_scroll: 0,
            detail_scroll: 0,
            agent_name,
            skill_name,
            price,
            wallet_balance,
            network,
            sound_enabled,
            sound_volume,
            ledger: None,
            recovery_entries: Vec::new(),
            recovery_table_state: TableState::default(),
            recovery_detail_scroll: 0,
            retry_tx: None,
        }
    }

    pub fn set_ledger(&mut self, ledger: std::sync::Arc<tokio::sync::Mutex<crate::ledger::JobLedger>>) {
        self.ledger = Some(ledger);
    }

    pub fn set_retry_tx(&mut self, tx: mpsc::UnboundedSender<String>) {
        self.retry_tx = Some(tx);
    }

    /// Retry the currently selected recovery entry.
    /// Resets Failed → Paid/Executed in ledger and triggers immediate recovery sweep.
    pub fn retry_selected(&mut self) -> bool {
        let idx = match self.recovery_table_state.selected() {
            Some(i) => i,
            None => return false,
        };
        let entry = match self.recovery_entries.get(idx) {
            Some(e) => e,
            None => return false,
        };

        let job_id = entry.job_id.clone();
        let status = entry.status.clone();

        if matches!(status, crate::ledger::LedgerStatus::Delivered) {
            return false;
        }

        let ledger = match self.ledger {
            Some(ref l) => l.clone(),
            None => return false,
        };

        let mut lg = match ledger.try_lock() {
            Ok(lg) => lg,
            Err(_) => return false,
        };

        let reset = match status {
            crate::ledger::LedgerStatus::Failed => {
                lg.reset_for_retry(&job_id).unwrap_or(false)
            }
            crate::ledger::LedgerStatus::Paid | crate::ledger::LedgerStatus::Executed => {
                // Already pending — just reset retry count to give it fresh attempts
                let _ = lg.increment_retry(&job_id);
                true
            }
            _ => false,
        };

        if reset {
            // Refresh the view while we still hold the lock
            self.recovery_entries = lg.all_entries();
            drop(lg);

            // Notify runtime to run recovery sweep now
            if let Some(ref tx) = self.retry_tx {
                let _ = tx.send(job_id.clone());
            }
            self.add_global_log("↻", format!("Manual retry queued: {}...", &job_id[..12.min(job_id.len())]));
            return true;
        }

        false
    }

    /// Refresh recovery entries from ledger (called when opening recovery screen).
    pub fn refresh_recovery(&mut self) {
        if let Some(ref ledger) = self.ledger {
            if let Ok(lg) = ledger.try_lock() {
                self.recovery_entries = lg.all_entries();
            }
        }
    }

    pub fn toggle_sound(&mut self) {
        self.sound_enabled = !self.sound_enabled;
        let status = if self.sound_enabled { "ON" } else { "OFF" };
        self.add_global_log("♪", format!("Sound {}", status));
        // Persist to global config
        let mut gc = crate::cli::global_config::load_global_config();
        gc.tui.sound_enabled = self.sound_enabled;
        let _ = crate::cli::global_config::save_global_config(&gc);
    }

    fn now_str() -> String {
        let now = chrono::Local::now();
        now.format("%H:%M:%S").to_string()
    }

    fn add_global_log(&mut self, icon: &'static str, message: String) {
        let line = LogLine {
            time: Self::now_str(),
            icon,
            message,
        };
        self.global_logs.push_back(line);
        if self.global_logs.len() > MAX_GLOBAL_LOGS {
            self.global_logs.pop_front();
        }
    }

    fn find_job_mut(&mut self, job_id: &str) -> Option<&mut JobEntry> {
        self.jobs.iter_mut().find(|j| j.job_id == job_id)
    }

    fn add_job_log(job: &mut JobEntry, icon: &'static str, message: String) {
        job.logs.push(LogLine {
            time: Self::now_str(),
            icon,
            message,
        });
    }

    pub fn update(&mut self, event: AppEvent) {
        match event {
            AppEvent::JobReceived {
                job_id,
                customer_id,
                input,
            } => {
                let short_id = &job_id[..12.min(job_id.len())];
                let short_customer = &customer_id[..12.min(customer_id.len())];
                self.add_global_log("▶", format!("New job {}... from {}...", short_id, short_customer));

                let mut entry = JobEntry {
                    job_id: job_id.clone(),
                    customer_id,
                    input,
                    status: JobStatus::PaymentPending,
                    skill_name: None,
                    price: None,
                    fee: None,
                    net_amount: None,
                    started_at: Instant::now(),
                    completed_at: None,
                    logs: Vec::new(),
                };
                Self::add_job_log(&mut entry, "▶", "Job received".into());
                self.jobs.push(entry);

                // Auto-select first job if none selected
                if self.table_state.selected().is_none() {
                    self.table_state.select(Some(self.jobs.len() - 1));
                }
            }
            AppEvent::PaymentRequested { job_id, price, fee } => {
                let short_id = &job_id[..12.min(job_id.len())];
                let price_sol = crate::util::format_sol_compact(price);
                self.add_global_log("$", format!("Requesting payment: {} SOL [{}...]", price_sol, short_id));

                if let Some(job) = self.find_job_mut(&job_id) {
                    job.price = Some(price);
                    job.fee = Some(fee);
                    Self::add_job_log(job, "$", format!("Requesting payment: {} SOL", price_sol));
                }
            }
            AppEvent::PaymentReceived { job_id, net_amount } => {
                let short_id = &job_id[..12.min(job_id.len())];
                let net_sol = crate::util::format_sol_compact(net_amount);
                self.add_global_log("$", format!("Payment received ({} SOL net) [{}...]", net_sol, short_id));

                if let Some(job) = self.find_job_mut(&job_id) {
                    job.net_amount = Some(net_amount);
                    job.status = JobStatus::Processing;
                    Self::add_job_log(job, "✓", format!("Payment received ({} SOL net)", net_sol));
                }
            }
            AppEvent::PaymentTimeout { job_id } => {
                let short_id = &job_id[..12.min(job_id.len())];
                self.add_global_log("✗", format!("Payment timeout [{}...]", short_id));

                if let Some(job) = self.find_job_mut(&job_id) {
                    job.status = JobStatus::Failed("payment timeout".into());
                    job.completed_at = Some(Instant::now());
                    Self::add_job_log(job, "✗", "Payment timeout".into());
                }
            }
            AppEvent::SkillStarted { job_id, skill_name } => {
                let short_id = &job_id[..12.min(job_id.len())];
                self.add_global_log("⚙", format!("Running skill {} [{}...]", skill_name, short_id));

                if let Some(job) = self.find_job_mut(&job_id) {
                    job.skill_name = Some(skill_name.clone());
                    job.status = JobStatus::Processing;
                    Self::add_job_log(job, "⚙", format!("Running skill {}", skill_name));
                }
            }
            AppEvent::LlmRound {
                job_id,
                round,
                max_rounds,
            } => {
                if let Some(job) = self.find_job_mut(&job_id) {
                    Self::add_job_log(job, "⚙", format!("LLM round {}/{}", round, max_rounds));
                }
            }
            AppEvent::ToolStarted { job_id, tool_name } => {
                let short_id = &job_id[..12.min(job_id.len())];
                self.add_global_log("→", format!("Running tool {} [{}...]", tool_name, short_id));

                if let Some(job) = self.find_job_mut(&job_id) {
                    Self::add_job_log(job, "→", format!("Running tool {}", tool_name));
                }
            }
            AppEvent::ToolCompleted {
                job_id,
                tool_name,
                output_len,
            } => {
                if let Some(job) = self.find_job_mut(&job_id) {
                    Self::add_job_log(
                        job,
                        "←",
                        format!("Tool {} done ({} chars)", tool_name, output_len),
                    );
                }
            }
            AppEvent::ToolFailed {
                job_id,
                tool_name,
                error,
            } => {
                if let Some(job) = self.find_job_mut(&job_id) {
                    Self::add_job_log(
                        job,
                        "✗",
                        format!("Tool {} failed: {}", tool_name, error),
                    );
                }
            }
            AppEvent::JobCompleted { job_id, result_len } => {
                let short_id = &job_id[..12.min(job_id.len())];
                self.add_global_log(
                    "✓",
                    format!("Job {}... done ({} chars)", short_id, result_len),
                );
                if self.sound_enabled {
                    play_sound("Blow", self.sound_volume);
                }

                if let Some(job) = self.find_job_mut(&job_id) {
                    job.status = JobStatus::Completed;
                    job.completed_at = Some(Instant::now());
                    Self::add_job_log(
                        job,
                        "✓",
                        format!("Result delivered ({} chars)", result_len),
                    );
                }
            }
            AppEvent::JobFailed { job_id, error } => {
                let short_id = &job_id[..12.min(job_id.len())];
                self.add_global_log("✗", format!("Job {}... failed: {}", short_id, error));

                if let Some(job) = self.find_job_mut(&job_id) {
                    job.status = JobStatus::Failed(error.clone());
                    job.completed_at = Some(Instant::now());
                    Self::add_job_log(job, "✗", format!("Failed: {}", error));
                }
            }
            AppEvent::WalletBalance(balance) => {
                self.wallet_balance = balance;
            }
            AppEvent::Ping { from } => {
                let short = &from[..12.min(from.len())];
                self.add_global_log("↔", format!("Ping from {}... — pong sent", short));
            }
        }
    }

    pub fn select_next(&mut self) {
        if self.jobs.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| if i + 1 >= self.jobs.len() { 0 } else { i + 1 })
            .unwrap_or(0);
        self.table_state.select(Some(i));
    }

    pub fn select_prev(&mut self) {
        if self.jobs.is_empty() {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map(|i| if i == 0 { self.jobs.len() - 1 } else { i - 1 })
            .unwrap_or(0);
        self.table_state.select(Some(i));
    }
}

pub fn create_event_channel() -> (mpsc::UnboundedSender<AppEvent>, mpsc::UnboundedReceiver<AppEvent>) {
    mpsc::unbounded_channel()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_app() -> App {
        App::new(
            "test-agent".into(),
            "test-skill".into(),
            100_000_000,
            500_000_000,
            "devnet".into(),
            false, // sound disabled for tests
            0.15,
        )
    }

    // ── App::update ──

    #[test]
    fn update_job_received() {
        let mut app = new_app();
        app.update(AppEvent::JobReceived {
            job_id: "job123456789abc".into(),
            customer_id: "cust123456789abc".into(),
            input: "hello".into(),
        });
        assert_eq!(app.jobs.len(), 1);
        assert_eq!(app.jobs[0].job_id, "job123456789abc");
        assert!(matches!(app.jobs[0].status, JobStatus::PaymentPending));
        assert_eq!(app.table_state.selected(), Some(0));
        assert!(!app.global_logs.is_empty());
    }

    #[test]
    fn update_payment_received() {
        let mut app = new_app();
        app.update(AppEvent::JobReceived {
            job_id: "job123456789abc".into(),
            customer_id: "cust123456789abc".into(),
            input: "hello".into(),
        });
        app.update(AppEvent::PaymentReceived {
            job_id: "job123456789abc".into(),
            net_amount: 97_000_000,
        });
        assert!(matches!(app.jobs[0].status, JobStatus::Processing));
        assert_eq!(app.jobs[0].net_amount, Some(97_000_000));
    }

    #[test]
    fn update_job_completed() {
        let mut app = new_app();
        app.update(AppEvent::JobReceived {
            job_id: "job123456789abc".into(),
            customer_id: "cust123456789abc".into(),
            input: "hello".into(),
        });
        app.update(AppEvent::JobCompleted {
            job_id: "job123456789abc".into(),
            result_len: 42,
        });
        assert!(matches!(app.jobs[0].status, JobStatus::Completed));
        assert!(app.jobs[0].completed_at.is_some());
    }

    #[test]
    fn update_wallet_balance() {
        let mut app = new_app();
        app.update(AppEvent::WalletBalance(999_000_000));
        assert_eq!(app.wallet_balance, 999_000_000);
    }

    // ── App::select_next / select_prev ──

    #[test]
    fn select_next_wraps_around() {
        let mut app = new_app();
        // Add 3 jobs
        for i in 0..3 {
            app.update(AppEvent::JobReceived {
                job_id: format!("job{:012}", i),
                customer_id: format!("cust{:012}", i),
                input: "x".into(),
            });
        }
        app.table_state.select(Some(2)); // last
        app.select_next();
        assert_eq!(app.table_state.selected(), Some(0)); // wrapped to first
    }

    #[test]
    fn select_prev_wraps_around() {
        let mut app = new_app();
        for i in 0..3 {
            app.update(AppEvent::JobReceived {
                job_id: format!("job{:012}", i),
                customer_id: format!("cust{:012}", i),
                input: "x".into(),
            });
        }
        app.table_state.select(Some(0)); // first
        app.select_prev();
        assert_eq!(app.table_state.selected(), Some(2)); // wrapped to last
    }

    #[test]
    fn select_next_empty_noop() {
        let mut app = new_app();
        app.select_next(); // should not panic
        assert!(app.table_state.selected().is_none());
    }

    // ── App::add_global_log max cap ──

    #[test]
    fn global_log_capped_at_max() {
        let mut app = new_app();
        for i in 0..600 {
            app.add_global_log("x", format!("log {}", i));
        }
        assert_eq!(app.global_logs.len(), MAX_GLOBAL_LOGS);
        // Oldest entries were dropped — first remaining should be "log 100"
        assert_eq!(app.global_logs.front().unwrap().message, "log 100");
    }
}

/// Play a macOS system sound by name (e.g. "Blow", "Glass", "Ping").
/// Non-blocking, fire-and-forget. Does nothing on non-macOS.
fn play_sound(name: &str, volume: f32) {
    #[cfg(target_os = "macos")]
    {
        let path = format!("/System/Library/Sounds/{}.aiff", name);
        let vol = format!("{:.2}", volume.clamp(0.0, 1.0));
        let _ = std::process::Command::new("afplay")
            .args([&path, "-v", &vol])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (name, volume);
    }
}
