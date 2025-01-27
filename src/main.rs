use lsp_muxd::WorkspaceManager;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let socket_path = dirs::runtime_dir()
        .expect("RUNTIME dir must be set")
        .join("lsp-mux.sock");
    let workspace_manager = Arc::new(Mutex::new(WorkspaceManager::new()));
    lsp_muxd::run_multiplexer(&socket_path, workspace_manager).await
}
