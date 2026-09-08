use anyhow::Result;
use signal_seerr::config::{Config, Secrets};
use signal_seerr::dialog::Dialog;
use signal_seerr::directory::{apply, diff::plan, AuthentikClient, Directory, Member};
use signal_seerr::i18n::Catalogue;
use signal_seerr::model::Aci;
use signal_seerr::seerr::SeerrClient;
use signal_seerr::signal::{Messenger, SignalClient};
use signal_seerr::state::State;
use signal_seerr::webhook;
use std::sync::{Arc, RwLock};

/// Waits for signal-cli's socket to appear, polling every 500ms, but never
/// past `limit`. signal-cli starts alongside us and creates the socket
/// itself; waiting for it is right, waiting for ever is not -- an unbounded
/// wait inside a container boot is exactly how a guest never finishes
/// booting and a deploy aborts.
///
/// Uses `tokio::time::Instant` rather than `std::time::Instant` throughout,
/// so both the deadline check and the poll sleep answer to the same clock --
/// under `tokio::time::pause`, that clock is the one a test controls.
async fn wait_for_socket(path: &std::path::Path, limit: std::time::Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + limit;
    while !path.exists() {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("{} did not appear within {limit:?}", path.display());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Ok(())
}

/// The mapping table, shared by the reconciler, the dialog and the webhook.
///
/// A STD lock, not a tokio one, and that is the whole design: `Directory` is
/// a synchronous trait, and the critical section here is a `find` over a
/// handful of entries with no `await` in it. The reconciler never holds this
/// lock across an await either -- it clones, works on the copy, and swaps
/// the result in (see below).
struct SharedDirectory {
    state: Arc<RwLock<State>>,
    media_group: String,
}

impl Directory for SharedDirectory {
    fn lookup(&self, aci: &Aci) -> Option<Member> {
        // NOT try_read(). A failed try_read would return None, and None here
        // means "I do not know you" -- so every message that happened to
        // arrive while the reconciler was writing would earn a known person
        // the greeting meant for a stranger, and the next message would
        // work again. A fault that repairs itself is the expensive kind.
        let state = self
            .state
            .read()
            .expect("the mapping lock is never poisoned");
        let entry = state.by_aci(aci)?;
        Some(Member {
            authentik_username: entry.authentik_username.clone(),
            locale: entry.locale,
            allowed: entry.groups.iter().any(|g| g == &self.media_group),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/signal-seerr.toml".into());
    let config = Config::load(std::path::Path::new(&path))?;
    let secrets = Secrets::read(&config)?;
    let catalogue = Arc::new(Catalogue::load());

    // signal-cli creates the socket and starts alongside us. Waiting is
    // right; waiting for ever is not. An unbounded wait inside a container
    // boot is exactly how a guest never finishes booting and a deploy
    // aborts.
    wait_for_socket(&config.signal_socket, std::time::Duration::from_secs(60)).await?;

    let (signal, mut incoming) =
        SignalClient::connect(&config.signal_socket, &config.signal_account).await?;

    let state = Arc::new(RwLock::new(State::load(&config.state_file)?));
    let seerr = Arc::new(SeerrClient::new(&config.seerr_url, secrets.seerr_key));
    let authentik = AuthentikClient::new(&config.authentik_url, secrets.authentik_token);

    // 1. The reconciler. Explicitly typed `JoinHandle<()>`: the loop below
    // never breaks, so left to infer its own type it would be the never
    // type `!` -- which happens to fall back to `()` today, but pinning it
    // down here means that stays true regardless, and documents that this
    // task is meant to run forever, ending only via panic or the process
    // exiting.
    let reconciler_task: tokio::task::JoinHandle<()> = {
        let state = state.clone();
        let signal = signal.clone();
        let catalogue = catalogue.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(config.poll_seconds));
            loop {
                ticker.tick().await;
                let users = match authentik.users("de").await {
                    Ok(users) => users,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot read authentik");
                        continue;
                    }
                };

                // Work on a COPY and swap it in. `apply` awaits on name
                // resolution and on sending the greeting; holding the lock
                // across those awaits would block every lookup for the
                // length of a network round trip, and with a std lock it
                // would block the runtime thread itself.
                let mut working = {
                    let guard = state.read().expect("the mapping lock is never poisoned");
                    guard.clone()
                };

                let changes = plan(&working, &users);
                if changes.is_empty() {
                    continue;
                }

                // `apply` returns nothing: every failure inside it (a
                // resolve, a greeting, a farewell) is already logged and
                // swallowed there, so there is no outcome left to branch on
                // here.
                let resolver = signal.clone();
                apply(
                    &mut working,
                    changes,
                    &users,
                    signal.as_ref() as &dyn Messenger,
                    &catalogue,
                    move |name: String| {
                        let s = resolver.clone();
                        async move { s.resolve_username(&name).await }
                    },
                )
                .await;

                if let Err(e) = working.save(&config.state_file) {
                    tracing::error!(error = %e, "cannot persist the mapping");
                }
                *state.write().expect("the mapping lock is never poisoned") = working;
            }
        })
    };

    // 2. The webhook listener.
    let webhook_task = {
        let app = webhook::router(webhook::WebhookState {
            messenger: signal.clone(),
            seerr: seerr.clone(),
            directory: state.clone(),
            catalogue: catalogue.clone(),
            token: Arc::new(secrets.webhook_token),
            jellyfin_url: config.jellyfin_url.clone(),
        });
        let listener = tokio::net::TcpListener::bind(config.webhook_listen).await?;
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "the webhook listener stopped");
            }
        })
    };

    // 3. The dialog, in its own task too. `seerr` is moved in as
    // `Arc<SeerrClient>` -- the same `Arc` already shared into the webhook
    // above (there as `Arc<dyn Requests>`, via the ordinary unsized
    // coercion). `Dialog` is generic over `R: Requests` and owns its `R`,
    // so the blanket `impl<T: Requests + ?Sized> Requests for Arc<T>` in
    // `seerr::mod` is what lets the same connection serve both without
    // being duplicated or the dialog getting one of its own.
    let dialog_task = {
        let mut dialog = Dialog::new(
            seerr,
            SharedDirectory {
                state: state.clone(),
                media_group: config.media_group.clone(),
            },
            Catalogue::load(),
            config.settings_url.clone(),
            config.operator_name.clone(),
        );
        tokio::spawn(async move {
            tracing::info!("signal-seerr is up");
            while let Some(message) = incoming.recv().await {
                for reply in dialog.handle(&message.from, &message.text).await {
                    if let Err(e) = signal.send(&message.from, &reply).await {
                        tracing::warn!(error = %e, "cannot reply");
                    }
                }
            }
        })
    };

    // Whichever of the three ends first ends the process. A reconciler that
    // stops reconciling, a webhook listener that stops listening, or a
    // dialog loop that stops answering is not a degraded bot, it is a
    // broken one -- and none of the three would otherwise show up anywhere:
    // `tokio::spawn`'s `JoinHandle` was previously discarded, so a panic in
    // any of them would have left the process running with the unit still
    // `active`. Exiting non-zero is what lets systemd's `Restart =
    // on-failure` and its bounded `StartLimitBurst` turn a transient
    // failure into a restart and a persistent one into `failed`, where the
    // guest check and the alarm mail can see it.
    tokio::select! {
        r = reconciler_task => tracing::error!(?r, "the reconciler stopped"),
        r = webhook_task => tracing::error!(?r, "the webhook listener stopped"),
        r = dialog_task => tracing::error!(?r, "the dialog loop stopped"),
    }
    std::process::exit(1);
}

#[cfg(test)]
mod wait_for_socket_tests {
    use super::wait_for_socket;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn returns_ok_once_the_socket_appears_partway_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signal-cli.sock");

        let waiting = tokio::spawn({
            let path = path.clone();
            async move { wait_for_socket(&path, Duration::from_secs(5)).await }
        });

        // Let the wait start and enter its first sleep before the socket
        // exists at all.
        tokio::task::yield_now().await;
        std::fs::write(&path, b"").unwrap();
        // Fire the pending poll tick so the loop notices.
        tokio::time::advance(Duration::from_millis(500)).await;

        waiting
            .await
            .unwrap()
            .expect("the socket appeared well within the limit");
    }

    #[tokio::test(start_paused = true)]
    async fn returns_err_naming_the_path_when_it_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signal-cli.sock"); // never created

        let waiting = tokio::spawn({
            let path = path.clone();
            async move { wait_for_socket(&path, Duration::from_millis(200)).await }
        });
        tokio::time::advance(Duration::from_millis(600)).await;

        let err = waiting.await.unwrap().unwrap_err().to_string();
        assert!(
            err.contains(&path.display().to_string()),
            "the error must name the path, got: {err}"
        );
    }
}
