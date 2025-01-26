use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;
use serde_json::json;

use crate::workspace_manager::WorkspaceManager;
use crate::protocol::{JsonRpcMessage, JsonRpcContent, ServerRequestId};

pub async fn handle_server_messages(
    mut server_stdout: impl AsyncBufReadExt + Unpin,
    workspace_manager: Arc<Mutex<WorkspaceManager>>,
) {
    let mut message_buf = String::new();

    loop {
        message_buf.clear();
        match server_stdout.read_line(&mut message_buf).await {
            Ok(0) => {
                // Server disconnected
                eprintln!("Server disconnected");
                break;
            }
            Ok(_) => {
                if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&message_buf) {
                    let mut manager = workspace_manager.lock().await;
                    match msg.content {
                        JsonRpcContent::Response { id, result, error } => {
                            // Find original client and request ID
                            if let Ok(server_id) = serde_json::from_value::<ServerRequestId>(id) {
                                if let Some((client_id, original_id)) =
                                    manager.get_request_mapping(&server_id)
                                {
                                    // Forward response to the original client
                                    let response = json!({
                                        "jsonrpc": "2.0",
                                        "id": original_id,
                                        "result": result,
                                        "error": error
                                    });
                                    // TODO: Send response back to client
                                    // This would require maintaining client streams or using a message channel
                                }
                            }
                        }
                        JsonRpcContent::Notification { method, params } => {
                            // Broadcast notification to all clients
                            let notification = json!({
                                "jsonrpc": "2.0",
                                "method": method,
                                "params": params
                            });
                            // TODO: Broadcast to all clients
                            // This would require maintaining client streams or using message channels
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                eprintln!("Error reading from server: {}", e);
                break;
            }
        }
    }
}