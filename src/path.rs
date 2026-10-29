//! Turning a path the model sent into a real one, and spelling it back for the
//! trace. Not a tool: every tool that names a file does this, so the rule lives
//! here rather than in `tools/`.

use std::borrow::Cow;
use std::env;
use std::path::{Component, Path, PathBuf};

/// Turn a model-supplied path into a real one. Paths arrive in four shapes —
/// `@`-prefixed (an editor mention), `~`-anchored, absolute, and relative —
/// and only the last of those is the working directory's to resolve.
///
/// Joining the cwd onto a `~` path is the mistake this centralises away:
/// `PathBuf::join` treats the tilde as an ordinary component, so a write of
/// `~/notes.md` would silently create a directory literally named `~` under
/// the cwd rather than touching the home directory.
pub fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
    resolve_with_home(cwd, raw, env::var("HOME").ok().as_deref())
}

/// The resolution itself, with `HOME` passed in so it can be tested without
/// mutating the process environment out from under every other test.
fn resolve_with_home(cwd: &Path, raw: &str, home: Option<&str>) -> PathBuf {
    let trimmed = raw.trim_start_matches('@');

    if let Some(home) = home {
        if trimmed == "~" {
            return PathBuf::from(home);
        }
        // Only `~/` is ours to expand: `~alice` names another user's home,
        // which we cannot resolve, so let it fail as itself rather than
        // mangling it into `{home}alice`.
        if let Some(rest) = trimmed.strip_prefix("~/") {
            return PathBuf::from(format!("{home}/{rest}"));
        }
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Collapse `.` and `..` lexically, without touching the filesystem, so two
/// spellings of one path agree — `sub/../note.txt` and `note.txt` must not
/// look like two different files to the lock or to the trace.
pub fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

// ── How a path is spelled in the trace ────────────────────────────────
//
// One rule for every tool that names a file: resolve the shape the model
// sent, collapse `.` and `..`, then show only the part that carries
// information — project-relative inside the working directory, `~/` inside
// the home directory, absolute otherwise. A trace line repeats on every
// call, so the prefix the reader already knows is the first thing to drop.

/// The trace form of a model-supplied path: `src/main.rs` from all of
/// `src/main.rs`, `./src/main.rs`, `@src/main.rs`, and the absolute path to
/// the same file.
pub fn display_path(cwd: &Path, raw: &str) -> String {
    display_with_home(cwd, raw, env::var("HOME").ok().as_deref())
}

/// The trace form of a path already resolved — a skill file found by search
/// rather than named by the model.
pub fn display_file(cwd: &Path, path: &Path) -> String {
    shorten(cwd, &normalize(path), env::var("HOME").ok().as_deref())
}

/// The whole rule for a path as the model sent it, with `HOME` passed in so it
/// can be tested without mutating the process environment out from under every
/// other test.
fn display_with_home(cwd: &Path, raw: &str, home: Option<&str>) -> String {
    shorten(cwd, &normalize(&resolve_with_home(cwd, raw, home)), home)
}

/// The spelling itself, for a path already resolved and collapsed. Both sides
/// are collapsed before the prefix is compared: a working directory carrying
/// `.` or `..` must still recognise the files resolved against it.
fn shorten(cwd: &Path, path: &Path, home: Option<&str>) -> String {
    if let Ok(rest) = path.strip_prefix(normalize(cwd)) {
        return if rest.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rest.display().to_string()
        };
    }
    home_relative(path, home).to_string_lossy().into_owned()
}

/// A path with the user's home directory shortened to `~`: the home prefix is
/// the part the trace already repeats on every line. A home of `/` names the
/// whole filesystem, so shortening against it would relabel every absolute
/// path; that, an empty home, and an unknown one all leave the path alone.
fn home_relative<'a>(path: &'a Path, home: Option<&str>) -> Cow<'a, Path> {
    home.filter(|home| !home.is_empty() && *home != "/")
        .and_then(|home| path.strip_prefix(home).ok())
        .map_or_else(
            || Cow::Borrowed(path),
            |rest| Cow::Owned(Path::new("~").join(rest)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A working directory that is deliberately *not* under the home
    /// directory: the two must never be conflated.
    const CWD: &str = "/home/kasim/.dotfiles";
    const HOME: Option<&str> = Some("/home/kasim");

    fn resolved(raw: &str) -> PathBuf {
        resolve_with_home(Path::new(CWD), raw, HOME)
    }

    fn shown(raw: &str) -> String {
        display_with_home(Path::new("/repo"), raw, HOME)
    }

    /// The regression: a `~` path joined onto the cwd yielded
    /// `{cwd}/~/repos/...`, which `write` would have created as a directory
    /// literally named `~`.
    #[test]
    fn a_tilde_path_is_never_joined_onto_the_working_directory() {
        assert_eq!(
            resolved("~/repos/OneLoop/README.md"),
            PathBuf::from("/home/kasim/repos/OneLoop/README.md")
        );
        assert_eq!(resolved("~"), PathBuf::from("/home/kasim"));
    }

    #[test]
    fn other_path_shapes_keep_their_meaning() {
        assert_eq!(resolved("/etc/hosts"), PathBuf::from("/etc/hosts"));
        assert_eq!(
            resolved("src/main.rs"),
            PathBuf::from("/home/kasim/.dotfiles/src/main.rs")
        );
        assert_eq!(
            resolved("@src/main.rs"),
            PathBuf::from("/home/kasim/.dotfiles/src/main.rs")
        );
        // Another user's home is not ours to resolve.
        assert_eq!(
            resolved("~alice/notes.md"),
            PathBuf::from("/home/kasim/.dotfiles/~alice/notes.md")
        );
    }

    /// Without `HOME` there is nothing to expand to, and inventing one would
    /// be the very guess this module exists to prevent.
    #[test]
    fn an_unknown_home_leaves_the_tilde_alone() {
        assert_eq!(
            resolve_with_home(Path::new(CWD), "~/notes.md", None),
            PathBuf::from("/home/kasim/.dotfiles/~/notes.md")
        );
    }

    /// The working directory's own files lose the prefix that every line of
    /// the trace would otherwise repeat.
    #[test]
    fn a_path_in_the_project_is_named_from_the_project_root() {
        assert_eq!(shown("src/main.rs"), "src/main.rs");
        assert_eq!(shown("/repo/src/main.rs"), "src/main.rs");
        assert_eq!(shown("@./src/main.rs"), "src/main.rs");
    }

    /// `.` and `..` are collapsed before the prefix is stripped, so a path
    /// leaving the project by one route and re-entering it is named once.
    #[test]
    fn dot_segments_are_collapsed_before_the_path_is_named() {
        assert_eq!(shown("src/../src/main.rs"), "src/main.rs");
        assert_eq!(shown("./src/./main.rs"), "src/main.rs");
    }

    /// Outside the project the home directory is still the shorter name, and a
    /// `~` the model wrote comes back as the `~` it wrote.
    #[test]
    fn a_path_in_the_home_directory_is_named_with_a_tilde() {
        assert_eq!(shown("~/notes.md"), "~/notes.md");
        assert_eq!(
            shorten(
                Path::new("/repo"),
                &Path::new("/home/kasim").join("notes.md"),
                HOME
            ),
            "~/notes.md"
        );
    }

    /// A path in neither place keeps its absolute form — shortening it would
    /// mean inventing a prefix that does not exist.
    #[test]
    fn a_path_outside_the_project_and_home_stays_absolute() {
        assert_eq!(shown("/etc/hosts"), "/etc/hosts");
    }

    /// A home of `/` names the whole filesystem, so shortening against it would
    /// relabel every absolute path as if it lived in the home directory. An
    /// empty home is the same mistake in a different spelling.
    #[test]
    fn a_home_that_names_everything_is_not_a_home() {
        for home in [Some("/"), Some("")] {
            assert_eq!(
                display_with_home(Path::new("/repo"), "/etc/hosts", home),
                "/etc/hosts"
            );
        }
    }

    /// The working directory is collapsed like the path is, so one carrying
    /// `.` or `..` still recognises the files resolved against it.
    #[test]
    fn a_working_directory_with_dot_segments_still_names_its_files() {
        assert_eq!(
            display_with_home(Path::new("/repo/./sub/.."), "/repo/src/main.rs", HOME),
            "src/main.rs"
        );
    }

    /// The working directory itself is named `.`, not an empty string.
    #[test]
    fn the_project_root_is_named_as_a_dot() {
        assert_eq!(shown("/repo"), ".");
        assert_eq!(shown("."), ".");
    }

    /// A skill file is found on disk rather than named by the model, and is
    /// shortened by the same rule.
    #[test]
    fn a_resolved_file_is_named_like_a_model_supplied_one() {
        assert_eq!(
            shorten(
                Path::new("/repo"),
                Path::new("/repo/.oneloop/skills/emacs.md"),
                HOME
            ),
            ".oneloop/skills/emacs.md"
        );
    }
}
