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

/// The directories searched for skills, in precedence order: the project's own
/// first, then the global one, so a project-local skill of a given name wins.
pub fn skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let project = cwd.join(".oneloop").join("skills");
    match env::var_os("HOME") {
        Some(home) => vec![project, PathBuf::from(home).join(".oneloop").join("skills")],
        None => vec![project],
    }
}

/// Every file a skill of this name could be loaded from, in search order. This
/// module owns the layout, so the skill tool and the trace that names the file
/// it would read cannot disagree about either.
pub fn skill_candidates(cwd: &Path, name: &str) -> Vec<PathBuf> {
    skill_dirs(cwd)
        .into_iter()
        .map(|dir| dir.join(format!("{name}.md")))
        .collect()
}

impl SkillTool {
    /// Every skill found in [`skill_dirs`], first match winning, so a
    /// project-local skill shadows a global one of the same name.
    pub fn new(cwd: &Path) -> Option<Self> {
        let mut skills: Vec<Skill> = Vec::new();

        for dir in skill_dirs(cwd) {
            collect_skills(&dir, &mut skills);
        }

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
        // A name already taken by an earlier directory stays taken: the search
        // order is the precedence order, so the shadowed copy is never read.
        if skills.iter().any(|skill| skill.name == name) {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let description = first_meaningful_line(&content).to_string();
        skills.push(Skill {
            name,
            description,
            path,
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-skill-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".oneloop").join("skills")).unwrap();
        dir
    }

    /// The order is the rule: the trace names the first candidate on disk and
    /// the tool loads the first candidate it finds, so the project comes first.
    #[test]
    fn the_project_skill_directory_is_searched_first() {
        assert_eq!(
            skill_candidates(Path::new("/repo"), "emacs").first(),
            Some(&PathBuf::from("/repo/.oneloop/skills/emacs.md"))
        );
    }

    /// A project-local skill shadows a global one of the same name: one entry
    /// in the menu, naming the project's file.
    #[test]
    fn a_project_skill_shadows_one_of_the_same_name() {
        let dir = project_dir("shadow");
        let file = dir.join(".oneloop").join("skills").join("emacs.md");
        fs::write(&file, "how to reason").unwrap();

        let tool = SkillTool::new(&dir).unwrap();
        let shadowing: Vec<&Skill> = tool.skills.iter().filter(|s| s.name == "emacs").collect();

        assert_eq!(shadowing.len(), 1);
        assert_eq!(shadowing[0].path, file);
        let _ = fs::remove_dir_all(&dir);
    }
}
