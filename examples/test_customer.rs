//! Test Customer — Solana devnet payment flow test.
//!
//! Creates a Solana wallet, waits for manual funding, discovers a provider,
//! submits a job, auto-pays, and displays the result.
//!
//! Run:
//!   cargo run --example test_customer

use elisym_core::*;
use nostr_sdk::ToBech32;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SAMPLE_TEXT: &str = "\
Artificial intelligence has rapidly evolved from a niche research field into a \
transformative force reshaping industries worldwide. Modern AI systems, powered \
by large language models and neural networks, can now understand and generate \
human language, analyze complex datasets, and even write software code.";

const JOB_KIND_OFFSET: u16 = 100;

#[tokio::main]
async fn main() -> Result<()> {
    let total_start = Instant::now();

    println!();
    println!("  === elisym test customer (Solana devnet) ===");
    println!();

    // ── Step 1: Load or create Solana wallet ──
    let keypair_path = customer_keypair_path();
    let (keypair, is_new) = load_or_create_keypair(&keypair_path);
    let address = keypair.pubkey().to_string();

    if is_new {
        println!("  [1/5] Created new wallet (saved to {})", keypair_path.display());
    } else {
        println!("  [1/5] Loaded existing wallet from {}", keypair_path.display());
    }

    let solana_config = SolanaPaymentConfig {
        network: SolanaNetwork::Devnet,
        rpc_url: None,
    };
    let solana_provider = SolanaPaymentProvider::new(solana_config, keypair);

    let balance = solana_provider.balance().unwrap_or(0);
    println!("         Address: {}", address);
    println!("         Balance: {} lamports ({:.4} SOL)", balance, balance as f64 / 1e9);
    println!();

    // ── Step 2: Wait for funding ──
    if balance > 0 {
        println!("  [2/5] Wallet already funded, skipping...");
    } else {
        println!("  [2/5] Wallet is empty — fund it to continue:");
        println!();
        println!("         Address: {}", address);
        println!("         e.g.: elisym-cli send <provider> {} 0.05", address);
        println!();
        println!("         Polling every 2s...");

        loop {
            let bal = solana_provider.balance().unwrap_or(0);
            if bal > 0 {
                println!(
                    "         Funded! Balance: {} lamports ({:.4} SOL)",
                    bal,
                    bal as f64 / 1e9
                );
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    println!();

    // ── Step 3: Start agent and discover provider ──
    println!("  [3/5] Starting agent + discovering provider...");

    let customer = AgentNodeBuilder::new(
        "test-customer",
        "Test customer — Solana devnet",
    )
    .capabilities(vec!["customer".into()])
    .solana_payment_provider(solana_provider)
    .build()
    .await?;

    let npub = customer.identity.npub();
    println!("         npub: {}", npub);

    let discovered = match discover_provider(&customer).await? {
        Some(p) => p,
        None => {
            tokio::task::spawn_blocking(move || drop(customer)).await.ok();
            return Ok(());
        }
    };

    let provider_npub = discovered
        .pubkey
        .to_bech32()
        .unwrap_or_else(|_| discovered.pubkey.to_hex());
    println!(
        "         Found: {} ({}...)",
        discovered.card.name,
        &provider_npub[..24]
    );
    println!();

    // ── Step 4: Submit job, auto-pay, get result ──
    println!("  [4/5] Submitting job...");

    let mut feedback_rx = customer.marketplace.subscribe_to_feedback().await?;
    let mut results_rx = customer
        .marketplace
        .subscribe_to_results(&[JOB_KIND_OFFSET], &[discovered.pubkey])
        .await?;

    let request_id = customer
        .marketplace
        .submit_job_request(
            JOB_KIND_OFFSET,
            SAMPLE_TEXT,
            "text",
            Some("text/plain"),
            Some(100_000),
            Some(&discovered.pubkey),
            vec!["summarization".into()],
        )
        .await?;

    println!("         Job submitted: {}", request_id.to_hex());

    let payments = customer
        .payments
        .as_ref()
        .expect("payments not configured");

    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            Some(fb) = feedback_rx.recv() => {
                if fb.request_id != request_id { continue; }
                match fb.parsed_status() {
                    Some(JobStatus::PaymentRequired) => {
                        if let Some(request) = &fb.payment_request {
                            println!("         Payment request received, paying...");
                            match payments.pay(request) {
                                Ok(pr) => {
                                    println!(
                                        "         Payment sent! Signature: {}...",
                                        &pr.payment_id[..20.min(pr.payment_id.len())]
                                    );
                                }
                                Err(e) => {
                                    println!("         Payment FAILED: {}", e);
                                    break;
                                }
                            }
                        }
                    }
                    Some(JobStatus::Processing) => {
                        println!("         Provider is processing...");
                    }
                    Some(JobStatus::Error) => {
                        println!(
                            "         Error: {}",
                            fb.extra_info.unwrap_or_default()
                        );
                        break;
                    }
                    _ => {
                        println!("         Feedback: {}", fb.status);
                    }
                }
            }
            Some(result) = results_rx.recv() => {
                if result.request_id != request_id { continue; }
                print_result(&result.content, total_start.elapsed());
                break;
            }
            _ = &mut timeout => {
                println!("  Timeout (5 min). Is the provider running?");
                break;
            }
        }
    }

    // ── Step 5: Show final balance ──
    println!("  [5/5] Final balance:");
    if let Some(solana) = customer.solana_payments() {
        let final_balance = solana.balance().unwrap_or(0);
        println!(
            "         {} lamports ({:.4} SOL)",
            final_balance,
            final_balance as f64 / 1e9
        );
    }
    println!();

    drop(feedback_rx);
    drop(results_rx);
    tokio::task::spawn_blocking(move || drop(customer)).await.ok();
    Ok(())
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Helpers
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

async fn discover_provider(customer: &AgentNode) -> Result<Option<DiscoveredAgent>> {
    let filter = AgentFilter {
        capabilities: vec!["summarization".into()],
        ..Default::default()
    };

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let found = customer.discovery.search_agents(&filter).await?;
        if !found.is_empty() {
            return Ok(Some(found.into_iter().next().unwrap()));
        }
        if attempt >= 20 {
            println!(
                "         No agents found after 20 attempts. Is the provider running?"
            );
            return Ok(None);
        }
        println!("         Retrying... ({}/20)", attempt);
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Path to persisted customer keypair: ~/.elisym/test-customer.key
fn customer_keypair_path() -> PathBuf {
    let home = dirs::home_dir().expect("cannot find home directory");
    home.join(".elisym").join("test-customer.key")
}

/// Load keypair from file, or create a new one and save it.
/// Returns (keypair, is_new).
fn load_or_create_keypair(path: &PathBuf) -> (Keypair, bool) {
    if path.exists() {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(kp) = Keypair::try_from(bytes.as_slice()) {
                return (kp, false);
            }
        }
    }

    let kp = Keypair::new();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, kp.to_bytes()).ok();
    (kp, true)
}

fn print_result(content: &str, elapsed: Duration) {
    println!();
    println!("  === RESULT ===");
    println!();
    println!("  {}", content);
    println!();
    println!("  Total time: {:.1}s", elapsed.as_secs_f64());
    println!();
}
