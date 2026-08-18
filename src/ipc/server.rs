//! Unix-domain-socket IPC server.
//!
//! One task per connection; requests are dispatched to the daemon handler
//! and events from the [`EventBus`] are pushed to every client.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

use super::events::EventBus;
use super::protocol::{Request, Response, RpcErrorCode};

/// What the daemon exposes to the protocol layer.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, method: &str, params: Value) -> Result<Value, (RpcErrorCode, String)>;
}

pub fn socket_path(engine_dir: &Path) -> PathBuf {
    engine_dir.join("engine.sock")
}

#[cfg(unix)]
pub async fn serve(
    engine_dir: &Path,
    handler: Arc<dyn Handler>,
    events: EventBus,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    use tokio::net::UnixListener;

    std::fs::create_dir_all(engine_dir)
        .with_context(|| format!("cannot create {}", engine_dir.display()))?;
    let path = socket_path(engine_dir);
    // A previous daemon may have crashed without cleanup; the socket file
    // is only meaningful while a listener holds it.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot bind {}", path.display()))?;
    debug!("IPC listening on {}", path.display());

    let mut shutdown = shutdown;
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = std::fs::remove_file(&path);
                    return Ok(());
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("IPC accept failed")?;
                let handler = handler.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_client(stream, handler, events).await {
                        debug!("IPC client ended: {err:#}");
                    }
                });
            }
        }
    }
}

#[cfg(unix)]
async fn serve_client(
    stream: tokio::net::UnixStream,
    handler: Arc<dyn Handler>,
    events: EventBus,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut rx = events.subscribe();

    loop {
        tokio::select! {
            event = rx.recv() => {
                if let Ok(event) = event {
                    let frame = serde_json::to_string(&event)?;
                    write.write_all(frame.as_bytes()).await?;
                    write.write_all(b"\n").await?;
                }
                // Lagged receivers just skip events; they are advisory.
            }
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                if line.trim().is_empty() {
                    continue;
                }
                let response = match serde_json::from_str::<Request>(&line) {
                    Ok(request) => {
                        let id = request.id;
                        match handler.handle(&request.method, request.params) {
                            Ok(result) => Response::ok(id, result),
                            Err((code, message)) => Response::err(id, code, message),
                        }
                    }
                    Err(err) => {
                        warn!("malformed IPC frame: {err}");
                        Response::err(0, RpcErrorCode::InvalidParams, err.to_string())
                    }
                };
                let frame = serde_json::to_string(&response)?;
                write.write_all(frame.as_bytes()).await?;
                write.write_all(b"\n").await?;
            }
        }
    }
}

#[cfg(not(unix))]
pub async fn serve(
    _engine_dir: &Path,
    _handler: Arc<dyn Handler>,
    _events: EventBus,
    _shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    anyhow::bail!("the IPC server currently supports Unix sockets only; Windows named pipes land with the Windows build")
}

/// One-shot client call, used by the CLI (`status`, `inspect --daemon`).
#[cfg(unix)]
pub async fn call(engine_dir: &Path, method: &str, params: Value) -> Result<Value> {
    use tokio::net::UnixStream;

    let path = socket_path(engine_dir);
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("daemon not reachable at {}", path.display()))?;
    let (read, mut write) = stream.into_split();
    let request = Request { id: 1, method: method.to_owned(), params };
    let frame = serde_json::to_string(&request)?;
    write.write_all(frame.as_bytes()).await?;
    write.write_all(b"\n").await?;

    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        // Skip event frames; we only want our response.
        if let Ok(response) = serde_json::from_str::<Response>(&line) {
            if response.id == request.id {
                if let Some(error) = response.error {
                    anyhow::bail!("{:?}: {}", error.code, error.message);
                }
                return Ok(response.result.unwrap_or(Value::Null));
            }
        }
    }
    anyhow::bail!("daemon closed the connection without answering")
}

/// The client half of the same gap as `serve` above: without it the Windows
/// binary does not link at all, which is a worse answer than this sentence.
#[cfg(not(unix))]
pub async fn call(_engine_dir: &Path, _method: &str, _params: Value) -> Result<Value> {
    anyhow::bail!("the daemon currently supports Unix sockets only; Windows named pipes land with the Windows build")
}
