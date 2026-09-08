pub mod rpc;

use crate::model::Aci;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use rpc::{Frame, Incoming};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Everything the dialog needs in order to talk. A trait so the state machine
/// can be tested without signal-cli, a socket or a network.
#[async_trait]
pub trait Messenger: Send + Sync {
    async fn send(&self, to: &Aci, text: &str) -> Result<()>;
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<serde_json::Value, String>>>>>;

pub struct SignalClient {
    account: String,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    pending: Pending,
    next_id: AtomicU64,
}

impl SignalClient {
    pub async fn connect(
        socket: &Path,
        account: &str,
    ) -> Result<(Arc<SignalClient>, mpsc::Receiver<Incoming>)> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("cannot connect to signal-cli socket {}", socket.display()))?;
        let (read_half, write_half) = stream.into_split();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel(64);

        let client = Arc::new(SignalClient {
            account: account.to_string(),
            writer: Arc::new(Mutex::new(write_half)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match rpc::parse_line(&line) {
                    Ok(Frame::Response { id, result }) => {
                        if let Some(waiter) = pending.lock().await.remove(&id) {
                            let _ = waiter.send(result);
                        }
                    }
                    Ok(Frame::Notification { note }) => {
                        if let Some(incoming) = note.into_incoming() {
                            if tx.send(incoming).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Frame::Other) => tracing::debug!("unmodelled frame"),
                    Err(e) => tracing::warn!(error = %e, "cannot parse a line from signal-cli"),
                }
            }
            tracing::error!("the signal-cli socket closed");
        });

        Ok((client, rx))
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let mut request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "id": id,
            "params": params,
        });
        // Multi-account mode wants the account on every call; single-account
        // mode ignores it. Always sending it means the daemon can be started
        // either way without a second code path.
        request["params"]["account"] = serde_json::Value::String(self.account.clone());

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.writer.lock().await.write_all(line.as_bytes()).await?;

        // A call that never comes back would wedge the dialog for good.
        let answer = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow!("signal-cli did not answer {method} within 30s"))?
            .map_err(|_| anyhow!("the signal-cli reader stopped"))?;

        answer.map_err(|e| anyhow!("signal-cli rejected {method}: {e}"))
    }
}

#[async_trait]
impl Messenger for SignalClient {
    async fn send(&self, to: &Aci, text: &str) -> Result<()> {
        self.call(
            "send",
            serde_json::json!({ "recipient": [to.0], "message": text }),
        )
        .await
        .map(|_| ())
    }
}
