//! Control socket between the daemon and everything else.
//!
//! The daemon owns the identity key, the iroh endpoint and the media ports, so
//! every other entry point has to ask it rather than act on its own. A `dial`
//! subprocess that built its own endpoint would carry a different `EndpointId`
//! -- the callee would see a stranger -- and could not own the media ports
//! anyway. That is why this exists.
//!
//! Three consumers: the CLI, `scripts/smoke.sh`, and the Omarchy shell plugin.
//!
//! Wire format is newline-delimited JSON, one request and one response per
//! connection. Deliberately boring: a QML plugin should be able to speak it
//! with no library.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Place a call. Resolves `name` against contacts, then rings.
    Dial { name: String },
    /// Invite someone into the call already in progress.
    Invite { name: String },
    /// End the current call.
    Hangup,
    /// Everything a status line, a bar widget or a test needs.
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error { message: String },
    Status(Status),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// "idle" | "ringing_out" | "ringing_in" | "in_call"
    pub state: String,
    pub endpoint_id: String,
    pub peers: Vec<PeerStatus>,
    /// Direct paths vs relay paths, summed across peers. The plugin renders
    /// this, and it is how a user tells "laggy" from "relayed".
    pub paths_direct: u32,
    pub paths_relay: u32,
}

/// Per-peer counters. `frames_decoded` is what `smoke.sh` asserts on: counting
/// datagrams only proves plumbing, and would pass with a caps typo or a dead
/// decoder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub id: String,
    pub datagrams_in: u64,
    pub datagrams_out: u64,
    pub frames_decoded: u64,
}

/// A request handed to the daemon, with a channel for its answer.
pub type Envelope = (Request, oneshot::Sender<Response>);

pub fn socket_path() -> PathBuf {
    resolve_socket_path(
        std::env::var("OMACALL_SOCKET_DIR").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

/// Pure, so it can be tested without mutating process environment -- which is
/// unsafe in edition 2024 and racy across test threads regardless.
pub fn resolve_socket_path(override_dir: Option<&str>, runtime_dir: Option<&str>) -> PathBuf {
    let base = override_dir.or(runtime_dir).unwrap_or("/tmp");
    Path::new(base).join("omacall.sock")
}

/// Bind the control socket, clearing a stale one left by a crashed daemon.
///
/// "Stale" means the file exists but nothing answers. A live daemon must not be
/// silently displaced, so a socket that still accepts connections is an error.
pub async fn bind(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => anyhow::bail!("another omacall daemon is already listening on {path:?}"),
            Err(_) => {
                tokio::fs::remove_file(path)
                    .await
                    .with_context(|| format!("removing stale socket {path:?}"))?;
            }
        }
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    UnixListener::bind(path).with_context(|| format!("binding {path:?}"))
}

/// Accept forever, forwarding each request to the daemon over `tx`.
pub async fn serve(listener: UnixListener, tx: mpsc::Sender<Envelope>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, tx).await {
                tracing::debug!("ipc connection ended: {e}");
            }
        });
    }
}

async fn handle_conn(stream: UnixStream, tx: mpsc::Sender<Envelope>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let (rtx, rrx) = oneshot::channel();
                if tx.send((req, rtx)).await.is_err() {
                    Response::Error { message: "daemon is shutting down".into() }
                } else {
                    rrx.await.unwrap_or(Response::Error {
                        message: "daemon dropped the request".into(),
                    })
                }
            }
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut buf = serde_json::to_vec(&response)?;
        buf.push(b'\n');
        write.write_all(&buf).await?;
        write.flush().await?;
    }
    Ok(())
}

/// Send one request to a running daemon and wait for its reply.
pub async fn request(path: &Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("no daemon listening on {path:?} -- is omacall.service running?"))?;
    let (read, mut write) = stream.into_split();

    let mut buf = serde_json::to_vec(req)?;
    buf.push(b'\n');
    write.write_all(&buf).await?;
    write.flush().await?;

    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await?
        .context("daemon closed the connection without replying")?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_fake_daemon(path: &Path) -> mpsc::Receiver<Envelope> {
        let listener = bind(path).await.unwrap();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(serve(listener, tx));
        rx
    }

    #[tokio::test]
    async fn request_and_response_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("omacall.sock");
        let mut rx = spawn_fake_daemon(&path).await;

        tokio::spawn(async move {
            while let Some((req, reply)) = rx.recv().await {
                let r = match req {
                    Request::Status => Response::Status(Status {
                        state: "in_call".into(),
                        endpoint_id: "abc".into(),
                        peers: vec![PeerStatus {
                            name: "carlos".into(),
                            frames_decoded: 42,
                            ..Default::default()
                        }],
                        paths_direct: 1,
                        paths_relay: 0,
                    }),
                    _ => Response::Ok,
                };
                let _ = reply.send(r);
            }
        });

        let got = request(&path, &Request::Status).await.unwrap();
        match got {
            Response::Status(s) => {
                assert_eq!(s.state, "in_call");
                assert_eq!(s.peers[0].frames_decoded, 42);
            }
            other => panic!("expected status, got {other:?}"),
        }

        assert_eq!(
            request(&path, &Request::Dial { name: "carlos".into() }).await.unwrap(),
            Response::Ok
        );
    }

    #[tokio::test]
    async fn malformed_request_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("omacall.sock");
        let mut rx = spawn_fake_daemon(&path).await;
        tokio::spawn(async move {
            while let Some((_, reply)) = rx.recv().await {
                let _ = reply.send(Response::Ok);
            }
        });

        // Garbage first, then a valid request on the same connection: the
        // daemon must answer both rather than dropping the client.
        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(b"{not json}\n").await.unwrap();
        write.flush().await.unwrap();
        let mut lines = BufReader::new(read).lines();
        let first: Response = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert!(matches!(first, Response::Error { .. }));

        write.write_all(b"{\"cmd\":\"hangup\"}\n").await.unwrap();
        write.flush().await.unwrap();
        let second: Response = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(second, Response::Ok);
    }

    #[tokio::test]
    async fn stale_socket_is_reclaimed_but_a_live_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("omacall.sock");

        // A leftover file with nothing behind it must not block startup.
        tokio::fs::write(&path, b"").await.unwrap();
        let listener = bind(&path).await.expect("stale socket should be reclaimed");

        // But a daemon that is actually listening must never be displaced.
        let (tx, _rx) = mpsc::channel(8);
        tokio::spawn(serve(listener, tx));
        assert!(
            bind(&path).await.is_err(),
            "a second daemon must refuse to steal a live socket"
        );
    }

    #[tokio::test]
    async fn client_reports_a_missing_daemon_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let err = request(&dir.path().join("nope.sock"), &Request::Status)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no daemon listening"), "unhelpful error: {err}");
    }

    #[test]
    fn socket_path_prefers_override_then_runtime_dir_then_tmp() {
        assert_eq!(
            resolve_socket_path(Some("/a"), Some("/b")),
            Path::new("/a/omacall.sock")
        );
        assert_eq!(
            resolve_socket_path(None, Some("/run/user/1000")),
            Path::new("/run/user/1000/omacall.sock")
        );
        assert_eq!(resolve_socket_path(None, None), Path::new("/tmp/omacall.sock"));
    }
}
