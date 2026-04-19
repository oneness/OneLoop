// TODO: Flesh out RPC protocol for external integrations.
// Currently only has a placeholder RpcRequest struct.
// Add response types, tool execution messages, streaming events, etc.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub method: String,
}
