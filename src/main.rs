mod cli;
mod constants;
pub mod runtime;
pub mod skill;
pub mod transport;
pub mod util;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    cli::run().await?;
    Ok(())
}
