// TODO: Wire LoopState into Agent to track agent loop status.
// Currently declared but never used. Integrate into agent/mod.rs run_once()
// so callers/UI can observe whether the agent is idle or running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopState {
    Idle,
    Running,
}
