use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use crate::protocol::{ClientId, RequestId, ServerRequestId};

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

    pub fn add_client(&mut self, workspace_root: String) -> Result<(ClientId, bool), std::io::Error> {
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
}