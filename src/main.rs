use lsp_multiplexer::Muxer;
use std::io::{BufReader, Write};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufStream};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::{self, Message, NotificationBuilder, RequestBuilder, ResponseBuilder};
use tower_lsp::lsp_types::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create the Unix domain socket listener
    let socket_path = "/tmp/lsp-multiplexer.sock";
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)?;
    println!("LSP multiplexer listening on {}", socket_path);

    // Create shared workspace manager
    let workspace_manager = Arc::new(Mutex::new(Muxer::new()));

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
                    return Ok(());
                }

                // Parse the initialize request
                if let Ok(message) = serde_json::from_str::<Message>(&message_buf) {
                    // Check if it's a request with method "initialize"
                    if message.method.as_deref() == Some("initialize") && message.id.is_some() {
                        // Extract workspace root from initialize params
                        if let Ok(init_params) = serde_json::from_value::<InitializeParams>(
                            message.params.unwrap_or_default(),
                        ) {
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
                                        let server_id = manager.get_next_server_request_id();
                                        manager.send_request_to_server(
                                            server_id,
                                            client_id,
                                            message.id.unwrap(),
                                            "initialize".to_string(),
                                            Some(serde_json::to_value(init_params).unwrap()),
                                        )?;

                                        // Spawn server message handling task
                                        let server_stdout = BufReader::new(
                                            manager
                                                .get_server_process_mut()
                                                .unwrap()
                                                .stdout
                                                .take()
                                                .unwrap(),
                                        );
                                        let workspace_manager_clone =
                                            Arc::clone(&workspace_manager);
                                        tokio::spawn(async move {
                                            workspace_manager_clone
                                                .lock()
                                                .await
                                                .handle_server_messages(server_stdout)
                                                .await;
                                        });
                                    } else {
                                        // For subsequent clients, send workspace/didChangeWorkspaceFolders
                                        let params = DidChangeWorkspaceFoldersParams {
                                            event: WorkspaceFoldersChangeEvent {
                                                added: vec![WorkspaceFolder {
                                                    uri: Url::parse(&format!(
                                                        "file://{}",
                                                        workspace_root
                                                    ))
                                                    .unwrap(),
                                                    name: workspace_root
                                                        .split('/')
                                                        .last()
                                                        .unwrap_or("unknown")
                                                        .to_string(),
                                                }],
                                                removed: vec![],
                                            },
                                        };

                                        manager.send_notification_to_server(
                                            "workspace/didChangeWorkspaceFolders".to_string(),
                                            Some(serde_json::to_value(params).unwrap()),
                                        )?;

                                        // Return cached initialize response
                                        // TODO: Implement caching of initialize response
                                    }

                                    // Start message forwarding loop
                                    let workspace_manager_clone = Arc::clone(&workspace_manager);
                                    tokio::spawn(async move {
                                        workspace_manager_clone
                                            .lock()
                                            .await
                                            .handle_client_messages(client_id, stream)
                                            .await;
                                    });
                                }
                                Err(e) => {
                                    eprintln!("Failed to add client: {}", e);
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        });
    }
}
