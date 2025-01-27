use fork::{fork, Fork};
use lsp_muxd::WorkspaceManager;
use std::{
    io::ErrorKind,
    os::unix::net::{UnixListener, UnixStream},
    sync::Arc,
};
use tokio::{
    io::{self, stdin, stdout},
    sync::Mutex,
};
use tracing::info;

enum ProcessKind {
    Server(UnixListener),
    Client(UnixStream),
}

fn main() -> anyhow::Result<()> {
    let runtime_dir = dirs::runtime_dir().expect("RUNTIME dir must be set");
    let socket_path = runtime_dir.join("lsp-muxd.sock");
    let lock_file = runtime_dir.join("lsp-muxd.lock");
    let mut lock_file = fd_lock::RwLock::new(std::fs::File::create(lock_file)?);
    let lock_file = lock_file.write()?;
    let proc = match UnixStream::connect(&socket_path) {
        Ok(stream) => ProcessKind::Client(stream),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused || e.kind() == ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&socket_path);
            let listener = UnixListener::bind(&socket_path)?;
            match fork().unwrap() {
                Fork::Parent(_) => ProcessKind::Client(UnixStream::connect(&socket_path)?),
                Fork::Child => ProcessKind::Server(listener),
            }
        }
        Err(e) => return Err(e.into()),
    };
    drop(lock_file);
    match proc {
        ProcessKind::Server(listener) => run_background(listener),
        ProcessKind::Client(stream) => run_client(stream),
    }
}

#[tokio::main]
async fn run_background(listener: UnixListener) -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    info!("running server");
    let workspace_manager = Arc::new(Mutex::new(WorkspaceManager::new()));
    return lsp_muxd::run_multiplexer(workspace_manager, listener).await;
}

#[tokio::main]
async fn run_client(stream: UnixStream) -> anyhow::Result<()> {
    // tracing_subscriber::fmt().init();
    info!("running client");
    let stream = tokio::net::UnixStream::from_std(stream)?;
    let (mut read, mut write) = tokio::io::split(stream);
    let (mut stdin, mut stdout) = (stdin(), stdout());
    tokio::try_join!(
        io::copy(&mut read, &mut stdout),
        io::copy(&mut stdin, &mut write)
    )?;
    Ok(())
}
