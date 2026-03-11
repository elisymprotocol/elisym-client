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

    /// Send SOL to an address
    Send {
        /// Agent name
        name: String,
        /// Destination Solana address
        address: String,
        /// Amount to send in SOL (e.g. "0.5")
        amount: String,
    },

}
