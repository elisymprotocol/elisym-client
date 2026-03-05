use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "elisym", version, about = "elisym protocol — AI agent runner")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new agent via interactive wizard
    Init,

    /// Start an agent (interactive selection if no name given)
    Start {
        /// Agent name to start directly
        name: Option<String>,
        /// Free mode: skip payments, process jobs for free (for testing)
        #[arg(long)]
        free: bool,
    },

    /// List all configured agents
    List,

    /// Show agent configuration details
    Status {
        /// Agent name
        name: String,
    },

    /// Delete an agent and its data
    Delete {
        /// Agent name
        name: String,
    },

    /// Edit agent configuration
    Config {
        /// Agent name
        name: String,
    },

    /// Show Solana wallet info (address, balance)
    Wallet {
        /// Agent name
        name: String,
    },

    /// Request devnet/testnet SOL airdrop
    Airdrop {
        /// Agent name
        name: String,
        /// Amount of SOL to airdrop
        #[arg(long, default_value = "1.0")]
        amount: f64,
    },

    /// Send SOL or tokens to an address
    Send {
        /// Agent name
        name: String,
        /// Destination Solana address
        address: String,
        /// Amount to send (in SOL or USDC depending on agent config)
        amount: f64,
    },

    /// Launch live protocol dashboard (global observer mode)
    Dashboard {
        /// Network to observe: mainnet (default), devnet, testnet
        #[arg(default_value = "mainnet")]
        network: String,
        /// Payment chain to observe (solana, lightning)
        #[arg(long, default_value = "solana")]
        chain: String,
        /// Custom Solana RPC URL (overrides network default)
        #[arg(long)]
        rpc_url: Option<String>,
    },
}
