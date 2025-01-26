mod protocol;
mod workspace_manager;
mod server_handler;
mod client_handler;

use std::io::{BufReader, Write};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufStream};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tower_lsp::lsp_types::*;
use serde_json::json;

use crate::workspace_manager::WorkspaceManager;
use crate::protocol::JsonRpcMessage;
use crate::server_handler::handle_server_messages;
use crate::client_handler::handle_client_messages;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create the Unix domain socket listener
    let socket_path = "/tmp/lsp-multiplexer.sock";
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    println!("LSP multiplexer listening on {}", socket_path);

    // Create shared workspace manager
    let workspace_manager = Arc::new(Mutex::new(WorkspaceManager::new()));

    // Accept connections
    loop {
        let (stream, _) = listener.accept().await?;
        let workspace_manager = Arc::clone(&workspace_manager);

        // Spawn a new task for each client connection
        tokio::spawn(async move {
            let mut stream = BufStream::new(stream);
            println!("New client connected");

            // Buffer for reading messages
            let mut message_buf = String::new();

            // Read the initialize request
            if let Ok(n) = stream.read_line(&mut message_buf).await {
                if n == 0 {
                    println!("Client disconnected before initialization");
                    return;
                }

                // Parse the initialize request
                if let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(&message_buf) {
                    match msg.content {
                        protocol::JsonRpcContent::Request { id, method, params }
                            if method == "initialize" =>
                        {
                            // Extract workspace root from initialize params
                            if let Some(params) = params {
                                if let Ok(init_params) =
                                    serde_json::from_value::<InitializeParams>(params)
                                {
                                    let workspace_root = init_params
                                        .root_uri
                                        .map(|uri| uri.to_string())
                                        .unwrap_or_else(|| "default_workspace".to_string());

                                    // Add client to workspace manager
                                    let mut manager = workspace_manager.lock().await;
                                    match manager.add_client(workspace_root.clone()) {
                                        Ok((client_id, is_first_client)) => {
                                            if is_first_client {
                                                // Forward initialize to the new server
                                                if let Some(server_process) =
                                                    manager.get_server_process_mut()
                                                {
                                                    let server_stdin =
                                                        server_process.stdin.as_mut().unwrap();
                                                    let server_id =
                                                        manager.get_next_server_request_id();
                                                    let init_request = json!({
                                                        "jsonrpc": "2.0",
                                                        "id": server_id,
                                                        "method": "initialize",
                                                        "params": params
                                                    });

                                                    manager.map_request(
                                                        server_id,
                                                        client_id,
                                                        serde_json::from_value(id).unwrap_or(0),
                                                    );

                                                    writeln!(
                                                        server_stdin,
                                                        "{}",
                                                        init_request.to_string()
                                                    )?;

                                                    // Spawn server message handling task
                                                    let server_stdout = BufReader::new(
                                                        server_process.stdout.take().unwrap(),
                                                    );
                                                    let workspace_manager_clone =
                                                        Arc::clone(&workspace_manager);
                                                    tokio::spawn(async move {
                                                        handle_server_messages(
                                                            server_stdout,
                                                            workspace_manager_clone,
                                                        )
                                                        .await;
                                                    });
                                                }
                                            } else {
                                                // For subsequent clients, send workspace/didChangeWorkspaceFolders
                                                if let Some(server_process) =
                                                    manager.get_server_process_mut()
                                                {
                                                    if let Some(stdin) =
                                                        server_process.stdin.as_mut()
                                                    {
                                                        let notification = json!({
                                                            "jsonrpc": "2.0",
                                                            "method": "workspace/didChangeWorkspaceFolders",
                                                            "params": {
                                                                "event": {
                                                                    "added": [{
                                                                        "uri": format!("file://{}", workspace_root),
                                                                        "name": workspace_root.split('/').last().unwrap_or("unknown")
                                                                    }],
                                                                    "removed": []
                                                                }
                                                            }
                                                        });

                                                        writeln!(
                                                            stdin,
                                                            "{}",
                                                            notification.to_string()
                                                        )?;
                                                    }
                                                }

                                                // Return cached initialize response
                                                // TODO: Implement caching of initialize response
                                            }

                                            // Start message forwarding loop
                                            handle_client_messages(
                                                client_id,
                                                stream,
                                                workspace_manager.clone(),
                                            )
                                            .await;
                                        }
                                        Err(e) => {
                                            eprintln!("Failed to add client: {}", e);
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            eprintln!("Expected initialize request as first message");
                            return;
                        }
                    }
                }
            }
        });
    }
}
