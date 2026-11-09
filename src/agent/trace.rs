//! How a tool call reads in the trace: the tool's name, a colon, and the label
//! the tool itself renders for its argument.
//!
//! The prefix is all the agent contributes. Everything after the colon belongs
//! to [`Tool::trace_label`](crate::tools::Tool::trace_label), because a command
//! wants eliding, a path wants resolving, and a skill wants its file looked up
//! on disk — three rules no single table can hold.

use std::path::Path;

use serde_json::Value;

use crate::tools::Tool;

/// One tool call's trace line: the tool's name, a colon, and the label the tool
/// renders for its own argument. A tool that names nothing traces bare.
pub(super) fn format_tool_call(tool: &dyn Tool, arguments: &Value, cwd: &Path) -> String {
    match tool.trace_label(arguments, cwd) {
        Some(label) => format!("{}: {label}", tool.name()),
        None => tool.name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::TRACE_WIDTH;
    use crate::tools::{
        bash::BashTool, edit::EditTool, elisp::ElispTool, read::ReadTool, skill::SkillTool,
        write::WriteTool,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn elisp_tool_call_shows_the_expression() {
        assert_eq!(
            format_tool_call(
                &ElispTool,
                &serde_json::json!({"expression": "(+ 1 2)"}),
                Path::new("/repo")
            ),
            "elisp: (+ 1 2)"
        );
    }

    #[test]
    fn elisp_tool_call_shows_only_the_first_expression_line() {
        assert_eq!(
            format_tool_call(
                &ElispTool,
                &serde_json::json!({"expression": "(progn\n  (+ 1 2))"}),
                Path::new("/repo")
            ),
            "elisp: (progn..."
        );
    }

    /// A skill found inside the project is named the way every other file in
    /// the trace is: from the project root, not by its absolute path.
    #[test]
    fn skill_tool_call_shows_its_markdown_filename() {
        let dir = project_dir("skill-label");
        let skill = dir.join(".oneloop").join("skills").join("emacs");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "how to reason").unwrap();

        assert_eq!(
            format_tool_call(
                &skill_tool_with(&dir),
                &serde_json::json!({"name": "emacs"}),
                &dir
            ),
            "skill: .oneloop/skills/emacs/SKILL.md"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_skill_falls_back_to_a_visible_failure() {
        let dir = project_dir("skill-missing");
        assert_eq!(
            format_tool_call(
                &skill_tool_with(&dir),
                &serde_json::json!({"name": "no-such-skill"}),
                &dir
            ),
            "skill: no-such-skill (not found)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The point of the rule: one file, one spelling. Five shapes of the same
    /// path and three tools that touch it all print the same line.
    #[test]
    fn every_shape_of_one_path_is_spelled_the_same_way() {
        for path in [
            "src/main.rs",
            "./src/main.rs",
            "@src/main.rs",
            "/repo/src/main.rs",
            "src/../src/main.rs",
        ] {
            for &tool in &[&ReadTool as &dyn Tool, &WriteTool, &EditTool] {
                let name = tool.name();
                assert_eq!(
                    format_tool_call(
                        tool,
                        &serde_json::json!({ "path": path }),
                        Path::new("/repo")
                    ),
                    format!("{name}: src/main.rs"),
                    "{name} was given {path}"
                );
            }
        }
    }

    /// A command is not a path: bash keeps the model's words verbatim.
    #[test]
    fn a_command_is_never_treated_as_a_path() {
        assert_eq!(
            format_tool_call(
                &BashTool,
                &serde_json::json!({"command": "cargo test"}),
                Path::new("/repo")
            ),
            "bash: cargo test"
        );
    }

    /// A command the model wrote as a whole script is cut to one readable
    /// trace line, and the cut says so.
    #[test]
    fn a_long_command_is_elided_at_the_trace_width() {
        let command = "x".repeat(400);
        assert_eq!(
            format_tool_call(
                &BashTool,
                &serde_json::json!({ "command": command }),
                Path::new("/repo")
            ),
            format!("bash: {}...", "x".repeat(TRACE_WIDTH))
        );
    }

    /// The cut counts characters, not bytes: half a multi-byte character would
    /// be mojibake on the terminal.
    #[test]
    fn elision_never_splits_a_character() {
        let command = "é".repeat(TRACE_WIDTH + 10);
        let label = format_tool_call(
            &BashTool,
            &serde_json::json!({ "command": command }),
            Path::new("/repo"),
        );
        assert_eq!(label, format!("bash: {}...", "é".repeat(TRACE_WIDTH)));
    }

    /// A command that fits is printed whole — no marker, nothing removed.
    #[test]
    fn a_command_at_the_width_is_printed_whole() {
        let command = "y".repeat(TRACE_WIDTH);
        let label = format_tool_call(
            &BashTool,
            &serde_json::json!({ "command": command }),
            Path::new("/repo"),
        );
        assert_eq!(label, format!("bash: {command}"));
    }

    /// A here-document is flattened into the one line the trace has, and the
    /// budget spans the whole command rather than stopping at the first line
    /// break: a script's opening line is not the command.
    #[test]
    fn a_multiline_command_spends_the_width_budget_across_its_lines() {
        assert_eq!(
            format_tool_call(
                &BashTool,
                &serde_json::json!({"command": "cd /repo && python3 - <<'PY'\np = 'x'\nPY"}),
                Path::new("/repo")
            ),
            "bash: cd /repo && python3 - <<'PY' p = 'x' PY..."
        );
    }

    /// The cut falls inside the command, not at its first line break, so the
    /// line is as wide as the budget allows.
    #[test]
    fn a_long_multiline_command_is_cut_at_the_width() {
        let command = format!("cd /repo && python3 - <<'PY'\n{}", "p = 'x'\n".repeat(40));
        let label = format_tool_call(
            &BashTool,
            &serde_json::json!({ "command": command }),
            Path::new("/repo"),
        );
        let shown = label.strip_prefix("bash: ").unwrap_or_default();
        assert!(shown.ends_with("..."), "{label}");
        assert_eq!(
            shown.trim_end_matches('.').chars().count(),
            TRACE_WIDTH,
            "{label}"
        );
    }

    /// A known tool that forgot its argument says so rather than naming the
    /// working directory, and rather than tracing bare like a tool that has
    /// nothing to say. Every tool that has a rule, not only the ones that name
    /// a path.
    #[test]
    fn a_missing_argument_stays_a_question_mark() {
        let dir = project_dir("missing-argument");
        let skill = skill_tool_with(&dir);

        for &tool in &[
            &ReadTool as &dyn Tool,
            &WriteTool,
            &EditTool,
            &ElispTool,
            &skill,
        ] {
            assert_eq!(
                format_tool_call(tool, &serde_json::json!({}), &dir),
                format!("{}: ?", tool.name()),
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `SkillTool` that exists: the tool is offered only when the project
    /// has at least one skill, so the trace has to be asked of a tool that was
    /// built. Which skill the project holds does not matter to either test.
    fn skill_tool_with(dir: &Path) -> SkillTool {
        let other = dir.join(".oneloop").join("skills").join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("SKILL.md"), "another skill").unwrap();
        SkillTool::new(dir).unwrap()
    }

    fn project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oneloop-agent-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
