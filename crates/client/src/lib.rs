//! IPC client for talking to a devstack supervisor daemon.
//!
//! Wraps a framed Unix socket with typed `request` and `next_event`
//! helpers. The TUI and CLI consume this — no one else should be poking
//! at the raw socket.
//!
//! Wire format: length-prefixed JSON-lines envelope, see ADR-0008.

use std::path::Path;

use devstack_core::{ClientMessage, Envelope, ServerMessage};
use devstack_ipc::FrameCodec;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding response failed: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("connection closed before a reply arrived")]
    Closed,
}

/// A connected client. Owns a framed Unix socket; can send messages and
/// receive server messages until the daemon disconnects.
pub struct Client {
    framed: Framed<UnixStream, FrameCodec>,
}

impl Client {
    /// Connect to a daemon listening on the Unix socket at `path`.
    pub async fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).await?;
        Ok(Self {
            framed: Framed::new(stream, FrameCodec),
        })
    }

    /// Send one [`ClientMessage`]. Returns once the bytes are in the
    /// kernel's outbound buffer — not when the server replies.
    pub async fn send(&mut self, msg: ClientMessage) -> Result<(), ClientError> {
        let env = Envelope::new(msg);
        let bytes = serde_json::to_vec(&env)?;
        self.framed.send(bytes.as_slice()).await?;
        Ok(())
    }

    /// Wait for the next [`ServerMessage`]. Returns `None` if the server
    /// disconnected cleanly.
    pub async fn next_event(&mut self) -> Result<Option<ServerMessage>, ClientError> {
        let Some(item) = self.framed.next().await else {
            return Ok(None);
        };
        let bytes = item?;
        let env: Envelope<ServerMessage> = serde_json::from_slice(&bytes)?;
        Ok(Some(env.payload))
    }

    /// Send a message and wait for the very next reply. Convenience for
    /// single-shot request/response flows (CLI commands).
    pub async fn request(&mut self, msg: ClientMessage) -> Result<ServerMessage, ClientError> {
        self.send(msg).await?;
        self.next_event().await?.ok_or(ClientError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devstack_supervisor::daemon::DaemonServer;
    use tempfile::TempDir;

    #[tokio::test]
    async fn client_receives_goodbye_from_daemon() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");

        let server = DaemonServer::bind(&sock).unwrap();
        let server_task = tokio::spawn(async move { server.accept_and_goodbye().await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut client = Client::connect(&sock).await.unwrap();
        let msg = client.next_event().await.unwrap().unwrap();
        assert!(matches!(msg, ServerMessage::Goodbye { .. }));

        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn client_subscribes_and_receives_snapshot() {
        let dir = TempDir::new().unwrap();
        let sock = dir.path().join("d.sock");

        let server = DaemonServer::bind(&sock).unwrap();
        let server_task = tokio::spawn(async move { server.handle_one_request().await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut client = Client::connect(&sock).await.unwrap();
        let resp = client
            .request(ClientMessage::Subscribe { services: vec![] })
            .await
            .unwrap();
        assert!(matches!(
            resp,
            ServerMessage::Subscribed { ref services, ref steps } if services.is_empty() && steps.is_empty()
        ));

        server_task.await.unwrap().unwrap();
    }
}
