use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
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
use tracing::{error, info};

mod request_map;

use lsp_codec::LspCodec;
use request_map::RequestMap;

pub type ClientId = u64;
pub type RequestId = jsonrpc::Id;
pub type ServerRequestId = i64;

/// An incoming or outgoing JSON-RPC message.
#[derive(Deserialize, Clone, Serialize)]
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
}

// Main workspace state management
pub struct WorkspaceManager {
    server_process: Arc<Mutex<Option<Child>>>,
    next_client_id: ClientId,
    clients: std::sync::Mutex<HashMap<ClientId, Client>>,
    request_map: Arc<RequestMap>,
    workspace_roots: Vec<String>,
}

impl WorkspaceManager {
    pub fn get_client(&self, client_id: ClientId) -> Result<Client> {
        let lock = self
            .clients
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to acquire lock"))?;
        if let Some(client) = lock.get(&client_id) {
            Ok(client.clone())
        } else {
            anyhow::bail!("Client with ID {} not found", client_id)
        }
    }

    pub fn new() -> Self {
        WorkspaceManager {
            server_process: Arc::new(Mutex::new(None)),
            next_client_id: 1,
            clients: std::sync::Mutex::new(HashMap::new()),
            request_map: Arc::new(RequestMap::new()),
            workspace_roots: Vec::new(),
        }
    }

    pub fn register_client(&mut self, client_response_tx: mpsc::Sender<Message>) -> ClientId {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        let client = Client {
            workspace_root: None,
            client_response_tx,
        };

        // Store in sync map
        self.clients.lock().unwrap().insert(client_id, client);

        client_id
    }

    pub async fn remove_client(&mut self, client_id: ClientId) -> Result<()> {
        let (client, is_last);
        // Remove from sync map
        {
            let mut clients = self.clients.lock().unwrap();
            client = clients.remove(&client_id).context("client not found")?;
            is_last = clients.is_empty();
        }

        let workspace_root = client.workspace_root;

        // Remove the workspace root from our list
        if let Some(ref root) = workspace_root {
            self.workspace_roots.retain(|r| r != root);
        }

        // If no clients left, shut down the server
        if is_last {
            let mut server_lock = self.server_process.lock().await;
            if let Some(mut server) = server_lock.take() {
                let _ = server.kill();
            }
        } else {
            if let Some(ref root) = workspace_root {
                let params = DidChangeWorkspaceFoldersParams {
                    event: WorkspaceFoldersChangeEvent {
                        added: vec![],
                        removed: vec![WorkspaceFolder {
                            uri: Url::parse(&format!("file://{}", root))
                                .context("Failed to parse workspace root URL")?,
                            name: root.split('/').last().unwrap_or("unknown").to_string(),
                        }],
                    },
                };

                self.handle_client_notification(
                    "workspace/didChangeWorkspaceFolders".into(),
                    Some(
                        serde_json::to_value(params)
                            .context("Failed to serialize workspace change params")?,
                    ),
                )
                .await?;
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

        self.send_server_message(notification).await?;
        Ok(())
    }

    async fn send_server_message(&mut self, request: Request) -> Result<(), anyhow::Error> {
        let mut child = self.server_process.lock().await;
        let stdin = child
            .as_mut()
            .and_then(|c| c.stdin.as_mut())
            .context("server is down")?;
        let mut writer = FramedWrite::new(stdin, LspCodec::default());
        writer
            .send(serde_json::to_value(request)?)
            .await
            .map_err(|_| anyhow::format_err!("failed to send request"))?;
        Ok(())
    }

    pub async fn handle_server_messages(&mut self, server_stdout: impl AsyncRead + Unpin) {
        let mut reader = FramedRead::new(server_stdout, LspCodec::default());

        while let Some(message_result) = reader.next().await {
            match message_result {
                Ok(json_val) => {
                    // Attempt to parse into Message.
                    if let Ok(message) = serde_json::from_value::<Message>(json_val.clone()) {
                        match message {
                            // If it's a Response, figure out which client it belongs to using request_map.
                            Message::Response(resp) => {
                                let (id, resp) = resp.into_parts();
                                // Attempt to parse the response ID as server_request_id (u64).
                                if let Some((mapped_client_id, original_req_id)) =
                                    self.request_map.remove(&id)
                                {
                                    let clients = self.clients.lock().unwrap();
                                    if let Some(client) = clients.get(&mapped_client_id) {
                                        let _ = client
                                            .client_response_tx
                                            .send(Message::Response(Response::from_parts(
                                                original_req_id,
                                                resp,
                                            )))
                                            .await;
                                    }
                                } else {
                                    tracing::error!("Unknown response")
                                }
                            }

                            // If it's a Request or a Notification from the server,
                            // forward to all clients
                            Message::Request(_) => {
                                let clients = self.clients.lock().unwrap();
                                for (_cid, client) in clients.iter() {
                                    let _ = client.client_response_tx.send(message.clone()).await;
                                }
                            }
                        }
                    } else {
                        error!("Server sent unrecognized JSON: {:?}", json_val);
                    }
                }
                Err(e) => {
                    error!("Error reading from server: {:?}", e);
                    break;
                }
            }
        }
        error!("Server disconnected");
    }

    pub async fn handle_client_message(
        &mut self,
        client_id: ClientId,
        request: Request,
    ) -> Result<()> {
        let (method, id, params) = request.into_parts();

        // Special handling for initialize request
        if method == "initialize" {
            let mut client = self.get_client(client_id)?;
            // Parse initialize params
            if let Ok(init_params) =
                serde_json::from_value::<InitializeParams>(params.clone().unwrap_or(Value::Null))
            {
                // Extract workspace root
                let workspace_root = init_params
                    .root_uri
                    .as_ref()
                    .map(|uri| uri.path().to_string())
                    .unwrap_or_else(|| "default_workspace".to_string());

                client.workspace_root = Some(workspace_root.clone());

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
                    drop(server_process);
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
                        "workspace/didChangeWorkspaceFolders".into(),
                        Some(
                            serde_json::to_value(params)
                                .context("Failed to serialize workspace change params")?,
                        ),
                    )
                    .await?;
                }
            }
        }

        let request = Request::build(method).params(params.unwrap_or(Value::Null));
        let request = if let Some(id) = id {
            let id = self.request_map.insert(client_id, id);
            request.id(id).finish()
        } else {
            request.finish()
        };
        self.send_server_message(request).await?;
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
    info!("LSP multiplexer listening on {}", socket_path);

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
                error!("Failed to handle connection: {}", e);
            }
        });
    }
}

pub async fn handle_connection(
    stream: UnixStream,
    workspace_manager: Arc<Mutex<WorkspaceManager>>,
) -> Result<()> {
    info!("New client connected");

    // Create channels for client communication
    let (client_response_tx, mut client_response_rx) = mpsc::channel(32);

    // Register client and get ID
    let client_id = {
        let mut manager = workspace_manager.lock().await;
        manager.register_client(client_response_tx)
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
                error!("Error sending response to client: {:?}", e);
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
                        error!("Error handling client message: {}", e);
                        break;
                    }
                }
            }
            Err(e) => {
                error!("Error reading from client: {:?}", e);
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
