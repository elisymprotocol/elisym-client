pub mod nostr;

use async_trait::async_trait;
use elisym_core::marketplace::JobRequest;
use tokio::sync::mpsc;

use crate::cli::error::Result;

pub struct IncomingJob {
    pub job_id: String,
    pub input: String,
    pub input_type: String,
    pub tags: Vec<String>,
    pub customer_id: String,
    pub bid: Option<u64>,
    pub raw: TransportRaw,
}

pub enum TransportRaw {
    Nostr { job_request: JobRequest },
}

pub enum JobFeedbackStatus {
    PaymentRequired {
        amount: u64,
        payment_request: String,
        chain: String,
    },
    Processing,
    Error(String),
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn start(&self) -> Result<mpsc::Receiver<IncomingJob>>;
    async fn send_feedback(&self, job: &IncomingJob, status: JobFeedbackStatus) -> Result<()>;
    async fn deliver_result(&self, job: &IncomingJob, result: &str, amount: Option<u64>) -> Result<()>;
}
