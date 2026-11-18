//! Every line range the model reads — around a place, an ambiguity's candidates,
//! the region an edit changed, the lines a miss almost hit — rendered from what
//! `matcher` found. The constants here are those renderings' sizes.

use super::matcher::{self, Span};

/// Enough of a line to copy; past this it is not something to retype, and a
/// minified file would otherwise be pasted back.
const MAX_CHARS: usize = 96;

/// Two lines of context each side of the anchor, for a hint.
const AROUND: usize = 2;

/// Candidate lines a refusal lists before counting the rest.
const LISTED: usize = 5;

/// The most lines of either side a changed region shows.
const SIDE: usize = 4;

/// One numbered line, marked by what it is: `|` for context, `-` for text the
/// edit removed, `+` for text it wrote.
fn marked(number: usize, marker: char, text: &str) -> String {
    let text = text.chars().take(MAX_CHARS).collect::<String>();
    format!("{number:4} {marker} {text}")
}

/// The lines either side of `line` (1-based), numbered.
fn lines_around(content: &str, line: usize) -> String {
    let file = matcher::file_lines(content);
    let first = line.saturating_sub(1 + AROUND);
    file.iter()
        .skip(first)
        .take(AROUND * 2 + 1)
        .enumerate()
        .map(|(offset, (_, text))| marked(first + offset + 1, '|', text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The candidate lines a refusal shows, and how many it left out: one value rather
/// than a tuple, because the two are printed together.
pub struct Excerpt {
    /// The lines, numbered.
    pub lines: String,
    /// Candidate lines `lines` does not include.
    pub hidden: usize,
}

/// The candidate lines a refusal shows, and how many it left out.
pub fn candidates(content: &str, numbers: &[usize]) -> Excerpt {
    let file = matcher::file_lines(content);
    let shown = &numbers[..numbers.len().min(LISTED)];
    let lines = shown
        .iter()
        .filter_map(|number| {
            file.get(number.saturating_sub(1))
                .map(|(_, text)| marked(*number, '|', text))
        })
        .collect::<Vec<_>>()
        .join("\n");
    Excerpt {
        lines,
        hidden: numbers.len() - shown.len(),
    }
}

/// The region an edit changed: the lines it replaced, then the lines it wrote.
/// This is what a fallback receipt is for — an exact anchor needs no evidence,
/// but a fallback matched text the model did not send.
pub fn change(original: &str, span: &Span, text: &str, line: usize) -> String {
    let mut out = Vec::new();
    let removed = removed_lines(original, span);
    for (number, text) in removed.iter().take(SIDE) {
        out.push(marked(*number, '-', text));
    }
    if removed.len() > SIDE {
        out.push(format!("       … {} more removed", removed.len() - SIDE));
    }
    let added = added_lines(text);
    for (offset, text) in added.iter().take(SIDE).enumerate() {
        out.push(marked(line + offset, '+', text));
    }
    if added.len() > SIDE {
        out.push(format!("       … {} more written", added.len() - SIDE));
    }
    out.join("\n")
}

/// The lines of `content` that `span` covers, with the file's own numbering.
fn removed_lines<'a>(content: &'a str, span: &Span) -> Vec<(usize, &'a str)> {
    if span.start >= span.end {
        return Vec::new();
    }
    let covered = &content[span.start..span.end];
    let breaks = covered.matches('\n').count();
    // A span ending at a line break ends where the next line starts, and that
    // line is not one it replaced.
    let count = if covered.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    };
    let first = matcher::line_of(content, span.start);
    matcher::file_lines(content)
        .iter()
        .skip(first - 1)
        .take(count)
        .enumerate()
        .map(|(offset, (_, text))| (first + offset, *text))
        .collect()
}

/// The lines `text` puts in the file. An empty replacement puts none, one trailing
/// newline contributes no empty line of its own, and `\r` is dropped for the same
/// reason [`matcher::file_lines`] drops it: a reader does not see it.
fn added_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// What a miss almost hit: the probes that would have carried it, with the lines
/// to copy. A model corrects an anchor from this instead of reading the file.
pub fn hint(content: &str, probes: &[(&'static str, Span)]) -> String {
    let Some((_, span)) = probes.first() else {
        return "Nothing in the file resembles old_text; read it again and copy the block."
            .to_string();
    };
    let ignored = probes
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(" and ");
    format!(
        "The file holds this text with {ignored}. Copy these lines exactly:\n{}",
        lines_around(content, matcher::line_of(content, span.start))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_around_shows_the_lines_either_side() {
        assert_eq!(
            lines_around("a\nb\nc\nd\ne\n", 3),
            "   1 | a\n   2 | b\n   3 | c\n   4 | d\n   5 | e"
        );
    }

    #[test]
    fn lines_around_the_first_line_starts_at_the_first_line() {
        assert_eq!(lines_around("a\nb\nc\n", 1), "   1 | a\n   2 | b\n   3 | c");
    }

    /// A CRLF file renders without its `\r`: an excerpt is for copying, and the
    /// carriage return is invisible anyway.
    #[test]
    fn an_excerpt_hides_carriage_returns() {
        assert!(!lines_around("alpha\r\nbeta\r\n", 1).contains('\r'));
    }

    #[test]
    fn candidates_show_only_the_lines_they_name() {
        let excerpt = candidates("a\nb\nc\n", &[1, 3]);
        assert_eq!(excerpt.lines, "   1 | a\n   3 | c");
        assert_eq!(excerpt.hidden, 0);
    }

    /// A refusal names a handful of candidate lines and counts the rest, so a
    /// one-character anchor cannot paste a file back.
    #[test]
    fn candidates_count_the_lines_they_leave_out() {
        let excerpt = candidates("a\nb\nc\nd\ne\nf\n", &[1, 2, 3, 4, 5, 6]);
        assert_eq!(excerpt.lines.lines().count(), LISTED);
        assert_eq!(excerpt.hidden, 1);
    }

    /// The receipt for a fallback shows the lines the file actually held and the
    /// lines that replaced them — the trailing space the model did not send is
    /// the whole reason this receipt exists.
    #[test]
    fn a_change_shows_what_was_replaced_and_what_was_written() {
        let change = change("alpha   \nbeta\n", &(0..8), "alpha", 1);
        assert_eq!(change, "   1 - alpha   \n   1 + alpha");
    }

    /// A deletion shows only what it removed, and a replacement of several lines
    /// numbers the new ones from where they landed.
    #[test]
    fn a_change_numbers_lines_from_where_they_landed() {
        let change = change("a\nb\nc\n", &(2..3), "B1\nB2", 2);
        assert_eq!(change, "   2 - b\n   2 + B1\n   3 + B2");
    }

    #[test]
    fn a_deletion_shows_only_what_it_removed() {
        assert_eq!(change("a\nb\nc\n", &(2..4), "", 2), "   2 - b");
    }

    /// Both sides of a receipt drop `\r`: one reads the file's lines and the other
    /// the replacement it was sent, but a reader sees neither carriage return.
    #[test]
    fn a_change_into_a_crlf_file_shows_no_carriage_return() {
        let change = change("alpha   \r\n", &(0..8), "A1\r\nA2", 1);
        assert!(!change.contains('\r'), "{change:?}");
    }

    /// A region larger than the receipt shows is capped on both sides and the
    /// remainder is counted rather than dropped in silence.
    #[test]
    fn a_change_caps_either_side_and_counts_the_rest() {
        let original = (1..=9).map(|n| format!("old{n}\n")).collect::<String>();
        let written = (1..=9)
            .map(|n| format!("new{n}\n"))
            .collect::<Vec<_>>()
            .join("");
        let change = change(&original, &(0..original.len()), &written, 1);

        assert!(change.contains("… 5 more removed"), "{change}");
        assert!(change.contains("… 5 more written"), "{change}");
    }

    /// The hint names the normalization that would have carried the anchor, so
    /// the model rewrites one block instead of re-reading the file.
    #[test]
    fn a_hint_names_the_probe_that_would_have_matched() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let hint = hint(content, &matcher::probes(content, "  fn main() {"));
        assert!(hint.contains("indentation ignored"), "{hint}");
        assert!(hint.contains("   1 | fn main() {"), "{hint}");
    }

    #[test]
    fn a_hint_without_a_probe_asks_for_a_fresh_copy() {
        let hint = hint("fn main() {}\n", &[]);
        assert!(hint.contains("read it again"), "{hint}");
    }

    /// A probe row exists to turn "nothing resembles this" into "here it is": the
    /// hint shows the lines the rewrapped anchor was copied from.
    #[test]
    fn a_hint_names_the_line_breaks_a_rewrap_moved() {
        let content = "alpha beta\ngamma delta\n";
        let hint = hint(
            content,
            &matcher::probes(content, "alpha beta gamma\ndelta"),
        );

        assert!(hint.contains("line breaks ignored"), "{hint}");
        assert!(hint.contains("   1 | alpha beta"), "{hint}");
        assert!(hint.contains("   2 | gamma delta"), "{hint}");
    }
}
