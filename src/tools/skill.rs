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
///
/// A skill is a directory named for it holding a `SKILL.md`, which is the
/// [Agent Skills](https://agentskills.io/specification) layout every other
/// coding agent reads too.
pub fn skill_candidates(cwd: &Path, name: &str) -> Vec<PathBuf> {
    skill_dirs(cwd)
        .into_iter()
        .map(|dir| dir.join(name).join(SKILL_FILE))
        .collect()
}

/// The file that makes a directory a skill.
const SKILL_FILE: &str = "SKILL.md";

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
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // A name already taken by an earlier directory stays taken: the search
        // order is the precedence order, so the shadowed copy is never read.
        if skills.iter().any(|skill| skill.name == name) {
            continue;
        }
        // The directory names the skill, so the menu, the file the trace shows
        // and the frontmatter's own `name` cannot drift apart. A directory
        // without a `SKILL.md` is not a skill and is skipped.
        let file = path.join(SKILL_FILE);
        let content = match fs::read_to_string(&file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let description = description(&content).to_string();
        skills.push(Skill {
            name,
            description,
            path: file,
        });
    }
}

/// The line the menu shows for a skill: the frontmatter's `description`, which
/// is what every agent reading Agent Skills uses to decide whether a skill is
/// relevant. A file with no `description` falls back to its first plain line so
/// that a half-written skill still says something.
fn description(content: &str) -> &str {
    frontmatter_field(content, "description")
        .unwrap_or_else(|| first_meaningful_line(body(content)))
}

/// The value of `key` in the YAML frontmatter that opens the file. Only a
/// single-line scalar is read: a description is one line of menu text, and a
/// folded or multi-line value would be a second layout to keep working.
fn frontmatter_field<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    if !opens_with_frontmatter(content) {
        return None;
    }
    for line in content.lines().skip(1) {
        if line.trim() == "---" {
            return None;
        }
        let Some(rest) = line
            .strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
        else {
            continue;
        };
        let value = rest.trim().trim_matches(|c| c == '"' || c == '\'');
        if !value.is_empty() {
            return Some(value);
        }
    }
    None
}

fn opens_with_frontmatter(content: &str) -> bool {
    content
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "---")
}

/// Everything after the frontmatter block, or the whole file when it opens with
/// no fence. The fallback description must never be the fence itself.
///
/// The body is found by walking the lines off the front of the string rather
/// than by adding up their lengths: `lines` strips a `\r` that `len` still
/// counts, so arithmetic lands inside the closing fence on a CRLF file.
fn body(content: &str) -> &str {
    if !opens_with_frontmatter(content) {
        return content;
    }
    let mut lines = content.lines();
    let mut rest = match lines.next() {
        Some(opening) => after_line(content, opening),
        None => return content,
    };
    for line in lines {
        rest = after_line(rest, line);
        if line.trim() == "---" {
            return rest;
        }
    }
    // A fence that never closes leaves no body: the file is frontmatter alone.
    ""
}

/// The text after `line`, which is `text`'s first line, its newline included.
fn after_line<'a>(text: &'a str, line: &str) -> &'a str {
    let rest = &text[line.len()..];
    let rest = rest.strip_prefix('\r').unwrap_or(rest);
    rest.strip_prefix('\n').unwrap_or(rest)
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

    /// Write one skill the way the standard lays it out, and return its file.
    fn write_skill(root: &Path, name: &str, content: &str) -> PathBuf {
        let dir = root.join(".oneloop").join("skills").join(name);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("SKILL.md");
        fs::write(&file, content).unwrap();
        file
    }

    /// The order is the rule: the trace names the first candidate on disk and
    /// the tool loads the first candidate it finds, so the project comes first.
    #[test]
    fn the_project_skill_directory_is_searched_first() {
        assert_eq!(
            skill_candidates(Path::new("/repo"), "emacs").first(),
            Some(&PathBuf::from("/repo/.oneloop/skills/emacs/SKILL.md"))
        );
    }

    /// A project-local skill shadows a global one of the same name: one entry
    /// in the menu, naming the project's file.
    #[test]
    fn a_project_skill_shadows_one_of_the_same_name() {
        let dir = project_dir("shadow");
        let file = write_skill(&dir, "emacs", "---\ndescription: reason about Emacs\n---\n");

        let tool = SkillTool::new(&dir).unwrap();
        let shadowing: Vec<&Skill> = tool.skills.iter().filter(|s| s.name == "emacs").collect();

        assert_eq!(shadowing.len(), 1);
        assert_eq!(shadowing[0].path, file);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The description is the discovery mechanism: it is the line every agent
    /// shows the model before the skill is loaded, so it comes from the
    /// frontmatter's `description` and never from the fence.
    #[test]
    fn the_menu_shows_the_frontmatter_description() {
        let dir = project_dir("description");
        write_skill(
            &dir,
            "oneloop-skill-description",
            "---\nname: oneloop-skill-description\ndescription: Search a repo or a past session\n---\n\n# Skill\n\nBody.\n",
        );

        let tool = SkillTool::new(&dir).unwrap();
        let skill = tool
            .skills
            .iter()
            .find(|s| s.name == "oneloop-skill-description")
            .unwrap();

        assert_eq!(skill.description, "Search a repo or a past session");
        assert!(
            tool.description()
                .contains("- oneloop-skill-description: Search a repo or a past session")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A skill written before the frontmatter rule still says something rather
    /// than advertising a bare fence.
    #[test]
    fn a_skill_without_frontmatter_falls_back_to_its_first_plain_line() {
        let dir = project_dir("no-frontmatter");
        write_skill(
            &dir,
            "oneloop-skill-plain",
            "# Heading\n\nFirst plain line.\n",
        );

        let tool = SkillTool::new(&dir).unwrap();
        let skill = tool
            .skills
            .iter()
            .find(|s| s.name == "oneloop-skill-plain")
            .unwrap();

        assert_eq!(skill.description, "First plain line.");
        let _ = fs::remove_dir_all(&dir);
    }

    /// One layout: the directory holding `SKILL.md`. A stray `<name>.md` beside
    /// it is not a skill, so the two layouts can never both be live.
    #[test]
    fn a_flat_markdown_file_is_not_a_skill() {
        let dir = project_dir("flat");
        // A real skill beside it keeps the tool registered whether or not the
        // home directory has any, so what follows is about the flat file rather
        // than about a tool that never built.
        write_skill(&dir, "oneloop-skill-kept", "---\ndescription: kept\n---\n");
        let flat = dir
            .join(".oneloop")
            .join("skills")
            .join("oneloop-skill-flat.md");
        fs::write(&flat, "how to reason").unwrap();

        let tool = SkillTool::new(&dir).unwrap();

        // The scan runs over the project and the home directory both, so the
        // comparison is limited to the names this test wrote itself.
        let found: Vec<&str> = tool
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .filter(|name| name.starts_with("oneloop-skill-"))
            .collect();
        assert_eq!(found, ["oneloop-skill-kept"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A nested key in the frontmatter is not the skill's description.
    #[test]
    fn a_nested_key_is_not_the_description() {
        let content = "---\nname: x\nmetadata:\n  description: nested\n---\n\nBody line.\n";

        assert_eq!(description(content), "Body line.");
    }

    /// The global skills are symlinks to the directory that owns them, so a
    /// skill directory reached through a link must load like any other.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_directory_is_read() {
        let dir = project_dir("symlink");
        let source = dir.join("elsewhere").join("oneloop-skill-linked");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join(SKILL_FILE),
            "---\ndescription: Reached through a link\n---\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            &source,
            dir.join(".oneloop")
                .join("skills")
                .join("oneloop-skill-linked"),
        )
        .unwrap();

        let tool = SkillTool::new(&dir).unwrap();
        let skill = tool
            .skills
            .iter()
            .find(|s| s.name == "oneloop-skill-linked")
            .unwrap();

        assert_eq!(skill.description, "Reached through a link");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Quoted values are unquoted, and a body-only file keeps its first line.
    #[test]
    fn a_quoted_description_is_unquoted() {
        assert_eq!(
            description("---\ndescription: \"Search the repo\"\n---\n"),
            "Search the repo"
        );
        assert_eq!(description("Plain line.\n"), "Plain line.");
    }

    /// A CRLF file keeps its body: the fallback must not land inside the fence.
    #[test]
    fn a_crlf_file_falls_back_to_its_body() {
        assert_eq!(
            description("---\r\ndescription:\r\n---\r\n\r\nBody line.\r\n"),
            "Body line."
        );
    }
}
