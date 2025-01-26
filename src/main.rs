use lsp_multiplexer::WorkspaceManager;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let socket_path = "/tmp/lsp-multiplexer.sock";
    let workspace_manager = Arc::new(Mutex::new(WorkspaceManager::new()));
    lsp_multiplexer::run_multiplexer(socket_path, workspace_manager).await
}
