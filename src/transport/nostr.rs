use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use elisym_core::types::JobStatus;
use elisym_core::AgentNode;
use nostr_sdk::Timestamp;
use tokio::sync::mpsc;

use crate::cli::error::Result;
use crate::cli::protocol::HeartbeatMessage;
use crate::tui::AppEvent;

use super::{IncomingJob, JobFeedbackStatus, Transport, TransportRaw};

pub struct NostrTransport {
    agent: Arc<AgentNode>,
    kind_offsets: Vec<u16>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    delivery_retries: u32,
}

impl NostrTransport {
    pub fn new(
        agent: Arc<AgentNode>,
        kind_offsets: Vec<u16>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
        delivery_retries: u32,
    ) -> Self {
        Self {
            agent,
            kind_offsets,
            event_tx,
            delivery_retries,
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
        let etx_ping = self.event_tx.clone();
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
                    let sender_str = msg.sender.to_string();
                    let _ = etx_ping.send(AppEvent::Ping {
                        from: sender_str,
                    });
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

    async fn deliver_result(&self, job: &IncomingJob, result: &str, amount: Option<u64>) -> Result<nostr_sdk::EventId> {
        let raw_event = match &job.raw {
            TransportRaw::Nostr { job_request } => &job_request.raw_event,
        };

        let max_attempts = self.delivery_retries.max(1);
        let mut last_err = None;
        for attempt in 0..max_attempts {
            match self
                .agent
                .marketplace
                .submit_job_result(raw_event, result, amount)
                .await
            {
                Ok(result_id) => {
                    if let Some(solana) = self.agent.solana_payments() {
                        if let Ok(balance) = solana.balance() {
                            let _ = self.event_tx.send(AppEvent::WalletBalance(balance));
                        }
                    }
                    return Ok(result_id);
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 < max_attempts {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
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
