use serde_json::Value;
use tower_lsp::lsp_types::*;

// Type aliases for identification and message handling
pub type ClientId = u64;
pub type RequestId = u64;
pub type ServerRequestId = u64;

// Message types for JSON-RPC
#[derive(Debug, serde::Deserialize)]
pub struct JsonRpcMessage {
    pub jsonrpc: String,
    #[serde(flatten)]
    pub content: JsonRpcContent,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum JsonRpcContent {
    Request {
        id: Value,
        method: String,
        params: Option<Value>,
    },
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
}