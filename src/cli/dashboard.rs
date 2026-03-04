use std::time::Instant;

/// Tracks runtime state for the TUI dashboard (stub — not yet wired up).
#[allow(dead_code)]
pub struct DashboardState {
    pub agent_name: String,
    pub started_at: Instant,
    pub jobs_received: u64,
    pub jobs_completed: u64,
    pub jobs_in_flight: u64,
    pub total_earned: u64,
    pub wallet_address: String,
    pub sol_balance: u64,
    pub token: String,
    pub network: String,
    pub last_event: Option<String>,
}

#[allow(dead_code)]
impl DashboardState {
    pub fn new(agent_name: String) -> Self {
        Self {
            agent_name,
            started_at: Instant::now(),
            jobs_received: 0,
            jobs_completed: 0,
            jobs_in_flight: 0,
            total_earned: 0,
            wallet_address: String::new(),
            sol_balance: 0,
            token: "sol".to_string(),
            network: "devnet".to_string(),
            last_event: None,
        }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

// TODO: Full ratatui event loop
