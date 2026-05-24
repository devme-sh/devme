//! Unix-socket IPC server for the per-instance supervisor.
//!
//! Clients (TUI, CLI, agents) connect, send [`ClientMessage`]s, and receive
//! [`ServerMessage`]s, all framed by [`devme_ipc::FrameCodec`] and
//! carried as JSON in an [`Envelope`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use devme_config::Stack;
use devme_core::{
    ClientMessage, Envelope, ServerMessage, ServiceSnapshot, ServiceState, StepSnapshot, StepState,
};
use devme_ipc::FrameCodec;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio_util::codec::Framed;

/// Initial snapshot built from a [`Stack`] — every service starts `Stopped`,
/// every step starts `Unknown`. Subsequent process events mutate the real
/// state machine; this is just the pre-run picture.
fn initial_snapshot(stack: &Stack) -> (Vec<ServiceSnapshot>, Vec<StepSnapshot>) {
    let services = stack
        .service
        .iter()
        .map(|(name, _)| ServiceSnapshot {
            name: name.clone(),
            state: ServiceState::Stopped,
            pid: None,
            port: None,
            restart_count: 0,
        })
        .collect();
    let steps = stack
        .step
        .iter()
        .map(|(name, _)| StepSnapshot {
            name: name.clone(),
            state: StepState::Unknown,
        })
        .collect();
    (services, steps)
}

/// Server-side handle to the supervisor's IPC socket.
pub struct DaemonServer {
    listener: UnixListener,
    path: PathBuf,
    stack: Option<Arc<Stack>>,
}

impl DaemonServer {
    /// Bind a Unix socket at `path` with no loaded config. `Subscribe`
    /// returns an empty snapshot — useful for daemon-handshake tests.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        Self::bind_inner(path, None)
    }

    /// Bind a Unix socket and serve responses derived from `stack`.
    pub fn bind_with_stack(path: &Path, stack: Stack) -> std::io::Result<Self> {
        Self::bind_inner(path, Some(Arc::new(stack)))
    }

    fn bind_inner(path: &Path, stack: Option<Arc<Stack>>) -> std::io::Result<Self> {
        // Remove a stale socket file from a prior daemon that died without
        // cleanup. Bind would otherwise fail with EADDRINUSE.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            stack,
        })
    }

    /// Accept one connection and send a [`ServerMessage::Goodbye`] before
    /// closing — placeholder until the full request handling lands.
    pub async fn accept_and_goodbye(&self) -> std::io::Result<()> {
        let (stream, _) = self.listener.accept().await?;
        let mut conn = framed(stream);
        send(&mut conn, ServerMessage::Goodbye {
            reason: "placeholder".into(),
        })
        .await
    }

    /// Accept one connection, read one [`ClientMessage`], reply with a
    /// matching [`ServerMessage`], then close. Used as a building block
    /// for the eventual long-running serve loop and for tests.
    pub async fn handle_one_request(&self) -> std::io::Result<()> {
        let (stream, _) = self.listener.accept().await?;
        let mut conn = framed(stream);
        let bytes = match conn.next().await {
            Some(Ok(b)) => b,
            Some(Err(e)) => return Err(e),
            None => return Ok(()),
        };
        let req: Envelope<ClientMessage> = serde_json::from_slice(&bytes)?;
        let reply = handle_message(&req.payload, self.stack.as_deref());
        send(&mut conn, reply).await
    }

    /// Long-running serve loop. Accepts connections concurrently; each one
    /// handles its own request stream. Exits cleanly when any client sends
    /// [`ClientMessage::Shutdown`] — pending connections receive Goodbye
    /// and disconnect.
    pub async fn serve(self) -> std::io::Result<()> {
        // capacity=16 because we only ever send one shutdown event; the
        // extra slots are for transient drops with slow connection tasks.
        let (shutdown_tx, _) = broadcast::channel::<()>(16);
        let stack = self.stack.clone();

        loop {
            let mut shutdown_rx = shutdown_tx.subscribe();
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => return Err(e),
                    };
                    let shutdown_tx = shutdown_tx.clone();
                    let conn_shutdown_rx = shutdown_tx.subscribe();
                    let conn_stack = stack.clone();
                    tokio::spawn(handle_connection(
                        stream,
                        conn_stack,
                        shutdown_tx,
                        conn_shutdown_rx,
                    ));
                }
                _ = shutdown_rx.recv() => break,
            }
        }
        Ok(())
    }
}

/// Per-connection handler. Reads requests until the socket closes or until
/// the broadcast `shutdown_rx` fires. On `Shutdown`, signals the daemon to
/// stop accepting AND replies with Goodbye on this connection.
async fn handle_connection(
    stream: UnixStream,
    stack: Option<Arc<Stack>>,
    shutdown_tx: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let mut conn = framed(stream);

    loop {
        tokio::select! {
            item = conn.next() => {
                let bytes = match item {
                    Some(Ok(b)) => b,
                    Some(Err(_)) | None => return Ok(()),
                };
                let req: Envelope<ClientMessage> = match serde_json::from_slice(&bytes) {
                    Ok(env) => env,
                    Err(e) => {
                        let _ = send(&mut conn, ServerMessage::Error {
                            code: devme_core::ErrorCode::Usage,
                            message: format!("malformed request: {e}"),
                        }).await;
                        continue;
                    }
                };

                match req.payload {
                    ClientMessage::Shutdown => {
                        let _ = send(&mut conn, ServerMessage::Goodbye {
                            reason: "shutdown requested".into(),
                        }).await;
                        let _ = shutdown_tx.send(());
                        return Ok(());
                    }
                    other => {
                        let reply = handle_message(&other, stack.as_deref());
                        send(&mut conn, reply).await?;
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                let _ = send(&mut conn, ServerMessage::Goodbye {
                    reason: "daemon shutting down".into(),
                }).await;
                return Ok(());
            }
        }
    }
}

fn handle_message(msg: &ClientMessage, stack: Option<&Stack>) -> ServerMessage {
    match msg {
        ClientMessage::Subscribe { .. } => {
            let (services, steps) = match stack {
                Some(s) => initial_snapshot(s),
                None => (Vec::new(), Vec::new()),
            };
            ServerMessage::Subscribed { services, steps }
        }
        _ => ServerMessage::Goodbye {
            reason: format!("unhandled: {msg:?}"),
        },
    }
}

async fn send(
    conn: &mut Framed<UnixStream, FrameCodec>,
    msg: ServerMessage,
) -> std::io::Result<()> {
    let env = Envelope::new(msg);
    let bytes = serde_json::to_vec(&env)?;
    conn.send(bytes.as_slice()).await
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn framed(stream: UnixStream) -> Framed<UnixStream, FrameCodec> {
    Framed::new(stream, FrameCodec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use devme_core::ClientMessage;
    use tempfile::TempDir;
    use tokio_stream::StreamExt;

    #[tokio::test]
    async fn daemon_serves_goodbye_to_one_client() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");

        let server = DaemonServer::bind(&sock).unwrap();
        let server_task = tokio::spawn(async move { server.accept_and_goodbye().await });

        // Tiny delay so accept() runs before we connect.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let client = UnixStream::connect(&sock).await.unwrap();
        let mut client_framed = framed(client);

        let bytes = client_framed
            .next()
            .await
            .expect("expected one frame")
            .unwrap();
        let env: Envelope<ServerMessage> = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(env.payload, ServerMessage::Goodbye { .. }));

        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn daemon_replies_to_subscribe_with_initial_snapshot() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");

        let server = DaemonServer::bind(&sock).unwrap();
        let server_task = tokio::spawn(async move { server.handle_one_request().await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let client = UnixStream::connect(&sock).await.unwrap();
        let mut client_framed = framed(client);

        let req = Envelope::new(ClientMessage::Subscribe { services: vec![] });
        let bytes = serde_json::to_vec(&req).unwrap();
        client_framed.send(bytes.as_slice()).await.unwrap();

        let resp_bytes = client_framed
            .next()
            .await
            .expect("expected response")
            .unwrap();
        let resp: Envelope<ServerMessage> = serde_json::from_slice(&resp_bytes).unwrap();
        assert!(matches!(
            resp.payload,
            ServerMessage::Subscribed { ref services, ref steps } if services.is_empty() && steps.is_empty()
        ));

        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn subscribe_reflects_configured_services_in_stopped_state() {
        use devme_config::Stack;

        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");

        let stack = Stack::parse(
            r#"
schema_version = 1

[step.tools]
check = "true"

[service.db]
cmd = "postgres"

[service.backend]
cmd = "server"
"#,
        )
        .unwrap();

        let server = DaemonServer::bind_with_stack(&sock, stack).unwrap();
        let server_task = tokio::spawn(server.serve());

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let stream = UnixStream::connect(&sock).await.unwrap();
        let mut conn = framed(stream);
        let req = Envelope::new(ClientMessage::Subscribe { services: vec![] });
        conn.send(serde_json::to_vec(&req).unwrap().as_slice())
            .await
            .unwrap();
        let resp: Envelope<ServerMessage> =
            serde_json::from_slice(&conn.next().await.unwrap().unwrap()).unwrap();

        match resp.payload {
            ServerMessage::Subscribed { services, steps } => {
                let svc_names: Vec<_> = services.iter().map(|s| s.name.as_str()).collect();
                assert_eq!(svc_names, vec!["db", "backend"]);
                assert_eq!(steps.len(), 1);
                assert_eq!(steps[0].name, "tools");
                // All services start Stopped; the step starts Unknown.
                assert!(matches!(
                    services[0].state,
                    devme_core::ServiceState::Stopped
                ));
                assert!(matches!(
                    steps[0].state,
                    devme_core::StepState::Unknown
                ));
            }
            other => panic!("expected Subscribed, got {other:?}"),
        }

        // Clean shutdown.
        let shutdown = Envelope::new(ClientMessage::Shutdown);
        conn.send(serde_json::to_vec(&shutdown).unwrap().as_slice())
            .await
            .unwrap();
        let _ = conn.next().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), server_task).await;
    }

    #[tokio::test]
    async fn serve_handles_two_clients_then_shuts_down_on_request() {
        use std::time::Duration;

        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");

        let server = DaemonServer::bind(&sock).unwrap();
        let server_task = tokio::spawn(server.serve());

        tokio::time::sleep(Duration::from_millis(20)).await;

        // Client A subscribes; expects snapshot back.
        let stream_a = UnixStream::connect(&sock).await.unwrap();
        let mut a = framed(stream_a);
        let req = Envelope::new(ClientMessage::Subscribe { services: vec![] });
        a.send(serde_json::to_vec(&req).unwrap().as_slice())
            .await
            .unwrap();
        let resp_a: Envelope<ServerMessage> =
            serde_json::from_slice(&a.next().await.unwrap().unwrap()).unwrap();
        assert!(matches!(resp_a.payload, ServerMessage::Subscribed { .. }));

        // Client B subscribes on a separate connection, concurrently.
        let stream_b = UnixStream::connect(&sock).await.unwrap();
        let mut b = framed(stream_b);
        b.send(serde_json::to_vec(&req).unwrap().as_slice())
            .await
            .unwrap();
        let resp_b: Envelope<ServerMessage> =
            serde_json::from_slice(&b.next().await.unwrap().unwrap()).unwrap();
        assert!(matches!(resp_b.payload, ServerMessage::Subscribed { .. }));

        // Client A sends Shutdown; daemon should send Goodbye to both.
        let shutdown = Envelope::new(ClientMessage::Shutdown);
        a.send(serde_json::to_vec(&shutdown).unwrap().as_slice())
            .await
            .unwrap();

        let goodbye_a: Envelope<ServerMessage> =
            serde_json::from_slice(&a.next().await.unwrap().unwrap()).unwrap();
        assert!(matches!(goodbye_a.payload, ServerMessage::Goodbye { .. }));

        let goodbye_b: Envelope<ServerMessage> =
            serde_json::from_slice(&b.next().await.unwrap().unwrap()).unwrap();
        assert!(matches!(goodbye_b.payload, ServerMessage::Goodbye { .. }));

        // Server task should exit cleanly.
        let result = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("serve() didn't exit after Shutdown");
        result.unwrap().unwrap();
    }
}
