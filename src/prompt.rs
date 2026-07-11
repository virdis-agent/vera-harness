use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::extensions::{SkillCatalog, discover_agents};

pub const STATIC_SYSTEM_PROMPT: &str = "You are Vera, a careful coding agent. Inspect before editing.\nUse the smallest safe change, explain assumptions, and validate your work.\nNever expose credentials. Ask approval before writes, commands, network, hooks, plugins, or MCP.\nRespect AGENTS.md, skills, the active plan, and the user's repository boundaries.\nReport failures honestly and leave unrelated changes untouched.";

pub struct PromptContext {
    pub system: String,
    pub instructions: Vec<PathBuf>,
    pub skills: Vec<String>,
}

pub fn build_context(
    project: &Path,
    global_agents: Option<&Path>,
    skills: &SkillCatalog,
) -> Result<PromptContext> {
    let mut system = STATIC_SYSTEM_PROMPT.to_string();
    let instructions = discover_agents(project, global_agents)?;
    for path in &instructions {
        system.push_str("\n\n# Instructions from ");
        system.push_str(&path.display().to_string());
        system.push('\n');
        system.push_str(&fs::read_to_string(path)?);
    }
    let active_skills = skills.active_descriptions();
    for (name, description) in &active_skills {
        system.push_str("\n\n# Skill: ");
        system.push_str(name);
        system.push('\n');
        system.push_str(description);
    }
    Ok(PromptContext {
        system,
        instructions,
        skills: active_skills.into_iter().map(|(name, _)| name).collect(),
    })
}

pub fn approximate_tokens(text: &str) -> usize {
    text.split_whitespace().count().max(text.len() / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_prompt_stays_compact() {
        assert!(approximate_tokens(STATIC_SYSTEM_PROMPT) < 1_000);
    }
}
