use std::{env, path::PathBuf};

/// Where this run works.
///
/// `ONELOOP_ORIGINAL_DIR` lets a wrapper say where the user was before it
/// changed directory — a launcher, an editor's compile command. Without it the
/// process's own working directory is the answer, and `.` if even that is
/// unavailable.
pub fn working_dir() -> PathBuf {
    env::var("ONELOOP_ORIGINAL_DIR")
        .map(PathBuf::from)
        .or_else(|_| env::current_dir())
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Attempts one model gets before the user is offered another.
pub const DEFAULT_MAX_RETRIES: usize = 10;

/// Read an env var, falling back to `default` when unset or unparsable.
pub fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A fresh scratch directory for a test, removed first so a leftover from an
/// earlier run cannot answer for this one.
///
/// The `name` is the test's own, and it lands in the directory along with the
/// process id, so two tests asking at once — which is what the harness does —
/// do not share a tree. `nested` is created inside it, for a test that needs
/// the layout the code expects rather than an empty directory: a session test
/// wants `.oneloop/sessions` to exist, a skill test `.oneloop/skills`.
///
/// Returns the root; the caller is responsible for the rest.
#[cfg(test)]
pub fn test_dir(name: &str, nested: Option<&str>) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oneloop-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    match nested {
        Some(relative) => std::fs::create_dir_all(dir.join(relative)).unwrap(),
        None => std::fs::create_dir_all(&dir).unwrap(),
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_falls_back_when_unset() {
        assert_eq!(env_or("ONELOOP_SURELY_UNSET_VAR", 7), 7);
    }
}
