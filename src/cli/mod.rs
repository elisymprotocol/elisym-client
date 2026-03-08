mod agent;
mod args;
mod banner;
pub(crate) mod config;
pub(crate) mod crypto;
mod customer;
mod dashboard;
pub(crate) mod error;
mod llm;
mod protocol;

/// Protocol fee in basis points (300 = 3%). Integer-only arithmetic.
/// Currently hardcoded — will move to on-chain governance in Phase 3.
pub(crate) const PROTOCOL_FEE_BPS: u64 = 300;
/// Solana address of the protocol treasury that receives the protocol fee.
/// Currently hardcoded — will move to on-chain governance in Phase 3.
pub(crate) const PROTOCOL_TREASURY: &str = "GY7vnWMkKpftU4nQ16C2ATkj1JwrQpHhknkaBUn67VTy";
/// Solana rent-exempt minimum for a 0-data account (lamports).
pub(crate) const RENT_EXEMPT_MINIMUM: u64 = 890_880;
/// Minimum password length for secret key encryption.
const MIN_PASSWORD_LEN: usize = 8;

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use console::style;
use dialoguer::{Confirm, Input, MultiSelect, Password, Select};
use nostr_sdk::{Keys, ToBech32};
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tracing::info;
use zeroize::Zeroizing;
use elisym_core::{DiscoveredAgent, SolanaNetwork};

use self::args::{Cli, Commands};
use self::config::{AgentConfig, LlmSection, PaymentSection};
use self::error::{CliError, Result};

/// Run an async operation with an animated spinner.
async fn with_spinner<F, T>(message: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let msg = message.to_string();

    let handle = tokio::spawn(async move {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut i = 0;
        loop {
            if stop_clone.load(Ordering::Relaxed) {
                print!("\r\x1b[2K  {} {}\n", style("⣿").green(), style("Connected.").dim());
                let _ = std::io::stdout().flush();
                break;
            }
            print!("\r\x1b[2K  {} {}", style(frames[i % frames.len()]).cyan(), style(&msg).dim());
            let _ = std::io::stdout().flush();
            i += 1;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
    });

    let result = fut.await;
    stop.store(true, Ordering::Relaxed);
    let _ = handle.await;
    result
}

// ── model fetching ───────────────────────────────────────────────────

/// Fetch available models from the provider API, with hardcoded fallback.
fn fetch_models(provider: &str, api_key: &str) -> Vec<String> {
    println!("  {} Fetching models...", style("~").dim());

    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(fetch_models_async(provider, api_key))
    });

    match result {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => {
            println!("  {} No models returned, using defaults.", style("!").yellow());
            fallback_models(provider)
        }
        Err(e) => {
            println!("  {} Could not fetch models: {}", style("!").yellow(), e);
            fallback_models(provider)
        }
    }
}

fn fallback_models(provider: &str) -> Vec<String> {
    match provider {
        "anthropic" => vec![
            "claude-sonnet-4-20250514".into(),
            "claude-haiku-4-5-20251001".into(),
            "claude-opus-4-20250514".into(),
        ],
        _ => vec!["gpt-4o".into(), "gpt-4o-mini".into(), "gpt-4-turbo".into()],
    }
}

async fn fetch_models_async(provider: &str, api_key: &str) -> std::result::Result<Vec<String>, reqwest::Error> {
    let client = reqwest::Client::new();

    match provider {
        "anthropic" => {
            let resp: serde_json::Value = client
                .get("https://api.anthropic.com/v1/models")
                .header("anthropic-version", "2023-06-01")
                .header("x-api-key", api_key)
                .query(&[("limit", "1000")])
                .send()
                .await?
                .json()
                .await?;

            let mut models: Vec<String> = resp["data"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| Some(m["id"].as_str()?.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            models.sort();
            Ok(models)
        }
        "openai" => {
            let resp: serde_json::Value = client
                .get("https://api.openai.com/v1/models")
                .header("Authorization", format!("Bearer {}", api_key))
                .send()
                .await?
                .json()
                .await?;

            let mut models: Vec<String> = resp["data"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let id = m["id"].as_str()?;
                            // Only keep chat-compatible models
                            let is_chat = (id.starts_with("gpt-")
                                || id.starts_with("o1")
                                || id.starts_with("o3")
                                || id.starts_with("o4")
                                || id.starts_with("chatgpt-"))
                                && !id.contains("instruct")
                                && !id.contains("realtime")
                                && !id.contains("audio")
                                && !id.contains("tts")
                                && !id.contains("whisper")
                                && !id.contains("davinci")
                                && !id.contains("babbage");
                            if is_chat {
                                Some(id.to_string())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            models.sort();
            models.dedup();
            Ok(models)
        }
        _ => Ok(vec![]),
    }
}

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter({
            let base = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            // Always suppress noisy relay pool logs (connection retries, timeouts)
            base.add_directive("nostr_relay_pool=off".parse().unwrap())
        })
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Init) => { cmd_init()?; },
        Some(Commands::Start { name, free }) => cmd_start(name, free).await?,
        Some(Commands::List) => cmd_list()?,
        Some(Commands::Status { name }) => cmd_status(&name)?,
        Some(Commands::Delete { name }) => cmd_delete(&name)?,
        Some(Commands::Config { name }) => cmd_config(&name)?,
        Some(Commands::Wallet { name }) => cmd_wallet(&name)?,
        Some(Commands::Send { name, address, amount }) => cmd_send(&name, &address, &amount)?,
        Some(Commands::Dashboard { chain, network, rpc_url }) => cmd_dashboard(&chain, &network, rpc_url).await?,
        None => {
            // No subcommand — show banner and help
            print!("{}", style(banner::BANNER).cyan());
            println!("  Run {} to get started.\n", style("elisym init").cyan());
            Cli::parse_from(["elisym", "--help"]);
        }
    }

    Ok(())
}

// ── init ──────────────────────────────────────────────────────────────

fn cmd_init() -> Result<String> {
    print!("{}", style(banner::BANNER).cyan());
    println!("  {}\n", style("Create a new agent").bold());
    println!("  {}", style("Your agent is an AI that lives on the elisym network.").dim());
    println!("  {}", style("It can earn SOL by completing tasks for other agents (provider),").dim());
    println!("  {}", style("or pay other agents to do work for you (customer).").dim());
    println!();
    println!("  {}", style("Let's set it up step by step. Type \"back\" at any prompt to go back.").dim());
    println!();

    // Wizard state
    let mut name = String::new();
    let mut description = String::new();
    let mut network = String::new();
    let mut rpc_url_opt: Option<String> = None;
    let mut provider = String::new();
    let mut api_key = String::new();
    let mut model = String::new();
    let mut max_tokens: u32 = 4096;
    let mut encryption_password: Option<Zeroizing<String>> = None;

    let mut step: usize = 0;
    loop {
        match step {
            // Step 0: Agent name (no back — first step)
            0 => {
                println!("  {}", style("A unique name for your agent (used in commands like `start my-agent`)").dim());
                let input: String = Input::new()
                    .with_prompt("Agent name")
                    .interact_text()?;

                if input.is_empty() {
                    println!("  {} Name cannot be empty.", style("!").yellow());
                    continue;
                }

                if config::config_path(&input)?.exists() {
                    println!(
                        "  {} Agent '{}' already exists. Choose a different name.",
                        style("!").yellow(),
                        style(&input).cyan()
                    );
                    continue;
                }

                name = input;
                step += 1;
            }

            // Step 1: Description
            1 => {
                println!("  {}", style("What does your agent do? This is shown to other agents on the network.").dim());
                let input: String = Input::new()
                    .with_prompt("Description (or \"back\")")
                    .default("An elisym AI agent".into())
                    .interact_text()?;

                if input.eq_ignore_ascii_case("back") {
                    step -= 1;
                    continue;
                }

                description = input;
                step += 1;
            }

            // Step 2: Solana network
            2 => {
                println!("  {}", style("Solana is used for payments between agents.").dim());
                println!("  {}", style("Use devnet for testing (free SOL via faucet.solana.com).").dim());
                let options = &[
                    "mainnet (default)",
                    "devnet (testing)",
                    "testnet (coming soon)",
                    "\u{2190} Back",
                ];
                let idx = Select::new()
                    .with_prompt("Solana network")
                    .items(options)
                    .default(0)
                    .interact()?;

                if idx == 3 {
                    step -= 1;
                    continue;
                }

                if idx == 2 {
                    println!("  {}", style("⚠ This network is not available yet.").yellow());
                    continue;
                }

                network = match idx {
                    1 => "devnet",
                    _ => "mainnet",
                }.to_string();
                step += 1;
            }

            // Step 3: RPC URL
            3 => {
                println!("  {}", style("Solana RPC endpoint. The default works fine — change only if you have a custom node.").dim());
                let default_rpc = match network.as_str() {
                    "devnet" => "https://api.devnet.solana.com",
                    "testnet" => "https://api.testnet.solana.com",
                    _ => "https://api.mainnet-beta.solana.com",
                };
                let input: String = Input::new()
                    .with_prompt("RPC URL (or \"back\")")
                    .default(default_rpc.into())
                    .interact_text()?;

                if input.eq_ignore_ascii_case("back") {
                    step -= 1;
                    continue;
                }

                rpc_url_opt = if input == default_rpc { None } else { Some(input) };
                step += 1;
            }

            // Step 4: LLM provider
            4 => {
                println!("  {}", style("Your agent uses an LLM to process tasks. Pick a provider and enter your API key.").dim());
                let options = &["Anthropic (Claude)", "OpenAI (GPT)", "\u{2190} Back"];
                let idx = Select::new()
                    .with_prompt("LLM provider")
                    .items(options)
                    .default(0)
                    .interact()?;

                if idx == 2 {
                    step -= 1;
                    continue;
                }

                provider = match idx {
                    0 => "anthropic".to_string(),
                    _ => "openai".to_string(),
                };
                step += 1;
            }

            // Step 5: API key
            5 => {
                let label = if provider == "anthropic" {
                    "Anthropic (Claude)"
                } else {
                    "OpenAI (GPT)"
                };
                let input: String = Password::new()
                    .with_prompt(format!("{} API key (or \"back\")", label))
                    .interact()?;

                if input.eq_ignore_ascii_case("back") {
                    step -= 1;
                    continue;
                }

                if input.is_empty() {
                    println!("  {} API key cannot be empty.", style("!").yellow());
                    continue;
                }

                api_key = input;
                step += 1;
            }

            // Step 6: Model selection (fetched from API)
            6 => {
                println!("  {}", style("Which model your agent will use. Faster models = lower cost, smarter models = better results.").dim());
                let mut models = fetch_models(&provider, &api_key);
                models.push("\u{2190} Back".to_string());

                let idx = Select::new()
                    .with_prompt("Model")
                    .items(&models)
                    .default(0)
                    .interact()?;

                if idx == models.len() - 1 {
                    step -= 1;
                    continue;
                }

                model = models[idx].clone();
                step += 1;
            }

            // Step 7: Max tokens
            7 => {
                println!("  {}", style("Maximum length of each LLM response (in tokens). 4096 is good for most tasks.").dim());
                let input: String = Input::new()
                    .with_prompt("Max tokens per LLM response (or \"back\")")
                    .default("4096".into())
                    .interact_text()?;

                if input.eq_ignore_ascii_case("back") {
                    step -= 1;
                    continue;
                }

                match input.parse::<u32>() {
                    Ok(val) if val > 0 => {
                        max_tokens = val;
                        step += 1;
                    }
                    _ => {
                        println!("  {} Invalid number.", style("!").yellow());
                    }
                }
            }

            // Step 8: Optional password encryption
            8 => {
                println!("  {}", style("Encrypt your agent's secret keys with a password.").dim());
                println!("  {}", style("You'll enter this password each time you start the agent.").dim());
                let options = &[
                    "Yes, set a password",
                    "No, store keys unencrypted",
                    "\u{2190} Back",
                ];
                let default = if network == "mainnet" { 0 } else { 1 };
                let idx = Select::new()
                    .with_prompt("Encrypt keys?")
                    .items(options)
                    .default(default)
                    .interact()?;

                match idx {
                    0 => {
                        let pw = Password::new()
                            .with_prompt("Password")
                            .with_confirmation("Confirm password", "Passwords don't match")
                            .interact()?;
                        if pw.len() < MIN_PASSWORD_LEN {
                            println!(
                                "  {} Password must be at least {} characters.",
                                style("!").yellow(),
                                MIN_PASSWORD_LEN,
                            );
                            continue;
                        }
                        encryption_password = Some(Zeroizing::new(pw));
                        step += 1;
                    }
                    1 => {
                        encryption_password = None;
                        step += 1;
                    }
                    _ => {
                        step -= 1;
                        continue;
                    }
                }
            }

            // Done — build config
            _ => break,
        }
    }

    // Generate Nostr keypair
    let keys = Keys::generate();
    let secret_key = keys.secret_key().to_secret_hex();

    // Generate Solana keypair
    let solana_keypair = Keypair::new();
    let solana_secret_key = bs58::encode(solana_keypair.to_bytes()).into_string();
    let solana_address = solana_keypair.pubkey().to_string();

    let llm_section = LlmSection {
        provider: provider.clone(),
        api_key,
        model,
        max_tokens,
    };

    let mut cfg = AgentConfig {
        name: name.clone(),
        description,
        capabilities: vec!["general".to_string()],
        relays: vec![
            "wss://relay.damus.io".into(),
            "wss://nos.lol".into(),
            "wss://relay.nostr.band".into(),
        ],
        secret_key,
        payment: PaymentSection {
            chain: "solana".to_string(),
            network: network.clone(),
            rpc_url: rpc_url_opt,
            job_price: 10_000_000, // 0.01 SOL default
            payment_timeout_secs: 120,
            solana_secret_key,
        },
        inactive_capabilities: vec![],
        capability_prompts: HashMap::new(),
        llm: Some(llm_section),
        customer_llm: None,
        encryption: None,
    };

    if let Some(ref password) = encryption_password {
        cfg.encrypt_secrets(password)?;
    }
    config::save_config(&cfg)?;
    // encryption_password is zeroized automatically on drop (Zeroizing<String>)

    let npub = keys.public_key().to_bech32().unwrap_or_default();
    println!("\n  {} Agent {} created!", style("✓").green().bold(), style(&name).cyan().bold());
    println!();
    println!("     {}  {}", style("npub").dim(), style(&npub).dim());
    println!("     {}  {}", style("wallet").dim(), style(&solana_address).dim());
    println!("     {}  {}", style("network").dim(), style(&network).cyan());
    println!("     {}  {}", style("config").dim(), style(config::config_path(&name)?.display()).dim());

    if network != "mainnet" {
        println!(
            "\n  Get devnet SOL   {}",
            style("https://faucet.solana.com").cyan()
        );
    }
    println!("  Start agent      {}\n", style(format!("elisym start {}", name)).cyan());

    Ok(name)
}

// ── config ────────────────────────────────────────────────────────────

fn cmd_config(name: &str) -> Result<()> {
    let mut cfg = config::load_config(name)?;
    let password: Option<Zeroizing<String>> = if cfg.is_encrypted() {
        let p = Zeroizing::new(Password::new()
            .with_prompt("Password")
            .interact()?);
        cfg.decrypt_secrets(&p)?;
        Some(p)
    } else {
        None
    };

    println!("{}\n", style(format!("Configure agent: {}", name)).bold());

    loop {
        let main_options = &["Provider settings", "Customer settings", "Done"];
        let main_idx = Select::new()
            .with_prompt("Settings")
            .items(main_options)
            .default(0)
            .interact()?;

        match main_idx {
            // Provider settings
            0 => {
                loop {
                    let sub_options = &["Capabilities", "Job price", "LLM provider", "\u{2190} Back"];
                    let sub_idx = Select::new()
                        .with_prompt("Provider settings")
                        .items(sub_options)
                        .default(0)
                        .interact()?;

                    match sub_idx {
                        // Capabilities
                        0 => {
                            let has_real_caps = cfg.capabilities != ["general"]
                                || !cfg.inactive_capabilities.is_empty();

                            if !has_real_caps {
                                // No real capabilities — directly offer LLM describe
                                let caps = prompt_capabilities_llm_sync(&cfg)?;
                                if !caps.is_empty() {
                                    cfg.capabilities = caps.iter().map(|(n, _)| n.clone()).collect();
                                    for (n, p) in &caps {
                                        cfg.capability_prompts.insert(n.clone(), p.clone());
                                    }
                                    save_config_encrypted(&mut cfg, &password)?;
                                    println!(
                                        "  {} Capabilities saved: {}",
                                        style("*").green(),
                                        cfg.capabilities.join(", ")
                                    );
                                }
                            } else {
                                // Has real capabilities — show submenu
                                let cap_sub = &["Toggle capabilities", "Add capabilities (describe)", "\u{2190} Back"];
                                let cap_idx = Select::new()
                                    .with_prompt("Capabilities")
                                    .items(cap_sub)
                                    .default(0)
                                    .interact()?;

                                match cap_idx {
                                    // Toggle
                                    0 => {
                                        let all_caps: Vec<String> = cfg
                                            .capabilities
                                            .iter()
                                            .chain(cfg.inactive_capabilities.iter())
                                            .cloned()
                                            .collect();
                                        let defaults: Vec<bool> = all_caps
                                            .iter()
                                            .map(|c| cfg.capabilities.contains(c))
                                            .collect();
                                        let selections = MultiSelect::new()
                                            .with_prompt("Capabilities (space to toggle, enter to confirm)")
                                            .items(&all_caps)
                                            .defaults(&defaults)
                                            .interact()?;

                                        let selected: Vec<String> = selections
                                            .iter()
                                            .map(|&i| all_caps[i].clone())
                                            .collect();
                                        let inactive: Vec<String> = all_caps
                                            .iter()
                                            .filter(|c| !selected.contains(c))
                                            .cloned()
                                            .collect();

                                        cfg.capabilities = if selected.is_empty() {
                                            vec!["general".to_string()]
                                        } else {
                                            selected
                                        };
                                        cfg.inactive_capabilities = inactive;
                                        save_config_encrypted(&mut cfg, &password)?;

                                        println!(
                                            "  {} Active: {}",
                                            style("*").green(),
                                            cfg.capabilities.join(", ")
                                        );
                                        if !cfg.inactive_capabilities.is_empty() {
                                            println!(
                                                "  {} Inactive: {}",
                                                style("~").dim(),
                                                cfg.inactive_capabilities.join(", ")
                                            );
                                        }
                                    }
                                    // Add via describe
                                    1 => {
                                        let caps = prompt_capabilities_llm_sync(&cfg)?;
                                        for (n, p) in caps {
                                            if !cfg.capabilities.contains(&n)
                                                && !cfg.inactive_capabilities.contains(&n)
                                            {
                                                cfg.capabilities.push(n.clone());
                                                cfg.capability_prompts.insert(n, p);
                                            } else {
                                                println!(
                                                    "  {} Skipped duplicate: {}",
                                                    style("~").dim(),
                                                    n
                                                );
                                            }
                                        }
                                        save_config_encrypted(&mut cfg, &password)?;
                                        println!(
                                            "  {} Capabilities saved: {}",
                                            style("*").green(),
                                            cfg.capabilities.join(", ")
                                        );
                                    }
                                    // Back
                                    _ => {}
                                }
                            }
                        }
                        // Job price
                        1 => {
                            println!(
                                "  {} Current price: {} SOL",
                                style("~").dim(),
                                format_sol(cfg.payment.job_price)
                            );
                            loop {
                                let input: String = Input::new()
                                    .with_prompt("Job price in SOL")
                                    .default(format_sol(cfg.payment.job_price))
                                    .interact_text()?;

                                match sol_to_lamports(&input) {
                                    Some(new_price) => {
                                        if let Some(err) = agent::validate_job_price(new_price) {
                                            println!("  {} {}", style("!").yellow(), err);
                                            continue;
                                        }
                                        cfg.payment.job_price = new_price;
                                        save_config_encrypted(&mut cfg, &password)?;
                                        println!(
                                            "  {} Price set to {} SOL",
                                            style("*").green(),
                                            format_sol(cfg.payment.job_price)
                                        );
                                        break;
                                    }
                                    _ => {
                                        println!("  {} Invalid amount.", style("!").yellow());
                                    }
                                }
                            }
                        }
                        // LLM provider
                        2 => {
                            if let Some(llm) = prompt_llm_section()? {
                                cfg.llm = Some(llm);
                                save_config_encrypted(&mut cfg, &password)?;
                                println!("  {} Provider LLM saved.", style("*").green());
                            }
                        }
                        // Back
                        _ => break,
                    }
                }
            }
            // Customer settings
            1 => {
                loop {
                    let sub_options = &["LLM provider", "\u{2190} Back"];
                    let sub_idx = Select::new()
                        .with_prompt("Customer settings")
                        .items(sub_options)
                        .default(0)
                        .interact()?;

                    match sub_idx {
                        0 => {
                            if let Some(llm) = prompt_llm_section()? {
                                cfg.customer_llm = Some(llm);
                                save_config_encrypted(&mut cfg, &password)?;
                                println!("  {} Customer LLM saved.", style("*").green());
                            }
                        }
                        _ => break,
                    }
                }
            }
            // Done
            _ => break,
        }
    }

    // password is zeroized automatically on drop (Zeroizing<String>)

    println!(
        "\n  {} All changes saved for '{}'.",
        style("*").green(),
        style(name).cyan()
    );
    println!("  Restart agent to publish updated capabilities.");

    Ok(())
}

/// Shared LLM configuration flow used by both init and config commands.
/// Returns None if the user backs out at the first prompt.
fn prompt_llm_section() -> Result<Option<LlmSection>> {
    // Provider
    let provider_options = &["Anthropic (Claude)", "OpenAI (GPT)", "\u{2190} Back"];
    let provider_idx = Select::new()
        .with_prompt("LLM provider")
        .items(provider_options)
        .default(0)
        .interact()?;

    if provider_idx == 2 {
        return Ok(None);
    }

    let provider = match provider_idx {
        0 => "anthropic",
        _ => "openai",
    };

    // API key
    let label = provider_options[provider_idx];
    let api_key: String = Password::new()
        .with_prompt(format!("{} API key", label))
        .interact()?;
    if api_key.is_empty() {
        println!("  {} API key cannot be empty.", style("!").yellow());
        return Ok(None);
    }

    // Model (fetched from API)
    let models = fetch_models(provider, &api_key);
    let model_idx = Select::new()
        .with_prompt("Model")
        .items(&models)
        .default(0)
        .interact()?;

    // Max tokens
    let max_tokens: u32 = Input::new()
        .with_prompt("Max tokens per LLM response")
        .default(4096)
        .interact_text()?;

    Ok(Some(LlmSection {
        provider: provider.to_string(),
        api_key,
        model: models[model_idx].to_string(),
        max_tokens,
    }))
}

// ── capability LLM helper ─────────────────────────────────────────────

/// Ask the user to describe what the agent does, then call the LLM to extract
/// capabilities and generate a system prompt for each.
/// Returns a Vec of (capability_name, capability_prompt) pairs, or empty if the user backs out.
async fn prompt_capabilities_llm(config: &AgentConfig) -> Result<Vec<(String, String)>> {
    let llm_section = config
        .llm
        .as_ref()
        .ok_or_else(|| CliError::Llm("no LLM configured — run `elisym init` to set up".into()))?;
    let llm = llm::LlmClient::new(llm_section)?;

    let description = match prompt_paste_aware("Describe what your agent can do (or \"back\")")? {
        Some(s) if !s.is_empty() && !s.eq_ignore_ascii_case("back") => s,
        _ => return Ok(vec![]),
    };

    println!("  {} Analyzing capabilities...", style("~").dim());

    let system = "You help AI agents define their capabilities for a marketplace.\n\
        Given a description of what an agent can do, return a JSON object with:\n\
        - \"capabilities\": array of objects, each with:\n\
          - \"name\": short capability keyword (lowercase, hyphenated, e.g. \"code-generation\")\n\
          - \"prompt\": a detailed system prompt (2-4 sentences) that instructs an AI to excel at this capability\n\
        Return 3-8 capabilities. Return ONLY the JSON, no other text.";

    let max_retries = 3;
    let mut last_err = None;
    let mut response = String::new();
    for attempt in 0..max_retries {
        match llm.complete(system, &description).await {
            Ok(r) => {
                response = r;
                last_err = None;
                break;
            }
            Err(e) => {
                if attempt + 1 < max_retries {
                    println!(
                        "  {} {}\n  {} Retrying in 10 seconds... (attempt {}/{})",
                        style("!").yellow(),
                        e,
                        style("~").dim(),
                        attempt + 2,
                        max_retries,
                    );
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e);
    }

    // Parse JSON from LLM response (handle possible markdown fencing)
    let json_str = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let parsed: serde_json::Value = serde_json::from_str(json_str).map_err(|e| {
        CliError::Llm(format!("failed to parse LLM response as JSON: {}", e))
    })?;

    let caps = parsed["capabilities"]
        .as_array()
        .ok_or_else(|| CliError::Llm("LLM response missing 'capabilities' array".into()))?;

    let mut results: Vec<(String, String)> = Vec::new();
    for cap in caps {
        let name = cap["name"].as_str().unwrap_or_default().to_string();
        let prompt = cap["prompt"].as_str().unwrap_or_default().to_string();
        if !name.is_empty() && !prompt.is_empty() {
            results.push((name, prompt));
        }
    }

    if results.is_empty() {
        println!("  {} No capabilities extracted. Try again with more detail.", style("!").yellow());
        return Ok(vec![]);
    }

    // Display detected capabilities
    println!("\n  {} Detected capabilities:\n", style("*").green());
    for (name, prompt) in &results {
        println!("  {} {}", style(name).cyan().bold(), style("—").dim());
        println!("    {}\n", style(prompt).dim());
    }

    let confirmed = Confirm::new()
        .with_prompt("Use these capabilities?")
        .default(true)
        .interact()?;

    if !confirmed {
        return Ok(vec![]);
    }

    Ok(results)
}

/// Sync wrapper for `prompt_capabilities_llm`, for use inside `cmd_config`.
fn prompt_capabilities_llm_sync(config: &AgentConfig) -> Result<Vec<(String, String)>> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(prompt_capabilities_llm(config))
    })
}

// ── start ─────────────────────────────────────────────────────────────

async fn cmd_start(name: Option<String>, free: bool) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => select_or_create_agent()?,
    };

    let mut cfg = config::load_config(&name)?;
    let enc_password: Option<Zeroizing<String>> = if cfg.is_encrypted() {
        let p = Zeroizing::new(Password::new()
            .with_prompt(format!("Password for '{}'", name))
            .interact()?);
        cfg.decrypt_secrets(&p)?;
        Some(p)
    } else {
        None
    };

    println!("{}", style(banner::BANNER).cyan());
    println!("  Starting agent {}...\n", style(&name).cyan().bold());

    if free {
        println!(
            "  {} FREE MODE — payments disabled, jobs processed for free\n",
            style("!").yellow().bold()
        );
    }

    // Show wallet status (no relay connection needed)
    let solana = agent::build_solana_provider(&cfg)?;
    display_wallet_status(&solana, &cfg)?;

    if !free {
        let balance = solana.balance().unwrap_or(0);
        if balance == 0 && cfg.payment.network != "mainnet" {
            println!(
                "\n  {} Wallet is empty. Get devnet SOL: {}",
                style("!").yellow(),
                style("https://faucet.solana.com").cyan()
            );
        }
    }
    drop(solana);

    // Mode selection
    println!();
    println!("  {} {}",
        style("Provider").cyan().bold(),
        style("— listen for jobs, complete tasks with your LLM, earn SOL").dim(),
    );
    println!("  {} {}",
        style("Customer").cyan().bold(),
        style("— send requests to other agents, pay them in SOL").dim(),
    );
    println!();
    let mode_options = &["Provider (listen for jobs)", "Customer (send requests)"];
    let mode_idx = Select::new()
        .with_prompt("Start as")
        .items(mode_options)
        .default(0)
        .interact()?;

    // enc_password is zeroized automatically on drop (Zeroizing<String>)

    println!();

    match mode_idx {
        // Provider mode
        0 => {
            // First-time capability setup: if only "general" and no inactive caps
            if cfg.capabilities == ["general"] && cfg.inactive_capabilities.is_empty() {
                let caps = prompt_capabilities_llm(&cfg).await?;
                if !caps.is_empty() {
                    cfg.capabilities = caps.iter().map(|(name, _)| name.clone()).collect();
                    for (name, prompt) in &caps {
                        cfg.capability_prompts.insert(name.clone(), prompt.clone());
                    }
                    save_config_encrypted(&mut cfg, &enc_password)?;
                    println!(
                        "  {} Capabilities: {}\n",
                        style("✓").green().bold(),
                        style(cfg.capabilities.join(", ")).cyan(),
                    );
                }
            }

            // Job price prompt (first-time or update)
            println!("  {}", style(
                "How much your agent charges per task (in SOL). A 3% protocol fee is deducted from each payment."
            ).dim());
            loop {
                let price_input: String = Input::new()
                    .with_prompt("Job price in SOL")
                    .default(format_sol(cfg.payment.job_price))
                    .interact_text()?;

                match sol_to_lamports(&price_input) {
                    Some(new_price) => {
                        if let Some(err) = agent::validate_job_price(new_price) {
                            println!("  {} {}", style("!").yellow(), err);
                            continue;
                        }
                        if new_price != cfg.payment.job_price {
                            cfg.payment.job_price = new_price;
                            save_config_encrypted(&mut cfg, &enc_password)?;
                        }
                        println!(
                            "  {} Price set to {} SOL\n",
                            style("✓").green().bold(),
                            style(format_sol(cfg.payment.job_price)).green().bold()
                        );
                        break;
                    }
                    _ => {
                        println!("  {} Invalid amount.", style("!").yellow());
                    }
                }
            }

            info!(agent = %name, "building agent node");
            let agent = with_spinner(
                "Connecting to relays and publishing capabilities...",
                agent::build_agent(&cfg),
            ).await?;
            info!(agent = %name, npub = %agent.identity.npub(), "agent node ready");
            println!();
            println!("  {} {}",
                style("⚡").yellow(),
                style("Agent is live — listening for jobs").bold(),
            );
            println!("     {}",
                style(format!("Capabilities: {}", cfg.capabilities.join(", "))).dim(),
            );
            println!("     {}",
                style(format!("Price: {} SOL per job", format_sol(cfg.payment.job_price))).dim(),
            );
            println!("     {}",
                style("Press Ctrl+C to stop.").dim(),
            );
            println!();
            agent::run_agent(agent, &cfg, free).await?;
        }
        // Customer mode
        _ => {
            if free {
                println!(
                    "  {} --free flag is ignored in customer mode\n",
                    style("!").yellow()
                );
            }
            info!(agent = %name, "building agent node");
            let agent = with_spinner(
                "Connecting to relays...",
                agent::build_agent(&cfg),
            ).await?;
            info!(agent = %name, npub = %agent.identity.npub(), "agent node ready");
            customer::run_customer_repl(agent, &cfg).await?;
        }
    }

    Ok(())
}

/// Let the user pick an existing agent or create a new one.
fn select_or_create_agent() -> Result<String> {
    let agents = config::list_agents()?;
    if agents.is_empty() {
        println!("No agents configured. Running init wizard...\n");
        return cmd_init();
    }

    let mut options: Vec<String> = agents.clone();
    options.push("+ Create new agent".into());

    let idx = Select::new()
        .with_prompt("Select agent to start")
        .items(&options)
        .default(0)
        .interact()?;

    if idx == agents.len() {
        return cmd_init();
    }

    Ok(agents[idx].clone())
}

// ── list ──────────────────────────────────────────────────────────────

fn cmd_list() -> Result<()> {
    let agents = config::list_agents()?;
    if agents.is_empty() {
        println!("No agents configured. Run {} to create one.", style("elisym init").cyan());
        return Ok(());
    }

    println!("{}", style("Configured agents:").bold());
    for name in &agents {
        match config::load_config_public(name) {
            Ok(cfg) => {
                println!(
                    "  {} — {} [{}]",
                    style(name).cyan(),
                    cfg.description,
                    cfg.capabilities.join(", ")
                );
            }
            Err(_) => {
                println!("  {} — {}", style(name).cyan(), style("(config error)").red());
            }
        }
    }
    Ok(())
}

// ── status ────────────────────────────────────────────────────────────

fn cmd_status(name: &str) -> Result<()> {
    let cfg = config::load_config_public(name)?;

    println!("{}", style(format!("Agent: {}", cfg.name)).bold());
    println!("  description:  {}", cfg.description);
    println!("  capabilities: {}", cfg.capabilities.join(", "));
    println!("  relays:       {}", cfg.relays.join(", "));
    println!("  chain:        {}", cfg.payment.chain);
    println!("  network:      {}", cfg.payment.network);
    println!(
        "  job price:    {} SOL ({} lamports)",
        format_sol(cfg.payment.job_price),
        cfg.payment.job_price
    );
    if let Some(addr) = cfg.payment.solana_address() {
        println!("  wallet:       {}", addr);
    }
    if let Some(ref llm) = cfg.llm {
        println!("  llm provider: {}", llm.provider);
        println!("  llm model:    {}", llm.model);
        println!("  max tokens:   {}", llm.max_tokens);
    } else {
        println!("  llm:          {}", style("not configured").dim());
    }
    if let Some(ref cllm) = cfg.customer_llm {
        println!("  customer llm: {} ({})", cllm.provider, cllm.model);
    }
    println!("  config:       {}", config::config_path(name)?.display());

    Ok(())
}

// ── delete ────────────────────────────────────────────────────────────

fn cmd_delete(name: &str) -> Result<()> {
    let confirmed = Confirm::new()
        .with_prompt(format!("Delete agent '{}' and all its data?", name))
        .default(false)
        .interact()?;

    if !confirmed {
        println!("Cancelled.");
        return Ok(());
    }

    config::delete_agent(name)?;
    println!("Deleted agent '{}'.", style(name).cyan());
    Ok(())
}

// ── wallet ────────────────────────────────────────────────────────────

fn cmd_wallet(name: &str) -> Result<()> {
    let mut cfg = config::load_config(name)?;
    if cfg.is_encrypted() {
        unlock_config(&mut cfg)?;
    }

    print!("{}", style(banner::BANNER).cyan());
    println!(
        "  Wallet for agent {}\n",
        style(name).cyan().bold()
    );

    let solana = agent::build_solana_provider(&cfg)?;
    display_wallet_status(&solana, &cfg)?;

    Ok(())
}

// ── send ─────────────────────────────────────────────────────────────

fn cmd_send(name: &str, address: &str, amount: &str) -> Result<()> {
    let mut cfg = config::load_config(name)?;
    if cfg.is_encrypted() {
        unlock_config(&mut cfg)?;
    }

    // Validate destination address early
    let dest_pubkey: solana_sdk::pubkey::Pubkey = address
        .parse()
        .map_err(|_| CliError::Other(format!("invalid Solana address: {}", address)))?;

    let solana = agent::build_solana_provider(&cfg)?;

    // Self-transfer warning
    let own_address = solana.address();
    if dest_pubkey.to_string() == own_address {
        println!(
            "  {} Destination is the agent's own wallet ({})",
            style("!").yellow().bold(),
            style(&own_address).dim(),
        );
        let proceed = Confirm::new()
            .with_prompt("Send to yourself?")
            .default(false)
            .interact()?;
        if !proceed {
            println!("  Cancelled.");
            return Ok(());
        }
    }

    // Convert SOL to lamports
    let base_amount = sol_to_lamports(amount)
        .ok_or_else(|| CliError::Other(format!("invalid SOL amount: {}", amount)))?;
    let unit_label = "SOL";

    // Show current balance
    let balance = solana.balance().unwrap_or(0);
    println!("  Balance: {} SOL ({} lamports)", format_sol(balance), balance);
    println!(
        "  Sending {} {} to {}",
        style(format_sol_compact(base_amount)).bold(),
        unit_label,
        style(address).dim()
    );

    let confirmed = Confirm::new()
        .with_prompt("Confirm send?")
        .default(false)
        .interact()?;

    if !confirmed {
        println!("  Cancelled.");
        return Ok(());
    }

    // Use a dummy reference key (not needed for direct sends, but required by the format)
    let reference = Keypair::new().pubkey().to_string();
    let request_json = serde_json::json!({
        "recipient": address,
        "amount": base_amount,
        "reference": reference,
    }).to_string();

    use elisym_core::PaymentProvider;
    match solana.pay(&request_json) {
        Ok(result) => {
            println!(
                "\n  {} Sent {} {}",
                style("*").green(),
                style(format_sol_compact(base_amount)).bold(),
                unit_label
            );
            println!("  Signature: {}", style(&result.payment_id).dim());

            // Show updated balance
            if let Ok(new_balance) = solana.balance() {
                println!(
                    "  Balance:   {} SOL ({} lamports)",
                    format_sol(new_balance),
                    new_balance
                );
            }
        }
        Err(e) => {
            println!("  {} Send failed: {}", style("!").red(), e);
            return Err(e.into());
        }
    }

    Ok(())
}

// ── wallet helpers ───────────────────────────────────────────────────

fn display_wallet_status(solana: &elisym_core::SolanaPaymentProvider, cfg: &AgentConfig) -> Result<()> {
    let address = solana.address();
    let balance = solana.balance().unwrap_or(0);

    println!("\n  {} {}", style("◆").cyan(), style("Wallet").bold());
    println!("     Network  {}", style(&cfg.payment.network).cyan());
    println!("     Address  {}", style(&address).dim());
    println!(
        "     Balance  {} SOL {}",
        style(format_sol(balance)).green().bold(),
        style(format!("({} lamports)", balance)).dim(),
    );

    Ok(())
}

// ── dashboard ─────────────────────────────────────────────────────

async fn cmd_dashboard(chain: &str, network: &str, rpc_url: Option<String>) -> Result<()> {
    dashboard::run_dashboard(chain.to_string(), network.to_string(), rpc_url).await
}

/// Format lamports as SOL with full 9-decimal precision (integer-only arithmetic).
fn format_sol(lamports: u64) -> String {
    let whole = lamports / 1_000_000_000;
    let frac = lamports % 1_000_000_000;
    format!("{}.{:09}", whole, frac)
}

/// Format lamports as SOL with 4-decimal compact display (integer-only arithmetic).
pub(crate) fn format_sol_compact(lamports: u64) -> String {
    let whole = lamports / 1_000_000_000;
    let frac = (lamports % 1_000_000_000) / 100_000;
    format!("{}.{:04}", whole, frac)
}

/// Format basis points as a percentage string (e.g., 300 bps → "3.00%").
pub(crate) fn format_bps_percent(bps: u64) -> String {
    let whole = bps / 100;
    let frac = bps % 100;
    format!("{}.{:02}%", whole, frac)
}

/// Parse a SOL amount string (e.g. "1.5") into lamports using integer-only arithmetic.
/// Returns None for invalid input, zero/negative amounts, or > 9 decimal places.
fn sol_to_lamports(sol_str: &str) -> Option<u64> {
    let s = sol_str.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.splitn(2, '.').collect();
    let whole: u64 = parts[0].parse().ok()?;
    let frac: u64 = if parts.len() == 2 {
        let frac_str = parts[1];
        if frac_str.is_empty() || frac_str.len() > 9 {
            return None;
        }
        let padded = format!("{:0<9}", frac_str);
        padded.parse().ok()?
    } else {
        0
    };
    whole.checked_mul(1_000_000_000)?.checked_add(frac)
}

/// Paste-aware single-line prompt using crossterm raw mode.
/// Handles bracketed paste and fallback paste detection (50ms timeout on Enter).
/// Returns the trimmed input string, or None if the user pressed Ctrl+C / Ctrl+D.
fn prompt_paste_aware(prompt: &str) -> Result<Option<String>> {
    use std::io::{self, Write};
    use crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        execute,
        event::{EnableBracketedPaste, DisableBracketedPaste},
        terminal,
    };

    print!("{}: ", style(prompt).bold());
    io::stdout().flush()?;

    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnableBracketedPaste)?;

    let mut buffer = String::new();
    let result = loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(e) => {
                terminal::disable_raw_mode()?;
                execute!(io::stdout(), DisableBracketedPaste)?;
                return Err(e.into());
            }
        };
        match ev {
            Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => match code {
                KeyCode::Enter => {
                    // Paste detection: if more input arrives within 50ms, treat Enter as
                    // part of a paste and convert to space (single-line prompt).
                    if event::poll(std::time::Duration::from_millis(50))? {
                        buffer.push(' ');
                        execute!(io::stdout(), crossterm::style::Print(" "))?;
                    } else {
                        break Some(buffer);
                    }
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    break None;
                }
                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                    if buffer.is_empty() { break None; }
                }
                KeyCode::Char(c) => {
                    buffer.push(c);
                    execute!(io::stdout(), crossterm::style::Print(c.to_string()))?;
                }
                KeyCode::Backspace => {
                    if buffer.pop().is_some() {
                        execute!(io::stdout(), crossterm::style::Print("\x08 \x08"))?;
                    }
                }
                _ => {}
            },
            Event::Paste(text) => {
                // Flatten pasted newlines to spaces (single-line prompt)
                let clean = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
                buffer.push_str(&clean);
                execute!(io::stdout(), crossterm::style::Print(&clean))?;
            }
            _ => {}
        }
    };

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), DisableBracketedPaste)?;
    println!(); // newline after input

    Ok(result.map(|s| s.trim().to_string()))
}

/// Parse a network name string into the elisym-core SolanaNetwork enum.
pub(crate) fn parse_network(s: &str) -> SolanaNetwork {
    match s {
        "mainnet" => SolanaNetwork::Mainnet,
        "testnet" => SolanaNetwork::Testnet,
        "devnet" => SolanaNetwork::Devnet,
        other => SolanaNetwork::Custom(other.to_string()),
    }
}

/// Resolve a Solana RPC URL from the network name, using a custom URL if provided.
pub(crate) fn resolve_rpc_url(network: &str, custom: Option<&str>) -> String {
    if let Some(url) = custom {
        return url.to_string();
    }
    parse_network(network).rpc_url()
}

/// Extract the payment chain from a discovered agent's metadata.
pub(crate) fn extract_chain(agent: &DiscoveredAgent) -> String {
    agent
        .card
        .metadata
        .as_ref()
        .and_then(|m| m["chain"].as_str())
        .unwrap_or("solana")
        .to_string()
}

/// Extract the payment network from a discovered agent's metadata.
pub(crate) fn extract_network(agent: &DiscoveredAgent) -> String {
    agent
        .card
        .metadata
        .as_ref()
        .and_then(|m| m["network"].as_str())
        .unwrap_or("devnet")
        .to_string()
}

/// Extract job price from a DiscoveredAgent's card metadata.
pub(crate) fn extract_job_price(agent: &elisym_core::DiscoveredAgent) -> u64 {
    agent
        .card
        .metadata
        .as_ref()
        .and_then(|m| m["job_price"].as_u64())
        .unwrap_or(0)
}

/// Prompt for password and decrypt secrets if the config is encrypted.
fn unlock_config(cfg: &mut config::AgentConfig) -> Result<()> {
    if !cfg.is_encrypted() {
        return Ok(());
    }
    let max_attempts = 3;
    for attempt in 1..=max_attempts {
        let password = Zeroizing::new(Password::new()
            .with_prompt("Password")
            .interact()?);
        match cfg.decrypt_secrets(&password) {
            Ok(()) => return Ok(()),
            Err(e) if attempt == max_attempts => return Err(e),
            Err(_) => {
                println!(
                    "  {} Wrong password ({}/{})",
                    style("!").yellow(),
                    attempt,
                    max_attempts,
                );
                // Throttle brute-force attempts
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
    unreachable!()
}

/// Save config, re-encrypting secrets if a password is provided.
/// Encrypts → saves → decrypts back so the in-memory config stays usable.
fn save_config_encrypted(cfg: &mut config::AgentConfig, password: &Option<Zeroizing<String>>) -> Result<()> {
    if let Some(ref p) = password {
        cfg.encrypt_secrets(p)?;
        config::save_config(cfg)?;
        cfg.decrypt_secrets(p)?;
    } else {
        config::save_config(cfg)?;
    }
    Ok(())
}