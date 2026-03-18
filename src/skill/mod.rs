pub mod loader;
pub mod script_skill;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::cli::error::Result;
use crate::cli::llm::LlmClient;
use crate::tui::AppEvent;

pub struct SkillInput {
    pub data: String,
    pub input_type: String,
    pub tags: Vec<String>,
    pub metadata: Value,
    pub job_id: String,
}

pub struct SkillOutput {
    pub data: String,
    pub output_mime: Option<String>,
}

pub struct SkillContext {
    pub llm: Option<Arc<LlmClient>>,
    pub agent_name: String,
    pub agent_description: String,
    pub event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
}

#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn capabilities(&self) -> &[String];
    async fn execute(&self, input: SkillInput, ctx: &SkillContext) -> Result<SkillOutput>;
}

pub struct SkillRegistry {
    skills: Vec<Arc<dyn Skill>>,
    default: Option<usize>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: Vec::new(),
            default: None,
        }
    }

    pub fn register(&mut self, skill: Arc<dyn Skill>) {
        let idx = self.skills.len();
        self.skills.push(skill);
        if self.default.is_none() {
            self.default = Some(idx);
        }
    }

    /// Route by matching tags against skill capabilities.
    pub fn route(&self, tags: &[String]) -> Option<&Arc<dyn Skill>> {
        for skill in &self.skills {
            let caps = skill.capabilities();
            for tag in tags {
                if caps.iter().any(|c| c == tag) {
                    return Some(skill);
                }
            }
        }
        self.default_skill()
    }

    pub fn default_skill(&self) -> Option<&Arc<dyn Skill>> {
        self.default.and_then(|i| self.skills.get(i))
    }

    pub fn all_capabilities(&self) -> Vec<String> {
        self.skills
            .iter()
            .flat_map(|s| s.capabilities().to_vec())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn skills(&self) -> &[Arc<dyn Skill>] {
        &self.skills
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummySkill {
        caps: Vec<String>,
    }

    #[async_trait]
    impl Skill for DummySkill {
        fn name(&self) -> &str { "dummy" }
        fn description(&self) -> &str { "test skill" }
        fn capabilities(&self) -> &[String] { &self.caps }
        async fn execute(&self, _input: SkillInput, _ctx: &SkillContext) -> Result<SkillOutput> {
            Ok(SkillOutput { data: "ok".into(), output_mime: None })
        }
    }

    fn _dummy_ctx() -> SkillContext {
        SkillContext {
            llm: None,
            agent_name: "test".into(),
            agent_description: "test".into(),
            event_tx: None,
        }
    }

    #[test]
    fn test_routing() {
        let mut reg = SkillRegistry::new();
        let skill_a = Arc::new(DummySkill { caps: vec!["code".into(), "debug".into()] });
        let skill_b = Arc::new(DummySkill { caps: vec!["translate".into()] });
        reg.register(skill_a);
        reg.register(skill_b);

        let found = reg.route(&["translate".into()]);
        assert_eq!(found.unwrap().name(), "dummy");
        assert!(found.unwrap().capabilities().contains(&"translate".into()));

        let found = reg.route(&["code".into()]);
        assert!(found.unwrap().capabilities().contains(&"code".into()));

        let found = reg.route(&["unknown".into()]);
        assert!(found.unwrap().capabilities().contains(&"code".into()));
    }

    #[test]
    fn test_all_capabilities() {
        let mut reg = SkillRegistry::new();
        reg.register(Arc::new(DummySkill { caps: vec!["a".into(), "b".into()] }));
        reg.register(Arc::new(DummySkill { caps: vec!["c".into()] }));
        let all = reg.all_capabilities();
        assert_eq!(all, vec!["a", "b", "c"]);
    }

    struct NamedDummySkill {
        skill_name: String,
        caps: Vec<String>,
    }

    #[async_trait]
    impl Skill for NamedDummySkill {
        fn name(&self) -> &str { &self.skill_name }
        fn description(&self) -> &str { "named test skill" }
        fn capabilities(&self) -> &[String] { &self.caps }
        async fn execute(&self, _input: SkillInput, _ctx: &SkillContext) -> Result<SkillOutput> {
            Ok(SkillOutput { data: "ok".into(), output_mime: None })
        }
    }

    #[test]
    fn test_empty_registry() {
        let reg = SkillRegistry::new();
        assert!(reg.is_empty());
        assert!(reg.all_capabilities().is_empty());
        assert!(reg.default_skill().is_none());
        assert!(reg.route(&["anything".into()]).is_none());
    }

    #[test]
    fn test_multiple_skills_same_tag() {
        let mut reg = SkillRegistry::new();
        let skill_a = Arc::new(NamedDummySkill {
            skill_name: "first".into(),
            caps: vec!["code".into()],
        });
        let skill_b = Arc::new(NamedDummySkill {
            skill_name: "second".into(),
            caps: vec!["code".into()],
        });
        reg.register(skill_a);
        reg.register(skill_b);

        let found = reg.route(&["code".into()]);
        assert_eq!(found.unwrap().name(), "first");
    }

    #[test]
    fn test_default_skill_empty() {
        let reg = SkillRegistry::new();
        assert!(reg.default_skill().is_none());
    }
}
