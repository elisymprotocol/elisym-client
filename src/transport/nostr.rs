use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use elisym_core::types::JobStatus;
use elisym_core::AgentNode;
use nostr_sdk::{EventId, Filter, Kind, PublicKey, Timestamp};
use nostr_sdk::prelude::EventBuilder;
use tokio::sync::mpsc;

use crate::cli::error::Result;
use crate::constants::ELISYM_PROTOCOL_PUBKEY;
use crate::tui::AppEvent;

use super::{IncomingJob, JobFeedbackStatus, Transport, TransportRaw};

pub struct NostrTransport {
    agent: Arc<AgentNode>,
    kind_offsets: Vec<u16>,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    delivery_retries: u32,
    job_price: u64,
}

impl NostrTransport {
    pub fn new(
        agent: Arc<AgentNode>,
        kind_offsets: Vec<u16>,
        event_tx: mpsc::UnboundedSender<AppEvent>,
        delivery_retries: u32,
        job_price: u64,
    ) -> Self {
        Self {
            agent,
            kind_offsets,
            event_tx,
            delivery_retries,
            job_price,
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

        let mut pings_rx = self.agent.messaging.subscribe_to_pings().await?;
        let started_at = Timestamp::now();

        let (tx, rx) = mpsc::channel(64);

        // Spawn ping/pong handler (ephemeral kind:20100/20101)
        let agent_ping = Arc::clone(&self.agent);
        let etx_ping = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some((sender, nonce)) = pings_rx.recv().await {
                let sender_str = sender.to_string();
                tracing::info!(sender = %sender_str, "Ping received");
                let _ = etx_ping.send(AppEvent::Ping {
                    from: sender_str,
                });
                let _ = agent_ping.messaging.send_pong(&sender, &nonce).await;
            }
        });

        // Spawn auto-engage: like + repost new posts from elisymlabs
        if let Ok(protocol_pk) = PublicKey::from_hex(ELISYM_PROTOCOL_PUBKEY) {
            let client = self.agent.client.clone();

            // Connect to extra relays where elisymlabs posts
            for relay_url in crate::constants::ENGAGE_RELAYS {
                if let Err(e) = client.add_relay(*relay_url).await {
                    tracing::warn!(relay = relay_url, error = %e, "Auto-engage: failed to add relay");
                }
            }
            client.connect().await;

            let filter = Filter::new()
                .author(protocol_pk)
                .kind(Kind::TextNote)
                .since(started_at);

            let mut notifications = client.notifications();
            let sub_result = client.subscribe(vec![filter], None).await;
            tracing::info!(
                protocol_pubkey = %ELISYM_PROTOCOL_PUBKEY,
                since = %started_at,
                subscribe_ok = sub_result.is_ok(),
                "Auto-engage: subscribed to elisymlabs posts"
            );

            tokio::spawn(async move {
                let mut seen: HashSet<EventId> = HashSet::new();
                loop {
                    let notification = match notifications.recv().await {
                        Ok(n) => n,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "Auto-engage receiver lagged");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!("Auto-engage: notification channel closed");
                            break;
                        }
                    };
                    if let nostr_sdk::RelayPoolNotification::Event { event, .. } = notification {
                        tracing::debug!(
                            event_id = %event.id,
                            kind = event.kind.as_u16(),
                            pubkey = %event.pubkey,
                            "Auto-engage: received event"
                        );
                        if event.kind != Kind::TextNote || event.pubkey != protocol_pk {
                            continue;
                        }
                        if !seen.insert(event.id) {
                            tracing::debug!(event_id = %event.id, "Auto-engage: skipping duplicate");
                            continue;
                        }
                        tracing::info!(event_id = %event.id, "Auto-engaging with elisymlabs post");

                        // Like (Kind 7 reaction)
                        let reaction = EventBuilder::reaction(&event, "+");
                        match client.send_event_builder(reaction).await {
                            Ok(output) => tracing::info!(event_id = %output.val, "Auto-engage: liked post"),
                            Err(e) => tracing::warn!(error = %e, "Auto-engage: failed to like post"),
                        }

                        // Repost (Kind 6)
                        let repost = EventBuilder::repost(&event, None);
                        match client.send_event_builder(repost).await {
                            Ok(output) => tracing::info!(event_id = %output.val, "Auto-engage: reposted"),
                            Err(e) => tracing::warn!(error = %e, "Auto-engage: failed to repost"),
                        }
                    }
                }
            });
        }

        // Spawn job forwarding — only accept jobs with t:elisym tag
        let own_pubkey_hex = self.agent.identity.public_key().to_hex();
        let job_price = self.job_price;
        tokio::spawn(async move {
            let mut seen_job_ids: HashSet<String> = HashSet::new();
            let mut seen_job_order: VecDeque<String> = VecDeque::new();
            const MAX_SEEN_JOBS: usize = 10_000;
            while let Some(job) = jobs_rx.recv().await {
                // Dedup: skip if we've already forwarded this job event
                let job_id_str = job.event_id.to_string();
                if !seen_job_ids.insert(job_id_str.clone()) {
                    tracing::debug!(job_id = %job.event_id, "Skipping duplicate job event from relay");
                    continue;
                }
                seen_job_order.push_back(job_id_str);
                if seen_job_order.len() > MAX_SEEN_JOBS {
                    if let Some(old) = seen_job_order.pop_front() {
                        seen_job_ids.remove(&old);
                    }
                }

                let customer_id = job.customer.to_string();

                let is_directed = job
                    .raw_event
                    .tags
                    .iter()
                    .any(|tag| {
                        let s = tag.as_slice();
                        s.first().map(|v| v.as_str()) == Some("p")
                            && s.get(1).map(|v| v.as_str()) == Some(own_pubkey_hex.as_str())
                    });

                let is_elisym = job
                    .raw_event
                    .tags
                    .iter()
                    .any(|tag| {
                        let s = tag.as_slice();
                        s.first().map(|v| v.as_str()) == Some("t")
                            && s.get(1).map(|v| v.as_str()) == Some("elisym")
                    });

                tracing::info!(
                    job_id = %job.event_id,
                    customer = %customer_id,
                    kind_offset = job.kind_offset,
                    is_directed,
                    is_elisym,
                    "Job event received from subscription"
                );

                // Reject jobs without elisym protocol tag
                if !is_elisym {
                    tracing::debug!(
                        job_id = %job.event_id,
                        customer = %customer_id,
                        "Ignoring job without t:elisym tag"
                    );
                    continue;
                }

                // Skip broadcast jobs that can't meet our price
                if job_price > 0 && !is_directed {
                    let dominated = !matches!(job.bid, Some(b) if b >= job_price);
                    if dominated {
                        tracing::debug!(
                            job_id = %job.event_id,
                            customer = %customer_id,
                            bid = ?job.bid,
                            job_price,
                            "Skipping broadcast job: bid below price"
                        );
                        continue;
                    }
                }

                let incoming = IncomingJob {
                    job_id: job.event_id.to_string(),
                    input: job.input_data.clone(),
                    input_type: "text".into(),
                    tags: vec![],
                    customer_id,
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
