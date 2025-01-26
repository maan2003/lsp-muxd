use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};
use tower_lsp::jsonrpc::{self, Request, Response};
use tower_lsp::lsp_types::*;

mod codec;
mod request_map;

use codec::LspCodec;
use request_map::RequestMap;

pub type ClientId = u64;
pub type RequestId = jsonrpc::Id;
pub type ServerRequestId = u64;

/// An incoming or outgoing JSON-RPC message.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub enum Message {
    /// A response message.
    Response(Response),
    /// A request or notification message.
    Request(Request),
}

use tokio::sync::mpsc;

// Structure to track client-specific state
#[derive(Clone)]
pub struct Client {
    workspace_root: Option<String>,
    client_response_tx: mpsc::Sender<Message>,
    initialized: bool,
}

// Main workspace state management
pub struct WorkspaceManager {
    server_process: Arc<Mutex<Option<Child>>>,
    next_client_id: ClientId,
    clients: Arc<Mutex<HashMap<ClientId, Client>>>,
    request_map: Arc<RequestMap>,
    next_server_request_id: ServerRequestId,
    workspace_roots: Vec<String>, // Track all workspace roots
}

impl WorkspaceManager {
    pub fn new() -> Self {
        WorkspaceManager {
            server_process: Arc::new(Mutex::new(None)),
            next_client_id: 1,
            clients: Arc::new(Mutex::new(HashMap::new())),
            request_map: Arc::new(RequestMap::new()),
            next_server_request_id: 1,
            workspace_roots: Vec::new(),
        }
    }

    pub async fn register_client(&mut self, client_response_tx: mpsc::Sender<Message>) -> ClientId {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        let client = Client {
            workspace_root: None,
            client_response_tx,
            initialized: false,
        };

        let mut clients = self.clients.lock().await;
        clients.insert(client_id, client);
        client_id
    }

    pub async fn remove_client(&mut self, client_id: ClientId) -> Result<Option<String>> {
        if let Some(state) = self.clients.remove(&client_id) {
            let workspace_root = state.workspace_root;

            // Remove the workspace root from our list
            self.workspace_roots.retain(|root| root != &workspace_root);

            // If no clients left, shut down the server
            if self.clients.is_empty() {
                if let Some(mut server) = self.server_process.lock().await.take() {
                    let _ = server.kill();
                }
            } else {
                // Only send workspace change notification if server is still running
                let params = DidChangeWorkspaceFoldersParams {
                    event: WorkspaceFoldersChangeEvent {
                        added: vec![],
                        removed: vec![WorkspaceFolder {
                            uri: Url::parse(&format!("file://{}", workspace_root))
                                .context("Failed to parse workspace root URL")?,
                            name: workspace_root
                                .split('/')
                                .last()
                                .unwrap_or("unknown")
                                .to_string(),
                        }],
                    },
                };

                self.handle_client_notification(
                    "workspace/didChangeWorkspaceFolders".to_string(),
                    Some(
                        serde_json::to_value(params)
                            .context("Failed to serialize workspace change params")?,
                    ),
                )
                .await?;
            }

            Ok(Some(workspace_root))
        } else {
            Ok(None)
        }
    }

    pub fn get_next_server_request_id(&mut self) -> ServerRequestId {
        let id = self.next_server_request_id;
        self.next_server_request_id += 1;
        id
    }

    pub fn map_request(
        &mut self,
        server_id: ServerRequestId,
        client_id: ClientId,
        request_id: RequestId,
    ) {
        self.request_map.insert(server_id, client_id, request_id);
    }

    pub fn get_request_mapping(
        &mut self,
        server_id: &ServerRequestId,
    ) -> Option<(ClientId, RequestId)> {
        self.request_map.remove(server_id)
    }

    pub async fn send_server_request(
        &mut self,
        client_id: ClientId,
        original_req_id: RequestId,
        method: Cow<'static, String>,
        params: Option<Value>,
    ) -> Result<()> {
        let server_req_id = self.get_next_server_request_id();
        self.request_map
            .insert(server_req_id, client_id, original_req_id);

        // Build the request
        let request = Request::build(method)
            .id(server_req_id.to_string())
            .params(params.unwrap_or(Value::Null))
            .finish();

        // Write request to server using framed writer
        let mut server_process = self.server_process.lock().await;
        if let Some(process) = server_process.as_mut() {
            if let Some(stdin) = process.stdin.as_mut() {
                let mut writer = FramedWrite::new(stdin, LspCodec::default());
                writer
                    .send(serde_json::to_value(request)?)
                    .await
                    .context("Failed to send request to server")?;
            }
        }
        Ok(())
    }

    pub async fn handle_client_notification(
        &mut self,
        method: Cow<'static, str>,
        params: Option<Value>,
    ) -> Result<()> {
        let notification = Request::build(method)
            .params(params.unwrap_or(Value::Null))
            .finish();

        let mut server_process = self.server_process.lock().await;
        if let Some(process) = server_process.as_mut() {
            if let Some(stdin) = process.stdin.as_mut() {
                let mut writer = FramedWrite::new(stdin, LspCodec::default());
                writer
                    .send(serde_json::to_value(notification)?)
                    .await
                    .context("Failed to send notification to server")?;
            }
        }
        Ok(())
    }

    pub async fn handle_server_messages(&mut self, server_stdout: impl AsyncRead + Unpin) {
        let mut reader = FramedRead::new(server_stdout, LspCodec::default());

        while let Some(message_result) = reader.next().await {
            match message_result {
                Ok(json_val) => {
                    // Parse as serde_json::Value
                    if let Ok(message) = serde_json::from_value::<Message>(json_val) {
                        let clients = self.clients.lock().await;
                        if let Some(client) = clients.get(&client_id) {
                            let _ = client.client_response_tx.send(client_response).await;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from server: {}", e);
                    break;
                }
            }
        }
        eprintln!("Server disconnected");
    }

    pub async fn handle_client_message(
        &mut self,
        client_id: ClientId,
        request: Request,
    ) -> Result<()> {
        let (method, id, params) = request.into_parts();

        // Special handling for initialize request
        if method == "initialize" {
            let mut clients = self.clients.lock().await;
            if let Some(client) = clients.get_mut(&client_id) {
                if !client.initialized {
                    // Parse initialize params
                    if let Ok(init_params) =
                        serde_json::from_value::<InitializeParams>(params.unwrap_or(Value::Null))
                    {
                        // Extract workspace root
                        let workspace_root = init_params
                            .root_uri
                            .as_ref()
                            .map(|uri| uri.path().to_string())
                            .unwrap_or_else(|| "default_workspace".to_string());

                        client.workspace_root = Some(workspace_root.clone());
                        client.initialized = true;

                        // Add workspace root if not already present
                        if !self.workspace_roots.contains(&workspace_root) {
                            self.workspace_roots.push(workspace_root.clone());
                        }

                        let mut server_process = self.server_process.lock().await;
                        if server_process.is_none() {
                            // Launch the global LSP server process
                            let new_server_process = Command::new("rust-analyzer")
                                .stdin(Stdio::piped())
                                .stdout(Stdio::piped())
                                .stderr(Stdio::piped())
                                .spawn()
                                .context("Failed to spawn rust-analyzer process")?;

                            *server_process = Some(new_server_process);

                            // Take server stdout for handling messages
                            if let Some(server) = server_process.as_mut() {
                                if let Some(stdout) = server.stdout.take() {
                                    let server_stdout = BufReader::new(stdout);
                                    // FIXME:
                                    // let this = Arc::new(Mutex::new(self.clone()));
                                    // tokio::spawn(async move {
                                    //     let mut manager = this.lock().await;
                                    //     manager.handle_server_messages(server_stdout).await;
                                    // });
                                }
                            }
                        } else {
                            // Send workspace/didChangeWorkspaceFolders for additional clients
                            let params = DidChangeWorkspaceFoldersParams {
                                event: WorkspaceFoldersChangeEvent {
                                    added: vec![WorkspaceFolder {
                                        uri: Url::parse(&format!("file://{}", workspace_root))
                                            .context("Failed to parse workspace root URL")?,
                                        name: workspace_root
                                            .split('/')
                                            .last()
                                            .unwrap_or("unknown")
                                            .to_string(),
                                    }],
                                    removed: vec![],
                                },
                            };

                            self.handle_client_notification(
                                "workspace/didChangeWorkspaceFolders".to_string(),
                                Some(
                                    serde_json::to_value(params)
                                        .context("Failed to serialize workspace change params")?,
                                ),
                            )
                            .await?;
                        }
                    }
                }
            }
        }

        if let Some(id) = id {
            self.send_server_request(client_id, id, method, params)
                .await?;
        } else {
            self.handle_client_notification(method, params).await?;
        }
        Ok(())
    }
}

pub async fn run_multiplexer(
    socket_path: &str,
    workspace_manager: Arc<Mutex<WorkspaceManager>>,
) -> Result<()> {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);

    // Bind UnixListener
    let listener =
        UnixListener::bind(socket_path).context("Failed to bind to Unix domain socket")?;
    println!("LSP multiplexer listening on {}", socket_path);

    // Accept connections in a loop
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("Failed to accept connection")?;
        let workspace_manager = Arc::clone(&workspace_manager);

        // Spawn a task for each connection, calling helper to handle it
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, workspace_manager).await {
                eprintln!("Failed to handle connection: {}", e);
            }
        });
    }
}

pub async fn handle_connection(
    stream: UnixStream,
    workspace_manager: Arc<Mutex<WorkspaceManager>>,
) -> Result<()> {
    println!("New client connected");

    // Create channels for client communication
    let (client_response_tx, mut client_response_rx) = mpsc::channel(32);

    // Register client and get ID
    let client_id = {
        let mut manager = workspace_manager.lock().await;
        manager.register_client(client_response_tx).await
    };

    // Split stream into read and write parts
    let (read_half, write_half) = stream.into_split();

    // Create framed reader and writer
    let mut reader = FramedRead::new(read_half, LspCodec::default());
    let writer = FramedWrite::new(write_half, LspCodec::default());

    // Spawn task to forward responses to client
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = client_response_rx.recv().await {
            if let Err(e) = writer
                .send(serde_json::to_value(msg).expect("serialize always works"))
                .await
            {
                eprintln!("Error sending response to client: {}", e);
                break;
            }
        }
    });

    // Handle incoming messages
    while let Some(message_result) = reader.next().await {
        match message_result {
            Ok(json_val) => {
                // Convert Message to Request if possible
                if let Ok(request) = serde_json::from_value::<Request>(json_val) {
                    let mut manager = workspace_manager.lock().await;
                    if let Err(e) = manager.handle_client_message(client_id, request).await {
                        eprintln!("Error handling client message: {}", e);
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("Error reading from client: {}", e);
                break;
            }
        }
    }

    // Client disconnected, clean up
    let mut manager = workspace_manager.lock().await;
    manager
        .remove_client(client_id)
        .await
        .context("Failed to remove client")?;

    Ok(())
}
