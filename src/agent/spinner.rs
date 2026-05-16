//! RAII guard for a spinner task. Aborts the spinner and clears the line on drop.
//!
//! Uses `JoinHandle::abort_handle()` — cheaply cloneable — behind an `Arc<Mutex>`
//! so the `start_callback` can replace it with a fresh handle.

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(super) struct SpinnerGuard {
    abort: std::sync::Arc<std::sync::Mutex<tokio::task::AbortHandle>>,
}

impl SpinnerGuard {
    pub fn new(label: &str) -> Self {
        let handle = tokio::spawn(Self::spin_loop(label.to_string()));
        Self {
            abort: std::sync::Arc::new(std::sync::Mutex::new(handle.abort_handle())),
        }
    }

    async fn spin_loop(label: String) {
        let mut i = 0;
        loop {
            let frame = SPINNER_FRAMES[i % SPINNER_FRAMES.len()];
            eprint!("\x1b[2K\r\x1b[90m  {frame} {label}\x1b[0m\r");
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            i += 1;
        }
    }

    pub fn stop(&self) {
        if let Ok(abort) = self.abort.lock() {
            abort.abort();
        }
        eprint!("\x1b[2K\r");
    }

    /// Returns a callback that stops the spinner, suitable for passing to
    /// `complete_with_retry` so it can halt the animation before showing an
    /// interactive prompt.
    pub fn stop_callback(&self) -> Box<dyn FnOnce() + Send> {
        let abort = self.abort.clone();
        Box::new(move || {
            if let Ok(abort) = abort.lock() {
                abort.abort();
            }
            eprint!("\x1b[2K\r");
        })
    }

    /// Returns a callback that starts a new spinner, used after an interactive
    /// prompt to resume the animation.
    pub fn start_callback(&self, label: &str) -> Box<dyn FnOnce() + Send> {
        let abort = self.abort.clone();
        let label = label.to_string();
        Box::new(move || {
            let new_handle = tokio::spawn(Self::spin_loop(label));
            if let Ok(mut abort) = abort.lock() {
                *abort = new_handle.abort_handle();
            }
        })
    }
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        self.stop();
    }
}
