use std::str::FromStr;

use reqwest::Client;
use serde_json::{json, Value};

use zeroize::Zeroizing;

use super::config::LlmSection;
use super::error::{CliError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProvider {
    Anthropic,
    OpenAi,
}

impl FromStr for LlmProvider {
    type Err = CliError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAi),
            other => Err(CliError::Llm(format!("unknown LLM provider: {}", other))),
        }
    }
}

/// A tool definition for LLM tool-use.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Vec<ToolParam>,
}

#[derive(Debug, Clone)]
pub struct ToolParam {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Result of an LLM completion that may include tool calls.
pub enum CompletionResult {
    /// LLM returned final text.
    Text(String),
    /// LLM wants to call tools. Includes the raw assistant message for continuation.
    ToolUse {
        calls: Vec<ToolCall>,
        assistant_message: Value,
    },
}

pub struct LlmClient {
    provider: LlmProvider,
    api_key: Zeroizing<String>,
    model: String,
    max_tokens: u32,
    http: Client,
}

impl LlmClient {
    pub fn new(config: &LlmSection) -> Result<Self> {
        let provider: LlmProvider = config.provider.parse()?;
        Ok(Self {
            provider,
            api_key: Zeroizing::new(config.api_key.clone()),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            http: Client::new(),
        })
    }

    /// Simple completion without tools.
    pub async fn complete(&self, system_prompt: &str, user_input: &str) -> Result<String> {
        match self.provider {
            LlmProvider::Anthropic => self.complete_anthropic(system_prompt, user_input).await,
            LlmProvider::OpenAi => self.complete_openai(system_prompt, user_input).await,
        }
    }

    /// Completion with tools — returns either text or tool calls.
    pub async fn complete_with_tools(
        &self,
        system_prompt: &str,
        messages: &[Value],
        tools: &[ToolDef],
    ) -> Result<CompletionResult> {
        match self.provider {
            LlmProvider::Anthropic => {
                self.complete_anthropic_tools(system_prompt, messages, tools).await
            }
            LlmProvider::OpenAi => {
                self.complete_openai_tools(system_prompt, messages, tools).await
            }
        }
    }

    // ── Anthropic ──────────────────────────────────────────────────

    async fn complete_anthropic(&self, system_prompt: &str, user_input: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system_prompt,
            "messages": [
                { "role": "user", "content": user_input }
            ]
        });

        let json = self.call_anthropic(&body).await?;

        json["content"][0]["text"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| CliError::Llm("unexpected Anthropic response format".into()))
    }

    async fn complete_anthropic_tools(
        &self,
        system_prompt: &str,
        messages: &[Value],
        tools: &[ToolDef],
    ) -> Result<CompletionResult> {
        let api_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                let mut properties = json!({});
                let mut required = vec![];
                for param in &t.parameters {
                    properties[&param.name] = json!({
                        "type": "string",
                        "description": param.description,
                    });
                    if param.required {
                        required.push(param.name.clone());
                    }
                }
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": {
                        "type": "object",
                        "properties": properties,
                        "required": required,
                    }
                })
            })
            .collect();

        let body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system_prompt,
            "messages": messages,
            "tools": api_tools,
        });

        let json = self.call_anthropic(&body).await?;

        // Parse content blocks
        let content = json["content"]
            .as_array()
            .ok_or_else(|| CliError::Llm("missing content in Anthropic response".into()))?;

        let mut tool_calls = vec![];
        let mut text_parts = vec![];

        for block in content {
            match block["type"].as_str() {
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                        arguments: block["input"].clone(),
                    });
                }
                Some("text") => {
                    if let Some(text) = block["text"].as_str() {
                        if !text.is_empty() {
                            text_parts.push(text.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        if tool_calls.is_empty() {
            Ok(CompletionResult::Text(text_parts.join("\n")))
        } else {
            Ok(CompletionResult::ToolUse {
                calls: tool_calls,
                assistant_message: json!({
                    "role": "assistant",
                    "content": json["content"],
                }),
            })
        }
    }

    async fn call_anthropic(&self, body: &Value) -> Result<Value> {
        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(body)
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

        Ok(json)
    }

    // ── OpenAI ─────────────────────────────────────────────────────

    async fn complete_openai(&self, system_prompt: &str, user_input: &str) -> Result<String> {
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

        let mut body = json!({
            "model": self.model,
            "messages": messages
        });
        body[tokens_key] = json!(self.max_tokens);

        let resp = self
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key.as_str()))
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

    async fn complete_openai_tools(
        &self,
        system_prompt: &str,
        messages: &[Value],
        tools: &[ToolDef],
    ) -> Result<CompletionResult> {
        let is_reasoning = self.model.starts_with("o1")
            || self.model.starts_with("o3")
            || self.model.starts_with("o4");

        let tokens_key = if is_reasoning {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };

        let system_role = if is_reasoning { "developer" } else { "system" };

        let api_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                let mut properties = json!({});
                let mut required = vec![];
                for param in &t.parameters {
                    properties[&param.name] = json!({
                        "type": "string",
                        "description": param.description,
                    });
                    if param.required {
                        required.push(param.name.clone());
                    }
                }
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": {
                            "type": "object",
                            "properties": properties,
                            "required": required,
                        }
                    }
                })
            })
            .collect();

        let mut all_messages = vec![json!({ "role": system_role, "content": system_prompt })];
        all_messages.extend_from_slice(messages);

        let mut body = json!({
            "model": self.model,
            "messages": all_messages,
            "tools": api_tools,
        });
        body[tokens_key] = json!(self.max_tokens);

        let resp = self
            .http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key.as_str()))
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

        let message = &json["choices"][0]["message"];
        let tool_calls_arr = message["tool_calls"].as_array();

        if let Some(calls) = tool_calls_arr {
            if !calls.is_empty() {
                let parsed: Vec<ToolCall> = calls
                    .iter()
                    .filter_map(|tc| {
                        let id = tc["id"].as_str()?.to_string();
                        let name = tc["function"]["name"].as_str()?.to_string();
                        let args: Value = serde_json::from_str(
                            tc["function"]["arguments"].as_str().unwrap_or("{}"),
                        )
                        .unwrap_or(json!({}));
                        Some(ToolCall {
                            id,
                            name,
                            arguments: args,
                        })
                    })
                    .collect();

                return Ok(CompletionResult::ToolUse {
                    calls: parsed,
                    assistant_message: json!({
                        "role": "assistant",
                        "content": message["content"],
                        "tool_calls": message["tool_calls"],
                    }),
                });
            }
        }

        let text = message["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        Ok(CompletionResult::Text(text))
    }
}
