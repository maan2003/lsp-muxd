use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use tokio::io::AsyncBufReadExt;
use tower_lsp::jsonrpc;

pub type ClientId = u64;
pub type RequestId = jsonrpc::Id;
pub type ServerRequestId = u64;

// Structure to track client-specific state
pub struct ClientState {
    pub workspace_root: String,
    pub initialized: bool,
}

// Main workspace state management
pub struct WorkspaceManager {
    server_process: Option<Child>,
    next_client_id: ClientId,
    client_states: HashMap<ClientId, ClientState>,
    request_map: HashMap<ServerRequestId, (ClientId, RequestId)>,
    next_server_request_id: ServerRequestId,
    workspace_roots: Vec<String>, // Track all workspace roots
}

impl WorkspaceManager {
    pub fn new() -> Self {
        WorkspaceManager {
            server_process: None,
            next_client_id: 1,
            client_states: HashMap::new(),
            request_map: HashMap::new(),
            next_server_request_id: 1,
            workspace_roots: Vec::new(),
        }
    }

    pub fn add_client(
        &mut self,
        workspace_root: String,
    ) -> Result<(ClientId, bool), std::io::Error> {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        // Create client state
        self.client_states.insert(
            client_id,
            ClientState {
                workspace_root: workspace_root.clone(),
                initialized: false,
            },
        );

        let is_first_client = self.server_process.is_none();

        if is_first_client {
            // Launch the global LSP server process
            let server_process = Command::new("rust-analyzer") // Example: using rust-analyzer
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            self.server_process = Some(server_process);
        }

        // Add workspace root if not already present
        if !self.workspace_roots.contains(&workspace_root) {
            self.workspace_roots.push(workspace_root);
        }

        Ok((client_id, is_first_client))
    }

    pub fn get_server_process_mut(&mut self) -> Option<&mut Child> {
        self.server_process.as_mut()
    }

    pub fn get_client_state(&self, client_id: ClientId) -> Option<&ClientState> {
        self.client_states.get(&client_id)
    }

    pub fn remove_client(&mut self, client_id: ClientId) -> Option<String> {
        if let Some(state) = self.client_states.remove(&client_id) {
            let workspace_root = state.workspace_root;

            // Remove the workspace root from our list
            self.workspace_roots.retain(|root| root != &workspace_root);

            // If no clients left, shut down the server
            if self.client_states.is_empty() {
                if let Some(mut server) = self.server_process.take() {
                    let _ = server.kill();
                }
            }

            Some(workspace_root)
        } else {
            None
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

    pub fn send_request_to_server(
        &mut self,
        server_req_id: ServerRequestId,
        client_id: ClientId,
        original_req_id: RequestId,
        method: String,
        params: Option<serde_json::Value>,
    ) -> std::io::Result<()> {
        // 1) Map server_req_id → (client_id, original_req_id)
        self.map_request(server_req_id.clone(), client_id, original_req_id);

        // 2) Build and serialize the request
        let request = tower_lsp::jsonrpc::RequestBuilder::default()
            .id(server_req_id)
            .method(method)
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

    pub fn send_notification_to_server(
        &mut self,
        method: String,
        params: Option<serde_json::Value>,
    ) -> std::io::Result<()> {
        let notification = tower_lsp::jsonrpc::NotificationBuilder::default()
            .method(method)
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
                                    // TODO: Send response back to client
                                    // This would require maintaining client streams or using a message channel
                                }
                            }
                        }
                        // Check if it's a notification (has method but no id)
                        else if message.method.is_some() && message.id.is_none() {
                            // Broadcast notification to all clients
                            // TODO: Broadcast to all clients
                            // This would require maintaining client streams or using message channels
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

    pub async fn handle_client_messages(
        &mut self,
        client_id: ClientId,
        mut stream: tokio::io::BufStream<tokio::net::UnixStream>,
    ) {
        let mut message_buf = String::new();

        loop {
            message_buf.clear();
            match stream.read_line(&mut message_buf).await {
                Ok(0) => {
                    // Client disconnected
                    if let Some(workspace_root) = self.remove_client(client_id) {
                        // Send workspace/didChangeWorkspaceFolders notification to server
                        let params = tower_lsp::lsp_types::DidChangeWorkspaceFoldersParams {
                            event: tower_lsp::lsp_types::WorkspaceFoldersChangeEvent {
                                added: vec![],
                                removed: vec![tower_lsp::lsp_types::WorkspaceFolder {
                                    uri: tower_lsp::lsp_types::Url::parse(&format!(
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
                            },
                        };

                        let _ = self.send_notification_to_server(
                            "workspace/didChangeWorkspaceFolders".to_string(),
                            Some(serde_json::to_value(params).unwrap()),
                        );
                    }
                    break;
                }
                Ok(_) => {
                    if let Ok(message) =
                        serde_json::from_str::<tower_lsp::jsonrpc::Message>(&message_buf)
                    {
                        if let Some(state) = self.get_client_state(client_id) {
                            // Check if it's a request (has method and id)
                            if message.method.is_some() && message.id.is_some() {
                                let server_id = self.get_next_server_request_id();
                                let _ = self.send_request_to_server(
                                    server_id,
                                    client_id,
                                    message.id.unwrap(),
                                    message.method.unwrap(),
                                    message.params,
                                );
                            }
                            // Check if it's a notification (has method but no id)
                            else if message.method.is_some() && message.id.is_none() {
                                let _ = self.send_notification_to_server(
                                    message.method.unwrap(),
                                    message.params,
                                );
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
}
