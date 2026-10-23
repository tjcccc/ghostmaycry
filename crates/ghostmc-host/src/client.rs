//! Synchronous client used by the CLI and the desktop app. Talks to the
//! running host if there is one, otherwise executes requests in-process
//! against the same registry ("direct mode").

use crate::service::Service;
use ghostmc_protocol::*;
use serde::de::DeserializeOwned;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub struct HostConnection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl HostConnection {
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(60)))?;
        Ok(Self {
            reader: BufReader::new(s.try_clone()?),
            writer: s,
        })
    }

    pub fn roundtrip(&mut self, env: &RequestEnvelope) -> Result<ResponseEnvelope, ApiError> {
        let io = |e: std::io::Error| {
            ApiError::new(ErrorCode::Unavailable, format!("host connection: {e}"))
        };
        let mut line = serde_json::to_vec(env)
            .map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))?;
        line.push(b'\n');
        self.writer.write_all(&line).map_err(io)?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp).map_err(io)? == 0 {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                "host closed the connection",
            ));
        }
        serde_json::from_str(&resp)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("bad response: {e}")))
    }
}

pub enum Backend {
    Host(std::sync::Mutex<HostConnection>),
    Direct(Arc<Service>),
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn decode<T: DeserializeOwned>(resp: ResponseEnvelope) -> Result<T, ApiError> {
    match resp.body {
        ResponseBody::Ok { result } => serde_json::from_value(result).map_err(|e| {
            ApiError::new(ErrorCode::Internal, format!("unexpected result shape: {e}"))
        }),
        ResponseBody::Error { error } => Err(error),
    }
}

impl Backend {
    /// Connect to the host at the default socket, or fall back to direct mode.
    pub fn connect_or_direct() -> Result<Self, String> {
        Self::connect_or_direct_at(&ghostmc_core::dirs::host_socket_path())
    }

    pub fn connect_or_direct_at(socket: &Path) -> Result<Self, String> {
        match HostConnection::connect(socket) {
            Ok(c) => Ok(Backend::Host(std::sync::Mutex::new(c))),
            Err(_) => Ok(Backend::Direct(Arc::new(Service::open_default(false)?))),
        }
    }

    pub fn is_host(&self) -> bool {
        matches!(self, Backend::Host(_))
    }

    pub fn call_raw(&self, request: Request) -> Result<ResponseEnvelope, ApiError> {
        let env = RequestEnvelope {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            protocol: PROTOCOL,
            request,
        };
        match self {
            Backend::Host(c) => c.lock().unwrap_or_else(|p| p.into_inner()).roundtrip(&env),
            Backend::Direct(s) => Ok(ResponseEnvelope {
                id: env.id,
                protocol: PROTOCOL,
                body: match s.handle(env.request) {
                    Ok(result) => ResponseBody::Ok { result },
                    Err(error) => ResponseBody::Error { error },
                },
            }),
        }
    }

    pub fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T, ApiError> {
        decode(self.call_raw(request)?)
    }
}
