//! Remote devices over SSH.
//!
//! Metadata: `ssh -T -o BatchMode=yes <target> -- ghostmc api` with one JSON
//! request on stdin. BatchMode makes unknown host keys and missing keys fail
//! instead of prompting; we never relax host key checking.
//!
//! Attach: `ssh -t <target> -- '<tmux> -S <socket> attach-session -t <id>'`
//! in the user's local terminal. The only new process on the remote device is
//! the tmux client; the session and the programs in it are untouched.

use crate::client::decode;
use ghostmc_core::model::Device;
use ghostmc_core::shell;
use ghostmc_protocol::*;
use ghostmc_tmux::AttachMode;
use serde::de::DeserializeOwned;
use std::io::Write;
use std::process::{Command, Stdio};

pub struct RemoteClient {
    pub target: String,
    pub ghostmc: String,
    /// Extra `ssh` options, e.g. `-p 2222` (from tests or advanced config).
    pub ssh_opts: Vec<String>,
}

fn validate_target(t: &str) -> Result<(), ApiError> {
    if t.is_empty() || t.starts_with('-') || t.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ApiError::new(
            ErrorCode::Invalid,
            format!("invalid ssh target {t:?}"),
        ));
    }
    Ok(())
}

impl RemoteClient {
    pub fn for_device(d: &Device) -> Result<Self, ApiError> {
        let target = d.ssh_target.clone().ok_or_else(|| {
            ApiError::new(
                ErrorCode::Invalid,
                format!("device {} has no ssh target", d.name),
            )
        })?;
        validate_target(&target)?;
        let ssh_opts = std::env::var("GHOSTMC_SSH_OPTS")
            .ok()
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        Ok(Self {
            target,
            ghostmc: d.remote_ghostmc.clone().unwrap_or_else(|| "ghostmc".into()),
            ssh_opts,
        })
    }

    /// argv for a metadata call.
    pub fn api_argv(&self, allow_write: bool) -> Vec<String> {
        let mut remote = vec![self.ghostmc.clone(), "api".into()];
        if allow_write {
            remote.push("--allow-write".into());
        }
        let mut v = vec![
            "ssh".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
        ];
        v.extend(self.ssh_opts.iter().cloned());
        v.push("--".into());
        v.push(self.target.clone());
        v.push(shell::join(&remote));
        v
    }

    pub fn call_raw(
        &self,
        request: Request,
        allow_write: bool,
    ) -> Result<ResponseEnvelope, ApiError> {
        let argv = self.api_argv(allow_write);
        let env = RequestEnvelope {
            id: 1,
            protocol: PROTOCOL,
            request,
        };
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("ssh: {e}")))?;
        let mut line = serde_json::to_vec(&env)
            .map_err(|e| ApiError::new(ErrorCode::Internal, e.to_string()))?;
        line.push(b'\n');
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&line);
        }
        let out = child
            .wait_with_output()
            .map_err(|e| ApiError::new(ErrorCode::Unavailable, format!("ssh: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(resp_line) = stdout.lines().rev().find(|l| l.starts_with('{')) else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                format!(
                    "{} did not answer ({}): {}",
                    self.target,
                    out.status,
                    if err.is_empty() {
                        "no output; is ghostmc installed there?".into()
                    } else {
                        err
                    }
                ),
            ));
        };
        serde_json::from_str(resp_line).map_err(|e| {
            ApiError::new(
                ErrorCode::Internal,
                format!("bad response from {}: {e}", self.target),
            )
        })
    }

    pub fn call<T: DeserializeOwned>(
        &self,
        request: Request,
        allow_write: bool,
    ) -> Result<T, ApiError> {
        decode(self.call_raw(request, allow_write)?)
    }

    /// argv that attaches the local terminal to a remote session.
    pub fn attach_argv(&self, session: &SessionView, mode: AttachMode) -> Vec<String> {
        let tmux = ghostmc_tmux::attach_argv(
            &session.tmux_binary,
            &session.r#ref.socket_path,
            &session.r#ref.session_id,
            mode,
        );
        let mut v = vec!["ssh".into(), "-t".into()];
        v.extend(self.ssh_opts.iter().cloned());
        v.push("--".into());
        v.push(self.target.clone());
        v.push(shell::join(&tmux));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(target: &str) -> Device {
        Device {
            id: "dev_x".into(),
            name: "mini".into(),
            os: "macos".into(),
            hostname: None,
            is_local: false,
            ssh_target: Some(target.into()),
            remote_ghostmc: Some("/opt/my bin/ghostmc".into()),
            last_seen: None,
        }
    }

    #[test]
    fn rejects_option_like_targets() {
        assert!(RemoteClient::for_device(&device("-oProxyCommand=evil")).is_err());
        assert!(RemoteClient::for_device(&device("a b")).is_err());
        assert!(RemoteClient::for_device(&device("me@mini.tail")).is_ok());
    }

    #[test]
    fn api_argv_is_batch_and_quoted() {
        let c = RemoteClient {
            target: "me@mini".into(),
            ghostmc: "/opt/my bin/ghostmc".into(),
            ssh_opts: vec![],
        };
        let v = c.api_argv(false);
        assert_eq!(
            v,
            [
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "--",
                "me@mini",
                "'/opt/my bin/ghostmc' api"
            ]
        );
        assert!(!v.iter().any(|a| a.contains("StrictHostKeyChecking")));
        assert!(c.api_argv(true).last().unwrap().ends_with("--allow-write"));
    }
}
