use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use console::style;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cli::error::{CliError, Result};
use crate::cli::llm::{CompletionResult, ToolCall, ToolDef, ToolParam};
use super::{Skill, SkillContext, SkillInput, SkillOutput};

/// Maximum tool-use rounds to prevent infinite loops.
const MAX_TOOL_ROUNDS: usize = 10;

/// A tool defined in SKILL.md.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillToolDef {
    pub name: String,
    pub description: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub parameters: Vec<SkillToolParam>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillToolParam {
    pub name: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

pub struct ScriptSkill {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub skill_dir: PathBuf,
    pub system_prompt: String,
    pub tools: Vec<SkillToolDef>,
}

impl ScriptSkill {
    /// Run a tool command with arguments extracted from LLM's tool call.
    async fn run_tool(&self, tool_def: &SkillToolDef, call: &ToolCall) -> Result<String> {
        // Build command: base command + named args (--name value)
        // First required parameter is positional, rest are --name value
        let mut args = tool_def.command.clone();
        let mut first_required = true;
        for param in &tool_def.parameters {
            if let Some(val) = call.arguments.get(&param.name) {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if first_required && param.required {
                    // First required param is positional (e.g. the URL)
                    args.push(s);
                    first_required = false;
                } else {
                    // Optional/subsequent params as --name value
                    args.push(format!("--{}", param.name));
                    args.push(s);
                }
            }
        }

        println!("     {} Running tool {}",
            style("→").dim(),
            style(&call.name).cyan(),
        );

        let child = tokio::process::Command::new(&args[0])
            .args(&args[1..])
            .current_dir(&self.skill_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CliError::Other(format!("failed to spawn tool '{}': {}", call.name, e)))?;

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| CliError::Other(format!("tool '{}' failed: {}", call.name, e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("     {} Tool {} failed: {}",
                style("✗").red(),
                style(&call.name).red(),
                stderr.trim(),
            );
            return Ok(format!("Error: tool exited with {}: {}", output.status, stderr.trim()));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        println!("     {} Tool {} done ({} chars)",
            style("←").dim(),
            style(&call.name).cyan(),
            stdout.len(),
        );
        Ok(stdout)
    }

    /// Find the tool definition by name.
    fn find_tool(&self, name: &str) -> Option<&SkillToolDef> {
        self.tools.iter().find(|t| t.name == name)
    }

    /// Convert skill tool definitions to LLM tool definitions.
    fn llm_tools(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t
                    .parameters
                    .iter()
                    .map(|p| ToolParam {
                        name: p.name.clone(),
                        description: p.description.clone(),
                        required: p.required,
                    })
                    .collect(),
            })
            .collect()
    }
}

#[async_trait]
impl Skill for ScriptSkill {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    async fn execute(&self, input: SkillInput, ctx: &SkillContext) -> Result<SkillOutput> {
        let llm = ctx
            .llm
            .as_ref()
            .ok_or_else(|| CliError::Llm("no LLM configured for skill".into()))?;

        let tools = self.llm_tools();

        // If no tools defined, just do a simple LLM call
        if tools.is_empty() {
            println!("     {} Calling LLM (no tools)", style("⚙").dim());
            let result = llm.complete(&self.system_prompt, &input.data).await?;
            return Ok(SkillOutput {
                data: result,
                output_mime: None,
            });
        }

        // Tool-use loop
        let mut messages: Vec<Value> = vec![json!({
            "role": "user",
            "content": input.data,
        })];

        for round in 0..MAX_TOOL_ROUNDS {
            println!("     {} LLM round {}/{}",
                style("⚙").dim(),
                round + 1,
                MAX_TOOL_ROUNDS,
            );

            let result = llm
                .complete_with_tools(&self.system_prompt, &messages, &tools)
                .await?;

            match result {
                CompletionResult::Text(text) => {
                    return Ok(SkillOutput {
                        data: text,
                        output_mime: None,
                    });
                }
                CompletionResult::ToolUse {
                    calls,
                    assistant_message,
                } => {
                    println!("     {} LLM wants {} tool call(s)",
                        style("⚙").dim(),
                        calls.len(),
                    );

                    // Add assistant message to conversation
                    messages.push(assistant_message);

                    // Execute each tool and add results
                    for call in &calls {
                        let tool_result = match self.find_tool(&call.name) {
                            Some(tool_def) => self.run_tool(tool_def, call).await?,
                            None => format!("Error: unknown tool '{}'", call.name),
                        };

                        // Anthropic format: tool_result in user message
                        messages.push(json!({
                            "role": "user",
                            "content": [{
                                "type": "tool_result",
                                "tool_use_id": call.id,
                                "content": tool_result,
                            }]
                        }));
                    }
                }
            }
        }

        Err(CliError::Other(format!(
            "skill '{}' exceeded max tool rounds ({})",
            self.name, MAX_TOOL_ROUNDS
        )))
    }
}
