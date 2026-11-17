//! Why an edit did not happen, in the words the model reads. A refusal is its
//! sentence — one constructor per reason — and it travels back as an answered call
//! with `is_error` set; an `Err` is kept for the tool's own failures.

use std::fmt;
use std::path::Path;

/// A call that was answered with "no", and what to do instead.
#[derive(Debug)]
pub struct Refusal(String);

impl Refusal {
    /// The arguments did not decode, so the call named no work. The shape is
    /// spelled out because a provider that ignored the schema still owes the
    /// model the reason it was wrong.
    pub fn bad_input(reason: &str) -> Self {
        Self(format!(
            "invalid edit input; expected {{ path: string, edits: [{{ old_text: \
             non-empty string, new_text: string }}] }} ({reason})"
        ))
    }

    /// `edits` was empty.
    pub fn empty_batch() -> Self {
        Self("edits must contain at least one replacement".to_string())
    }

    /// An anchor was empty, which names no place in any file.
    pub fn empty_anchor(index: usize) -> Self {
        Self(format!("edits[{index}].old_text must not be empty"))
    }

    /// The path is not there.
    pub fn not_found(path: &Path) -> Self {
        Self(format!("file does not exist: {}", path.display()))
    }

    /// The bytes are not UTF-8, so an anchor cannot be found in them.
    pub fn not_text(path: &Path) -> Self {
        Self(format!(
            "not a UTF-8 text file, so it cannot be edited by anchor: {} \
             (nothing was written)",
            path.display()
        ))
    }

    /// An anchor matched nothing. The hint names the lines that almost carried
    /// it, so the retry is a rewrite rather than a re-read.
    pub fn miss(index: usize, path: &Path, hint: &str) -> Self {
        Self(format!(
            "edits[{index}]: old_text not found in file: {} (nothing was written)\n{hint}",
            path.display()
        ))
    }

    /// An anchor matched several places, so it names no single one. `places`
    /// counts matches and the excerpt shows the lines they start on, which is
    /// why the two can disagree.
    pub fn ambiguous(
        index: usize,
        path: &Path,
        matcher: &str,
        places: usize,
        excerpt: &str,
        hidden: usize,
    ) -> Self {
        let mut message = format!(
            "edits[{index}]: old_text matches {places} places in file: {} under \
             {matcher} (nothing was written)\n{excerpt}",
            path.display()
        );
        if hidden > 0 {
            message.push_str(&format!(
                "\n… and {hidden} more line{} not shown",
                if hidden == 1 { "" } else { "s" }
            ));
        }
        message.push_str(
            "\nExtend old_text with the lines that differ between the matches, \
             so it names one place.",
        );
        Self(message)
    }

    /// Two edits claimed the same bytes of the original, so the result would
    /// depend on which was applied first.
    pub fn overlap(earlier: usize, later: usize, path: &Path) -> Self {
        Self(format!(
            "edits[{earlier}] and edits[{later}] overlap in file: {} (nothing was \
             written); combine overlapping changes into one edit",
            path.display()
        ))
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> &'static Path {
        Path::new("/repo/file.rs")
    }

    /// A refusal says what happened, where, and that the file is unchanged — the
    /// last part is what tells the model a retry is safe.
    #[test]
    fn a_miss_names_the_path_and_the_absence_of_a_write() {
        assert_eq!(
            Refusal::miss(2, file(), "look at line 4").to_string(),
            "edits[2]: old_text not found in file: /repo/file.rs (nothing was written)\n\
             look at line 4"
        );
    }

    /// The count is of places and the excerpt is of lines, so the two can
    /// disagree; the tail counts the lines the excerpt left out.
    #[test]
    fn an_ambiguous_refusal_counts_the_lines_it_did_not_show() {
        let text = Refusal::ambiguous(0, file(), "exact", 7, "   1 | x", 3).to_string();
        assert!(text.contains("matches 7 places"), "{text}");
        assert!(text.contains("… and 3 more lines not shown"), "{text}");
    }

    /// One hidden line reads as a line, because the sentence is quoted back.
    #[test]
    fn a_single_hidden_line_is_not_pluralised() {
        let text = Refusal::ambiguous(0, file(), "exact", 2, "   1 | x", 1).to_string();
        assert!(text.contains("… and 1 more line not shown"), "{text}");
    }

    #[test]
    fn an_overlap_names_the_pair_to_combine() {
        let text = Refusal::overlap(0, 3, file()).to_string();
        assert!(text.contains("edits[0] and edits[3] overlap"), "{text}");
    }

    #[test]
    fn a_bad_input_names_the_shape_it_expected() {
        let text = Refusal::bad_input("missing field `edits`").to_string();
        assert!(text.contains("expected { path: string, edits:"), "{text}");
        assert!(text.contains("missing field `edits`"), "{text}");
    }
}
