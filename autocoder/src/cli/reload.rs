//! `autocoder reload` — connect to the running daemon's control socket
//! and request a config reload. The CLI does not parse or apply the
//! config itself; the daemon owns that work and reports back the
//! per-section result.

use crate::control_socket;
use anyhow::{Result, anyhow};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn execute() -> Result<()> {
    execute_at(&control_socket::socket_path()).await
}

pub async fn execute_at(socket: &Path) -> Result<()> {
    let mut stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(e) => {
            return Err(anyhow!(
                "could not connect to control socket {}: {e}\n\
                 hint: the daemon may not be running, or may be running under a \
                 different user. Try `sudo -u autocoder autocoder reload`.",
                socket.display(),
            ));
        }
    };
    stream
        .write_all(b"{\"action\":\"reload\"}\n")
        .await
        .map_err(|e| anyhow!("writing to control socket: {e}"))?;
    let (read_half, _write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| anyhow!("reading control-socket response: {e}"))?;
    if line.is_empty() {
        return Err(anyhow!("control socket closed without responding"));
    }
    let resp: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| anyhow!("parsing control-socket response: {e}\nraw: {line}"))?;
    let pretty = serde_json::to_string_pretty(&resp).unwrap_or_else(|_| line.clone());
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if ok {
        println!("{pretty}");
        Ok(())
    } else {
        eprintln!("{pretty}");
        Err(anyhow!("daemon rejected reload"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// Spawn a minimal fake server on a tempfile Unix socket that
    /// responds to a single connection with the given JSON line, then
    /// shuts down. Returns the socket path.
    async fn fake_server(response: &'static str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let response_owned = response.to_string();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut buf = String::new();
            // Consume the request line so the client doesn't see ECONNRESET.
            let _ = reader.read_line(&mut buf).await;
            let mut bytes = response_owned.into_bytes();
            if !bytes.ends_with(b"\n") {
                bytes.push(b'\n');
            }
            let _ = write_half.write_all(&bytes).await;
            let _ = write_half.shutdown().await;
        });
        (dir, socket)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exits_zero_on_ok_response() {
        let (_dir, socket) = fake_server(r#"{"ok":true,"applied":[],"requires_restart":[],"unchanged":["github"]}"#).await;
        let res = execute_at(&socket).await;
        assert!(res.is_ok(), "expected Ok on ok=true response, got: {res:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exits_nonzero_on_failure_response() {
        let (_dir, socket) = fake_server(r#"{"ok":false,"error":"x"}"#).await;
        let res = execute_at(&socket).await;
        assert!(res.is_err(), "expected Err on ok=false response, got: {res:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn errors_when_daemon_not_running() {
        let dir = TempDir::new().unwrap();
        let socket = dir.path().join("nope.sock");
        let res = execute_at(&socket).await;
        let err = res.expect_err("must fail when socket missing");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(socket.to_string_lossy().as_ref()),
            "error must name socket path: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("daemon")
                || msg.to_lowercase().contains("not running")
                || msg.to_lowercase().contains("connect"),
            "error must hint at cause: {msg}"
        );
    }
}
