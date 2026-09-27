//! Discussion request selection; standalone prompts bypass this menu.

use std::io::{self, IsTerminal};

use anyhow::{Context, Result, bail};
use rustyline::{DefaultEditor, error::ReadlineError};

use crate::output;

use super::client::DEFAULT_INSTRUCTION;

const BRAINSTORM_INSTRUCTION: &str = "Brainstorm about this discussion. Explore distinct options and their trade-offs, surface open questions, and recommend the simplest viable approach. Do not implement anything.";

#[derive(Debug, PartialEq)]
enum Choice {
    Review,
    Brainstorm,
    Ask,
    Cancel,
}

fn parse_choice(input: &str) -> Result<Choice> {
    match input.trim() {
        "" | "r" | "R" => Ok(Choice::Review),
        "b" | "B" => Ok(Choice::Brainstorm),
        "a" | "A" => Ok(Choice::Ask),
        "c" | "C" => Ok(Choice::Cancel),
        _ => bail!("invalid selection — choose R, B, A, or C"),
    }
}

pub(crate) async fn select() -> Result<Option<String>> {
    if !io::stdin().is_terminal() {
        bail!(
            "the Claude menu requires a terminal; use /claude <prompt> for a standalone question"
        );
    }
    tokio::task::spawn_blocking(select_blocking)
        .await
        .context("Claude menu input thread failed")?
}

fn select_blocking() -> Result<Option<String>> {
    let mut editor = DefaultEditor::new()?;
    output::head("Claude — use the current discussion");
    eprintln!("  R. Review (default) — find weaknesses, gaps, and simpler alternatives");
    eprintln!("  B. Brainstorm — explore options and trade-offs");
    eprintln!("  A. Ask — ask your own question about this discussion");
    eprintln!("  C. Cancel");
    let Some(choice) = read_line(&mut editor, "Select [R/B/A/C], Enter to review: ")? else {
        return Ok(None);
    };
    match parse_choice(&choice)? {
        Choice::Review => Ok(Some(DEFAULT_INSTRUCTION.to_string())),
        Choice::Brainstorm => Ok(Some(BRAINSTORM_INSTRUCTION.to_string())),
        Choice::Ask => Ok(read_line(
            &mut editor,
            "Ask Claude (Enter with no question to cancel): ",
        )?
        .map(|question| question.trim().to_string())
        .filter(|question| !question.is_empty())),
        Choice::Cancel => Ok(None),
    }
}

fn read_line(editor: &mut DefaultEditor, prompt: &str) -> Result<Option<String>> {
    match editor.readline(prompt) {
        Ok(line) => Ok(Some(line)),
        Err(ReadlineError::Interrupted | ReadlineError::Eof) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_selects_review() {
        assert_eq!(parse_choice("").unwrap(), Choice::Review);
    }

    #[test]
    fn r_selects_review() {
        assert_eq!(parse_choice("r").unwrap(), Choice::Review);
    }

    #[test]
    fn b_selects_brainstorm() {
        assert_eq!(parse_choice("b").unwrap(), Choice::Brainstorm);
    }

    #[test]
    fn a_selects_custom_question() {
        assert_eq!(parse_choice("a").unwrap(), Choice::Ask);
    }

    #[test]
    fn c_selects_cancel() {
        assert_eq!(parse_choice("c").unwrap(), Choice::Cancel);
    }

    #[test]
    fn uppercase_letters_are_accepted() {
        assert_eq!(
            ["R", "B", "A", "C"].map(|input| parse_choice(input).unwrap()),
            [
                Choice::Review,
                Choice::Brainstorm,
                Choice::Ask,
                Choice::Cancel
            ],
        );
    }

    #[test]
    fn invalid_choice_does_not_fall_back_to_review() {
        assert!(parse_choice("4").is_err());
    }
}
