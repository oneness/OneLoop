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
            "invalid edit input; expected one edit {{ path: string, old_text: \
             non-empty string, new_text: string }} or a batch {{ edits: [that same \
             object] }} ({reason})"
        ))
    }

    /// Neither form was filled in, so the call carries no edit at all.
    pub fn no_edit() -> Self {
        Self(
            "a call carries no edit: give one as { path, old_text, new_text } or \
             several as { edits: [{ path, old_text, new_text }, …] } (nothing was \
             written)"
                .to_string(),
        )
    }

    /// One field of a single edit, without the other.
    pub fn half_edit() -> Self {
        Self(
            "an edit needs both old_text and new_text: this call had only one of \
             them (nothing was written)"
                .to_string(),
        )
    }

    /// The single edit's anchor was empty, which names no place in any file. Not
    /// [`Refusal::empty_anchor`]: there is no batch to give a position in.
    pub fn empty_old_text() -> Self {
        Self(
            "old_text must not be empty: it names no place in the file (nothing was \
             written)"
                .to_string(),
        )
    }

    /// A single edit and a batch in the same call: two answers to one question.
    pub fn both_forms() -> Self {
        Self(
            "a call is either one edit (path, old_text, new_text) or a batch \
             (edits), not both: put the single edit in `edits` too, or drop `edits` \
             (nothing was written)"
                .to_string(),
        )
    }

    /// The single-edit form, with no file named for it to land in.
    pub fn flat_needs_path() -> Self {
        Self(
            "one edit needs `path` beside old_text and new_text; to change a file by \
             name, send all three (nothing was written)"
                .to_string(),
        )
    }

    /// An edit named no file, and the call had no path to default to, so there is
    /// nothing to read.
    pub fn no_path(index: usize) -> Self {
        Self(format!(
            "edits[{index}] names no path, and the call has no `path` to default to \
             (nothing was written)"
        ))
    }

    /// An anchor was empty, which names no place in any file.
    pub fn empty_anchor(index: usize) -> Self {
        Self(format!("edits[{index}].old_text must not be empty"))
    }

    /// The path is not there. Nothing has been written yet, because every file is
    /// read before any is written.
    pub fn not_found(path: &Path) -> Self {
        Self(format!(
            "file does not exist: {} (nothing was written)",
            path.display()
        ))
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
        assert!(text.contains("expected one edit { path:"), "{text}");
        assert!(text.contains("or a batch { edits:"), "{text}");
        assert!(text.contains("missing field `edits`"), "{text}");
    }

    /// The refusal a model sees after sending no edit at all spells out both
    /// shapes, because it guessed and the answer is which two it has.
    #[test]
    fn a_call_with_no_edit_is_told_both_shapes() {
        let text = Refusal::no_edit().to_string();
        assert!(text.contains("{ path, old_text, new_text }"), "{text}");
        assert!(
            text.contains("edits: [{ path, old_text, new_text }"),
            "{text}"
        );
        assert!(text.contains("nothing was written"), "{text}");
    }

    /// Two forms in one call is refused with the fix named, not merged silently.
    #[test]
    fn both_forms_names_the_fix() {
        let text = Refusal::both_forms().to_string();
        assert!(text.contains("not both"), "{text}");
        assert!(
            text.contains("put the single edit in `edits` too"),
            "{text}"
        );
    }

    /// An edit that names no file is refused by the position it was sent in, so
    /// the model knows which member of the batch to fix.
    #[test]
    fn a_pathless_edit_is_refused_by_position() {
        let text = Refusal::no_path(2).to_string();
        assert!(text.contains("edits[2] names no path"), "{text}");
        assert!(text.contains("no `path` to default to"), "{text}");
    }
}
