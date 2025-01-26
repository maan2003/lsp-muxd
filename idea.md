# LSP Multiplexer: A Language Server Protocol Proxy

## Overview

The LSP Multiplexer is a proxy server that enables multiple client workspaces to share a single Language Server Protocol (LSP) server instance. This project demonstrates how to efficiently manage multiple workspace roots under one language server, leveraging the LSP's workspace folders feature to provide seamless multi-root support.

## Why a Language Server Proxy?

Language servers can be resource-intensive, and running multiple instances (one per workspace) is often inefficient. The LSP Multiplexer solves this by:

1. Allowing multiple editor instances/windows to connect to a single shared language server
2. Managing workspace roots dynamically as clients connect and disconnect
3. Multiplexing requests and responses between clients and the server
4. Preserving context and state for each client while sharing server resources

## Architecture

### Connection Flow

1. The proxy listens on a Unix domain socket (`/tmp/lsp-multiplexer.sock`)
2. Clients connect to this socket instead of launching their own language server
3. The proxy manages the lifecycle of a single language server instance (rust-analyzer in this implementation)
4. All communication between clients and server flows through the proxy

### Core Components

#### WorkspaceManager

The `WorkspaceManager` is the heart of the proxy, responsible for:

- Tracking client connections and their workspace roots
- Managing the single server process
- Maintaining request/response mappings between clients and server
- Coordinating workspace folder changes

Key state tracking includes:
```rust
struct WorkspaceManager {
    server_process: Option<Child>,
    next_client_id: ClientId,
    client_states: HashMap<ClientId, ClientState>,
    request_map: HashMap<ServerRequestId, (ClientId, RequestId)>,
    workspace_roots: Vec<String>,
}
```

### Message Flow

#### Initial Connection

When a client connects, the following sequence occurs:

1. Client sends an "initialize" request with its workspace root
2. If this is the first client:
   - The proxy launches the language server
   - Forwards the initialize request to the server
   - Sets up message handling for server responses
3. For subsequent clients:
   - The proxy sends a "workspace/didChangeWorkspaceFolders" notification to add the new root
   - Manages the client connection without reinitializing the server

## Example Sequence

Here's a typical interaction sequence:

1. Client A connects with workspace root `/projectA`:
   ```json
   {
     "jsonrpc": "2.0",
     "id": 1,
     "method": "initialize",
     "params": {
       "rootUri": "file:///projectA",
       ...
     }
   }
   ```

2. The proxy:
   - Launches rust-analyzer (first client)
   - Forwards the initialize request
   - Stores the mapping between client A and `/projectA`

3. Client B connects with workspace root `/projectB`
   - The proxy sends to the server:
   ```json
   {
     "jsonrpc": "2.0",
     "method": "workspace/didChangeWorkspaceFolders",
     "params": {
       "event": {
         "added": [{
           "uri": "file:///projectB",
           "name": "projectB"
         }],
         "removed": []
       }
     }
   }
   ```

4. Both clients can now make concurrent requests:
   - The proxy maintains request ID mappings
   - Responses are routed back to the correct client
   - Notifications are broadcast as needed
