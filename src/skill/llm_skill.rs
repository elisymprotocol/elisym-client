use std::collections::HashMap;

use async_trait::async_trait;

use crate::cli::error::{CliError, Result};
use super::{Skill, SkillContext, SkillInput, SkillOutput};

pub struct LlmSkill {
    capabilities: Vec<String>,
    capability_prompts: HashMap<String, String>,
}

impl LlmSkill {
    pub fn new(capabilities: Vec<String>, capability_prompts: HashMap<String, String>) -> Self {
        Self {
            capabilities,
            capability_prompts,
        }
    }

    fn build_system_prompt(&self, ctx: &SkillContext) -> String {
        let mut prompt = format!(
            "You are {}, an AI agent on the elisym protocol.\n\
             Description: {}\n\n",
            ctx.agent_name, ctx.agent_description
        );

        for cap in &self.capabilities {
            if let Some(cap_prompt) = self.capability_prompts.get(cap) {
                prompt.push_str(&format!("[{}]: {}\n\n", cap, cap_prompt));
            }
        }

        prompt.push_str(
            "IMPORTANT: You are a job-processing agent, NOT an interactive chatbot.\n\
             You receive a single request and must return a complete, ready-to-use result.\n\
             Do NOT ask follow-up questions, offer menus, or suggest options.\n\
             Do NOT use emojis or conversational filler.\n\
             Just do what is asked and return the result directly.\n\n\
             If the request is vague, general, or exploratory (e.g. \"I'm looking for X\" or \
             \"teach me about Y\"), DO NOT explain what you can do — instead, immediately \
             demonstrate your capabilities by providing a useful, substantive response. \
             Pick a concrete example from your domain and deliver real value. \
             The customer has already paid for this job, so always deliver content, never a menu.",
        );
        prompt
    }
}

#[async_trait]
impl Skill for LlmSkill {
    fn name(&self) -> &str {
        "llm"
    }

    fn description(&self) -> &str {
        "LLM-powered skill that processes jobs using configured language model"
    }

    fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    async fn execute(&self, input: SkillInput, ctx: &SkillContext) -> Result<SkillOutput> {
        let llm = ctx
            .llm
            .as_ref()
            .ok_or_else(|| CliError::Llm("no LLM configured".into()))?;

        let system_prompt = self.build_system_prompt(ctx);
        let result = llm.complete(&system_prompt, &input.data).await?;

        Ok(SkillOutput {
            data: result,
            output_mime: None,
        })
    }
}
