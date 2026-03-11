use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use elisym_core::types::JobStatus;
use elisym_core::AgentNode;
use nostr_sdk::Timestamp;
use tokio::sync::mpsc;
use console::style;

use crate::cli::error::Result;
use crate::cli::protocol::HeartbeatMessage;

use super::{IncomingJob, JobFeedbackStatus, Transport, TransportRaw};

pub struct NostrTransport {
    agent: Arc<AgentNode>,
    kind_offsets: Vec<u16>,
}

impl NostrTransport {
    pub fn new(agent: Arc<AgentNode>, kind_offsets: Vec<u16>) -> Self {
        Self {
            agent,
            kind_offsets,
        }
    }
}

#[async_trait]
impl Transport for NostrTransport {
    async fn start(&self) -> Result<mpsc::Receiver<IncomingJob>> {
        let mut jobs_rx = self
            .agent
            .marketplace
            .subscribe_to_job_requests(&self.kind_offsets)
            .await?;

        let mut messages_rx = self.agent.messaging.subscribe_to_messages().await?;
        let started_at = Timestamp::now();

        let (tx, rx) = mpsc::channel(64);

        // Spawn ping/pong handler
        let agent_ping = Arc::clone(&self.agent);
        tokio::spawn(async move {
            while let Some(msg) = messages_rx.recv().await {
                if msg.timestamp < started_at {
                    continue;
                }
                let heartbeat: HeartbeatMessage = match serde_json::from_str(&msg.content) {
                    Ok(hb) => hb,
                    Err(_) => continue,
                };
                if heartbeat.is_ping() {
                    let short_sender = &msg.sender.to_string()[..12.min(msg.sender.to_string().len())];
                    println!("  {} Ping from {}... — pong sent",
                        style("↔").dim(),
                        style(short_sender).dim(),
                    );
                    let pong = HeartbeatMessage::pong(heartbeat.nonce);
                    let _ = agent_ping
                        .messaging
                        .send_structured_message(&msg.sender, &pong)
                        .await;
                }
            }
        });

        // Spawn job forwarding
        tokio::spawn(async move {
            while let Some(job) = jobs_rx.recv().await {
                let incoming = IncomingJob {
                    job_id: job.event_id.to_string(),
                    input: job.input_data.clone(),
                    input_type: "text".into(),
                    tags: vec![],
                    customer_id: job.customer.to_string(),
                    bid: job.bid,
                    raw: TransportRaw::Nostr { job_request: job },
                };
                if tx.send(incoming).await.is_err() {
                    break;
                }
            }
        });

        Ok(rx)
    }

    async fn send_feedback(&self, job: &IncomingJob, status: JobFeedbackStatus) -> Result<()> {
        let raw_event = match &job.raw {
            TransportRaw::Nostr { job_request } => &job_request.raw_event,
        };

        match status {
            JobFeedbackStatus::PaymentRequired {
                amount,
                payment_request,
                chain,
            } => {
                self.agent
                    .marketplace
                    .submit_job_feedback(
                        raw_event,
                        JobStatus::PaymentRequired,
                        None,
                        Some(amount),
                        Some(&payment_request),
                        Some(&chain),
                    )
                    .await?;
            }
            JobFeedbackStatus::Processing => {
                self.agent
                    .marketplace
                    .submit_job_feedback(
                        raw_event,
                        JobStatus::Processing,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await?;
            }
            JobFeedbackStatus::Error(msg) => {
                self.agent
                    .marketplace
                    .submit_job_feedback(
                        raw_event,
                        JobStatus::Error,
                        Some(&msg),
                        None,
                        None,
                        None,
                    )
                    .await?;
            }
        }

        Ok(())
    }

    async fn deliver_result(&self, job: &IncomingJob, result: &str, amount: Option<u64>) -> Result<()> {
        let raw_event = match &job.raw {
            TransportRaw::Nostr { job_request } => &job_request.raw_event,
        };

        let mut last_err = None;
        for attempt in 0..3 {
            match self
                .agent
                .marketplace
                .submit_job_result(raw_event, result, amount)
                .await
            {
                Ok(_result_id) => {
                    if let Some(solana) = self.agent.solana_payments() {
                        if let Ok(balance) = solana.balance() {
                            println!("     {} Wallet: {} SOL",
                                style("$").dim(),
                                style(crate::util::format_sol_compact(balance)).dim(),
                            );
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    if attempt < 2 {
                        eprintln!("     {} Delivery retry {}/3...",
                            style("↻").yellow(),
                            attempt + 1,
                        );
                    }
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
            }
        }

        let err = last_err.unwrap();
        self.agent
            .marketplace
            .submit_job_feedback(
                raw_event,
                JobStatus::Error,
                Some(&format!("delivery failed: {}", err)),
                None,
                None,
                None,
            )
            .await?;

        Err(err.into())
    }
}
