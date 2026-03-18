mod agent;
mod args;
mod banner;
pub(crate) mod config;
pub(crate) mod crypto;
pub(crate) mod error;
pub(crate) mod global_config;
pub(crate) mod llm;
pub(crate) mod protocol;


use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Parser;
use console::style;
use dialoguer::{Confirm, Input, Password, Select};
use nostr_sdk::{Keys, ToBech32};
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use zeroize::Zeroizing;

use self::args::{Cli, Commands};
use self::config::{AgentConfig, LlmSection, PaymentSection};
use self::error::{CliError, Result};

use crate::util::{format_sol, format_sol_compact, sol_to_lamports};

/// Initialize tracing subscriber with a reloadable filter.
/// stderr layer can be silenced when TUI is active.
/// When `file_log` is true, also writes to ~/.elisym/agent.log (survives TUI silence).
fn init_tracing(file_log: bool) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::reload;

    let base_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("nostr_relay_pool=off".parse().unwrap());

    let (filter_layer, handle) = reload::Layer::new(base_filter);

    // stderr layer with reloadable filter (can be silenced by TUI)
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_filter(filter_layer);

    // File-based log layer (--verbose / -v)
    let file_layer = if file_log {
        let log_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".elisym");
        let _ = std::fs::create_dir_all(&log_dir);
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("agent.log"))
            .ok();
        log_file.map(|f| {
            tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(f))
                .with_ansi(false)
                .with_filter(
                    EnvFilter::new("elisym_core=debug,elisym=debug,elisym_client=debug")
                        .add_directive("nostr_relay_pool=off".parse().unwrap()),
                )
        })
    } else {
        None
    };

    let subscriber = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(file_layer);

    let _ = tracing::subscriber::set_global_default(subscriber);

    // Store the handle for later use
    TRACING_RELOAD_HANDLE.get_or_init(|| std::sync::Mutex::new(handle));
}

static TRACING_RELOAD_HANDLE: std::sync::OnceLock<
    std::sync::Mutex<tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>>
> = std::sync::OnceLock::new();

/// Silence all tracing output (call before TUI starts).
fn silence_tracing() {
    if let Some(handle) = TRACING_RELOAD_HANDLE.get() {
        if let Ok(handle) = handle.lock() {
            let _ = handle.reload(tracing_subscriber::EnvFilter::new("off"));
        }
    }
}

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
    let cli = Cli::parse();

    let file_log = matches!(&cli.command, Some(Commands::Start { log: true, .. }));
    init_tracing(file_log);

    match cli.command {
        Some(Commands::Init) => { cmd_init()?; },
        Some(Commands::Start { name, headless, price, .. }) => cmd_start(name, headless, price).await?,
        Some(Commands::List) => cmd_list()?,
        Some(Commands::Status { name }) => cmd_status(&name)?,
        Some(Commands::Delete { name }) => cmd_delete(&name)?,
        Some(Commands::Config { name }) => cmd_config(&name)?,
        Some(Commands::Wallet { name }) => cmd_wallet(&name)?,
        Some(Commands::Send { name, address, amount }) => cmd_send(&name, &address, &amount)?,
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
                    "devnet (default, testing)",
                    "mainnet (coming soon)",
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

                if idx == 1 || idx == 2 {
                    println!("  {}", style("⚠ This network is not available yet.").yellow());
                    continue;
                }

                network = "devnet".to_string();
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

            // Step 8: Password encryption
            8 => {
                println!("  {}", style("Set a password to encrypt secret keys (Nostr, Solana, API keys).").dim());
                println!("  {}", style("Leave empty for no encryption. Type \"back\" to go back.").dim());
                let pw: String = Password::new()
                    .with_prompt("Password (empty = no encryption)")
                    .allow_empty_password(true)
                    .interact()?;

                if pw.eq_ignore_ascii_case("back") {
                    step -= 1;
                    continue;
                }

                if pw.is_empty() {
                    encryption_password = None;
                    println!("  {} Keys will be stored unencrypted.", style("~").dim());
                } else {
                    let pw_confirm: String = Password::new()
                        .with_prompt("Confirm password")
                        .allow_empty_password(true)
                        .interact()?;
                    if pw != pw_confirm {
                        println!("  {} Passwords don't match.", style("!").yellow());
                        continue;
                    }
                    encryption_password = Some(Zeroizing::new(pw));
                    println!("  {} Keys will be encrypted.", style("*").green());
                }
                step += 1;
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
        llm: Some(llm_section),
        customer_llm: None,
        encryption: None,
        recovery: Default::default(),
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
        let main_options = &["Provider settings", "Done"];
        let main_idx = Select::new()
            .with_prompt("Settings")
            .items(main_options)
            .default(0)
            .interact()?;

        match main_idx {
            // Provider settings
            0 => {
                loop {
                    let sub_options = &["Job price", "LLM provider", "\u{2190} Back"];
                    let sub_idx = Select::new()
                        .with_prompt("Provider settings")
                        .items(sub_options)
                        .default(0)
                        .interact()?;

                    match sub_idx {
                        // Job price
                        0 => {
                            println!(
                                "  {} Current price: {} SOL",
                                style("~").dim(),
                                format_sol(cfg.payment.job_price)
                            );
                            let funded = agent::is_account_funded(&cfg);
                            loop {
                                let input: String = Input::new()
                                    .with_prompt("Job price in SOL")
                                    .default(format_sol(cfg.payment.job_price))
                                    .interact_text()?;

                                match sol_to_lamports(&input) {
                                    Some(new_price) => {
                                        if let Some(err) = agent::validate_job_price(new_price, funded) {
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
                        // LLM settings
                        1 => {
                            if let Some(ref llm) = cfg.llm {
                                println!(
                                    "  {} Current: {} / {}",
                                    style("~").dim(),
                                    style(&llm.provider).cyan(),
                                    style(&llm.model).cyan().bold(),
                                );
                                let llm_options = &[
                                    "Change model",
                                    "Change max tokens",
                                    "Change provider + API key",
                                    "\u{2190} Back",
                                ];
                                let llm_idx = Select::new()
                                    .with_prompt("LLM settings")
                                    .items(llm_options)
                                    .default(0)
                                    .interact()?;

                                match llm_idx {
                                    // Change model only
                                    0 => {
                                        let llm = cfg.llm.as_ref().unwrap();
                                        let mut models = fetch_models(&llm.provider, &llm.api_key);
                                        // Put current model first if present
                                        if let Some(pos) = models.iter().position(|m| m == &llm.model) {
                                            models.swap(0, pos);
                                        }
                                        let model_idx = Select::new()
                                            .with_prompt("Model")
                                            .items(&models)
                                            .default(0)
                                            .interact()?;
                                        cfg.llm.as_mut().unwrap().model = models[model_idx].clone();
                                        save_config_encrypted(&mut cfg, &password)?;
                                        println!(
                                            "  {} Model set to {}",
                                            style("*").green(),
                                            style(&cfg.llm.as_ref().unwrap().model).cyan().bold(),
                                        );
                                    }
                                    // Change max tokens
                                    1 => {
                                        let current = cfg.llm.as_ref().unwrap().max_tokens;
                                        let new_tokens: u32 = Input::new()
                                            .with_prompt("Max tokens per LLM response")
                                            .default(current)
                                            .interact_text()?;
                                        cfg.llm.as_mut().unwrap().max_tokens = new_tokens;
                                        save_config_encrypted(&mut cfg, &password)?;
                                        println!("  {} Max tokens set to {}", style("*").green(), new_tokens);
                                    }
                                    // Full reconfigure
                                    2 => {
                                        if let Some(llm) = prompt_llm_section()? {
                                            cfg.llm = Some(llm);
                                            save_config_encrypted(&mut cfg, &password)?;
                                            println!("  {} Provider LLM saved.", style("*").green());
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                // No LLM configured yet — full setup
                                if let Some(llm) = prompt_llm_section()? {
                                    cfg.llm = Some(llm);
                                    save_config_encrypted(&mut cfg, &password)?;
                                    println!("  {} Provider LLM saved.", style("*").green());
                                }
                            }
                        }
                        // Back
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
    println!("  Restart agent to apply changes.");

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

// ── start ─────────────────────────────────────────────────────────────

async fn cmd_start(name: Option<String>, headless: bool, price: Option<String>) -> Result<()> {
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

    // Show wallet status (no relay connection needed)
    let solana = agent::build_solana_provider(&cfg)?;
    display_wallet_status(&solana, &cfg)?;

    let balance = solana.balance().unwrap_or(0);
    let account_funded = balance > 0;
    if balance == 0 && cfg.payment.network != "mainnet" {
        println!(
            "\n  {} Wallet is empty. Get devnet SOL: {}",
            style("!").yellow(),
            style("https://faucet.solana.com").cyan()
        );
    }
    drop(solana);

    println!();

    // ── LLM setup ──────────────────────────────────────────────
    if cfg.llm.is_none() {
        println!("  {} No LLM configured. Let's set one up.\n", style("!").yellow());
        match prompt_llm_section()? {
            Some(llm_section) => {
                cfg.llm = Some(llm_section);
                save_config_encrypted(&mut cfg, &enc_password)?;
                println!("  {} LLM saved.\n", style("*").green());
            }
            None => {
                return Err(CliError::Other("LLM is required to run skills".into()));
            }
        }
    }

    // ── Load skills ────────────────────────────────────────────
    let skills_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("skills");
    let all_skills = crate::skill::loader::load_skills_from_dir(&skills_dir)?;

    if all_skills.is_empty() {
        println!(
            "  {} No skills found in {}\n",
            style("!").yellow(),
            style(skills_dir.display()).dim()
        );
        println!("  Create a skill directory with a SKILL.md to get started.");
        println!("  Example: ./skills/my-skill/SKILL.md\n");
        return Err(CliError::Other("no skills configured".into()));
    }

    // ── Skill selection ────────────────────────────────────────
    let selected_skill = if all_skills.len() == 1 {
        println!("  {} Skill: {} — {}",
            style("*").green(),
            style(all_skills[0].name()).cyan().bold(),
            style(all_skills[0].description()).dim(),
        );
        all_skills.into_iter().next().unwrap()
    } else {
        let skill_names: Vec<String> = all_skills
            .iter()
            .map(|s| format!("{} — {}", s.name(), s.description()))
            .collect();
        let idx = Select::new()
            .with_prompt("Select skill")
            .items(&skill_names)
            .default(0)
            .interact()?;
        println!();
        let mut skills = all_skills;
        skills.swap_remove(idx)
    };

    let mut registry = crate::skill::SkillRegistry::new();
    let skill_name = selected_skill.name().to_string();
    let skill_description = selected_skill.description().to_string();
    let skill_caps: Vec<String> = selected_skill.capabilities().to_vec();
    registry.register(selected_skill);

    // ── Price ──────────────────────────────────────────────────
    if let Some(ref price_str) = price {
        // --price flag: set price without interactive prompt (--price 0 = free mode)
        let lamports = sol_to_lamports(price_str)
            .ok_or_else(|| CliError::Other(format!("invalid --price value: {}", price_str)))?;
        if let Some(err) = agent::validate_job_price(lamports, account_funded) {
            return Err(CliError::Other(err));
        }
        if lamports != cfg.payment.job_price {
            cfg.payment.job_price = lamports;
            save_config_encrypted(&mut cfg, &enc_password)?;
        }
        if cfg.payment.job_price == 0 {
            println!(
                "  {} FREE MODE — payments disabled, jobs processed for free\n",
                style("!").yellow().bold()
            );
        } else {
            println!(
                "  {} Price: {} SOL per job\n",
                style("*").green(),
                style(format_sol(cfg.payment.job_price)).green().bold()
            );
        }
    } else {
        // Interactive price prompt
        println!("  {}", style("How much to charge per job (in SOL). 3% protocol fee is deducted. Enter 0 for free mode.").dim());
        loop {
            let price_input: String = Input::new()
                .with_prompt("Job price in SOL")
                .default(format_sol(cfg.payment.job_price))
                .interact_text()?;

            match sol_to_lamports(&price_input) {
                Some(new_price) => {
                    if let Some(err) = agent::validate_job_price(new_price, account_funded) {
                        println!("  {} {}", style("!").yellow(), err);
                        continue;
                    }
                    if new_price != cfg.payment.job_price {
                        cfg.payment.job_price = new_price;
                        save_config_encrypted(&mut cfg, &enc_password)?;
                    }
                    if cfg.payment.job_price == 0 {
                        println!(
                            "  {} FREE MODE — payments disabled\n",
                            style("!").yellow().bold()
                        );
                    } else {
                        println!(
                            "  {} Price: {} SOL per job\n",
                            style("*").green(),
                            style(format_sol(cfg.payment.job_price)).green().bold()
                        );
                    }
                    break;
                }
                _ => {
                    println!("  {} Invalid amount.", style("!").yellow());
                }
            }
        }
    }

    // Update config description and capabilities from skill
    let mut skill_config_changed = false;
    if skill_description != cfg.description {
        cfg.description = skill_description;
        skill_config_changed = true;
    }
    if skill_caps != cfg.capabilities {
        cfg.capabilities = skill_caps.clone();
        skill_config_changed = true;
    }
    if skill_config_changed {
        save_config_encrypted(&mut cfg, &enc_password)?;
    }

    // Build LLM client
    let llm_client = Arc::new(llm::LlmClient::new(
        cfg.llm.as_ref().expect("LLM already validated above"),
    )?);

    let ctx = crate::skill::SkillContext {
        llm: Some(llm_client),
        agent_name: cfg.name.clone(),
        agent_description: cfg.description.clone(),
        event_tx: None, // will be replaced after event channel is created
    };

    let runtime_config = crate::runtime::RuntimeConfig {
        job_price: cfg.payment.job_price,
        payment_timeout_secs: cfg.payment.payment_timeout_secs,
        max_concurrent_jobs: 10,
        recovery_max_retries: cfg.recovery.max_retries,
        recovery_interval_secs: cfg.recovery.interval_secs,
        network: cfg.payment.network.clone(),
    };

    let agent_node = with_spinner(
        "Connecting to relays and publishing capabilities...",
        agent::build_agent(&cfg),
    ).await?;

    let agent_node = Arc::new(agent_node);

    // Get wallet balance for TUI header
    let wallet_balance = {
        let solana2 = agent::build_solana_provider(&cfg)?;
        solana2.balance().unwrap_or(0)
    };

    let (event_tx, event_rx) = crate::tui::create_event_channel();

    let ledger = Arc::new(tokio::sync::Mutex::new(
        crate::ledger::JobLedger::open(&name)?,
    ));

    let ctx = crate::skill::SkillContext {
        event_tx: Some(event_tx.clone()),
        ..ctx
    };

    let mut runtime = crate::runtime::AgentRuntime::new(
        Arc::clone(&agent_node),
        registry,
        ctx,
        runtime_config,
        event_tx.clone(),
        Arc::clone(&ledger),
    );
    let transport: Box<dyn crate::transport::Transport> = Box::new(
        crate::transport::nostr::NostrTransport::new(
            agent_node,
            vec![elisym_core::DEFAULT_KIND_OFFSET],
            event_tx.clone(),
            cfg.recovery.delivery_retries,
            cfg.payment.job_price,
        ),
    );

    if headless {
        // Headless mode: run runtime directly, logs go to stdout
        println!("  {} Running in headless mode (no TUI)\n", style("*").green());

        // Drain events to log and prevent channel backpressure
        let mut event_rx = event_rx;
        let log_handle = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                log_event(&event);
            }
        });

        let result = runtime.run(transport).await;
        drop(log_handle);
        result?;
    } else {
        // Silence tracing before TUI to prevent log corruption
        silence_tracing();

        // Create retry channel for manual recovery from TUI
        let (retry_tx, retry_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        runtime.set_retry_rx(retry_rx);

        let gc = global_config::load_global_config();
        let mut app = crate::tui::App::new(
            name.clone(),
            skill_name,
            cfg.payment.job_price,
            wallet_balance,
            cfg.payment.network.clone(),
            gc.tui.sound_enabled,
            gc.tui.sound_volume,
        );
        app.set_ledger(Arc::clone(&ledger));
        app.set_retry_tx(retry_tx);

        crate::tui::event::run_tui(app, event_rx, runtime, transport).await?;
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

    match solana.send_transfer(address, base_amount) {
        Ok(signature) => {
            println!(
                "\n  {} Sent {} {}",
                style("*").green(),
                style(format_sol_compact(base_amount)).bold(),
                unit_label
            );
            println!("  Signature: {}", style(&signature).dim());

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

/// Log an AppEvent to stdout in headless mode.
fn log_event(event: &crate::tui::AppEvent) {
    use crate::tui::AppEvent;
    match event {
        AppEvent::JobReceived { job_id, customer_id, .. } => {
            println!("[job] received {} from {}", &job_id[..12.min(job_id.len())], &customer_id[..12.min(customer_id.len())]);
        }
        AppEvent::PaymentRequested { job_id, price, fee } => {
            println!("[job] {} requesting payment: {} lamports (fee {})", &job_id[..12.min(job_id.len())], price, fee);
        }
        AppEvent::PaymentReceived { job_id, net_amount } => {
            println!("[job] {} payment received: {} lamports", &job_id[..12.min(job_id.len())], net_amount);
        }
        AppEvent::PaymentTimeout { job_id } => {
            println!("[job] {} payment timeout", &job_id[..12.min(job_id.len())]);
        }
        AppEvent::SkillStarted { job_id, skill_name } => {
            println!("[job] {} skill started: {}", &job_id[..12.min(job_id.len())], skill_name);
        }
        AppEvent::LlmRound { job_id, round, max_rounds } => {
            println!("[job] {} LLM round {}/{}", &job_id[..12.min(job_id.len())], round, max_rounds);
        }
        AppEvent::ToolStarted { job_id, tool_name } => {
            println!("[job] {} tool started: {}", &job_id[..12.min(job_id.len())], tool_name);
        }
        AppEvent::ToolCompleted { job_id, tool_name, output_len } => {
            println!("[job] {} tool completed: {} ({} bytes)", &job_id[..12.min(job_id.len())], tool_name, output_len);
        }
        AppEvent::ToolFailed { job_id, tool_name, error } => {
            println!("[job] {} tool failed: {} — {}", &job_id[..12.min(job_id.len())], tool_name, error);
        }
        AppEvent::JobCompleted { job_id, result_len } => {
            println!("[job] {} completed ({} bytes)", &job_id[..12.min(job_id.len())], result_len);
        }
        AppEvent::JobFailed { job_id, error } => {
            println!("[job] {} failed: {}", &job_id[..12.min(job_id.len())], error);
        }
        AppEvent::WalletBalance(bal) => {
            println!("[wallet] balance: {} lamports", bal);
        }
        AppEvent::Ping { from } => {
            println!("[ping] from {}", &from[..12.min(from.len())]);
        }
    }
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