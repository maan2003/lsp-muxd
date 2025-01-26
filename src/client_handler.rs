use std::io::Write;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufStream};
use tokio::sync::Mutex;
use serde_json::json;

use crate::workspace_manager::WorkspaceManager;
use crate::protocol::{ClientId, JsonRpcMessage, JsonRpcContent};

pub async fn handle_client_messages(
    client_id: ClientId,
    mut stream: BufStream<tokio::net::UnixStream>,
    workspace_manager: Arc<Mutex<WorkspaceManager>>,
) {
    let mut message_buf = String::new();

    loop {
        message_buf.clear();
        match stream.read_line(&mut message_buf).await {
            Ok(0) => {
                // Client disconnected
                let mut manager = workspace_manager.lock().await;
                if let Some(workspace_root) = manager.remove_client(client_id) {
                    // Send workspace/didChangeWorkspaceFolders notification to server
                    if let Some(server_process) = manager.get_server_process_mut() {
                        if let Some(stdin) = server_process.stdin.as_mut() {
                            let notification = json!({
                                "jsonrpc": "2.0",
                                "method": "workspace/didChangeWorkspaceFolders",
                                "params": {
                                    "event": {
                                        "added": [],
                                        "removed": [{
                                            "uri": format!("file://{}", workspace_root),
                                            "name": workspace_root.split('/').last().unwrap_or("unknown")
                                        }]
                                    }
                                }
                            });

                            let _ = writeln!(stdin, "{}", notification.to_string());
                        }
                    }
                }
                break;
            }
            Ok(_) => {
                if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&message_buf) {
                    let mut manager = workspace_manager.lock().await;
                    if let Some(state) = manager.get_client_state(client_id) {
                        if let Some(server_process) = manager.get_server_process_mut() {
                            if let Some(stdin) = server_process.stdin.as_mut() {
                                match msg.content {
                                    JsonRpcContent::Request { id, method, params } => {
                                        let server_id = manager.get_next_server_request_id();
                                        let server_request = json!({
                                            "jsonrpc": "2.0",
                                            "id": server_id,
                                            "method": method,
                                            "params": params
                                        });

                                        manager.map_request(
                                            server_id,
                                            client_id,
                                            serde_json::from_value(id).unwrap_or(0),
                                        );

                                        if let Err(e) = writeln!(stdin, "{}", server_request.to_string()) {
                                            eprintln!("Failed to write to server: {}", e);
                                            break;
                                        }
                                    }
                                    JsonRpcContent::Notification { method, params } => {
                                        let notification = json!({
                                            "jsonrpc": "2.0",
                                            "method": method,
                                            "params": params
                                        });

                                        if let Err(e) = writeln!(stdin, "{}", notification.to_string()) {
                                            eprintln!("Failed to write to server: {}", e);
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Error reading from client: {}", e);
                break;
            }
        }
    }
}