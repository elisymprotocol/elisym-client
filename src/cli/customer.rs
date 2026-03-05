use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use console::style;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    EnableBracketedPaste, DisableBracketedPaste,
};
use crossterm::style::Print;
use crossterm::terminal::{enable_raw_mode, disable_raw_mode};
use dialoguer::Select;
use elisym_core::{AgentFilter, AgentNode, DiscoveredAgent, types::JobStatus};
use nostr_sdk::PublicKey;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::config::AgentConfig;
use super::error::{CliError, Result};
use super::llm::LlmClient;
use super::protocol::{self, HeartbeatMessage};

enum RequestOutcome {
    Done,
    Continue,
    Interrupted,
    Err(CliError),
}

enum InputResult {
    Text(String),
    Eof,
    Interrupted,
}

/// Read multi-line input using crossterm raw mode.
/// Enter → submit, Ctrl+J → newline (works in all terminals).
/// Pasted text is captured as a single block (bracketed paste).
/// Fallback: if bracketed paste is unsupported, Enter during a fast stream
/// of characters (paste) is treated as a newline, not submit.
fn read_multiline_input() -> io::Result<InputResult> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnableBracketedPaste)?;

    let result = (|| -> io::Result<InputResult> {
        let mut buffer = String::new();

        loop {
            match event::read()? {
                Event::Key(KeyEvent {
                    code, modifiers, kind: KeyEventKind::Press, ..
                }) => match code {
                    // Ctrl+J → insert newline (universally works in raw mode)
                    KeyCode::Char('j') if modifiers.contains(KeyModifiers::CONTROL) => {
                        buffer.push('\n');
                        crossterm::execute!(stdout, Print("\r\n"))?;
                    }
                    KeyCode::Enter => {
                        // Shift+Enter / Alt+Enter → newline (works in some terminals)
                        if modifiers.contains(KeyModifiers::SHIFT)
                            || modifiers.contains(KeyModifiers::ALT)
                        {
                            buffer.push('\n');
                            crossterm::execute!(stdout, Print("\r\n"))?;
                        } else {
                            // Check if more keys arrive quickly (paste without
                            // bracketed paste support). If another key comes within
                            // 50ms, this Enter is part of a paste → treat as newline.
                            if event::poll(std::time::Duration::from_millis(50))? {
                                buffer.push('\n');
                                crossterm::execute!(stdout, Print("\r\n"))?;
                            } else {
                                // No more input → real submit
                                crossterm::execute!(stdout, Print("\r\n"))?;
                                return Ok(InputResult::Text(buffer));
                            }
                        }
                    }
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        crossterm::execute!(stdout, Print("\r\n"))?;
                        return Ok(InputResult::Interrupted);
                    }
                    KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                        if buffer.is_empty() {
                            return Ok(InputResult::Eof);
                        }
                    }
                    KeyCode::Char(c) => {
                        buffer.push(c);
                        crossterm::execute!(stdout, Print(c.to_string()))?;
                    }
                    KeyCode::Backspace => {
                        if let Some(c) = buffer.pop() {
                            if c == '\n' {
                                crossterm::execute!(
                                    stdout,
                                    crossterm::cursor::MoveUp(1),
                                    crossterm::cursor::MoveToColumn(
                                        buffer.lines().last().map_or(0, |l| l.len() as u16)
                                            + 3 // ">> " prompt width
                                    ),
                                )?;
                            } else {
                                crossterm::execute!(stdout, Print("\x08 \x08"))?;
                            }
                        }
                    }
                    _ => {}
                },
                Event::Paste(text) => {
                    // Normalize line endings: \r\n → \n, lone \r → \n
                    let clean = text.replace("\r\n", "\n").replace('\r', "\n");
                    buffer.push_str(&clean);
                    let display = clean.replace('\n', "\r\n");
                    crossterm::execute!(stdout, Print(&display))?;
                }
                _ => {}
            }
        }
    })();

    // Always restore terminal state
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    let _ = disable_raw_mode();

    result
}

/// Run the interactive customer REPL.
pub async fn run_customer_repl(mut agent: AgentNode, config: &AgentConfig) -> Result<()> {
    // Resolve LLM: prefer customer_llm, fall back to llm
    let llm_section = config
        .customer_llm
        .as_ref()
        .or(config.llm.as_ref())
        .ok_or_else(|| CliError::Llm("no LLM configured — run `elisym-cli init` to set up".into()))?;
    let llm = LlmClient::new(llm_section)?;

    println!(
        "\n  {} Customer mode — type a request, or {} to quit\n",
        style("*").green(),
        style("exit").dim(),
    );

    // Show initial balance
    if let Some(solana) = agent.solana_payments() {
        if let Ok(balance) = solana.balance() {
            println!(
                "  {} Balance: {} SOL",
                style("$").yellow(),
                style(format!("{:.4}", balance as f64 / 1_000_000_000.0)).green(),
            );
        }
    }

    // Start background balance monitor
    let balance_stop = Arc::new(AtomicBool::new(false));
    let balance_handle = if let Some(solana) = agent.solana_payments() {
        let initial = solana.balance().unwrap_or(0);
        let last_balance = Arc::new(AtomicU64::new(initial));
        let stop = Arc::clone(&balance_stop);
        let rpc_url = config.payment.rpc_url.clone().unwrap_or_else(|| {
            match config.payment.network.as_str() {
                "devnet" => "https://api.devnet.solana.com",
                "testnet" => "https://api.testnet.solana.com",
                _ => "https://api.mainnet-beta.solana.com",
            }.to_string()
        });
        let address = solana.address();
        Some(tokio::spawn(balance_monitor(rpc_url, address, last_balance, stop)))
    } else {
        None
    };

    // Subscribe to feedback once (reused across jobs)
    let mut feedback_rx = agent.marketplace.subscribe_to_feedback().await?;

    println!(
        "  {} Ctrl+J for new line, Enter to send, paste supported",
        style("~").dim(),
    );
    println!("  {}\n", style("Describe what you need — your agent will find the best provider and handle payment.").dim());

    loop {
        // Prompt
        print!("{} ", style(">>").cyan().bold());
        io::stdout().flush()?;

        // Read input with crossterm raw mode (supports multi-line + paste)
        let input = match tokio::task::spawn_blocking(read_multiline_input).await {
            Ok(Ok(InputResult::Text(s))) => s.trim().to_string(),
            Ok(Ok(InputResult::Interrupted)) => {
                println!("  Goodbye!");
                break;
            }
            Ok(Ok(InputResult::Eof)) => break,
            Ok(Err(e)) => return Err(e.into()),
            Err(e) => return Err(CliError::Other(format!("input task: {}", e))),
        };

        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            println!("  Goodbye!");
            break;
        }

        // Run all steps in a cancellable block — Ctrl+C at any point breaks out
        let step_result = handle_request(&agent, &llm, &input, &mut feedback_rx, &config.payment.chain, &config.payment.network).await;

        match step_result {
            RequestOutcome::Done | RequestOutcome::Continue => {}
            RequestOutcome::Interrupted => {
                println!("\n  Interrupted.");
                break;
            }
            RequestOutcome::Err(e) => return Err(e),
        }
    }

    info!("customer REPL exited — shutting down agent");

    // Stop balance monitor
    balance_stop.store(true, Ordering::Relaxed);
    if let Some(handle) = balance_handle {
        let _ = handle.await;
    }

    // Disconnect from relays first (stops background tasks), then drop.
    drop(feedback_rx);
    agent.shutdown().await;
    tokio::task::spawn_blocking(move || drop(agent)).await.ok();

    info!("agent shut down cleanly");
    Ok(())
}

// ── request handler (cancellable with Ctrl+C) ──────────────────────

use elisym_core::marketplace::JobFeedback;

/// Run steps 1–5 wrapped in a Ctrl+C guard. Every await point can be interrupted.
async fn handle_request(
    agent: &AgentNode,
    llm: &LlmClient,
    input: &str,
    feedback_rx: &mut mpsc::Receiver<JobFeedback>,
    chain: &str,
    network: &str,
) -> RequestOutcome {
    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => RequestOutcome::Interrupted,
        outcome = handle_request_inner(agent, llm, input, feedback_rx, chain, network) => outcome,
    }
}

/// The actual request logic — all async, cancelable via the outer select.
async fn handle_request_inner(
    agent: &AgentNode,
    llm: &LlmClient,
    input: &str,
    feedback_rx: &mut mpsc::Receiver<JobFeedback>,
    chain: &str,
    network: &str,
) -> RequestOutcome {
    // ── Step 1: Intent extraction ────────────────────────────────
    println!(
        "\n  {} Analyzing your request...",
        style("[1]").cyan().bold(),
    );

    let keywords = match extract_intent(llm, input).await {
        Ok(kw) if !kw.is_empty() => {
            info!(keywords = ?kw, "extracted intent keywords");
            println!("      Keywords: {}", style(kw.join(", ")).dim());
            kw
        }
        Ok(_) | Err(_) => {
            info!("intent extraction returned empty — using unfiltered search");
            vec![]
        }
    };

    // ── Step 2: Discovery ────────────────────────────────────────
    println!("  {} Searching for providers...", style("[2]").cyan().bold());

    let filter = if keywords.is_empty() {
        AgentFilter::default()
    } else {
        AgentFilter {
            capabilities: keywords.clone(),
            ..Default::default()
        }
    };

    let mut providers = match agent.discovery.search_agents(&filter).await {
        Ok(p) => p,
        Err(e) => return RequestOutcome::Err(e.into()),
    };

    // Filter by chain + network: only show providers on the same payment chain and network
    let filter_chain_network = |p: &DiscoveredAgent| -> bool {
        let meta = match p.card.metadata.as_ref() {
            Some(m) => m,
            None => return false,
        };
        let agent_chain = meta["chain"].as_str().unwrap_or("solana");
        let agent_network = meta["network"].as_str().unwrap_or("devnet");
        agent_chain.eq_ignore_ascii_case(chain) && agent_network.eq_ignore_ascii_case(network)
    };
    providers.retain(filter_chain_network);

    if providers.is_empty() && !keywords.is_empty() {
        info!("filtered search returned 0 — falling back to unfiltered (same chain/network)");
        providers = match agent.discovery.search_agents(&AgentFilter::default()).await {
            Ok(p) => p,
            Err(e) => return RequestOutcome::Err(e.into()),
        };
        providers.retain(filter_chain_network);
    }

    if providers.is_empty() {
        println!(
            "      {} No providers found on the network. Try again later.\n",
            style("!").yellow(),
        );
        return RequestOutcome::Continue;
    }

    info!(count = providers.len(), "discovered providers");
    println!("      Found {} providers", providers.len());

    // ── Step 3: LLM scoring ──────────────────────────────────────
    println!(
        "  {} Matching {} providers to your request...",
        style("[3]").cyan().bold(),
        providers.len(),
    );

    let scored = match score_providers(llm, input, &providers).await {
        Ok(s) if s.is_empty() => {
            println!(
                "      {} No providers matched your request. Try a different query.\n",
                style("!").yellow(),
            );
            return RequestOutcome::Continue;
        }
        Ok(s) => s,
        Err(e) => {
            println!("      {} LLM matching failed: {}\n", style("!").red(), e);
            return RequestOutcome::Continue;
        }
    };

    let candidates: Vec<&ScoredProvider> = scored.iter().take(10).collect();
    let candidate_pubkeys: Vec<PublicKey> = candidates
        .iter()
        .map(|sp| providers[sp.index].pubkey)
        .collect();

    info!(candidates = candidates.len(), "top candidates selected for heartbeat check");

    // ── Step 4: Heartbeat check ─────────────────────────────────
    println!(
        "  {} Checking {} providers are online...",
        style("[4]").cyan().bold(),
        candidates.len(),
    );

    let online_pubkeys =
        ping_providers(agent, &candidate_pubkeys, Duration::from_secs(15)).await;

    info!(
        sent = candidate_pubkeys.len(),
        online = online_pubkeys.len(),
        "heartbeat check complete"
    );

    let online_scored: Vec<&ScoredProvider> = candidates
        .iter()
        .filter(|sp| online_pubkeys.contains(&providers[sp.index].pubkey))
        .take(5)
        .copied()
        .collect();

    if online_scored.is_empty() {
        println!(
            "      {} No providers are currently online. Try again later.\n",
            style("!").yellow(),
        );
        return RequestOutcome::Continue;
    }

    // ── Step 5: Display and select ──────────────────────────────
    println!("\n  {} Online providers:\n", style("[5]").cyan().bold());

    let mut select_items: Vec<String> = Vec::new();
    for (i, sp) in online_scored.iter().enumerate() {
        let p = &providers[sp.index];
        let (price_str, token) = extract_price(p);
        println!(
            "  {}. {}",
            style(i + 1).bold(),
            style(&p.card.name).cyan().bold(),
        );
        println!("     \"{}\"", p.card.description);
        println!("     Capabilities: {}", p.card.capabilities.join(", "));
        let fee_note = p.card.metadata.as_ref()
            .and_then(|m| m["protocol_fee_bps"].as_u64())
            .map(|bps| format!(" (incl. {:.2}% protocol fee)", bps as f64 / 100.0))
            .unwrap_or_default();
        println!(
            "     Price: {} {}{}",
            format_price(price_str, &token),
            token.to_uppercase(),
            fee_note,
        );
        println!(
            "     Relevance: {}/100 — \"{}\"\n",
            style(sp.score).bold(),
            sp.reason,
        );

        select_items.push(format!(
            "{} — {} {} — score: {}",
            p.card.name,
            format_price(price_str, &token),
            token.to_uppercase(),
            sp.score,
        ));
    }
    select_items.push("Cancel".into());

    let selection = match Select::new()
        .with_prompt("Select provider")
        .items(&select_items)
        .default(0)
        .interact()
    {
        Ok(s) => s,
        Err(dialoguer::Error::IO(ref e)) if e.kind() == io::ErrorKind::Interrupted => {
            return RequestOutcome::Interrupted;
        }
        Err(e) => return RequestOutcome::Err(e.into()),
    };

    if selection == online_scored.len() {
        println!("  Cancelled.\n");
        return RequestOutcome::Continue;
    }

    let chosen = online_scored[selection];
    let provider = &providers[chosen.index];
    info!(
        provider = %provider.card.name,
        pubkey = %provider.pubkey,
        "submitting job request"
    );

    let (price, _token) = extract_price(provider);

    // Relay tag value limit is typically 1024 bytes (strfry default).
    // Truncate to 950 bytes and strip \r to stay safely under the limit.
    let job_input = truncate_to_bytes(&input.replace('\r', ""), 950);
    if job_input.len() < input.len() {
        println!(
            "      {} Input truncated to ~950 bytes for relay tag limit",
            style("~").dim(),
        );
    }

    info!(
        input_bytes = job_input.len(),
        original_bytes = input.len(),
        "submitting job request to relay"
    );

    let job_event_id = match agent
        .marketplace
        .submit_job_request(100, &job_input, "text", None, Some(price), Some(&provider.pubkey), vec![])
        .await
    {
        Ok(id) => id,
        Err(e) => {
            println!(
                "  {} Failed to submit job: {}",
                style("!").red(),
                e,
            );
            println!(
                "      Input was {} bytes (original {} bytes).",
                job_input.len(),
                input.len(),
            );
            println!(
                "      Relays may reject large events or unsupported kinds (5100).\n",
            );
            return RequestOutcome::Continue;
        }
    };

    println!(
        "  {} Job submitted ({})",
        style("*").green(),
        style(job_event_id.to_string().chars().take(12).collect::<String>()).dim(),
    );

    let mut results_rx = match agent
        .marketplace
        .subscribe_to_results(&[100], &[provider.pubkey])
        .await
    {
        Ok(rx) => rx,
        Err(e) => return RequestOutcome::Err(e.into()),
    };

    println!("  {} Waiting for provider response...", style("~").dim());

    let timeout = Duration::from_secs(300);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                return RequestOutcome::Interrupted;
            }
            Some(feedback) = feedback_rx.recv() => {
                if feedback.request_id != job_event_id {
                    continue;
                }
                match feedback.parsed_status() {
                    Some(JobStatus::PaymentRequired) => {
                        if let Some(ref pay_req) = feedback.payment_request {
                            // Parse fee info from request for display
                            if let Ok(req_data) = serde_json::from_str::<Value>(pay_req) {
                                let total = req_data["amount"].as_u64().unwrap_or(0);
                                if let (Some(fee_amt), Some(_fee_addr)) = (
                                    req_data["fee_amount"].as_u64(),
                                    req_data["fee_address"].as_str(),
                                ) {
                                    let provider_net = total.saturating_sub(fee_amt);
                                    let token = req_data["mint"].as_str().map_or("SOL", |_| "USDC");
                                    println!(
                                        "  {} Payment: {} total → {} provider + {} protocol fee ({})",
                                        style("$").yellow(),
                                        style(total).bold(),
                                        provider_net,
                                        fee_amt,
                                        token,
                                    );
                                } else {
                                    println!("  {} Payment required — paying...", style("$").yellow());
                                }
                            } else {
                                println!("  {} Payment required — paying...", style("$").yellow());
                            }

                            let payments = match agent.payments.as_ref() {
                                Some(p) => p,
                                None => {
                                    println!("  {} Payments not configured\n", style("!").red());
                                    break;
                                }
                            };
                            match payments.pay(pay_req) {
                                Ok(result) => {
                                    info!(payment_id = %result.payment_id, "payment sent");
                                    println!(
                                        "  {} Payment sent ({})",
                                        style("*").green(),
                                        style(&result.payment_id.chars().take(16).collect::<String>()).dim(),
                                    );
                                }
                                Err(e) => {
                                    println!("  {} Payment failed: {}\n", style("!").red(), e);
                                    break;
                                }
                            }
                        }
                    }
                    Some(JobStatus::Processing) => {
                        println!("  {} Provider is processing...", style("~").dim());
                    }
                    Some(JobStatus::Error) => {
                        let msg = feedback.extra_info.as_deref().unwrap_or("unknown error");
                        println!("  {} Provider error: {}\n", style("!").red(), msg);
                        break;
                    }
                    _ => {
                        if let Some(ref info) = feedback.extra_info {
                            info!(status = %feedback.status, info = %info, "feedback");
                        }
                    }
                }
            }
            Some(result) = results_rx.recv() => {
                if result.request_id != job_event_id {
                    continue;
                }
                println!(
                    "\n  {} {}\n",
                    style("Result:").green().bold(),
                    style("─".repeat(40)).dim(),
                );
                println!("{}", result.content);
                println!("\n  {}", style("─".repeat(47)).dim());

                if let Some(solana) = agent.solana_payments() {
                    if let Ok(balance) = solana.balance() {
                        println!(
                            "  Balance: {} SOL\n",
                            style(format!("{:.4}", balance as f64 / 1_000_000_000.0)).green(),
                        );
                    }
                }
                return RequestOutcome::Done;
            }
            _ = tokio::time::sleep_until(deadline) => {
                println!("  {} Timed out waiting for result.\n", style("!").red());
                break;
            }
        }
    }

    RequestOutcome::Done
}

// ── helpers ──────────────────────────────────────────────────────────

/// Truncate input to a reasonable size for LLM calls.
/// Keeps the first `max_chars` characters so the LLM sees enough context
/// without hitting token limits or causing slow responses.
fn truncate_for_llm(input: &str, max_chars: usize) -> &str {
    if input.len() <= max_chars {
        input
    } else {
        // Find a char boundary near max_chars
        let end = input
            .char_indices()
            .take_while(|(i, _)| *i <= max_chars)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(max_chars);
        &input[..end]
    }
}

/// Truncate a string to fit within `max_bytes` UTF-8 bytes.
/// Cuts at a char boundary so the result is always valid UTF-8.
fn truncate_to_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    // Walk char boundaries to find the last one that fits
    let truncated = &input[..input.floor_char_boundary(max_bytes)];
    truncated.to_string()
}

// ── Intent extraction ────────────────────────────────────────────────

/// Use LLM to extract capability keywords from a user request.
async fn extract_intent(llm: &LlmClient, input: &str) -> Result<Vec<String>> {
    let system_prompt = "\
You classify user requests for the elisym AI agent marketplace.\n\
Given a user request, return a JSON object with:\n\
- \"keywords\": array of 1-3 short capability keywords that best describe \
what kind of AI agent could handle this request. Use lowercase, \
hyphenated terms (e.g. \"code-generation\", \"image-editing\", \
\"legal-advice\", \"math-tutoring\"). Any domain is valid.\n\
- \"intent\": a short one-sentence description of the intent\n\n\
Return ONLY the JSON object, no other text.";

    // Truncate to avoid hitting token limits with very large inputs
    let trimmed = truncate_for_llm(input, 2000);
    let response = llm.complete(system_prompt, trimmed).await?;

    let json_str = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let parsed: Value = serde_json::from_str(json_str).map_err(|e| {
        warn!(response = %response, "failed to parse intent extraction response");
        CliError::Llm(format!("failed to parse intent response: {}", e))
    })?;

    let keywords: Vec<String> = parsed["keywords"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if let Some(intent) = parsed["intent"].as_str() {
        info!(intent, "extracted intent");
    }

    Ok(keywords)
}

// ── Heartbeat ping ──────────────────────────────────────────────────

/// Ping a set of providers in parallel and return pubkeys of those that responded.
///
/// Uses a single overall deadline for both sending pings and collecting pongs.
/// Sends are fire-and-forget (not awaited) so a slow relay can't block the whole operation.
async fn ping_providers(
    agent: &AgentNode,
    providers: &[PublicKey],
    timeout: Duration,
) -> Vec<PublicKey> {
    if providers.is_empty() {
        return vec![];
    }

    // Subscribe to incoming messages for pong collection BEFORE sending pings
    let mut messages_rx = match agent.messaging.subscribe_to_messages().await {
        Ok(rx) => rx,
        Err(e) => {
            warn!("failed to subscribe for pong messages: {}", e);
            return vec![];
        }
    };

    // Single deadline covers both sending and receiving
    let deadline = tokio::time::Instant::now() + timeout;

    // Generate nonces and fire-and-forget pings (don't await sends)
    let mut nonce_map: HashMap<String, PublicKey> = HashMap::new();

    for &pubkey in providers {
        let nonce = protocol::random_nonce();
        nonce_map.insert(nonce.clone(), pubkey);
        let ping = HeartbeatMessage::ping(nonce);
        let messaging = agent.messaging.clone();
        // Fire-and-forget: don't hold a JoinHandle, let it complete on its own
        tokio::spawn(async move {
            // Per-send timeout so one slow relay can't leak tasks forever
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                if let Err(e) = messaging.send_structured_message(&pubkey, &ping).await {
                    warn!(provider = %pubkey, "failed to send ping: {}", e);
                }
            })
            .await;
        });
    }

    info!(count = providers.len(), "pings dispatched — collecting pongs");

    // Collect pongs until deadline
    let mut online: Vec<PublicKey> = Vec::new();

    loop {
        tokio::select! {
            Some(msg) = messages_rx.recv() => {
                if let Ok(hb) = serde_json::from_str::<HeartbeatMessage>(&msg.content) {
                    if hb.is_pong() {
                        if let Some(&expected_pubkey) = nonce_map.get(&hb.nonce) {
                            if msg.sender == expected_pubkey && !online.contains(&expected_pubkey) {
                                info!(provider = %expected_pubkey, "pong received");
                                online.push(expected_pubkey);

                                if online.len() == providers.len() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                break;
            }
        }
    }

    drop(messages_rx);
    online
}

// ── LLM scoring ──────────────────────────────────────────────────────

#[derive(Debug)]
struct ScoredProvider {
    index: usize,
    score: u32,
    reason: String,
}

async fn score_providers(
    llm: &LlmClient,
    request: &str,
    providers: &[DiscoveredAgent],
) -> Result<Vec<ScoredProvider>> {
    let provider_list: Vec<Value> = providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (price, token) = extract_price(p);
            json!({
                "index": i,
                "name": p.card.name,
                "description": p.card.description,
                "capabilities": p.card.capabilities,
                "price": price,
                "token": token,
            })
        })
        .collect();

    let system_prompt = "\
You are an AI agent matchmaker for the elisym protocol. Given a user's \
request and a list of available AI agent providers, score each provider \
from 0 to 100 based on how relevant their capabilities are to the request.\n\n\
Return ONLY a JSON array like: [{\"index\": 0, \"score\": 85, \"reason\": \"...\"}]\n\
Providers with 0 relevance should be excluded from the response.\n\
Do not include any text outside the JSON array.";

    let user_msg = json!({
        "request": truncate_for_llm(request, 2000),
        "providers": provider_list,
    })
    .to_string();

    let response = llm.complete(system_prompt, &user_msg).await?;

    // Parse the JSON response — be tolerant of markdown fences
    let json_str = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let parsed: Vec<Value> = serde_json::from_str(json_str).map_err(|e| {
        warn!(response = %response, "failed to parse LLM scoring response");
        CliError::Llm(format!("failed to parse LLM response: {}", e))
    })?;

    let mut scored: Vec<ScoredProvider> = parsed
        .iter()
        .filter_map(|v| {
            let index = v["index"].as_u64()? as usize;
            let score = v["score"].as_u64()? as u32;
            let reason = v["reason"].as_str().unwrap_or("").to_string();
            if index < providers.len() && score > 0 {
                Some(ScoredProvider { index, score, reason })
            } else {
                None
            }
        })
        .collect();

    // Sort: score desc, then price asc
    scored.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| {
            let (pa, _) = extract_price(&providers[a.index]);
            let (pb, _) = extract_price(&providers[b.index]);
            pa.cmp(&pb)
        })
    });

    Ok(scored)
}

/// Extract price and token from a DiscoveredAgent's card metadata.
fn extract_price(agent: &DiscoveredAgent) -> (u64, String) {
    if let Some(ref meta) = agent.card.metadata {
        let price = meta["job_price"].as_u64().unwrap_or(0);
        let token = meta["token"].as_str().unwrap_or("sol").to_string();
        (price, token)
    } else {
        (0, "sol".to_string())
    }
}

/// Background task that polls SOL balance and prints updates when it changes.
async fn balance_monitor(
    rpc_url: String,
    address: String,
    last_balance: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    let rpc = solana_client::rpc_client::RpcClient::new(&rpc_url);
    let pubkey = match Pubkey::from_str(&address) {
        Ok(pk) => pk,
        Err(_) => return,
    };

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(balance) = rpc.get_balance(&pubkey) {
            let prev = last_balance.swap(balance, Ordering::Relaxed);
            if balance != prev {
                let sol = balance as f64 / 1_000_000_000.0;
                let diff = balance as i64 - prev as i64;
                let diff_sol = diff as f64 / 1_000_000_000.0;
                let sign = if diff > 0 { "+" } else { "" };
                // Clear current line (may have prompt), print balance, reprint prompt
                print!(
                    "\r\x1b[2K  {} Balance: {} SOL ({}{})\r\n{} ",
                    style("$").yellow(),
                    style(format!("{:.4}", sol)).green(),
                    sign,
                    style(format!("{:.4}", diff_sol)).dim(),
                    style(">>").cyan().bold(),
                );
                let _ = io::stdout().flush();
            }
        }
    }
}

/// Format a price in human-readable form.
fn format_price(base_amount: u64, token: &str) -> String {
    match token {
        "usdc" => format!("{:.6}", base_amount as f64 / 1_000_000.0),
        _ => format!("{:.4}", base_amount as f64 / 1_000_000_000.0),
    }
}
