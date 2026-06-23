use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::AgentContext;

use super::{Tool, ToolResult};

struct Skill {
    name: String,
    description: String,
    path: PathBuf,
}

pub struct SkillTool {
    skills: Vec<Skill>,
}

impl SkillTool {
    /// Scan global (~/.oneloop/skills/) then project-local (.oneloop/skills/) skill dirs.
    /// Project-local skills shadow global ones with the same name.
    /// Returns None if no skill files are found.
    pub fn new(cwd: &Path) -> Option<Self> {
        let mut skills: Vec<Skill> = Vec::new();

        if let Ok(home) = env::var("HOME") {
            collect_skills(
                &PathBuf::from(home).join(".oneloop").join("skills"),
                &mut skills,
            );
        }
        collect_skills(&cwd.join(".oneloop").join("skills"), &mut skills);

        if skills.is_empty() {
            None
        } else {
            Some(Self { skills })
        }
    }
}

fn collect_skills(dir: &Path, skills: &mut Vec<Skill>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let description = first_meaningful_line(&content).to_string();
        // Project-local shadows global: replace existing entry with the same name.
        if let Some(existing) = skills.iter_mut().find(|s| s.name == name) {
            existing.description = description;
            existing.path = path;
        } else {
            skills.push(Skill {
                name,
                description,
                path,
            });
        }
    }
}

/// Return the first non-empty, non-heading line of a markdown file as its description.
fn first_meaningful_line(content: &str) -> &str {
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("no description")
}

#[derive(Debug, Deserialize)]
struct SkillInput {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> String {
        let menu = self
            .skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Load a skill's instructions into context. Available skills:\n{menu}")
    }

    fn schema(&self) -> Value {
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the skill to load",
                    "enum": names
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &AgentContext) -> Result<ToolResult> {
        let input: SkillInput = serde_json::from_value(input)
            .context("invalid skill input; expected { name: string }")?;

        match self.skills.iter().find(|s| s.name == input.name) {
            None => Ok(ToolResult {
                content: format!(
                    "skill '{}' not found. available: {}",
                    input.name,
                    self.skills
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                is_error: true,
            }),
            Some(skill) => {
                let content = fs::read_to_string(&skill.path)
                    .with_context(|| format!("failed to read skill: {}", skill.path.display()))?;
                Ok(ToolResult {
                    content,
                    is_error: false,
                })
            }
        }
    }
}
