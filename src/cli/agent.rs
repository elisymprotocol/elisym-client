use std::sync::Arc;
use std::time::Duration;

use elisym_core::marketplace::JobRequest;
use elisym_core::messaging::PrivateMessage;
use elisym_core::types::JobStatus;
use elisym_core::{
    AgentNode, AgentNodeBuilder, FeeConfig, PaymentChain,
    SolanaPaymentConfig, SolanaPaymentProvider, SolanaNetwork, SolanaToken,
    USDC_MINT_DEVNET, USDC_MINT_MAINNET,
};

/// Protocol fee in basis points (300 = 3%). Integer-only arithmetic.
const PROTOCOL_FEE_BPS: u64 = 300;
/// Solana address of the protocol treasury.
const PROTOCOL_TREASURY: &str = "GY7vnWMkKpftU4nQ16C2ATkj1JwrQpHhknkaBUn67VTy";
/// Solana rent-exempt minimum for a 0-data account (lamports).
const RENT_EXEMPT_MINIMUM: u64 = 890_880;

/// Validate that the provider's net amount (price minus protocol fee) is above
/// Solana's rent-exempt minimum. Returns an error message if invalid, None if OK.
pub fn validate_job_price(lamports: u64) -> Option<String> {
    if lamports == 0 {
        return None; // free mode
    }
    let fee = (lamports * PROTOCOL_FEE_BPS).div_ceil(10_000);
    let provider_net = lamports.saturating_sub(fee);
    if provider_net < RENT_EXEMPT_MINIMUM {
        Some(format!(
            "Price too low: after {:.2}% protocol fee the provider receives {} lamports, \
             which is below Solana rent-exempt minimum ({} lamports).",
            PROTOCOL_FEE_BPS as f64 / 100.0,
            provider_net,
            RENT_EXEMPT_MINIMUM,
        ))
    } else {
        None
    }
}
use nostr_sdk::Timestamp;
use solana_sdk::pubkey::Pubkey;
use tokio::task::JoinSet;
use tracing::{error, info, trace, warn};

use super::protocol::HeartbeatMessage;

use super::config::AgentConfig;
use super::error::{CliError, Result};
use super::llm::LlmClient;

/// Build a SolanaPaymentProvider directly from config (no relay connections needed).
/// Use this for wallet-only operations: send, airdrop, balance checks.
pub fn build_solana_provider(config: &AgentConfig) -> Result<SolanaPaymentProvider> {
    let network = match config.payment.network.as_str() {
        "mainnet" => SolanaNetwork::Mainnet,
        "testnet" => SolanaNetwork::Testnet,
        "devnet" => SolanaNetwork::Devnet,
        other => SolanaNetwork::Custom(other.to_string()),
    };

    let token = match config.payment.token.as_str() {
        "usdc" => {
            let mint_str = match &config.payment.network as &str {
                "mainnet" => USDC_MINT_MAINNET,
                _ => USDC_MINT_DEVNET,
            };
            let mint: Pubkey = mint_str.parse().map_err(|e| {
                CliError::Other(format!("invalid USDC mint address: {:?}", e))
            })?;
            SolanaToken::Spl { mint, decimals: 6 }
        }
        _ => SolanaToken::Sol,
    };

    let solana_config = SolanaPaymentConfig {
        network,
        rpc_url: config.payment.rpc_url.clone(),
        token,
    };

    let mut provider = SolanaPaymentProvider::from_secret_key(
        solana_config,
        &config.payment.solana_secret_key,
    )?;

    provider.set_fee_config(FeeConfig {
        app_fee_bps: PROTOCOL_FEE_BPS,
        app_fee_address: PROTOCOL_TREASURY.to_string(),
        app_fee_chain: PaymentChain::Solana,
    });

    Ok(provider)
}

/// Build an AgentNode from a persisted config (connects to relays).
/// Use this only when relay connectivity is needed: start, job processing.
pub async fn build_agent(config: &AgentConfig) -> Result<AgentNode> {
    let provider = build_solana_provider(config)?;

    let mut agent = AgentNodeBuilder::new(&config.name, &config.description)
        .capabilities(config.capabilities.clone())
        .relays(config.relays.clone())
        .supported_job_kinds(vec![5100])
        .secret_key(&config.secret_key)
        .solana_payment_provider(provider)
        .build()
        .await?;

    // Set payment address so other agents / dashboard can query on-chain balance
    let solana_address = agent
        .solana_payments()
        .map(|s| s.address());
    if let Some(addr) = solana_address {
        agent.capability_card.set_payment_address(addr);
    }

    // Publish capability card with pricing metadata
    agent.capability_card.metadata = Some(serde_json::json!({
        "job_price": config.payment.job_price,
        "token": config.payment.token,
        "chain": config.payment.chain,
        "network": config.payment.network,
        "protocol_fee_bps": PROTOCOL_FEE_BPS,
    }));
    agent
        .discovery
        .publish_capability(&agent.capability_card, &[5100])
        .await?;

    Ok(agent)
}

/// Build a system prompt from the agent config, including per-capability prompts.
fn build_system_prompt(config: &AgentConfig) -> String {
    let mut prompt = format!(
        "You are {}, an AI agent on the elisym protocol.\n\
         Description: {}\n\n",
        config.name, config.description
    );

    // Append per-capability instructions (only active capabilities)
    for cap in &config.capabilities {
        if let Some(cap_prompt) = config.capability_prompts.get(cap) {
            prompt.push_str(&format!("[{}]: {}\n\n", cap, cap_prompt));
        }
    }

    prompt.push_str(
        "IMPORTANT: You are a job-processing agent, NOT an interactive chatbot.\n\
         You receive a single request and must return a complete, ready-to-use result.\n\
         Do NOT ask follow-up questions, offer menus, or suggest options.\n\
         Do NOT use emojis or conversational filler.\n\
         Just do what is asked and return the result directly.",
    );
    prompt
}

/// Run the agent's job processing loop with payment-first flow and parallel execution.
pub async fn run_agent(agent: AgentNode, config: &AgentConfig, free_mode: bool) -> Result<()> {
    let llm_section = config
        .llm
        .as_ref()
        .ok_or_else(|| CliError::Llm("no LLM configured — run `elisym-cli init` to set up".into()))?;
    let llm = Arc::new(LlmClient::new(llm_section)?);

    let agent = Arc::new(agent);
    let system_prompt = build_system_prompt(config);
    let job_price = config.payment.job_price;
    let payment_timeout_secs = config.payment.payment_timeout_secs;

    info!(
        agent = %config.name,
        npub = %agent.identity.npub(),
        free_mode,
        "agent is live — listening for jobs on kind {}",
        5100
    );

    let mut jobs_rx = agent
        .marketplace
        .subscribe_to_job_requests(&[100])
        .await?;

    let mut messages_rx = agent.messaging.subscribe_to_messages().await?;
    let started_at = Timestamp::now();

    let mut tasks: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            Some(msg) = messages_rx.recv() => {
                // Skip messages from before this provider started
                if msg.timestamp < started_at {
                    trace!(
                        sender = %msg.sender,
                        msg_ts = %msg.timestamp,
                        "ignoring old message (before startup)"
                    );
                    continue;
                }
                let agent = Arc::clone(&agent);
                tokio::spawn(async move {
                    handle_ping(&agent, msg).await;
                });
            }
            Some(job) = jobs_rx.recv() => {
                info!(
                    job_id = %job.event_id,
                    customer = %job.customer,
                    input_len = job.input_data.len(),
                    "received job request"
                );

                let agent = Arc::clone(&agent);
                let llm = Arc::clone(&llm);
                let system_prompt = system_prompt.clone();

                tasks.spawn(async move {
                    let result = if free_mode {
                        process_job_free(
                            &agent,
                            &llm,
                            job,
                            &system_prompt,
                        ).await
                    } else {
                        process_job(
                            &agent,
                            &llm,
                            job,
                            &system_prompt,
                            job_price,
                            payment_timeout_secs,
                        ).await
                    };
                    if let Err(e) = result {
                        error!("job processing failed: {}", e);
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received — shutting down gracefully");
                break;
            }
            Some(result) = tasks.join_next() => {
                if let Err(e) = result {
                    error!("job task panicked: {}", e);
                }
            }
        }
    }

    // Drain remaining tasks with timeout
    if !tasks.is_empty() {
        info!("waiting up to 30s for {} in-flight jobs to finish", tasks.len());
        let deadline = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                Some(result) = tasks.join_next() => {
                    if let Err(e) = result {
                        error!("job task panicked during shutdown: {}", e);
                    }
                }
                _ = &mut deadline => {
                    warn!("shutdown timeout — aborting {} remaining tasks", tasks.len());
                    tasks.abort_all();
                    break;
                }
            }
            if tasks.is_empty() {
                break;
            }
        }
    }

    // Drop receivers first, then agent on blocking thread to avoid async drop issues
    drop(jobs_rx);
    drop(messages_rx);
    let agent = Arc::try_unwrap(agent).ok();
    if let Some(agent) = agent {
        tokio::task::spawn_blocking(move || drop(agent)).await.ok();
    }

    info!("agent shut down cleanly");
    Ok(())
}

/// Process a single job: payment-first, then LLM, then deliver.
async fn process_job(
    agent: &AgentNode,
    llm: &LlmClient,
    job: JobRequest,
    system_prompt: &str,
    price: u64,
    payment_timeout_secs: u32,
) -> Result<()> {
    let job_id = job.event_id;

    // 1. Generate payment request via PaymentProvider trait
    let payments = agent
        .payments
        .as_ref()
        .ok_or_else(|| CliError::Other("payments not configured".into()))?;

    let payment_request = match payments.create_payment_request(
        price,
        &format!("elisym job {}", job_id),
        payment_timeout_secs,
    ) {
        Ok(req) => req,
        Err(e) => {
            error!(job_id = %job_id, "payment request creation failed: {}", e);
            agent
                .marketplace
                .submit_job_feedback(
                    &job.raw_event,
                    JobStatus::Error,
                    Some(&format!("payment error: {}", e)),
                    None,
                    None,
                    None,
                )
                .await?;
            return Err(e.into());
        }
    };

    let chain_str = payment_request.chain.to_string();
    let fee_amount = (price * PROTOCOL_FEE_BPS).div_ceil(10_000);
    let provider_net = price.saturating_sub(fee_amount);
    info!(
        job_id = %job_id,
        total = price,
        provider_net,
        protocol_fee = fee_amount,
        chain = %chain_str,
        "requesting payment ({:.2}% protocol fee)",
        PROTOCOL_FEE_BPS as f64 / 100.0,
    );

    // 2. Send PaymentRequired feedback with payment request
    agent
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::PaymentRequired,
            None,
            Some(price),
            Some(&payment_request.request),
            Some(&chain_str),
        )
        .await?;

    // 3. Poll for payment
    let timeout = Duration::from_secs(payment_timeout_secs as u64);
    let deadline = tokio::time::Instant::now() + timeout;
    let poll_interval = Duration::from_secs(2);

    let paid = loop {
        match payments.lookup_payment(&payment_request.request) {
            Ok(status) if status.settled => break true,
            Ok(_) => {}
            Err(e) => {
                warn!("payment lookup error: {}", e);
            }
        }
        if tokio::time::Instant::now() + poll_interval > deadline {
            break false;
        }
        tokio::time::sleep(poll_interval).await;
    };

    if !paid {
        warn!(job_id = %job_id, "payment timeout");
        agent
            .marketplace
            .submit_job_feedback(
                &job.raw_event,
                JobStatus::Error,
                Some("payment timeout"),
                None,
                None,
                None,
            )
            .await?;
        return Ok(());
    }

    info!(job_id = %job_id, "payment received — processing");

    // 4. Send Processing feedback
    agent
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::Processing,
            None,
            None,
            None,
            None,
        )
        .await?;

    // 5. Call LLM
    let result = match llm.complete(system_prompt, &job.input_data).await {
        Ok(text) => text,
        Err(e) => {
            error!(job_id = %job_id, "LLM error: {}", e);
            agent
                .marketplace
                .submit_job_feedback(
                    &job.raw_event,
                    JobStatus::Error,
                    Some(&format!("LLM error: {}", e)),
                    None,
                    None,
                    None,
                )
                .await?;
            return Err(e);
        }
    };

    // 6. Deliver result (3 retries with backoff)
    let mut last_err = None;
    for attempt in 0..3 {
        match agent
            .marketplace
            .submit_job_result(&job.raw_event, &result, Some(price))
            .await
        {
            Ok(result_id) => {
                info!(
                    job_id = %job_id,
                    result_id = %result_id,
                    "job completed — result delivered"
                );
                // Show updated balance
                if let Some(solana) = agent.solana_payments() {
                    if let Ok(balance) = solana.balance() {
                        info!(
                            balance_lamports = balance,
                            balance_sol = format!("{:.4}", balance as f64 / 1_000_000_000.0),
                            "wallet balance after payment"
                        );
                    }
                }
                return Ok(());
            }
            Err(e) => {
                warn!(
                    job_id = %job_id,
                    attempt = attempt + 1,
                    "failed to deliver result: {}",
                    e
                );
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
        }
    }

    let err = last_err.unwrap();
    agent
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::Error,
            Some(&format!("delivery failed: {}", err)),
            None,
            None,
            None,
        )
        .await?;

    Err(err.into())
}

/// Process a single job in free mode: skip payment, go straight to LLM.
async fn process_job_free(
    agent: &AgentNode,
    llm: &LlmClient,
    job: JobRequest,
    system_prompt: &str,
) -> Result<()> {
    let job_id = job.event_id;

    info!(job_id = %job_id, "free mode — skipping payment, processing directly");

    // 1. Send Processing feedback
    agent
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::Processing,
            None,
            None,
            None,
            None,
        )
        .await?;

    // 2. Call LLM
    let result = match llm.complete(system_prompt, &job.input_data).await {
        Ok(text) => text,
        Err(e) => {
            error!(job_id = %job_id, "LLM error: {}", e);
            agent
                .marketplace
                .submit_job_feedback(
                    &job.raw_event,
                    JobStatus::Error,
                    Some(&format!("LLM error: {}", e)),
                    None,
                    None,
                    None,
                )
                .await?;
            return Err(e);
        }
    };

    // 3. Deliver result (3 retries with backoff)
    let mut last_err = None;
    for attempt in 0..3 {
        match agent
            .marketplace
            .submit_job_result(&job.raw_event, &result, None)
            .await
        {
            Ok(result_id) => {
                info!(
                    job_id = %job_id,
                    result_id = %result_id,
                    "job completed (free) — result delivered"
                );
                return Ok(());
            }
            Err(e) => {
                warn!(
                    job_id = %job_id,
                    attempt = attempt + 1,
                    "failed to deliver result: {}",
                    e
                );
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
        }
    }

    let err = last_err.unwrap();
    agent
        .marketplace
        .submit_job_feedback(
            &job.raw_event,
            JobStatus::Error,
            Some(&format!("delivery failed: {}", err)),
            None,
            None,
            None,
        )
        .await?;

    Err(err.into())
}

/// Handle an incoming private message: if it's a ping, respond with pong.
async fn handle_ping(agent: &AgentNode, msg: PrivateMessage) {
    let heartbeat: HeartbeatMessage = match serde_json::from_str(&msg.content) {
        Ok(hb) => hb,
        Err(_) => {
            trace!(sender = %msg.sender, "ignoring non-heartbeat message");
            return;
        }
    };

    if heartbeat.is_ping() {
        info!(sender = %msg.sender, nonce = %heartbeat.nonce, "received ping — sending pong");
        let pong = HeartbeatMessage::pong(heartbeat.nonce);
        if let Err(e) = agent
            .messaging
            .send_structured_message(&msg.sender, &pong)
            .await
        {
            warn!(sender = %msg.sender, "failed to send pong: {}", e);
        }
    } else {
        trace!(msg_type = %heartbeat.msg_type, sender = %msg.sender, "ignoring heartbeat message");
    }
}
