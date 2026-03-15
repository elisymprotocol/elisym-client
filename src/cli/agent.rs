use elisym_core::{
    AgentNode, AgentNodeBuilder,
    SolanaPaymentConfig, SolanaPaymentProvider,
};
use nostr_sdk::{Contact, EventBuilder, PublicKey};

use super::config::AgentConfig;
use super::error::Result;

/// Validate that the provider's net amount (price minus protocol fee) is above
/// Solana's rent-exempt minimum. Returns an error message if invalid, None if OK.
pub fn validate_job_price(lamports: u64) -> Option<String> {
    crate::constants::validate_job_price(lamports)
}

/// Build a SolanaPaymentProvider directly from config (no relay connections needed).
/// Use this for wallet-only operations: send, balance checks.
pub fn build_solana_provider(config: &AgentConfig) -> Result<SolanaPaymentProvider> {
    let network = crate::util::parse_network(&config.payment.network);

    let solana_config = SolanaPaymentConfig {
        network,
        rpc_url: config.payment.rpc_url.clone(),
    };

    let provider = SolanaPaymentProvider::from_secret_key(
        solana_config,
        &config.payment.solana_secret_key,
    )?;

    Ok(provider)
}

/// Build an AgentNode from a persisted config (connects to relays).
/// Use this only when relay connectivity is needed: start, job processing.
pub async fn build_agent(config: &AgentConfig) -> Result<AgentNode> {
    let provider = build_solana_provider(config)?;

    let mut agent = AgentNodeBuilder::new(&config.name, &config.description)
        .capabilities(config.capabilities.clone())
        .relays(config.relays.clone())
        .supported_job_kinds(vec![elisym_core::KIND_JOB_REQUEST_BASE + elisym_core::DEFAULT_KIND_OFFSET])
        .secret_key(&config.secret_key)
        .solana_payment_provider(provider)
        .build()
        .await?;

    // Set payment info on capability card
    if let Some(solana) = agent.solana_payments() {
        agent.capability_card.set_payment(elisym_core::PaymentInfo {
            chain: config.payment.chain.clone(),
            network: config.payment.network.clone(),
            address: solana.address(),
            job_price: Some(config.payment.job_price),
        });
    }
    agent
        .discovery
        .publish_capability(&agent.capability_card, &[elisym_core::KIND_JOB_REQUEST_BASE + elisym_core::DEFAULT_KIND_OFFSET])
        .await?;

    // Auto-follow the elisymprotocol account
    if let Ok(protocol_pk) = PublicKey::from_hex(crate::constants::ELISYM_PROTOCOL_PUBKEY) {
        let contacts = vec![Contact::new::<String>(protocol_pk, None, None)];
        let builder = EventBuilder::contact_list(contacts);
        let _ = agent.client.send_event_builder(builder).await;
    }

    Ok(agent)
}
