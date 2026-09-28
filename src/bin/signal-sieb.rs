//! `signal-sieb LISTEN UPSTREAM` — the sieve between the bot and signal-cli
//! (homeserver audit 3, B94; the rules are in `signal::sieb`).
//!
//! One upstream connection per client. Client lines are checked; allowed ones
//! go to signal-cli, the others are answered with a JSON-RPC error and never
//! reach it. Everything signal-cli writes goes back unchanged.

use anyhow::{bail, Context, Result};
use signal_seerr::signal::sieb::{check, ALLOWED, MAX_LINE};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let (Some(listen), Some(upstream), None) = (args.next(), args.next(), args.next()) else {
        bail!("usage: signal-sieb LISTEN-SOCKET UPSTREAM-SOCKET");
    };
    serve(&listen, &upstream).await
}

async fn serve(listen: &str, upstream: &str) -> Result<()> {
    let _ = std::fs::remove_file(listen);
    let l = UnixListener::bind(listen).with_context(|| format!("bind {listen}"))?;
    tracing::info!("signal-sieb: {listen} -> {upstream}, allowed: {ALLOWED:?}");
    loop {
        let (client, _) = l.accept().await?;
        let upstream = upstream.to_owned();
        tokio::spawn(async move {
            if let Err(e) = relay(client, &upstream).await {
                tracing::warn!("signal-sieb: connection ended: {e:#}");
            }
        });
    }
}

async fn relay(client: UnixStream, upstream: &str) -> Result<()> {
    let up = UnixStream::connect(upstream)
        .await
        .with_context(|| format!("connect {upstream}"))?;
    let (cr, cw) = client.into_split();
    let (ur, mut uw) = up.into_split();
    // Both directions write to the client: one writer, whole lines only.
    let cw = Arc::new(Mutex::new(cw));

    let back = {
        let cw = cw.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(ur).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut w = cw.lock().await;
                if w.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        })
    };

    let mut reader = BufReader::new(cr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await?;
        if n == 0 {
            break;
        }
        if buf.len() > MAX_LINE {
            bail!("a line longer than {MAX_LINE} bytes");
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        match check(line, ALLOWED) {
            Ok(()) => uw.write_all(format!("{line}\n").as_bytes()).await?,
            Err(answer) => {
                tracing::warn!("signal-sieb: refused a request ({})", answer.len());
                cw.lock()
                    .await
                    .write_all(format!("{answer}\n").as_bytes())
                    .await?;
            }
        }
    }
    back.abort();
    Ok(())
}
