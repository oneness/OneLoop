//! The banner a REPL prints before the first prompt: what this agent is, what it
//! can reach, and how much of the conversation is left.
//!
//! Its own module because it is the one place in the agent that is pure
//! presentation — every decision here is where a column falls.

use crate::path::display_file;

use super::Agent;

impl Agent {
    pub fn summary(&self) -> String {
        let model = self.models.active();
        let available = packed(&self.models.by_provider());
        let message_count = self.session.messages().len();
        let tools = self.spec.tools.names().join(", ");
        let context = if self.spec.instructions.is_empty() {
            "none".to_string()
        } else {
            self.spec
                .instructions
                .iter()
                .map(|instruction| instruction.name)
                .collect::<Vec<_>>()
                .join(", ")
        };

        let session_state = if message_count > 0 {
            format!("{message_count} messages")
        } else {
            "new".to_string()
        };
        let session_path = display_file(self.cwd(), self.session.path());

        format!(
            "\x1b[1mmodel\x1b[22m    : {model}\n\
             \x1b[1mavailable\x1b[22m: {available}\n\
             \x1b[1mtools\x1b[22m    : {tools}\n\
             \x1b[1msession\x1b[22m  : {session_path} ({session_state})\n\
             \x1b[1mcontext\x1b[22m  : {context}"
        )
    }
}

/// Every summary label is a word, padding, and a colon, so each value starts in
/// the same column; wrapped values keep it and the block stays square.
const VALUE_COLUMN: usize = 11;

/// Wider than a realistic model list, narrow enough to read on a laptop.
const SUMMARY_WIDTH: usize = 80;

/// Provider groups packed into as few lines as fit, each continuation indented
/// to the value column and opened by a comma, so a break reads as "more
/// follows". A group never splits: a provider keeps its aliases together.
fn packed(groups: &[String]) -> String {
    // One column is held back for the comma a wrapped line ends with.
    let budget = SUMMARY_WIDTH - VALUE_COLUMN - 1;
    let mut lines: Vec<String> = Vec::new();
    for group in groups {
        match lines.last_mut() {
            Some(line) if line.len() + 2 + group.len() <= budget => {
                line.push_str(", ");
                line.push_str(group);
            }
            _ => lines.push(group.clone()),
        }
    }
    let indent = format!("\n{}", " ".repeat(VALUE_COLUMN));
    lines.join(&format!(",{indent}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_provider_list_stays_on_one_line() {
        let groups = ["local (qwen)".to_string(), "openai (astra)".to_string()];
        assert_eq!(packed(&groups), "local (qwen), openai (astra)");
    }

    #[test]
    fn a_long_provider_list_wraps_under_the_value_column() {
        let groups = [
            "local (qwen)".to_string(),
            "openai (astra, luna)".to_string(),
            "opencode-go (deepseek-flash, meta-muse, mimo-flash)".to_string(),
            "openrouter (laguna, ling, nvidia)".to_string(),
        ];
        let expected = [
            "local (qwen), openai (astra, luna),",
            "           opencode-go (deepseek-flash, meta-muse, mimo-flash),",
            "           openrouter (laguna, ling, nvidia)",
        ]
        .join("\n");
        assert_eq!(packed(&groups), expected);
    }
}
