use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex};
use tokio_util::codec::{FramedRead, FramedWrite};
use tower_lsp::jsonrpc::{self, Request, Response};
use tower_lsp::lsp_types::*;
use tracing::{error, info, warn};

mod request_map;

use lsp_codec::LspCodec;
use request_map::RequestMap;

pub type ClientId = u64;
pub type RequestId = jsonrpc::Id;
pub type ServerRequestId = i64;
const INIT_ID: jsonrpc::Id = RequestId::Number(0);

/// An incoming or outgoing JSON-RPC message.
#[derive(Deserialize, Clone, Serialize, Debug)]
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
    workspace_root: Option<Url>,
    client_response_tx: mpsc::Sender<Message>,
}

// Main workspace state management
pub struct WorkspaceManager {
    server_process: Option<Child>,
    next_client_id: ClientId,
    clients: HashMap<ClientId, Client>,
    request_map: Arc<RequestMap>,
    initialize_response: watch::Sender<Option<Result<serde_json::Value, jsonrpc::Error>>>,
}

impl WorkspaceManager {
    pub fn new() -> Self {
        WorkspaceManager {
            server_process: None,
            next_client_id: 1,
            clients: HashMap::new(),
            request_map: Arc::new(RequestMap::new()),
            initialize_response: Default::default(),
        }
    }

    pub fn register_client(&mut self, client_response_tx: mpsc::Sender<Message>) -> ClientId {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        let client = Client {
            workspace_root: None,
            client_response_tx,
        };

        self.clients.insert(client_id, client);

        client_id
    }

    pub async fn remove_client(&mut self, client_id: ClientId) -> Result<()> {
        let client = self
            .clients
            .remove(&client_id)
            .context("client not found")?;
        // If no clients left, shut down the server
        if self.clients.is_empty() {
            if let Some(mut server) = self.server_process.take() {
                let _ = server.kill().await;
            }
        } else {
            if let Some(root) = client.workspace_root {
                let params = DidChangeWorkspaceFoldersParams {
                    event: WorkspaceFoldersChangeEvent {
                        added: vec![],
                        removed: vec![WorkspaceFolder {
                            uri: root,
                            name: Default::default(),
                        }],
                    },
                };

                self.send_server_message(
                    Request::build("workspace/didChangeWorkspaceFolders")
                        .params(
                            serde_json::to_value(params)
                                .context("Failed to serialize workspace change params")?,
                        )
                        .finish(),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn send_server_message(&mut self, request: Request) -> Result<(), anyhow::Error> {
        let stdin = self
            .server_process
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

    pub async fn handle_server_message(&mut self, message: Message) -> anyhow::Result<()> {
        match message {
            // If it's a Response, figure out which client it belongs to using request_map.
            Message::Response(resp) => {
                let (id, resp) = resp.into_parts();
                // resp
                if id == INIT_ID {
                    self.initialize_response.send(Some(resp)).ok();
                    return Ok(());
                }
                // Attempt to parse the response ID as server_request_id (u64).
                let (mapped_client_id, original_req_id) =
                    self.request_map.remove(&id).context("unknown response")?;
                self.send_client_message(
                    mapped_client_id,
                    Message::Response(Response::from_parts(original_req_id, resp)),
                )
                .await;
            }

            Message::Request(_) => {
                for client in self.clients.values() {
                    client.client_response_tx.send(message.clone()).await.ok();
                }
            }
        }
        Ok(())
    }

    pub async fn send_client_message(&mut self, client: ClientId, message: Message) {
        let Some(client) = self.clients.get(&client) else {
            warn!("client gone");
            return;
        };
        client.client_response_tx.send(message).await.ok();
    }

    pub async fn handle_server_messages(this: &Mutex<Self>, server_stdout: impl AsyncRead + Unpin) {
        let mut reader = FramedRead::new(server_stdout, LspCodec::default());

        while let Some(message_result) = reader.next().await {
            match message_result {
                Ok(json_val) => {
                    // Attempt to parse into Message.
                    if let Ok(message) = serde_json::from_value::<Message>(json_val.clone()) {
                        if let Err(e) = this.lock().await.handle_server_message(message).await {
                            error!("Failed to handle server message: {e:?}");
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
        this: &Arc<Mutex<Self>>,
        client_id: ClientId,
        request: Request,
    ) -> Result<()> {
        let (method, id, params) = request.into_parts();

        info!("method: {method}");
        // Special handling for initialize request
        if method == "initialize" {
            Self::handle_client_initialize(this, client_id, id.as_ref().unwrap(), &params).await?;
        } else if method == "exit" || method == "shutdown" {
            // ignore
        } else {
            let mut this = this.lock().await;
            let request = Request::build(method).params(params.unwrap_or(Value::Null));
            let request = if let Some(id) = id {
                let id = this.request_map.insert(client_id, id);
                request.id(id).finish()
            } else {
                request.finish()
            };
            this.send_server_message(request).await?;
        }
        Ok(())
    }

    async fn handle_client_initialize(
        this: &Arc<Mutex<Self>>,
        client_id: ClientId,
        id: &RequestId,
        params: &Option<Value>,
    ) -> Result<(), anyhow::Error> {
        let mut thil = this.lock().await;
        let init_params =
            serde_json::from_value::<InitializeParams>(params.clone().unwrap_or(Value::Null))
                .context("invalid request")?;
        let workspace_root = init_params.root_uri.context("root uri not present")?;
        thil.clients
            .get_mut(&client_id)
            .context("client gone")?
            .workspace_root = Some(workspace_root.clone());
        if thil.server_process.is_none() {
            info!("spawning server");
            // Launch the global LSP server process
            let mut new_server_process = Command::new("rust-analyzer")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .context("Failed to spawn rust-analyzer process")?;

            let stdout = BufReader::new(new_server_process.stdout.take().unwrap());
            let server_stdout = BufReader::new(stdout);
            thil.server_process = Some(new_server_process);

            thil.send_server_message(
                Request::build("initialize")
                    .params(
                        serde_json::to_value(params)
                            .context("Failed to serialize workspace change params")?,
                    )
                    .id(INIT_ID)
                    .finish(),
            )
            .await?;
            // Take server stdout for handling messages
            let this = this.clone();
            tokio::spawn(async move { Self::handle_server_messages(&this, server_stdout).await });
        } else {
            let params = DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![WorkspaceFolder {
                        uri: workspace_root,
                        name: Default::default(),
                    }],
                    removed: vec![],
                },
            };

            thil.send_server_message(
                Request::build("workspace/didChangeWorkspaceFolders")
                    .params(
                        serde_json::to_value(params)
                            .context("Failed to serialize workspace change params")?,
                    )
                    .finish(),
            )
            .await?;
        }
        let mut sub = thil.initialize_response.subscribe();
        drop(thil);
        let value = sub.wait_for(|x| x.is_some()).await?.clone().unwrap();
        this.lock()
            .await
            .send_client_message(
                client_id,
                Message::Response(Response::from_parts(id.clone(), value)),
            )
            .await;
        Ok(())
    }
}

pub async fn run_multiplexer(
    workspace_manager: Arc<Mutex<WorkspaceManager>>,
    listener: UnixListener,
) -> Result<()> {
    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("Failed to accept connection")?;
        let workspace_manager = Arc::clone(&workspace_manager);

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

    let (client_response_tx, mut client_response_rx) = mpsc::channel(32);

    let client_id = workspace_manager
        .lock()
        .await
        .register_client(client_response_tx);

    let (read_half, write_half) = stream.into_split();

    let mut reader = FramedRead::new(read_half, LspCodec::default());
    let writer = FramedWrite::new(write_half, LspCodec::default());

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

    while let Some(message) = reader
        .next()
        .await
        .transpose()
        .map_err(|e| anyhow::format_err!("invalid input: {e:?}"))?
    {
        let message = serde_json::from_value::<Message>(message).context("invalid request")?;
        match message {
            Message::Response(_) => warn!("ignore client response"),
            Message::Request(request) => {
                WorkspaceManager::handle_client_message(&workspace_manager, client_id, request)
                    .await?
            }
        }
    }

    workspace_manager
        .lock()
        .await
        .remove_client(client_id)
        .await
        .context("Failed to remove client")?;

    Ok(())
}
