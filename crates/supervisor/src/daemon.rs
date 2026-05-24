//! Unix-socket IPC server for the per-instance supervisor.
//!
//! Clients (TUI, CLI, agents) connect, send [`ClientMessage`]s, and receive
//! [`ServerMessage`]s, all framed by [`devme_ipc::FrameCodec`] and
//! carried as JSON in an [`Envelope`].

use std::path::{Path, PathBuf};

use devme_core::{ClientMessage, Envelope, ServerMessage};
use devme_ipc::FrameCodec;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::Framed;

/// Server-side handle to the supervisor's IPC socket.
pub struct DaemonServer {
    listener: UnixListener,
    path: PathBuf,
}

impl DaemonServer {
    /// Bind a Unix socket at `path`. The file is created here and unlinked
    /// when the [`DaemonServer`] is dropped.
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        // Remove a stale socket file from a prior daemon that died without
        // cleanup. Bind would otherwise fail with EADDRINUSE.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
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
        let reply = handle_message(&req.payload);
        send(&mut conn, reply).await
    }
}

fn handle_message(msg: &ClientMessage) -> ServerMessage {
    match msg {
        ClientMessage::Subscribe { .. } => ServerMessage::Subscribed {
            services: Vec::new(),
            steps: Vec::new(),
        },
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
}
