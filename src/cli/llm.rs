use reqwest::Client;
use serde_json::{json, Value};

use super::config::LlmSection;
use super::error::{CliError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    Anthropic,
    OpenAi,
}

impl LlmProvider {
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAi),
            other => Err(CliError::Llm(format!("unknown LLM provider: {}", other))),
        }
    }
}

pub struct LlmClient {
    provider: LlmProvider,
    api_key: String,
    model: String,
    max_tokens: u32,
    http: Client,
}

impl LlmClient {
    pub fn new(config: &LlmSection) -> Result<Self> {
        let provider = LlmProvider::from_str(&config.provider)?;
        Ok(Self {
            provider,
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            http: Client::new(),
        })
    }

    pub async fn complete(&self, system_prompt: &str, user_input: &str) -> Result<String> {
        match self.provider {
            LlmProvider::Anthropic => self.complete_anthropic(system_prompt, user_input).await,
            LlmProvider::OpenAi => self.complete_openai(system_prompt, user_input).await,
        }
    }

    async fn complete_anthropic(&self, system_prompt: &str, user_input: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system_prompt,
            "messages": [
                { "role": "user", "content": user_input }
            ]
        });

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let json: Value = resp.json().await?;

        if !status.is_success() {
            let msg = json["error"]["message"]
                .as_str()
                .unwrap_or("unknown API error");
            return Err(CliError::Llm(format!("Anthropic API {}: {}", status, msg)));
        }

        json["content"][0]["text"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| CliError::Llm("unexpected Anthropic response format".into()))
    }

    async fn complete_openai(&self, system_prompt: &str, user_input: &str) -> Result<String> {
        // o1/o3/o4 models require max_completion_tokens instead of max_tokens
        // and use "developer" role instead of "system"
        let is_reasoning = self.model.starts_with("o1")
            || self.model.starts_with("o3")
            || self.model.starts_with("o4");

        let tokens_key = if is_reasoning {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };

        let messages = if is_reasoning {
            json!([
                { "role": "developer", "content": system_prompt },
                { "role": "user", "content": user_input }
            ])
        } else {
            json!([
                { "role": "system", "content": system_prompt },
                { "role": "user", "content": user_input }
            ])
        };

        let body = json!({
            "model": self.model,
            tokens_key: self.max_tokens,
            "messages": messages
        });

        let resp = self
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let json: Value = resp.json().await?;

        if !status.is_success() {
            let msg = json["error"]["message"]
                .as_str()
                .unwrap_or("unknown API error");
            return Err(CliError::Llm(format!("OpenAI API {}: {}", status, msg)));
        }

        json["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| CliError::Llm("unexpected OpenAI response format".into()))
    }
}
