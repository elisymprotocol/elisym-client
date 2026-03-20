use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::cli::error::{CliError, Result};
use crate::ledger::{JobLedger, LedgerStatus};
use crate::skill::{SkillContext, SkillInput, SkillRegistry};
use crate::transport::{IncomingJob, JobFeedbackStatus, Transport, TransportRaw};
use crate::tui::AppEvent;

use nostr_sdk::{EventId, EventBuilder, PublicKey, ToBech32};
use nostr_sdk::nips::nip19::Nip19Event;

use elisym_core::{AgentNode, calculate_protocol_fee};

use crate::util::format_sol_compact;

/// RAII guard that removes a job_id from the in-flight set on drop.
/// Guarantees cleanup even on panic or early return.
struct InFlightGuard {
    job_id: String,
    set: Arc<Mutex<HashSet<String>>>,
}

impl InFlightGuard {
    fn new(job_id: String, set: Arc<Mutex<HashSet<String>>>) -> Self {
        Self { job_id, set }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let job_id = self.job_id.clone();
        let set = Arc::clone(&self.set);
        // Use try_lock to avoid blocking in drop; if lock is held, spawn a task
        let removed = {
            if let Ok(mut guard) = set.try_lock() {
                guard.remove(&job_id);
                true
            } else {
                false
            }
        };
        if !removed {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    set.lock().await.remove(&job_id);
                });
            }
        }
    }
}

pub struct AgentRuntime {
    agent: Arc<AgentNode>,
    skills: SkillRegistry,
    ctx: SkillContext,
    config: RuntimeConfig,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    ledger: Arc<Mutex<JobLedger>>,
    retry_rx: Option<mpsc::UnboundedReceiver<String>>,
}

pub struct RuntimeConfig {
    pub job_price: u64,
    pub payment_timeout_secs: u32,
    pub max_concurrent_jobs: usize,
    pub recovery_max_retries: u32,
    pub recovery_interval_secs: u64,
    pub network: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            job_price: 10_000_000,
            payment_timeout_secs: 120,
            max_concurrent_jobs: 10,
            recovery_max_retries: 5,
            recovery_interval_secs: 60,
            network: "devnet".to_string(),
        }
    }
}

impl AgentRuntime {
    pub fn new(
        agent: Arc<AgentNode>,
        skills: SkillRegistry,
        ctx: SkillContext,
        config: RuntimeConfig,
        event_tx: mpsc::UnboundedSender<AppEvent>,
        ledger: Arc<Mutex<JobLedger>>,
    ) -> Self {
        Self {
            agent,
            skills,
            ctx,
            config,
            event_tx,
            ledger,
            retry_rx: None,
        }
    }

    pub fn set_retry_rx(&mut self, rx: mpsc::UnboundedReceiver<String>) {
        self.retry_rx = Some(rx);
    }

    pub async fn run(self, transport: Box<dyn Transport>) -> Result<()> {
        let mut jobs_rx = transport.start().await?;

        let transport = Arc::new(transport);
        let skills = Arc::new(self.skills);
        let ctx = Arc::new(self.ctx);
        let agent = self.agent;
        let config = Arc::new(self.config);
        let event_tx = self.event_tx;
        let ledger = self.ledger;
        let mut retry_rx = self.retry_rx;

        let mut tasks: JoinSet<()> = JoinSet::new();
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs));
        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        // Run startup recovery
        recover_pending_jobs(
            &ledger,
            &agent,
            &skills,
            &ctx,
            &config,
            transport.as_ref().as_ref(),
            &event_tx,
            &in_flight,
        )
        .await;

        // GC old entries (older than 7 days)
        {
            let mut lg = ledger.lock().await;
            let _ = lg.gc(7 * 24 * 3600);
        }

        // Spawn periodic recovery sweep
        let recovery_transport = Arc::clone(&transport);
        let recovery_skills = Arc::clone(&skills);
        let recovery_ctx = Arc::clone(&ctx);
        let recovery_agent = Arc::clone(&agent);
        let recovery_config = Arc::clone(&config);
        let recovery_etx = event_tx.clone();
        let recovery_ledger = Arc::clone(&ledger);
        let recovery_in_flight = Arc::clone(&in_flight);
        let recovery_interval = config.recovery_interval_secs;
        let recovery_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(recovery_interval));
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                recover_pending_jobs(
                    &recovery_ledger,
                    &recovery_agent,
                    &recovery_skills,
                    &recovery_ctx,
                    &recovery_config,
                    recovery_transport.as_ref().as_ref(),
                    &recovery_etx,
                    &recovery_in_flight,
                )
                .await;
            }
        });

        loop {
            tokio::select! {
                Some(job) = jobs_rx.recv() => {
                    // Dedup: atomic check+insert into in-flight set
                    {
                        let mut ifl = in_flight.lock().await;
                        if !ifl.insert(job.job_id.clone()) {
                            tracing::debug!(job_id = %job.job_id, "Skipping duplicate job (in-flight)");
                            continue;
                        }
                    }
                    // Check ledger separately (without holding in_flight lock)
                    {
                        if ledger.lock().await.get_status(&job.job_id).is_some() {
                            in_flight.lock().await.remove(&job.job_id);
                            tracing::debug!(job_id = %job.job_id, "Skipping duplicate job (in ledger)");
                            continue;
                        }
                    }

                    let _ = event_tx.send(AppEvent::JobReceived {
                        job_id: job.job_id.clone(),
                        customer_id: job.customer_id.clone(),
                        input: job.input.clone(),
                    });

                    let transport = Arc::clone(&transport);
                    let skills = Arc::clone(&skills);
                    let ctx = Arc::clone(&ctx);
                    let agent = Arc::clone(&agent);
                    let config = Arc::clone(&config);
                    let sem = Arc::clone(&semaphore);
                    let etx = event_tx.clone();
                    let ledger = Arc::clone(&ledger);
                    let in_flight = Arc::clone(&in_flight);

                    tasks.spawn(async move {
                        let _guard = InFlightGuard::new(job.job_id.clone(), Arc::clone(&in_flight));
                        let _permit = match sem.acquire().await {
                            Ok(p) => p,
                            Err(_) => return,
                        };
                        let job_id = job.job_id.clone();
                        if let Err(e) = process_job(&agent, &skills, &ctx, &config, job, transport.as_ref().as_ref(), &etx, &ledger).await {
                            let _ = etx.send(AppEvent::JobFailed {
                                job_id,
                                error: e.to_string(),
                            });
                        }
                    });
                }
                Some(_job_id) = async { match retry_rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                    // Manual retry triggered from TUI — run immediate recovery sweep
                    recover_pending_jobs(
                        &ledger,
                        &agent,
                        &skills,
                        &ctx,
                        &config,
                        transport.as_ref().as_ref(),
                        &event_tx,
                        &in_flight,
                    )
                    .await;
                }
                _ = tokio::signal::ctrl_c() => {
                    break;
                }
                Some(result) = tasks.join_next() => {
                    if let Err(e) = result {
                        let _ = event_tx.send(AppEvent::JobFailed {
                            job_id: String::new(),
                            error: format!("task panicked: {}", e),
                        });
                    }
                }
            }
            while let Some(result) = tasks.try_join_next() {
                if let Err(e) = result {
                    let _ = event_tx.send(AppEvent::JobFailed {
                        job_id: String::new(),
                        error: format!("task panicked: {}", e),
                    });
                }
            }
        }

        // Stop recovery sweep
        recovery_handle.abort();

        // Drain remaining tasks with timeout
        if !tasks.is_empty() {
            let deadline = tokio::time::sleep(Duration::from_secs(30));
            tokio::pin!(deadline);

            loop {
                tokio::select! {
                    Some(_result) = tasks.join_next() => {}
                    _ = &mut deadline => {
                        tasks.abort_all();
                        break;
                    }
                }
                if tasks.is_empty() {
                    break;
                }
            }
        }

        // Drop agent on blocking thread to avoid async drop issues
        match Arc::try_unwrap(agent) {
            Ok(agent) => {
                tokio::task::spawn_blocking(move || drop(agent)).await.ok();
            }
            Err(arc) => {
                tokio::task::spawn_blocking(move || drop(arc)).await.ok();
            }
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_job(
    agent: &AgentNode,
    skills: &SkillRegistry,
    ctx: &SkillContext,
    config: &RuntimeConfig,
    job: IncomingJob,
    transport: &dyn Transport,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    ledger: &Mutex<JobLedger>,
) -> Result<()> {
    let job_id = job.job_id.clone();
    let (amount, payment_request_str, tx_signature) = if config.job_price == 0 {
        (None, None, None)
    } else {
        let (net, pr, tx_sig) = collect_payment(agent, &job, transport, config.job_price, config.payment_timeout_secs, event_tx).await?;
        (Some(net), Some(pr), tx_sig)
    };

    // Record in ledger after payment confirmed (before execution)
    if let Some(ref pr) = payment_request_str {
        let raw_json = match &job.raw {
            TransportRaw::Nostr { job_request } => {
                serde_json::to_string(&job_request.raw_event).unwrap_or_default()
            }
        };
        let mut lg = ledger.lock().await;
        let _ = lg.record_paid(
            &job_id,
            &job.input,
            &job.input_type,
            &job.tags,
            &job.customer_id,
            job.bid,
            pr,
            amount.unwrap_or(0),
            &raw_json,
        );
    }

    // Send Processing feedback
    transport
        .send_feedback(&job, JobFeedbackStatus::Processing)
        .await?;

    // Route to skill and execute
    let skill = skills
        .route(&job.tags)
        .ok_or_else(|| CliError::Other("no skill available to handle this job".into()))?;

    let _ = event_tx.send(AppEvent::SkillStarted {
        job_id: job_id.clone(),
        skill_name: skill.name().to_string(),
    });

    let input = SkillInput {
        data: job.input.clone(),
        input_type: job.input_type.clone(),
        tags: job.tags.clone(),
        metadata: serde_json::Value::Null,
        job_id: job_id.clone(),
    };

    let output = match skill.execute(input, ctx).await {
        Ok(out) => out,
        Err(e) => {
            let _ = event_tx.send(AppEvent::JobFailed {
                job_id: job_id.clone(),
                error: e.to_string(),
            });
            transport
                .send_feedback(
                    &job,
                    JobFeedbackStatus::Error(format!("processing failed: {}", e)),
                )
                .await?;
            // Mark failed in ledger
            let mut lg = ledger.lock().await;
            let _ = lg.mark_failed(&job_id);
            return Err(e);
        }
    };

    // Mark executed with cached result (before delivery attempt)
    {
        let mut lg = ledger.lock().await;
        let _ = lg.mark_executed(&job_id, &output.data);
    }

    let result_len = output.data.len();
    let result_event_id = transport.deliver_result(&job, &output.data, amount).await?;

    // Mark delivered in ledger
    {
        let mut lg = ledger.lock().await;
        let _ = lg.mark_delivered(&job_id);
    }

    let _ = event_tx.send(AppEvent::JobCompleted {
        job_id,
        result_len,
    });

    // Publish deal note
    if let Some(net_amount) = amount {
        publish_deal_note(agent, &job, result_event_id, net_amount, tx_signature.as_deref(), &config.network).await;
    } else {
        publish_free_note(agent, &job, result_event_id).await;
    }

    // Update wallet balance
    if let Some(solana) = agent.solana_payments() {
        if let Ok(balance) = solana.balance() {
            let _ = event_tx.send(AppEvent::WalletBalance(balance));
        }
    }

    Ok(())
}

/// Recover pending jobs from ledger.
///
/// - `Executed` jobs (have result cached) → retry delivery only.
/// - `Paid` jobs (no result) → re-execute skill + deliver.
#[allow(clippy::too_many_arguments)]
async fn recover_pending_jobs(
    ledger: &Mutex<JobLedger>,
    agent: &AgentNode,
    skills: &SkillRegistry,
    ctx: &SkillContext,
    config: &RuntimeConfig,
    transport: &dyn Transport,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    in_flight: &Mutex<HashSet<String>>,
) {
    let pending: Vec<_> = {
        let lg = ledger.lock().await;
        lg.pending_jobs().into_iter().cloned().collect()
    };

    if pending.is_empty() {
        return;
    }

    for entry in pending {
        // Skip jobs currently being processed by the main loop
        if in_flight.lock().await.contains(&entry.job_id) {
            tracing::debug!(job_id = %entry.job_id, "Recovery: skipping in-flight job");
            continue;
        }
        if entry.retry_count >= config.recovery_max_retries {
            let mut lg = ledger.lock().await;
            let _ = lg.mark_failed(&entry.job_id);
            let _ = event_tx.send(AppEvent::JobFailed {
                job_id: entry.job_id.clone(),
                error: format!("max retries ({}) exceeded", config.recovery_max_retries),
            });
            continue;
        }

        // Increment retry count
        {
            let mut lg = ledger.lock().await;
            let _ = lg.increment_retry(&entry.job_id);
        }

        // Verify payment is still confirmed on-chain
        if config.job_price != 0 {
            let still_paid = if let Some(payments) = agent.payments.as_ref() {
                match payments.lookup_payment(&entry.payment_request) {
                    Ok(status) => status.settled,
                    Err(_) => false,
                }
            } else {
                false
            };

            if !still_paid {
                let mut lg = ledger.lock().await;
                let _ = lg.mark_failed(&entry.job_id);
                continue;
            }
        }

        // Reconstruct the raw Nostr event
        let raw_event: nostr_sdk::Event = match serde_json::from_str(&entry.raw_event_json) {
            Ok(ev) => ev,
            Err(_) => {
                let mut lg = ledger.lock().await;
                let _ = lg.mark_failed(&entry.job_id);
                let _ = event_tx.send(AppEvent::JobFailed {
                    job_id: entry.job_id.clone(),
                    error: "cannot deserialize stored event".into(),
                });
                continue;
            }
        };

        // Reconstruct IncomingJob from ledger entry
        let job = IncomingJob {
            job_id: entry.job_id.clone(),
            input: entry.input.clone(),
            input_type: entry.input_type.clone(),
            tags: entry.tags.clone(),
            customer_id: entry.customer_id.clone(),
            bid: entry.bid,
            raw: TransportRaw::Nostr {
                job_request: elisym_core::marketplace::JobRequest {
                    event_id: raw_event.id,
                    customer: raw_event.pubkey,
                    kind_offset: raw_event.kind.as_u16().saturating_sub(5000),
                    input_data: entry.input.clone(),
                    input_type: entry.input_type.clone(),
                    output_mime: None,
                    bid: entry.bid,
                    tags: entry.tags.clone(),
                    raw_event,
                },
            },
        };

        let amount = if config.job_price == 0 {
            None
        } else {
            Some(entry.net_amount)
        };

        match entry.status {
            LedgerStatus::Executed => {
                // Re-check status — normal flow may have delivered while we iterated
                {
                    let lg = ledger.lock().await;
                    if lg.get_status(&entry.job_id) != Some(LedgerStatus::Executed) {
                        continue;
                    }
                }

                // Result cached — just retry delivery
                if let Some(ref result) = entry.result {
                    let _ = event_tx.send(AppEvent::SkillStarted {
                        job_id: entry.job_id.clone(),
                        skill_name: "recovery:deliver".into(),
                    });

                    match transport.deliver_result(&job, result, amount).await {
                        Ok(_) => {
                            let mut lg = ledger.lock().await;
                            let _ = lg.mark_delivered(&entry.job_id);
                            let _ = event_tx.send(AppEvent::JobCompleted {
                                job_id: entry.job_id.clone(),
                                result_len: result.len(),
                            });
                        }
                        Err(e) => {
                            let _ = event_tx.send(AppEvent::JobFailed {
                                job_id: entry.job_id.clone(),
                                error: format!("recovery delivery failed: {}", e),
                            });
                        }
                    }
                } else {
                    // Marked as executed but no result cached — treat as Paid
                    recover_execute_and_deliver(
                        &entry, &job, amount, skills, ctx, transport, event_tx, ledger,
                    )
                    .await;
                }
            }
            LedgerStatus::Paid => {
                // Re-check status
                {
                    let lg = ledger.lock().await;
                    if lg.get_status(&entry.job_id) != Some(LedgerStatus::Paid) {
                        continue;
                    }
                }
                // Need to re-execute skill and deliver
                recover_execute_and_deliver(
                    &entry, &job, amount, skills, ctx, transport, event_tx, ledger,
                )
                .await;
            }
            _ => {} // Delivered/Failed — skip
        }
    }
}

/// Re-execute a skill for a recovered job and deliver the result.
#[allow(clippy::too_many_arguments)]
async fn recover_execute_and_deliver(
    entry: &crate::ledger::LedgerEntry,
    job: &IncomingJob,
    amount: Option<u64>,
    skills: &SkillRegistry,
    ctx: &SkillContext,
    transport: &dyn Transport,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
    ledger: &Mutex<JobLedger>,
) {
    let skill = match skills.route(&entry.tags) {
        Some(s) => s,
        None => {
            let mut lg = ledger.lock().await;
            let _ = lg.mark_failed(&entry.job_id);
            let _ = event_tx.send(AppEvent::JobFailed {
                job_id: entry.job_id.clone(),
                error: "no skill available for recovery".into(),
            });
            return;
        }
    };

    let _ = event_tx.send(AppEvent::SkillStarted {
        job_id: entry.job_id.clone(),
        skill_name: format!("recovery:{}", skill.name()),
    });

    let input = SkillInput {
        data: entry.input.clone(),
        input_type: entry.input_type.clone(),
        tags: entry.tags.clone(),
        metadata: serde_json::Value::Null,
        job_id: entry.job_id.clone(),
    };

    match skill.execute(input, ctx).await {
        Ok(output) => {
            // Cache result
            {
                let mut lg = ledger.lock().await;
                let _ = lg.mark_executed(&entry.job_id, &output.data);
            }

            match transport.deliver_result(job, &output.data, amount).await {
                Ok(_) => {
                    let mut lg = ledger.lock().await;
                    let _ = lg.mark_delivered(&entry.job_id);
                    let _ = event_tx.send(AppEvent::JobCompleted {
                        job_id: entry.job_id.clone(),
                        result_len: output.data.len(),
                    });
                }
                Err(e) => {
                    let _ = event_tx.send(AppEvent::JobFailed {
                        job_id: entry.job_id.clone(),
                        error: format!("recovery delivery failed: {}", e),
                    });
                }
            }
        }
        Err(e) => {
            let mut lg = ledger.lock().await;
            let _ = lg.mark_failed(&entry.job_id);
            let _ = event_tx.send(AppEvent::JobFailed {
                job_id: entry.job_id.clone(),
                error: format!("recovery execution failed: {}", e),
            });
        }
    }
}

/// Publish a kind:1 Nostr note celebrating a completed paid job.
/// Best-effort: logs warning on failure, never propagates errors.
async fn publish_deal_note(
    agent: &AgentNode,
    job: &IncomingJob,
    result_event_id: EventId,
    net_amount: u64,
    tx_signature: Option<&str>,
    network: &str,
) {
    if net_amount == 0 {
        tracing::debug!("Skipping deal note for job {} — zero amount", job.job_id);
        return;
    }

    let sol_display = if network != "mainnet" {
        format!("{} SOL ({})", format_sol_compact(net_amount), network)
    } else {
        format!("{} SOL", format_sol_compact(net_amount))
    };

    // Encode job event ID as nevent
    let job_event_id = match EventId::parse(&job.job_id) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("Failed to parse job event ID for deal note: {}", e);
            return;
        }
    };
    let relays: Vec<String> = vec![
        "wss://relay.damus.io".into(),
        "wss://nos.lol".into(),
    ];
    let job_nevent = Nip19Event {
        event_id: job_event_id,
        relays: relays.clone(),
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_default();

    let result_nevent = Nip19Event {
        event_id: result_event_id,
        relays,
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_default();

    // Encode customer npub
    let customer_npub = PublicKey::parse(&job.customer_id)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_else(|| job.customer_id.clone());

    // Build Solscan URL
    let tx_line = match tx_signature {
        Some(sig) => {
            let cluster_suffix = if network == "mainnet" {
                String::new()
            } else {
                format!("?cluster={}", network)
            };
            format!("🔗 Transaction: https://solscan.io/tx/{}{}\n", sig, cluster_suffix)
        }
        None => String::new(),
    };

    let note = format!(
        "⚡ I just earned {} completing a task on the elisym protocol!\n\n\
         📤 Job request: https://njump.me/{}\n\
         📥 Job result: https://njump.me/{}\n\
         👤 Customer: https://jumble.social/users/{}\n\
         {}\n\
         https://elisym.network\n\n\
         #nostr #ai #aiagents #solana #elisym #dvm",
        sol_display, job_nevent, result_nevent, customer_npub, tx_line
    );

    match agent.client.send_event_builder(EventBuilder::text_note(&note)).await {
        Ok(output) => {
            tracing::info!(
                event_id = %output.val,
                "Published deal note for job {}",
                job.job_id
            );
        }
        Err(e) => {
            tracing::warn!("Failed to publish deal note for job {}: {}", job.job_id, e);
        }
    }
}

/// Publish a kind:1 Nostr note for a completed free job.
/// Includes the request text and response text.
/// Best-effort: logs warning on failure, never propagates errors.
async fn publish_free_note(
    agent: &AgentNode,
    job: &IncomingJob,
    result_event_id: EventId,
) {
    // Encode job event ID as nevent
    let job_event_id = match EventId::parse(&job.job_id) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("Failed to parse job event ID for free note: {}", e);
            return;
        }
    };
    let relays: Vec<String> = vec![
        "wss://relay.damus.io".into(),
        "wss://nos.lol".into(),
    ];
    let job_nevent = Nip19Event {
        event_id: job_event_id,
        relays: relays.clone(),
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_default();

    let result_nevent = Nip19Event {
        event_id: result_event_id,
        relays,
        author: None,
        kind: None,
    }
    .to_bech32()
    .unwrap_or_default();

    // Encode customer npub
    let customer_npub = PublicKey::parse(&job.customer_id)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_else(|| job.customer_id.clone());

    let note = format!(
        "🤖 I just helped with a free task on the elisym protocol!\n\n\
         📤 Job request: https://njump.me/{}\n\
         📥 Job result: https://njump.me/{}\n\
         👤 Customer: https://jumble.social/users/{}\n\n\
         https://elisym.network\n\n\
         #nostr #ai #aiagents #elisym #dvm",
        job_nevent, result_nevent, customer_npub
    );

    match agent.client.send_event_builder(EventBuilder::text_note(&note)).await {
        Ok(output) => {
            tracing::info!(
                event_id = %output.val,
                "Published free note for job {}",
                job.job_id
            );
        }
        Err(e) => {
            tracing::warn!("Failed to publish free note for job {}: {}", job.job_id, e);
        }
    }
}

async fn collect_payment(
    agent: &AgentNode,
    job: &IncomingJob,
    transport: &dyn Transport,
    price: u64,
    payment_timeout_secs: u32,
    event_tx: &mpsc::UnboundedSender<AppEvent>,
) -> Result<(u64, String, Option<String>)> {
    let job_id = job.job_id.clone();

    let solana = agent
        .solana_payments()
        .ok_or_else(|| CliError::Other("solana payments not configured".into()))?;

    let payment_request = match solana.create_payment_request_with_protocol_fee(
        price,
        &format!("elisym job {}", job.job_id),
        payment_timeout_secs,
    ) {
        Ok(req) => req,
        Err(e) => {
            transport
                .send_feedback(
                    job,
                    JobFeedbackStatus::Error(format!("payment error: {}", e)),
                )
                .await?;
            return Err(e.into());
        }
    };

    let fee_amount = calculate_protocol_fee(price).unwrap_or(0);

    let _ = event_tx.send(AppEvent::PaymentRequested {
        job_id: job_id.clone(),
        price,
        fee: fee_amount,
    });

    let chain_str = payment_request.chain.to_string();
    let provider_net = price.saturating_sub(fee_amount);
    let pr_string = payment_request.request.clone();

    // Send PaymentRequired feedback
    transport
        .send_feedback(
            job,
            JobFeedbackStatus::PaymentRequired {
                amount: price,
                payment_request: payment_request.request.clone(),
                chain: chain_str,
            },
        )
        .await?;

    // Poll for payment
    let timeout = Duration::from_secs(payment_timeout_secs as u64);
    let deadline = tokio::time::Instant::now() + timeout;
    let poll_interval = Duration::from_secs(2);

    let mut tx_signature: Option<String> = None;
    let paid = loop {
        match agent.payments.as_ref().unwrap().lookup_payment(&payment_request.request) {
            Ok(status) if status.settled => {
                tx_signature = status.tx_signature;
                break true;
            }
            Ok(_) => {}
            Err(_) => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break false;
        }
        tokio::time::sleep_until(deadline.min(now + poll_interval)).await;
    };

    if !paid {
        let _ = event_tx.send(AppEvent::PaymentTimeout {
            job_id,
        });
        transport
            .send_feedback(
                job,
                JobFeedbackStatus::Error("payment timeout".into()),
            )
            .await?;
        return Err(CliError::Other("payment timeout".into()));
    }

    let _ = event_tx.send(AppEvent::PaymentReceived {
        job_id,
        net_amount: provider_net,
    });

    Ok((provider_net, pr_string, tx_signature))
}

#[cfg(test)]
mod tests {
    use elisym_core::calculate_protocol_fee;

    #[test]
    fn test_fee_math() {
        // 0.01 SOL = 10_000_000 lamports, 3% fee = 300_000 lamports
        let price: u64 = 10_000_000;
        let fee = calculate_protocol_fee(price).unwrap();
        assert_eq!(fee, 300_000);
        assert_eq!(price.saturating_sub(fee), 9_700_000);
    }

    #[test]
    fn test_fee_math_rounding() {
        // Test rounding up with div_ceil
        let price: u64 = 10_000_001;
        let fee = calculate_protocol_fee(price).unwrap();
        // 10_000_001 * 300 = 3_000_000_300, div_ceil(10_000) = 300_001
        assert_eq!(fee, 300_001);
    }
}
