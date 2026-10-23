//! Local IPC server: newline-delimited JSON over a Unix socket in a private
//! (0700) directory. Peers must run as the same uid; the socket file is 0600.

use crate::service::Service;
use ghostmc_protocol::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Largest accepted request line.
const MAX_LINE: usize = 4 * 1024 * 1024;

pub fn handle_envelope(service: &Service, line: &str, allow_write: bool) -> ResponseEnvelope {
    let parsed: Result<RequestEnvelope, _> = serde_json::from_str(line);
    let (id, body) = match parsed {
        Err(e) => (
            0,
            ResponseBody::Error {
                error: ApiError::new(ErrorCode::Invalid, format!("bad request: {e}")),
            },
        ),
        Ok(env) if !env.protocol.compatible_with(PROTOCOL) => (
            env.id,
            ResponseBody::Error {
                error: ApiError::new(
                    ErrorCode::IncompatibleProtocol,
                    format!(
                        "client speaks protocol {}.{}, this ghostmc speaks {}.{}; upgrade one side",
                        env.protocol.major, env.protocol.minor, PROTOCOL.major, PROTOCOL.minor
                    ),
                ),
            },
        ),
        Ok(env) if !allow_write && !is_read_only(&env.request) => (
            env.id,
            ResponseBody::Error {
                error: ApiError::new(
                    ErrorCode::Forbidden,
                    "write requests are disabled for this caller (use --allow-write)",
                ),
            },
        ),
        Ok(env) => (
            env.id,
            match service.handle(env.request) {
                Ok(result) => ResponseBody::Ok { result },
                Err(error) => ResponseBody::Error { error },
            },
        ),
    };
    ResponseEnvelope {
        id,
        protocol: PROTOCOL,
        body,
    }
}

async fn serve_conn(service: Arc<Service>, stream: UnixStream) -> std::io::Result<()> {
    let me = unsafe { libc::geteuid() };
    match stream.peer_cred() {
        Ok(c) if c.uid() == me => {}
        Ok(c) => {
            tracing::warn!(uid = c.uid(), "rejected connection from another user");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!("peer credentials unavailable: {e}");
            return Ok(());
        }
    }
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut reader)
            .take(MAX_LINE as u64)
            .read_line(&mut line)
            .await?;
        if n == 0 {
            return Ok(());
        }
        if !line.ends_with('\n') && n >= MAX_LINE {
            return Ok(()); // oversized: drop the connection
        }
        let svc = service.clone();
        let req = line.clone();
        let resp = tokio::task::spawn_blocking(move || handle_envelope(&svc, req.trim_end(), true))
            .await
            .map_err(std::io::Error::other)?;
        let mut out = serde_json::to_vec(&resp).map_err(std::io::Error::other)?;
        out.push(b'\n');
        w.write_all(&out).await?;
    }
}

use tokio::io::AsyncReadExt;

/// Bind the socket, refusing to steal it from a running host.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        ghostmc_core::dirs::ensure_private_dir(dir)?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("a ghostmc host is already running on {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?; // stale socket from a crashed host
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Run the host until SIGINT/SIGTERM.
pub async fn run(service: Arc<Service>, socket: &Path) -> std::io::Result<()> {
    let listener = bind(socket)?;
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "ghostmc host listening");

    let maint = service.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let s = maint.clone();
            let _ = tokio::task::spawn_blocking(move || s.reconcile()).await;
        }
    });

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let svc = service.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_conn(svc, stream).await {
                                tracing::debug!("connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("accept failed: {e}"),
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        }
    }
    let _ = std::fs::remove_file(socket);
    tracing::info!("ghostmc host stopped");
    Ok(())
}
