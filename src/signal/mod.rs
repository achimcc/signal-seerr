pub mod rpc;
pub mod sieb;

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

/// How many incoming messages wait for the dialog before the next one is
/// dropped. The dialog handles one message at a time and answers it before it
/// takes the next, so a queue this long is minutes of ordinary chat.
pub const INCOMING_QUEUE: usize = 64;

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<serde_json::Value, String>>>>>;

pub struct SignalClient {
    account: String,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    pending: Pending,
    next_id: AtomicU64,
    /// Incoming messages dropped because the dialog's queue was full.
    dropped: Arc<AtomicU64>,
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
        let (tx, rx) = mpsc::channel(INCOMING_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));

        let client = Arc::new(SignalClient {
            account: account.to_string(),
            writer: Arc::new(Mutex::new(write_half)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            dropped: dropped.clone(),
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
                        let Some(incoming) = note.into_incoming() else {
                            continue;
                        };
                        // NEVER an awaiting `send` (Audit 3, B124). This task
                        // also routes the responses to `call()`, and the
                        // dialog waits in `call()` for its reply to go out --
                        // so a reader waiting on the dialog is a deadlock
                        // that only the 30s timeout breaks, for every reply,
                        // for as long as somebody keeps writing. What does
                        // not fit is dropped and counted; the dialog's queue
                        // is the only buffer, and it is bounded.
                        match tx.try_send(incoming) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                let total = dropped.fetch_add(1, Ordering::Relaxed) + 1;
                                tracing::warn!(
                                    dropped = total,
                                    "the dialog is behind, an incoming message was dropped"
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => break,
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

    /// How many incoming messages were dropped so far because the dialog's
    /// queue was full.
    pub fn dropped_incoming(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// `params` must be a JSON object. The account gets merged into it below
    /// via `Value`'s index-assignment, which panics on anything that is
    /// neither an object nor `null` -- every current call site passes an
    /// object, but a future one passing an array or a scalar would crash the
    /// calling task instead of returning an `Err`.
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
        if let Err(e) = self.writer.lock().await.write_all(line.as_bytes()).await {
            // The entry has to go in before the write (the answer can arrive
            // before we would otherwise have registered it), so every path
            // that does not consume it has to take it back out.
            self.pending.lock().await.remove(&id);
            return Err(e.into());
        }

        // A call that never comes back would wedge the dialog for good.
        let answer = match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("the signal-cli reader stopped"));
            }
            Err(_) => {
                // A slow answer, not a dead socket: the bot carries on, so an
                // entry left behind here is the leak that actually
                // accumulates over time.
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("signal-cli did not answer {method} within 30s"));
            }
        };

        answer.map_err(|e| anyhow!("signal-cli rejected {method}: {e}"))
    }

    /// Resolves a Signal username ("alice.42") to the account id behind it.
    /// `Ok(None)` means the name does not exist -- a typo, not a failure.
    pub async fn resolve_username(&self, username: &str) -> Result<Option<Aci>> {
        let answer = self
            .call(
                "getUserStatus",
                serde_json::json!({ "username": [username] }),
            )
            .await?;
        Ok(aci_from_user_status(&answer))
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

fn aci_from_user_status(answer: &serde_json::Value) -> Option<Aci> {
    answer
        .as_array()?
        .iter()
        .find_map(|entry| entry.get("uuid")?.as_str().map(|s| Aci(s.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_username_yields_its_aci() {
        let answer = serde_json::json!([
            { "recipient": "u:alice.42", "number": null, "uuid": "aaaa-bbbb", "isRegistered": true }
        ]);
        assert_eq!(aci_from_user_status(&answer), Some(Aci("aaaa-bbbb".into())));
    }

    #[test]
    fn an_unknown_username_is_none_not_an_error() {
        // signal-cli answers with an entry whose uuid is null rather than with
        // an empty list. Reading "the list is not empty" as "the name exists"
        // would pin a mapping onto None.
        let answer = serde_json::json!([
            { "recipient": "u:nope.99", "number": null, "uuid": null, "isRegistered": false }
        ]);
        assert_eq!(aci_from_user_status(&answer), None);
    }

    #[test]
    fn an_empty_answer_is_none() {
        assert_eq!(aci_from_user_status(&serde_json::json!([])), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_call_removes_its_pending_entry() {
        // signal-cli being merely slow, not the socket being dead, is the
        // path that keeps happening while the bot otherwise runs fine -- and
        // it is the one that would grow `pending` by one entry per call,
        // forever, if the timeout did not clean up after itself.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("signal-cli.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        // Accept the connection but never answer it.
        let accept_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(stream);
        });

        let (client, _rx) = SignalClient::connect(&socket_path, "+490000")
            .await
            .unwrap();

        let call_task = tokio::spawn({
            let client = client.clone();
            async move { client.call("getUserStatus", serde_json::json!({})).await }
        });

        // The call is now waiting on its oneshot; let the clock run past the
        // 30s ceiling without actually waiting 30 real seconds.
        tokio::time::advance(std::time::Duration::from_secs(31)).await;

        let result = call_task.await.unwrap();
        assert!(result.is_err(), "the call should time out");
        assert!(
            client.pending.lock().await.is_empty(),
            "the timed-out entry must not linger in `pending`"
        );

        accept_task.abort();
    }

    /// Audit 3, B124 (B2-SS-2). The reader routes both the responses to
    /// `call()` and the incoming messages, and it used to hand an incoming
    /// message over with an awaiting `send` -- so once the dialog stopped
    /// reading (it is itself waiting in `call()` for a reply to go out), a
    /// full channel stopped the reader, the response queued up behind the
    /// notifications never got routed, and every reply ran into the 30s
    /// timeout. A flood from one sender was enough.
    ///
    /// Nobody reads `rx` here, on purpose: that is the dialog being busy.
    #[tokio::test]
    async fn a_flood_of_incoming_messages_does_not_stall_a_response() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("signal-cli.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let request_line = lines.next_line().await.unwrap().unwrap();
            let request: serde_json::Value = serde_json::from_str(&request_line).unwrap();
            let id = request["id"].as_str().unwrap().to_string();

            let mut burst = String::new();
            for n in 0..(INCOMING_QUEUE * 3) {
                let note = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "receive",
                    "params": {
                        "envelope": {
                            "sourceUuid": "aaaa-bbbb",
                            "dataMessage": { "message": format!("flood {n}") }
                        },
                        "account": "+490000"
                    }
                });
                burst.push_str(&format!("{note}\n"));
            }
            let response = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": {} });
            burst.push_str(&format!("{response}\n"));
            write_half.write_all(burst.as_bytes()).await.unwrap();
            // Hold the connection open: a closed socket would end the reader
            // for a different reason than the one under test.
            std::future::pending::<()>().await;
        });

        let (client, rx) = SignalClient::connect(&socket_path, "+490000")
            .await
            .unwrap();

        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.call("send", serde_json::json!({})),
        )
        .await;
        assert!(
            matches!(answer, Ok(Ok(_))),
            "the response must get through the flood, got {answer:?}"
        );
        // What did not fit was dropped, and counted -- not silently lost.
        assert_eq!(rx.len(), INCOMING_QUEUE);
        assert_eq!(client.dropped_incoming(), (INCOMING_QUEUE * 2) as u64);

        server_task.abort();
    }

    #[tokio::test]
    async fn a_response_reaches_the_call_that_is_waiting_for_it() {
        // The reader task must route a Response frame to the oneshot
        // registered under its id -- the single routing decision every other
        // feature in this crate depends on.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("signal-cli.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();

            // Take the id from the request `call()` actually sent, not a
            // hard-coded one -- otherwise the test would still pass even if
            // `call()` sent one id and waited on another, which is precisely
            // the defect worth catching.
            let request_line = lines.next_line().await.unwrap().unwrap();
            let request: serde_json::Value = serde_json::from_str(&request_line).unwrap();
            let id = request["id"].as_str().unwrap().to_string();

            // A notification arriving on the same stream while the call is
            // still in flight must not disturb it -- that is the demux's
            // real job. Written before the response, so the reader task
            // processes it first.
            let note = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "receive",
                "params": {
                    "envelope": {
                        "sourceUuid": "aaaa-bbbb",
                        "dataMessage": { "message": "blade runner" }
                    },
                    "account": "+490000"
                }
            });
            write_half
                .write_all(format!("{note}\n").as_bytes())
                .await
                .unwrap();

            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "timestamp": 1234 }
            });
            write_half
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        });

        let (client, mut rx) = SignalClient::connect(&socket_path, "+490000")
            .await
            .unwrap();

        let result = client
            .call("getUserStatus", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({ "timestamp": 1234 }));

        let incoming = rx
            .recv()
            .await
            .expect("the notification must still arrive, undisturbed by the call in flight");
        assert_eq!(incoming.from.0, "aaaa-bbbb");
        assert_eq!(incoming.text, "blade runner");

        server_task.await.unwrap();
    }
}
