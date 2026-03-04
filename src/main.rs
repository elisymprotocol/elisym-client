mod agent;
mod banner;
mod cli;
mod config;
mod customer;
mod dashboard;
mod error;
mod llm;
mod protocol;

use clap::Parser;
use console::style;
use dialoguer::{Confirm, Input, MultiSelect, Select};
use nostr_sdk::{Keys, ToBech32};
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use tracing::info;

use crate::cli::{Cli, Commands};
use crate::config::{AgentConfig, LlmSection, PaymentSection};
use crate::error::{CliError, Result};

#[tokio::main]
async fn main() -> Result<()> {
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
        Some(Commands::Wallet { name }) => cmd_wallet(&name)?,
        Some(Commands::Airdrop { name, amount }) => cmd_airdrop(&name, amount)?,
        Some(Commands::Send { name, address, amount }) => cmd_send(&name, &address, amount)?,
        None => {
            // No subcommand — show banner and help
            print!("{}", banner::BANNER);
            println!("  Run {} to get started.\n", style("elisym-cli init").cyan());
            Cli::parse_from(["elisym-cli", "--help"]);
        }
    }

    Ok(())
}

// ── init ──────────────────────────────────────────────────────────────

fn cmd_init() -> Result<()> {
    print!("{}", banner::BANNER);
    println!("  {}\n", style("Create a new agent").bold());

    let name: String = Input::new()
        .with_prompt("Agent name")
        .interact_text()?;

    if name.is_empty() {
        return Err(CliError::Other("name cannot be empty".into()));
    }

    // Check if agent already exists
    if config::config_path(&name)?.exists() {
        return Err(CliError::Other(format!("agent '{}' already exists", name)));
    }

    let description: String = Input::new()
        .with_prompt("Description")
        .default("An elisym AI agent".into())
        .interact_text()?;

    let cap_options = &[
        "summarization",
        "translation",
        "code-generation",
        "image-generation",
        "data-analysis",
        "research",
    ];
    let cap_selections = MultiSelect::new()
        .with_prompt("Capabilities (space to select, enter to confirm)")
        .items(cap_options)
        .interact()?;
    let capabilities: Vec<String> = if cap_selections.is_empty() {
        vec!["general".to_string()]
    } else {
        cap_selections.iter().map(|&i| cap_options[i].to_string()).collect()
    };

    // Solana network
    let network_options = &["devnet (default)", "mainnet", "testnet"];
    let network_idx = Select::new()
        .with_prompt("Solana network")
        .items(network_options)
        .default(0)
        .interact()?;
    let network = match network_idx {
        1 => "mainnet".to_string(),
        2 => "testnet".to_string(),
        _ => "devnet".to_string(),
    };

    // RPC URL
    let default_rpc = match network.as_str() {
        "mainnet" => "https://api.mainnet-beta.solana.com",
        "testnet" => "https://api.testnet.solana.com",
        _ => "https://api.devnet.solana.com",
    };
    let rpc_url: String = Input::new()
        .with_prompt("RPC URL")
        .default(default_rpc.into())
        .interact_text()?;
    let rpc_url_opt = if rpc_url == default_rpc {
        None
    } else {
        Some(rpc_url)
    };

    // Token
    let token_options = &["SOL (native)", "USDC (SPL)"];
    let token_idx = Select::new()
        .with_prompt("Payment token")
        .items(token_options)
        .default(0)
        .interact()?;
    let token = match token_idx {
        1 => "usdc".to_string(),
        _ => "sol".to_string(),
    };

    // Job price
    let (default_price, price_label) = match token.as_str() {
        "usdc" => (10_000u64, "Job price (USDC base units, 10000 = 0.01 USDC)"),
        _ => (10_000_000u64, "Job price (lamports, 10000000 = 0.01 SOL)"),
    };
    let job_price: u64 = Input::new()
        .with_prompt(price_label)
        .default(default_price)
        .interact_text()?;

    // LLM provider configuration
    let llm_providers = &["Anthropic (Claude)", "OpenAI (GPT)"];
    let llm_idx = Select::new()
        .with_prompt("LLM provider")
        .items(llm_providers)
        .default(0)
        .interact()?;
    let (provider, default_model) = match llm_idx {
        0 => ("anthropic", "claude-sonnet-4-20250514"),
        _ => ("openai", "gpt-4o"),
    };

    let api_key: String = Input::new()
        .with_prompt(format!("{} API key", llm_providers[llm_idx]))
        .interact_text()?;
    if api_key.is_empty() {
        return Err(CliError::Other("API key cannot be empty".into()));
    }

    let model: String = Input::new()
        .with_prompt("Model")
        .default(default_model.into())
        .interact_text()?;

    let max_tokens: u32 = Input::new()
        .with_prompt("Max tokens")
        .default(4096)
        .interact_text()?;

    let llm_section = LlmSection {
        provider: provider.to_string(),
        api_key: api_key.clone(),
        model: model.clone(),
        max_tokens,
    };

    // Customer LLM configuration
    let customer_llm_options = &[
        "Use same as provider LLM (default)",
        "Configure separately",
    ];
    let customer_llm_idx = Select::new()
        .with_prompt("Customer LLM config")
        .items(customer_llm_options)
        .default(0)
        .interact()?;

    let customer_llm = if customer_llm_idx == 1 {
        let cllm_providers = &["Anthropic (Claude)", "OpenAI (GPT)"];
        let cllm_idx = Select::new()
            .with_prompt("Customer LLM provider")
            .items(cllm_providers)
            .default(llm_idx)
            .interact()?;
        let (cprovider, cdefault_model) = match cllm_idx {
            0 => ("anthropic", "claude-sonnet-4-20250514"),
            _ => ("openai", "gpt-4o"),
        };

        let capi_key: String = Input::new()
            .with_prompt(format!("{} API key", cllm_providers[cllm_idx]))
            .default(api_key)
            .interact_text()?;

        let cmodel: String = Input::new()
            .with_prompt("Model")
            .default(if cllm_idx == llm_idx { model } else { cdefault_model.into() })
            .interact_text()?;

        let cmax_tokens: u32 = Input::new()
            .with_prompt("Max tokens")
            .default(max_tokens)
            .interact_text()?;

        Some(LlmSection {
            provider: cprovider.to_string(),
            api_key: capi_key,
            model: cmodel,
            max_tokens: cmax_tokens,
        })
    } else {
        None
    };

    // Generate Nostr keypair
    let keys = Keys::generate();
    let secret_key = keys.secret_key().to_secret_hex();

    // Generate Solana keypair
    let solana_keypair = Keypair::new();
    let solana_secret_key = bs58::encode(solana_keypair.to_bytes()).into_string();
    let solana_address = solana_keypair.pubkey().to_string();

    let cfg = AgentConfig {
        name: name.clone(),
        description,
        capabilities,
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
            token: token.clone(),
            job_price,
            payment_timeout_secs: 120,
            solana_secret_key,
        },
        llm: Some(llm_section),
        customer_llm,
    };

    config::save_config(&cfg)?;

    let npub = keys.public_key().to_bech32().unwrap_or_default();
    println!("\n  {} Agent '{}' created!", style("*").green(), style(&name).cyan());
    println!("  npub:    {}", style(&npub).dim());
    println!("  wallet:  {}", style(&solana_address).dim());
    println!("  network: {} ({})", style(&network).dim(), style(&token).dim());
    println!("  config:  {}", style(config::config_path(&name)?.display()).dim());

    if network != "mainnet" {
        println!(
            "\n  Get devnet SOL: {}",
            style(format!("elisym-cli airdrop {}", name)).cyan()
        );
    }
    println!("  Start agent:    {}\n", style(format!("elisym-cli start {}", name)).cyan());

    Ok(())
}

// ── start ─────────────────────────────────────────────────────────────

async fn cmd_start(name: Option<String>, free: bool) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => select_or_create_agent()?,
    };

    let cfg = config::load_config(&name)?;

    print!("{}", banner::BANNER);
    println!("  Starting agent {}...\n", style(&name).cyan().bold());

    if free {
        println!(
            "  {} FREE MODE — payments disabled, jobs processed for free\n",
            style("!").yellow().bold()
        );
    }

    info!(agent = %name, "building agent node");
    let agent = agent::build_agent(&cfg).await?;

    info!(agent = %name, npub = %agent.identity.npub(), "agent node ready");

    if let Some(solana) = agent.solana_payments() {
        display_wallet_status(solana, &cfg)?;

        if !free {
            let balance = solana.balance().unwrap_or(0);
            if balance == 0 && cfg.payment.network != "mainnet" {
                println!(
                    "\n  {} Wallet is empty. Get devnet SOL: {}",
                    style("!").yellow(),
                    style(format!("elisym-cli airdrop {}", name)).cyan()
                );
            }
        }
    }

    // Mode selection
    let mode_options = &["Provider (listen for jobs)", "Customer (send requests)"];
    let mode_idx = Select::new()
        .with_prompt("Start as")
        .items(mode_options)
        .default(0)
        .interact()?;

    println!();

    match mode_idx {
        0 => {
            agent::run_agent(agent, &cfg, free).await?;
        }
        _ => {
            if free {
                println!(
                    "  {} --free flag is ignored in customer mode\n",
                    style("!").yellow()
                );
            }
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
        println!("No agents configured. Run {} to create one.", style("elisym-cli init").cyan());
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
    println!(
        "  job price:    {} {}",
        cfg.payment.job_price,
        if cfg.payment.token == "sol" { "lamports" } else { "base units" }
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

fn format_sol(lamports: u64) -> String {
    format!("{:.9}", lamports as f64 / 1_000_000_000.0)
}

fn format_usdc(base_units: u64) -> String {
    format!("{:.6}", base_units as f64 / 1_000_000.0)
}
