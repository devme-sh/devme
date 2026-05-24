//! `devme-shared-supervisor` — per-repo daemon for `scope = "repo"` services.
//!
//! Bound at `paths::shared_socket(cwd)` (under the repo-hash directory),
//! it's spawned on demand by the first instance daemon in a repo that
//! sees a `repo`-scoped service. See ADR-0007 for the lifecycle model.
//!
//! Scope of this binary today (task #23):
//! - Resolve repo identity from cwd.
//! - Bind the shared socket. Refuse to start if another shared daemon for
//!   this repo is already bound.
//! - Accept connections and respond to `Subscribe` with an empty snapshot
//!   tagged with the shared instance id. Real `repo`-scoped service
//!   ownership and ref-counted lifecycle land in #24.

use std::path::PathBuf;
use std::sync::Arc;

use devme_core::{
    ClientMessage, Envelope, InstanceInfo, NoticeLevel, ServerMessage,
};
use devme_ipc::FrameCodec;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, mpsc};
use tokio_util::codec::Framed;

fn main() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("devme-shared-supervisor: tokio init failed: {e}");
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async {
        match real_main().await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("devme-shared-supervisor: {e}");
                1
            }
        }
    });
    std::process::exit(code);
}

async fn real_main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let repo_id = devme_config::paths::repo_id(&cwd);
    let sock = devme_config::paths::shared_socket(&cwd)?;

    let listener = match try_bind(&sock).await? {
        Some(l) => l,
        None => {
            return Err(anyhow::anyhow!(
                "shared daemon already running for repo {} at {}",
                repo_id,
                sock.display()
            ));
        }
    };

    let info = InstanceInfo {
        id: format!("shared::{repo_id}"),
        label: format!("shared ({})", repo_id_short(&repo_id)),
        cwd: cwd.display().to_string(),
    };

    let shutdown = Arc::new(Mutex::new(false));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(repo_id = %repo_id, socket = %sock.display(), "shared supervisor up");

    let result = serve(listener, info, shutdown.clone()).await;
    let _ = std::fs::remove_file(&sock);
    result
}

/// Try to bind `path`. Returns `Ok(None)` if another process already owns
/// the socket (a probe connection succeeded), `Ok(Some(listener))` on
/// successful bind, or `Err` for unexpected I/O failure.
async fn try_bind(path: &std::path::Path) -> anyhow::Result<Option<UnixListener>> {
    if UnixStream::connect(path).await.is_ok() {
        return Ok(None);
    }
    // Stale socket left by a crashed previous daemon — remove and rebind.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    Ok(Some(listener))
}

async fn serve(
    listener: UnixListener,
    info: InstanceInfo,
    shutdown: Arc<Mutex<bool>>,
) -> anyhow::Result<()> {
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<()>();

    loop {
        if *shutdown.lock().await {
            return Ok(());
        }
        tokio::select! {
            _ = notify_rx.recv() => return Ok(()),
            res = listener.accept() => match res {
                Ok((stream, _addr)) => {
                    let info = info.clone();
                    let shutdown_t = shutdown.clone();
                    let notify_t = notify_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, info, shutdown_t, notify_t).await {
                            tracing::debug!(?e, "connection ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(?e, "accept failed");
                    return Err(e.into());
                }
            }
        }
    }
}

async fn handle(
    stream: UnixStream,
    info: InstanceInfo,
    shutdown: Arc<Mutex<bool>>,
    notify: mpsc::UnboundedSender<()>,
) -> anyhow::Result<()> {
    let mut framed = Framed::new(stream, FrameCodec);
    while let Some(msg) = framed.next().await {
        let bytes = msg?;
        let env: Envelope<ClientMessage> = serde_json::from_slice(&bytes)?;
        let reply = match env.payload {
            ClientMessage::Subscribe { .. } => Some(ServerMessage::Subscribed {
                instance: info.clone(),
                services: vec![],
                steps: vec![],
            }),
            ClientMessage::Unsubscribe => None,
            ClientMessage::Shutdown => {
                let reply = ServerMessage::Goodbye {
                    reason: "shutdown requested".into(),
                };
                let env = Envelope::new(reply);
                let out = serde_json::to_vec(&env)?;
                let _ = framed.send(out.as_slice()).await;
                *shutdown.lock().await = true;
                let _ = notify.send(());
                return Ok(());
            }
            _ => Some(ServerMessage::Notice {
                level: NoticeLevel::Warn,
                message: "shared supervisor: repo-scoped services not yet implemented (#24)"
                    .into(),
            }),
        };
        if let Some(r) = reply {
            let env = Envelope::new(r);
            let out = serde_json::to_vec(&env)?;
            framed.send(out.as_slice()).await?;
        }
    }
    Ok(())
}

fn repo_id_short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

#[allow(dead_code)]
fn _unused(_: PathBuf) {}
