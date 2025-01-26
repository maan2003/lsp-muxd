use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufStream};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::{self, Message};
use tower_lsp::lsp_types::*;

pub type ClientId = u64;
pub type RequestId = jsonrpc::Id;
pub type ServerRequestId = u64;

use tokio::sync::mpsc;

// Structure to track client-specific state
#[derive(Clone)]
pub struct Client {
    workspace_root: Option<String>,
    client_response_tx: mpsc::Sender<jsonrpc::Response>,
    server_request_tx: mpsc::Sender<jsonrpc::Request>,
    initialized: bool,
}

// Main workspace state management
#[derive(Clone)]
pub struct WorkspaceManager {
    server_process: Arc<Mutex<Option<Child>>>,
    next_client_id: ClientId,
    clients: HashMap<ClientId, Mutex<Client>>,
    request_map: HashMap<ServerRequestId, (ClientId, RequestId)>,
    next_server_request_id: ServerRequestId,
    workspace_roots: Vec<String>, // Track all workspace roots
}

impl WorkspaceManager {
    pub fn new() -> Self {
        WorkspaceManager {
            server_process: Arc::new(Mutex::new(None)),
            next_client_id: 1,
            clients: HashMap::new(),
            request_map: HashMap::new(),
            next_server_request_id: 1,
            workspace_roots: Vec::new(),
        }
    }

    pub fn register_client(
        &mut self,
        client_response_tx: mpsc::Sender<jsonrpc::Response>,
        server_request_tx: mpsc::Sender<jsonrpc::Request>,
    ) -> ClientId {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        let client = Client {
            workspace_root: None,
            client_response_tx,
            server_request_tx,
            initialized: false,
        };

        self.clients.insert(client_id, Mutex::new(client));
        client_id
    }

    pub fn get_server_process_mut(&mut self) -> Option<&mut Child> {
        self.server_process.lock().unwrap().as_mut()
    }

    pub fn get_client_state(&self, client_id: ClientId) -> Option<&Client> {
        self.clients.get(&client_id)
    }

    pub fn remove_client(&mut self, client_id: ClientId) -> std::io::Result<Option<String>> {
        if let Some(state) = self.clients.remove(&client_id) {
            let workspace_root = state.workspace_root;

            // Remove the workspace root from our list
            self.workspace_roots.retain(|root| root != &workspace_root);

            // If no clients left, shut down the server
            if self.clients.is_empty() {
                if let Some(mut server) = self.server_process.lock().unwrap().take() {
                    let _ = server.kill();
                }
            } else {
                // Only send workspace change notification if server is still running
                let params = DidChangeWorkspaceFoldersParams {
                    event: WorkspaceFoldersChangeEvent {
                        added: vec![],
                        removed: vec![WorkspaceFolder {
                            uri: Url::parse(&format!("file://{}", workspace_root))
                                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
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
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
                    ),
                )?;
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
        self.request_map.insert(server_id, (client_id, request_id));
    }

    pub fn get_request_mapping(
        &mut self,
        server_id: &ServerRequestId,
    ) -> Option<(ClientId, RequestId)> {
        self.request_map.remove(server_id)
    }

    pub fn handle_client_request(
        &mut self,
        server_req_id: ServerRequestId,
        client_id: ClientId,
        original_req_id: RequestId,
        method: Cow<'static, str>,
        params: Option<serde_json::Value>,
    ) -> std::io::Result<()> {
        let server_req_id = self.get_next_server_request_id();
        self.map_request(server_req_id.clone(), client_id, original_req_id);

        // 2) Build and serialize the request
        let request = tower_lsp::jsonrpc::Request::build(method)
            .id(server_req_id)
            .params(params.unwrap_or(serde_json::Value::Null))
            .build();

        // 3) Write JSON to the server stdin
        if let Some(process) = self.get_server_process_mut() {
            if let Some(stdin) = process.stdin.as_mut() {
                writeln!(stdin, "{}", serde_json::to_string(&request)?)?;
            }
        }
        Ok(())
    }

    pub fn handle_client_notification(
        &mut self,
        method: String,
        params: Option<serde_json::Value>,
    ) -> std::io::Result<()> {
        let notification = tower_lsp::jsonrpc::Request::build(method)
            .params(params.unwrap_or(serde_json::Value::Null))
            .build();

        if let Some(process) = self.get_server_process_mut() {
            if let Some(stdin) = process.stdin.as_mut() {
                writeln!(stdin, "{}", serde_json::to_string(&notification)?)?;
            }
        }
        Ok(())
    }

    pub async fn handle_server_messages(
        &mut self,
        mut server_stdout: impl AsyncBufReadExt + Unpin,
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
                    if let Ok(message) =
                        serde_json::from_str::<tower_lsp::jsonrpc::Message>(&message_buf)
                    {
                        // Check if it's a response (has result or error)
                        if message.result.is_some() || message.error.is_some() {
                            // Find original client and request ID
                            if let Some(id) = message.id.as_ref() {
                                if let Some((client_id, original_id)) = self.get_request_mapping(id)
                                {
                                    // Forward response to the original client
                                    let client_response =
                                        tower_lsp::jsonrpc::ResponseBuilder::default()
                                            .id(original_id)
                                            .result(message.result)
                                            .error(message.error)
                                            .build();

                                    // Send response through the client's channel
                                    if let Some(client_state) = self.clients.get(&client_id) {
                                        let _ =
                                            client_state.client_sender.send(client_response).await;
                                    }
                                }
                            }
                        }
                        // Check if it's a notification (has method but no id)
                        else if message.method.is_some() && message.id.is_none() {
                            // Broadcast notification to all clients
                            for client_state in self.clients.values() {
                                let _ = client_state.client_sender.send(message.clone()).await;
                            }
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

    pub async fn handle_client_message(
        &mut self,
        client_id: ClientId,
        request: tower_lsp::jsonrpc::Request,
    ) -> std::io::Result<()> {
        let (method, id, params) = request.into_parts();

        // Special handling for initialize request
        if method == "initialize" {
            if let Some(client_state) = self.clients.get_mut(&client_id) {
                let mut client = client_state.lock().await;
                if !client.initialized {
                    // Parse initialize params
                    if let Ok(init_params) =
                        serde_json::from_value::<InitializeParams>(params.unwrap_or_default())
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
                                .spawn()?;

                            *server_process = Some(new_server_process);

                            // Take server stdout for handling messages
                            if let Some(server) = server_process.as_mut() {
                                if let Some(stdout) = server.stdout.take() {
                                    let server_stdout = std::io::BufReader::new(stdout);
                                    let this = self.clone();
                                    tokio::spawn(async move {
                                        this.handle_server_messages(server_stdout).await;
                                    });
                                }
                            }
                        } else {
                            // Send workspace/didChangeWorkspaceFolders for additional clients
                            let params = DidChangeWorkspaceFoldersParams {
                                event: WorkspaceFoldersChangeEvent {
                                    added: vec![WorkspaceFolder {
                                        uri: Url::parse(&format!("file://{}", workspace_root))
                                            .map_err(|e| {
                                                std::io::Error::new(std::io::ErrorKind::Other, e)
                                            })?,
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
                                Some(serde_json::to_value(params).map_err(|e| {
                                    std::io::Error::new(std::io::ErrorKind::Other, e)
                                })?),
                            )?;
                        }
                    }
                }
            }
        }

        // Forward request to server
        let server_req_id = self.get_next_server_request_id();
        if let Some(id) = id {
            self.handle_client_request(server_req_id, client_id, id, method, params)?;
        } else {
            self.handle_client_notification(method, params)?;
        }
        Ok(())
    }
}

pub async fn run_multiplexer(
    socket_path: &str,
    workspace_manager: Arc<Mutex<WorkspaceManager>>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);

    // Bind UnixListener
    let listener = UnixListener::bind(socket_path)?;
    println!("LSP multiplexer listening on {}", socket_path);

    // Accept connections in a loop
    loop {
        let (stream, _) = listener.accept().await?;
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
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = BufStream::new(stream);
    println!("New client connected");

    // Create channels for client communication
    let (client_response_tx, mut client_response_rx) = mpsc::channel::<jsonrpc::Response>(32);
    let (server_request_tx, mut server_request_rx) = mpsc::channel::<jsonrpc::Request>(32);

    // Register client and get ID
    let client_id = {
        let mut manager = workspace_manager.lock().await;
        manager.register_client(client_response_tx, server_request_tx)
    };

    // Spawn task to forward responses to client
    let mut stream_clone = stream.clone();
    tokio::spawn(async move {
        while let Some(msg) = client_response_rx.recv().await {
            if let Ok(msg_str) = serde_json::to_string(&msg) {
                let _ = stream_clone.write_all(msg_str.as_bytes()).await;
                let _ = stream_clone.write_all(b"\n").await;
                let _ = stream_clone.flush().await;
            }
        }
    });

    // Handle incoming messages
    let mut message_buf = String::new();
    loop {
        message_buf.clear();
        match stream.read_line(&mut message_buf).await {
            Ok(0) => {
                // Client disconnected
                let mut manager = workspace_manager.lock().await;
                let _ = manager.remove_client(client_id)?;
                break;
            }
            Ok(_) => {
                if let Ok(message) =
                    serde_json::from_str::<tower_lsp::jsonrpc::Request>(&message_buf)
                {
                    let mut manager = workspace_manager.lock().await;
                    let _ = manager.handle_client_message(client_id, message).await;
                }
            }
            Err(e) => {
                eprintln!("Error reading from client: {}", e);
                break;
            }
        }
    }

    Ok(())
}
