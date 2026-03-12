use std::path::Path;
use std::sync::Arc;

use console::style;
use serde::Deserialize;

use crate::cli::error::{CliError, Result};
use super::{Skill, SkillRegistry};
use super::script_skill::{ScriptSkill, SkillToolDef};

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    tools: Vec<SkillToolDef>,
    #[serde(default)]
    max_tool_rounds: Option<usize>,
}

/// Parse a SKILL.md file: TOML frontmatter between `---` lines, markdown body = system prompt.
fn parse_skill_md(content: &str) -> Result<(SkillFrontmatter, String)> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return Err(CliError::Other(
            "SKILL.md must start with --- (TOML frontmatter)".into(),
        ));
    }

    // Find the closing ---
    let after_first = &trimmed[3..];
    let end = after_first.find("\n---").ok_or_else(|| {
        CliError::Other("SKILL.md missing closing --- for frontmatter".into())
    })?;

    let frontmatter_str = &after_first[..end];
    let body = after_first[end + 4..].trim().to_string(); // skip \n---

    let frontmatter: SkillFrontmatter = toml::from_str(frontmatter_str).map_err(|e| {
        CliError::Other(format!("invalid TOML frontmatter in SKILL.md: {}", e))
    })?;

    Ok((frontmatter, body))
}

/// Load all skills from a directory. Each subdirectory with a SKILL.md is a skill.
pub fn load_skills_from_dir(skills_dir: &Path) -> Result<Vec<Arc<dyn Skill>>> {
    let mut skills: Vec<Arc<dyn Skill>> = Vec::new();

    if !skills_dir.exists() {
        return Ok(skills);
    }

    let entries = std::fs::read_dir(skills_dir).map_err(|e| {
        CliError::Other(format!("cannot read skills directory {}: {}", skills_dir.display(), e))
    })?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        match load_skill(&path, &skill_md) {
            Ok(skill) => {
                skills.push(Arc::new(skill));
            }
            Err(e) => {
                eprintln!("  {} Failed to load {}: {}",
                    style("!").yellow(),
                    style(skill_md.display()).dim(),
                    e,
                );
            }
        }
    }

    Ok(skills)
}

fn load_skill(skill_dir: &Path, md_path: &Path) -> Result<ScriptSkill> {
    let contents = std::fs::read_to_string(md_path).map_err(|e| {
        CliError::Other(format!("cannot read {}: {}", md_path.display(), e))
    })?;

    let (fm, body) = parse_skill_md(&contents)?;

    let system_prompt = if body.is_empty() {
        format!("You are an AI agent with the skill: {}. {}", fm.name, fm.description)
    } else {
        body
    };

    Ok(ScriptSkill {
        name: fm.name,
        description: fm.description,
        capabilities: fm.capabilities,
        skill_dir: skill_dir.to_path_buf(),
        system_prompt,
        tools: fm.tools,
        max_tool_rounds: fm.max_tool_rounds.unwrap_or(super::script_skill::DEFAULT_MAX_TOOL_ROUNDS),
    })
}

/// Load skills from directory and register them all into a SkillRegistry.
pub fn load_skills_into_registry(skills_dir: &Path) -> Result<SkillRegistry> {
    let skills = load_skills_from_dir(skills_dir)?;
    let mut registry = SkillRegistry::new();
    for skill in skills {
        registry.register(skill);
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_md() {
        let content = r#"---
name = "test-skill"
description = "A test skill"
capabilities = ["test"]

[[tools]]
name = "my_tool"
description = "Does stuff"
command = ["echo", "hello"]

[[tools.parameters]]
name = "input"
description = "The input"
required = true
---

You are a test agent. Do test things.
"#;

        let (fm, body) = parse_skill_md(content).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.description, "A test skill");
        assert_eq!(fm.capabilities, vec!["test"]);
        assert_eq!(fm.tools.len(), 1);
        assert_eq!(fm.tools[0].name, "my_tool");
        assert_eq!(fm.tools[0].parameters.len(), 1);
        assert_eq!(body, "You are a test agent. Do test things.");
    }

    #[test]
    fn test_parse_skill_md_no_tools() {
        let content = r#"---
name = "simple"
description = "Simple LLM skill"
capabilities = ["chat"]
---

Just answer questions.
"#;

        let (fm, body) = parse_skill_md(content).unwrap();
        assert_eq!(fm.name, "simple");
        assert!(fm.tools.is_empty());
        assert_eq!(body, "Just answer questions.");
    }
}
