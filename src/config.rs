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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_falls_back_when_unset() {
        assert_eq!(env_or("ONELOOP_SURELY_UNSET_VAR", 7), 7);
    }
}
