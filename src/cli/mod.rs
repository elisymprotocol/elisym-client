mod agent;
mod args;
mod banner;
pub(crate) mod config;
mod customer;
mod dashboard;
pub(crate) mod error;
mod llm;
mod protocol;

use std::collections::HashMap;

use clap::Parser;
use console::style;
use dialoguer::{Confirm, Input, MultiSelect, Select};
use nostr_sdk::{Keys, ToBech32};
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tracing::info;

use self::args::{Cli, Commands};
use self::config::{AgentConfig, LlmSection, PaymentSection};
use self::error::{CliError, Result};

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
        Some(Commands::Init) => cmd_init()?,
        Some(Commands::Start { name, free }) => cmd_start(name, free).await?,
        Some(Commands::List) => cmd_list()?,
        Some(Commands::Status { name }) => cmd_status(&name)?,
        Some(Commands::Delete { name }) => cmd_delete(&name)?,
        Some(Commands::Config { name }) => cmd_config(&name)?,
        Some(Commands::Wallet { name }) => cmd_wallet(&name)?,
        Some(Commands::Airdrop { name, amount }) => cmd_airdrop(&name, amount)?,
        Some(Commands::Send { name, address, amount }) => cmd_send(&name, &address, amount)?,
        Some(Commands::Dashboard { chain, network, rpc_url }) => cmd_dashboard(&chain, &network, rpc_url).await?,
        None => {
            // No subcommand — show banner and help
            print!("{}", banner::BANNER);
            println!("  Run {} to get started.\n", style("elisym init").cyan());
            Cli::parse_from(["elisym", "--help"]);
        }
    }

    Ok(())
}

// ── init ──────────────────────────────────────────────────────────────

fn cmd_init() -> Result<()> {
    print!("{}", banner::BANNER);
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
    let mut job_price: u64 = 0;
    let mut provider = String::new();
    let mut api_key = String::new();
    let mut model = String::new();
    let mut max_tokens: u32 = 4096;

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
                println!("  {}", style("Use devnet for testing (free SOL via airdrop).").dim());
                let options = &[
                    "devnet (default)",
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
                    println!("  {}", style("⚠ This network is not available yet. Please select devnet.").yellow());
                    continue;
                }

                network = "devnet".to_string();
                step += 1;
            }

            // Step 3: RPC URL
            3 => {
                println!("  {}", style("Solana RPC endpoint. The default works fine — change only if you have a custom node.").dim());
                let default_rpc = match network.as_str() {
                    "mainnet" => "https://api.mainnet-beta.solana.com",
                    "testnet" => "https://api.testnet.solana.com",
                    _ => "https://api.devnet.solana.com",
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

            // Step 4: Job price in SOL
            4 => {
                println!("  {}", style("How much your agent charges per task (in SOL). On devnet 0.01 SOL is a good default.").dim());
                let input: String = Input::new()
                    .with_prompt("Job price in SOL, e.g. 0.01 (or \"back\")")
                    .default("0.01".into())
                    .interact_text()?;

                if input.eq_ignore_ascii_case("back") {
                    step -= 1;
                    continue;
                }

                match input.parse::<f64>() {
                    Ok(sol) if sol >= 0.0 => {
                        job_price = (sol * 1_000_000_000.0) as u64;
                        step += 1;
                    }
                    _ => {
                        println!("  {} Invalid amount. Enter a number like 0.01", style("!").yellow());
                    }
                }
            }

            // Step 5: LLM provider
            5 => {
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

            // Step 6: API key
            6 => {
                let label = if provider == "anthropic" {
                    "Anthropic (Claude)"
                } else {
                    "OpenAI (GPT)"
                };
                let input: String = Input::new()
                    .with_prompt(format!("{} API key (or \"back\")", label))
                    .interact_text()?;

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

            // Step 7: Model selection (fetched from API)
            7 => {
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

            // Step 8: Max tokens
            8 => {
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

    let cfg = AgentConfig {
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
            token: "sol".to_string(),
            job_price,
            payment_timeout_secs: 120,
            solana_secret_key,
        },
        inactive_capabilities: vec![],
        capability_prompts: HashMap::new(),
        llm: Some(llm_section),
        customer_llm: None,
    };

    config::save_config(&cfg)?;

    let npub = keys.public_key().to_bech32().unwrap_or_default();
    println!("\n  {} Agent '{}' created!", style("*").green(), style(&name).cyan());
    println!("  npub:    {}", style(&npub).dim());
    println!("  wallet:  {}", style(&solana_address).dim());
    println!("  network: {}", style(&network).dim());
    println!(
        "  price:   {} SOL ({} lamports)",
        style(format_sol(job_price)).dim(),
        style(job_price).dim()
    );
    println!("  config:  {}", style(config::config_path(&name)?.display()).dim());

    if network != "mainnet" {
        println!(
            "\n  Run command to get devnet SOL: {}",
            style(format!("elisym airdrop {}", name)).cyan()
        );
    }
    println!("  Start agent:    {}\n", style(format!("elisym start {}", name)).cyan());

    Ok(())
}

// ── config ────────────────────────────────────────────────────────────

fn cmd_config(name: &str) -> Result<()> {
    let mut cfg = config::load_config(name)?;

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
                    let sub_options = &["Capabilities", "LLM provider", "\u{2190} Back"];
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
                                    println!(
                                        "  {} Capabilities: {}",
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
                                        // Prompts preserved in capability_prompts for both

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
                                        println!(
                                            "  {} Capabilities: {}",
                                            style("*").green(),
                                            cfg.capabilities.join(", ")
                                        );
                                    }
                                    // Back
                                    _ => {}
                                }
                            }
                        }
                        // LLM provider
                        1 => {
                            if let Some(llm) = prompt_llm_section()? {
                                cfg.llm = Some(llm);
                                println!("  {} Provider LLM updated.", style("*").green());
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
                                println!("  {} Customer LLM updated.", style("*").green());
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

    config::save_config(&cfg)?;
    println!(
        "\n  {} Configuration saved for '{}'.",
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
    let api_key: String = Input::new()
        .with_prompt(format!("{} API key", label))
        .interact_text()?;
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

    let description: String = Input::new()
        .with_prompt("Describe what your agent can do (or \"back\")")
        .interact_text()?;

    if description.eq_ignore_ascii_case("back") {
        return Ok(vec![]);
    }

    println!("  {} Analyzing capabilities...", style("~").dim());

    let system = "You help AI agents define their capabilities for a marketplace.\n\
        Given a description of what an agent can do, return a JSON object with:\n\
        - \"capabilities\": array of objects, each with:\n\
          - \"name\": short capability keyword (lowercase, hyphenated, e.g. \"code-generation\")\n\
          - \"prompt\": a detailed system prompt (2-4 sentences) that instructs an AI to excel at this capability\n\
        Return 3-8 capabilities. Return ONLY the JSON, no other text.";

    let max_retries = 3;
    let mut response = String::new();
    for attempt in 0..max_retries {
        match llm.complete(system, &description).await {
            Ok(r) => {
                response = r;
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
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                } else {
                    return Err(e);
                }
            }
        }
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

    print!("{}", banner::BANNER);
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
                style(format!("elisym airdrop {}", name)).cyan()
            );
        }
    }
    drop(solana);

    // Mode selection
    println!("  {}", style("Provider — your agent listens for jobs from the network, completes tasks").dim());
    println!("  {}", style("           using your LLM, and earns SOL for each completed job.").dim());
    println!();
    println!("  {}", style("Customer — you send requests to other agents on the network, they do the").dim());
    println!("  {}", style("           work, and you pay them in SOL.").dim());
    println!();
    let mode_options = &["Provider (listen for jobs)", "Customer (send requests)"];
    let mode_idx = Select::new()
        .with_prompt("Start as")
        .items(mode_options)
        .default(0)
        .interact()?;

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
                    config::save_config(&cfg)?;
                    println!(
                        "  {} Capabilities saved. Publishing to network...\n",
                        style("*").green()
                    );
                }
            }

            println!("  {}", style("Connecting to Nostr relays and publishing your capabilities...").dim());
            println!();
            info!(agent = %name, "building agent node");
            let agent = agent::build_agent(&cfg).await?;
            info!(agent = %name, npub = %agent.identity.npub(), "agent node ready");
            println!();
            println!("  {}", style("Other agents will be able to discover and send jobs to you.").dim());
            println!("  {}", style("Press Ctrl+C to stop.").dim());
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
            let agent = agent::build_agent(&cfg).await?;
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
        cmd_init()?;
        let agents = config::list_agents()?;
        return agents.into_iter().next().ok_or_else(|| CliError::Other("no agent created".into()));
    }

    let mut options: Vec<String> = agents.clone();
    options.push("+ Create new agent".into());

    let idx = Select::new()
        .with_prompt("Select agent to start")
        .items(&options)
        .default(0)
        .interact()?;

    if idx == agents.len() {
        cmd_init()?;
        let updated = config::list_agents()?;
        return updated.into_iter().last().ok_or_else(|| CliError::Other("no agent created".into()));
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
        match config::load_config(name) {
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
    let cfg = config::load_config(name)?;

    println!("{}", style(format!("Agent: {}", cfg.name)).bold());
    println!("  description:  {}", cfg.description);
    println!("  capabilities: {}", cfg.capabilities.join(", "));
    println!("  relays:       {}", cfg.relays.join(", "));
    println!("  chain:        {}", cfg.payment.chain);
    println!("  network:      {}", cfg.payment.network);
    println!("  token:        {}", cfg.payment.token);
    if cfg.payment.token == "sol" {
        println!(
            "  job price:    {} SOL ({} lamports)",
            format_sol(cfg.payment.job_price),
            cfg.payment.job_price
        );
    } else {
        println!(
            "  job price:    {} USDC ({} base units)",
            format_usdc(cfg.payment.job_price),
            cfg.payment.job_price
        );
    }
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
    let cfg = config::load_config(name)?;

    print!("{}", banner::BANNER);
    println!(
        "  Wallet for agent {}\n",
        style(name).cyan().bold()
    );

    let solana = agent::build_solana_provider(&cfg)?;
    display_wallet_status(&solana, &cfg)?;

    Ok(())
}

// ── airdrop ──────────────────────────────────────────────────────────

fn cmd_airdrop(name: &str, amount: f64) -> Result<()> {
    let cfg = config::load_config(name)?;

    if cfg.payment.network == "mainnet" {
        return Err(CliError::Other("airdrop is only available on devnet/testnet".into()));
    }

    let solana = agent::build_solana_provider(&cfg)?;

    let lamports = (amount * 1_000_000_000.0) as u64;
    println!(
        "  Requesting airdrop of {} SOL ({} lamports) on {}...",
        amount, lamports, cfg.payment.network
    );

    let sig = solana.request_airdrop(lamports)?;
    println!("  {} Airdrop requested!", style("*").green());
    println!("  Signature: {}", style(&sig).dim());

    // Brief pause then show balance
    std::thread::sleep(std::time::Duration::from_secs(2));

    let balance = solana.balance().unwrap_or(0);
    println!(
        "  Balance:   {} SOL ({} lamports)",
        format_sol(balance),
        balance
    );

    Ok(())
}

// ── send ─────────────────────────────────────────────────────────────

fn cmd_send(name: &str, address: &str, amount: f64) -> Result<()> {
    let cfg = config::load_config(name)?;

    let solana = agent::build_solana_provider(&cfg)?;

    // Convert amount to base units
    let (base_amount, unit_label) = match cfg.payment.token.as_str() {
        "usdc" => {
            let base = (amount * 1_000_000.0) as u64; // 6 decimals
            (base, "USDC")
        }
        _ => {
            let base = (amount * 1_000_000_000.0) as u64; // 9 decimals (lamports)
            (base, "SOL")
        }
    };

    // Show current balance
    let balance = solana.balance().unwrap_or(0);
    println!("  Balance: {} SOL ({} lamports)", format_sol(balance), balance);
    println!(
        "  Sending {} {} to {}",
        style(amount).bold(),
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

    // Construct the request JSON matching SolanaPaymentRequestData format
    let mint_info = match cfg.payment.token.as_str() {
        "usdc" => {
            let mint = match cfg.payment.network.as_str() {
                "mainnet" => elisym_core::USDC_MINT_MAINNET,
                _ => elisym_core::USDC_MINT_DEVNET,
            };
            format!(r#","mint":"{}","decimals":6"#, mint)
        }
        _ => String::new(),
    };

    // Use a dummy reference key (not needed for direct sends, but required by the format)
    let reference = Keypair::new().pubkey().to_string();
    let request_json = format!(
        r#"{{"recipient":"{}","amount":{},"reference":"{}"{}}}"#,
        address, base_amount, reference, mint_info
    );

    use elisym_core::PaymentProvider;
    match solana.pay(&request_json) {
        Ok(result) => {
            println!(
                "\n  {} Sent {} {}",
                style("*").green(),
                style(amount).bold(),
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

    println!("\n  {}", style("Solana Wallet").bold().underlined());
    println!("  Network:  {}", style(&cfg.payment.network).dim());
    println!("  Token:    {}", style(&cfg.payment.token).dim());
    println!("  Address:  {}", style(&address).dim());
    println!(
        "  Balance:  {} SOL ({} lamports)",
        style(format_sol(balance)).green(),
        balance
    );

    if cfg.payment.token == "usdc" {
        let token_balance = solana.token_balance().unwrap_or(0);
        println!(
            "  USDC:     {} ({} base units)",
            style(format_usdc(token_balance)).green(),
            token_balance
        );
    }

    Ok(())
}

// ── dashboard ─────────────────────────────────────────────────────

async fn cmd_dashboard(chain: &str, network: &str, rpc_url: Option<String>) -> Result<()> {
    dashboard::run_dashboard(chain.to_string(), network.to_string(), rpc_url).await
}

fn format_sol(lamports: u64) -> String {
    format!("{:.9}", lamports as f64 / 1_000_000_000.0)
}

fn format_usdc(base_units: u64) -> String {
    format!("{:.6}", base_units as f64 / 1_000_000.0)
}
