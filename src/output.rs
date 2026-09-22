use std::sync::OnceLock;

pub const DEFAULT_MAX_BYTES: usize = 128 * 1024;
pub const DEFAULT_MAX_LINES: usize = 1000;

// ── Terminal output ───────────────────────────────────────────────────
//
// The single home for ANSI escapes and for the shape of a status line: a
// color-scheme change, a `--no-color` flag, or `NO_COLOR` support only ever
// touches this block.
//
// The streams are split by what the text *is*, not by how bad it is:
// **stdout carries the model's answer and nothing else**, so
// `ol "..." > answer.md` captures the answer while the tool trace, the
// warnings, and the errors stay on the terminal where they are read.
// Everything OneLoop says about itself goes to stderr.

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[90m";
/// Erase the current line (spinner redraws over it).
pub const CLEAR_LINE: &str = "\x1b[2K";

// Private: what an alarm looks like is this module's to decide, and a call
// site that could reach for red is a call site that can drift.
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

/// True when the terminal cannot take cursor-addressing output. Comint runs
/// its children with `TERM=dumb` and marks them in `INSIDE_EMACS`; there a
/// spinner's per-frame rewrite is not animation but a firehose that Emacs's
/// single thread grinds on in `comint-output-filter`, even when the buffer
/// is not displayed. Checked once — one process has one terminal.
pub fn plain() -> bool {
    static PLAIN: OnceLock<bool> = OnceLock::new();
    *PLAIN.get_or_init(|| {
        plain_terminal(
            std::env::var("INSIDE_EMACS").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    })
}

/// The pure half of [`plain`], split out so the predicate is testable
/// without touching process environment. `INSIDE_EMACS` is a comma-separated
/// token list, so `comint` must be a whole token.
fn plain_terminal(inside_emacs: Option<&str>, term: Option<&str>) -> bool {
    inside_emacs_has(inside_emacs, "comint") || term.is_some_and(|term| term == "dumb")
}

/// True only under comint — the one consumer of the OSC 9;4 sentinels that
/// [`agent::status`] emits. A dumb terminal outside Emacs has no consumer,
/// and there the spinner suppression is all the state a user gets.
pub fn comint() -> bool {
    static COMINT: OnceLock<bool> = OnceLock::new();
    *COMINT
        .get_or_init(|| inside_emacs_has(std::env::var("INSIDE_EMACS").ok().as_deref(), "comint"))
}

/// Whole-token check against the comma-separated `INSIDE_EMACS` list.
fn inside_emacs_has(inside_emacs: Option<&str>, feature: &str) -> bool {
    inside_emacs.is_some_and(|inside| inside.split(',').any(|token| token == feature))
}

/// Two-space indent, an optional glyph, then the message — the one shape
/// every status line has.
fn line(style: &str, glyph: &str, message: &str) -> String {
    if glyph.is_empty() {
        format!("{style}  {message}{RESET}")
    } else {
        format!("{style}  {glyph} {message}{RESET}")
    }
}

/// Something failed. Red `✗`.
pub fn fail(message: &str) {
    eprintln!("{}", line(RED, "✗", message));
}

/// Something wants attention, but the run goes on. Yellow `⚠`.
pub fn warn(message: &str) {
    eprintln!("{}", line(YELLOW, "⚠", message));
}

/// The user interrupted the run. Yellow `⏹`.
pub fn stopped(message: &str) {
    eprintln!("{}", line(YELLOW, "⏹", message));
}

/// Something the user asked for succeeded. Green `✓`.
pub fn ok(message: &str) {
    eprintln!("{}", line(GREEN, "✓", message));
}

/// A step of the running trace finished. Dim `✓` — a trace should recede,
/// which is what separates this from [`ok`].
pub fn tick(message: &str) {
    eprintln!("{}", line(DIM, "✓", message));
}

/// Something happened that is worth mentioning in passing. Dim `→`.
pub fn step(message: &str) {
    eprintln!("{}", line(DIM, "→", message));
}

/// An aside — dim, and no glyph, because it is usually a second line
/// qualifying the one above it.
pub fn note(message: &str) {
    eprintln!("{}", line(DIM, "", message));
}

/// A section heading above an interactive block: `── message ──`.
pub fn head(message: &str) {
    eprintln!("{BOLD}  ── {message} ──{RESET}");
}

/// Truncate keeping the first lines, appending a notice when content was dropped.
pub fn truncate_head(input: &str, max_bytes: usize, max_lines: usize) -> String {
    with_notice(truncate(input, max_bytes, max_lines, Keep::Head))
}

/// Truncate keeping the last lines, appending a notice when content was dropped.
pub fn truncate_tail(input: &str, max_bytes: usize, max_lines: usize) -> String {
    with_notice(truncate(input, max_bytes, max_lines, Keep::Tail))
}

/// Longest prefix of `input` at most `max_bytes` long that ends on a UTF-8
/// character boundary — plain byte-index slicing panics mid-character.
pub fn truncate_at_char_boundary(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

struct TruncationResult {
    content: String,
    truncated: bool,
    original_bytes: usize,
    original_lines: usize,
    shown_bytes: usize,
    shown_lines: usize,
}

#[derive(Clone, Copy)]
enum Keep {
    Head,
    Tail,
}

fn truncate(input: &str, max_bytes: usize, max_lines: usize, keep: Keep) -> TruncationResult {
    let lines: Vec<&str> = input.lines().collect();
    let normalized_input = lines.join("\n");
    let original_bytes = normalized_input.len();
    let original_lines = lines.len();

    let mut chosen: Vec<&str> = match keep {
        Keep::Head => lines.iter().copied().take(max_lines).collect(),
        Keep::Tail => {
            let start = original_lines.saturating_sub(max_lines);
            lines.iter().copied().skip(start).collect()
        }
    };

    while joined_len(&chosen) > max_bytes && !chosen.is_empty() {
        match keep {
            Keep::Head => {
                chosen.pop();
            }
            Keep::Tail => {
                chosen.remove(0);
            }
        }
    }

    let content = chosen.join("\n");
    let shown_bytes = content.len();
    let shown_lines = chosen.len();
    let truncated = shown_lines < original_lines || shown_bytes < original_bytes;

    TruncationResult {
        content,
        truncated,
        original_bytes,
        original_lines,
        shown_bytes,
        shown_lines,
    }
}

fn joined_len(lines: &[&str]) -> usize {
    if lines.is_empty() {
        0
    } else {
        lines.iter().map(|line| line.len()).sum::<usize>() + (lines.len() - 1)
    }
}

fn with_notice(result: TruncationResult) -> String {
    if !result.truncated {
        return result.content;
    }

    let mut content = result.content;
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&format!(
        "[output truncated: showing {} of {} lines, {} of {} bytes]",
        result.shown_lines, result.original_lines, result.shown_bytes, result.original_bytes
    ));
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_boundary_truncation_never_splits_a_character() {
        // "é" is two bytes; byte index 3 falls mid-character.
        let truncated = truncate_at_char_boundary("aaéé", 3);
        assert_eq!(truncated, "aa");
    }

    #[test]
    fn char_boundary_truncation_returns_short_input_whole() {
        assert_eq!(truncate_at_char_boundary("abc", 200), "abc");
    }

    #[test]
    fn a_status_line_is_indented_glyphed_and_closed() {
        assert_eq!(line(RED, "✗", "boom"), "\x1b[31m  ✗ boom\x1b[0m");
    }

    /// An aside carries no glyph, and must not carry its space either.
    #[test]
    fn a_glyphless_line_keeps_the_indent_and_nothing_more() {
        assert_eq!(line(DIM, "", "aside"), "\x1b[90m  aside\x1b[0m");
    }

    /// Every helper closes the escape it opens, or the color bleeds into
    /// whatever the model prints next.
    #[test]
    fn every_style_is_reset() {
        for style in [RED, GREEN, YELLOW, DIM, BOLD] {
            assert!(line(style, "✓", "x").ends_with(RESET));
        }
    }

    /// A comint child announces itself in `INSIDE_EMACS` regardless of what
    /// `TERM` says, and any `TERM=dumb` terminal gets the same treatment.
    #[test]
    fn comint_and_dumb_terminals_are_plain() {
        assert!(plain_terminal(Some("31.1,comint"), Some("dumb")));
        assert!(plain_terminal(Some("31.1,comint"), None));
        assert!(plain_terminal(None, Some("dumb")));
    }

    /// Whole-token matching keeps a name that merely contains "comint"
    /// from triggering the gate, and other Emacs contexts stay plain only
    /// by their own tokens.
    #[test]
    fn an_inside_emacs_without_the_comint_token_is_not_plain() {
        assert!(!plain_terminal(Some("31.1,shell-mode"), Some("xterm")));
        assert!(!plain_terminal(Some("comint-extension"), None));
    }

    /// An interactive terminal keeps the spinner.
    #[test]
    fn a_real_terminal_is_not_plain() {
        assert!(!plain_terminal(None, Some("xterm-256color")));
        assert!(!plain_terminal(None, None));
    }
}
