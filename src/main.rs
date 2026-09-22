use anyhow::{Context, Result};
use signal_seerr::arr::{ArrClient, Insight, ReleaseSearch};
use signal_seerr::config::{Config, InsightConfig, Secrets};
use signal_seerr::dialog::Dialog;
use signal_seerr::directory::{apply, diff::plan, AuthentikClient, Directory, Member};
use signal_seerr::i18n::Catalogue;
use signal_seerr::model::Aci;
use signal_seerr::notices::Notices;
use signal_seerr::secret::Secret;
use signal_seerr::seerr::{Requests, SeerrClient};
use signal_seerr::signal::{Messenger, SignalClient};
use signal_seerr::state::State;
use signal_seerr::watch::{WatchSettings, Watcher};
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

/// What the `[insight]` section adds to this process.
///
/// `notices` and `watcher` are `None` together, and for one reason: a
/// notices file that cannot be read must never be replaced by an empty
/// record -- an emptied record would tell everybody about every wish all
/// over again. The dialog keeps its `Insight` in that case, so `/status`
/// still answers truthfully; only the loop that speaks up unasked stays off,
/// and it stays off until somebody looks at the file.
///
/// The default -- all three `None` -- is exactly the behaviour of a config
/// without an `[insight]` section: no arr client is built, no arr key is
/// read, and nothing ever connects to Radarr or Sonarr.
#[derive(Default)]
struct InsightParts {
    insight: Option<Arc<dyn Insight>>,
    notices: Option<Arc<RwLock<Notices>>>,
    watcher: Option<Watcher>,
}

/// A year of hours. Nothing sensible sits above it -- a deadline a wish can
/// never reach is the same thing as switching the unasked message off -- and
/// it keeps the conversion below far away from the point where
/// `time::Duration::hours` panics on overflow.
const MAX_STALL_AFTER_HOURS: i64 = 24 * 365;

/// The watcher's settings from the `[insight]` section.
///
/// Pure, and on its own, so the one conversion in the whole wiring that
/// could be silently wrong -- a count of hours into a `time::Duration` -- is
/// tested without building an HTTP client or a runtime. Seconds instead of
/// hours here would make the bot announce every open wish within the minute.
///
/// FALLIBLE, not panicking: `stall_after_hours` is a `u64` out of a config
/// file, `as i64` would wrap a large one into a negative deadline, and
/// `time::Duration::hours` panics outright on an overflow. A typo in a TOML
/// file deserves a named error at startup, not a backtrace.
fn watch_settings(insight: &InsightConfig) -> Result<WatchSettings> {
    let hours = i64::try_from(insight.stall_after_hours)
        .ok()
        .filter(|hours| *hours <= MAX_STALL_AFTER_HOURS)
        .with_context(|| {
            format!(
                "stall_after_hours = {} is out of range: it must be at most \
                 {MAX_STALL_AFTER_HOURS} (a year)",
                insight.stall_after_hours
            )
        })?;
    Ok(WatchSettings {
        stall_after: time::Duration::hours(hours),
        max_searches_per_day: insight.max_reason_searches_per_day,
        notices_file: insight.notices_file.clone(),
        profile_languages: insight.profile_languages.clone(),
    })
}

/// Builds the arr client, loads the record, and assembles the watcher --
/// kept out of `main` so the startup sequence there stays readable.
///
/// One `ArrClient` serves both roles: the dialog gets it as `Arc<dyn
/// Insight>`, which by its very type cannot trigger a search, and the
/// watcher gets the same connection a second time as `Arc<dyn
/// ReleaseSearch>` -- but only when the operator switched `reason_search`
/// on. Off, that cast never happens at all.
fn insight_parts(
    insight: &InsightConfig,
    radarr_key: Secret,
    sonarr_key: Option<Secret>,
    seerr: Arc<dyn Requests>,
    messenger: Arc<dyn Messenger>,
    directory: Arc<RwLock<State>>,
    catalogue: Arc<Catalogue>,
) -> Result<InsightParts> {
    // BEFORE anything is built: a `stall_after_hours` out of range is a
    // misconfiguration that has to stop the start, not one that surfaces
    // ten minutes later on the first tick.
    let settings = watch_settings(insight)?;

    // `zip`, not two separate options: `config.rs` has already rejected a
    // URL without its key file and vice versa, so either both are here or
    // neither is.
    let sonarr = insight.sonarr_url.as_deref().zip(sonarr_key);
    let arr = Arc::new(ArrClient::new(&insight.radarr_url, radarr_key, sonarr));

    let notices = match Notices::load(&insight.notices_file) {
        Ok(notices) => Some(Arc::new(RwLock::new(notices))),
        Err(e) => {
            // Deliberately NOT `Notices::default()`: see the struct doc.
            // Loud, naming the path, and the process carries on -- dialog,
            // webhook and reconciler have nothing to do with this file.
            tracing::error!(
                error = %e,
                path = %insight.notices_file.display(),
                "cannot read the notices file -- the watch loop stays off and the file is left untouched"
            );
            None
        }
    };

    let watcher = notices.clone().map(|notices| Watcher {
        seerr,
        insight: arr.clone() as Arc<dyn Insight>,
        search: insight
            .reason_search
            .then(|| arr.clone() as Arc<dyn ReleaseSearch>),
        messenger,
        directory,
        notices,
        catalogue,
        settings,
    });

    Ok(InsightParts {
        insight: Some(arr as Arc<dyn Insight>),
        notices,
        watcher,
    })
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
    // 20s, not 60s: chosen together with the unit's restart limit
    // (nix/module.nix) so a persistently failing start actually reaches
    // that limit inside its window rather than staying just under it. If
    // the socket is not there after 20s, waiting another 40 rarely helps.
    wait_for_socket(&config.signal_socket, std::time::Duration::from_secs(20)).await?;

    let (signal, mut incoming) =
        SignalClient::connect(&config.signal_socket, secrets.signal_account.expose()).await?;

    let state = Arc::new(RwLock::new(State::load(&config.state_file)?));
    let seerr = Arc::new(SeerrClient::new(&config.seerr_url, secrets.seerr_key));
    let authentik = AuthentikClient::new(&config.authentik_url, secrets.authentik_token);

    // What [insight] adds, wired here rather than next to the task that
    // uses it: a notices file that cannot be read is an error somebody has
    // to find in the journal at startup, not one that surfaces ten minutes
    // later on the first tick.
    let parts = match &config.insight {
        None => InsightParts::default(),
        Some(insight) => {
            // `Secrets::read` reads the radarr key whenever [insight] is
            // present, so a None here is a bug in this process rather than
            // a misconfiguration -- and quietly carrying on without insight
            // would hide it.
            let radarr_key = secrets
                .radarr_key
                .context("[insight] is configured but no radarr key was read")?;
            insight_parts(
                insight,
                radarr_key,
                secrets.sonarr_key,
                seerr.clone() as Arc<dyn Requests>,
                signal.clone() as Arc<dyn Messenger>,
                state.clone(),
                catalogue.clone(),
            )?
        }
    };

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
            config.quality_profiles.clone(),
            // Both None without an [insight] section -- then the dialog
            // works from Seerr alone, exactly as it did before insight
            // existed. `insight` without `notices` is the unreadable-record
            // case: /status still answers, nothing is said unasked.
            parts.insight,
            parts.notices,
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

    // 4. The watcher -- the loop that speaks up unasked. It exists only
    // when [insight] is configured AND its record could be read; see
    // `InsightParts`.
    let watch_task = config
        .insight
        .as_ref()
        .zip(parts.watcher)
        .map(|(insight, watcher)| {
            let period = std::time::Duration::from_secs(insight.poll_seconds);
            // Typed for the same reason the reconciler above is: the loop
            // never breaks, so the block's own type is `!`, and pinning it
            // to () here says that this task is meant to run forever.
            let handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(period);
                loop {
                    ticker.tick().await;
                    // The clock arrives as an argument everywhere below
                    // this line; this is the one place that reads it.
                    watcher.round(time::OffsetDateTime::now_utc()).await;
                }
            });
            handle
        });

    // The fourth arm of the `select!` below, as one future either way. With
    // no watcher there is nothing to wait for, and `pending()` never
    // completes -- so that arm is simply never picked, rather than firing
    // at once and taking the whole process down with it. `select!` takes a
    // fixed set of arms; leaving one out is not something a `match` can do.
    let watcher_ended = async move {
        match watch_task {
            Some(handle) => handle.await,
            None => std::future::pending().await,
        }
    };

    // Whichever of the four ends first ends the process. A reconciler that
    // stops reconciling, a webhook listener that stops listening, a dialog
    // loop that stops answering, or a watcher that stops watching is not a
    // degraded bot, it is a broken one -- and none of them would otherwise
    // show up anywhere:
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
        r = watcher_ended => tracing::error!(?r, "the watcher stopped"),
    }
    std::process::exit(1);
}

#[cfg(test)]
mod watch_settings_tests {
    use super::watch_settings;
    use signal_seerr::config::InsightConfig;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn insight_config() -> InsightConfig {
        InsightConfig {
            radarr_url: "https://radarr.example.invalid".into(),
            radarr_key_file: "/dev/null".into(),
            sonarr_url: None,
            sonarr_key_file: None,
            poll_seconds: 600,
            stall_after_hours: 36,
            reason_search: false,
            max_reason_searches_per_day: 7,
            notices_file: "/var/lib/signal-seerr/notices.json".into(),
            profile_languages: BTreeMap::from([(
                "Dual Language, then German (1080p)".to_string(),
                vec!["German".to_string(), "English".to_string()],
            )]),
        }
    }

    #[test]
    fn stall_after_hours_becomes_that_many_hours() {
        // The whole point of this test: hours, not seconds and not minutes.
        // Either of those would leave the deadline looking plausible in the
        // config while the bot announces every open wish within the minute.
        let settings = watch_settings(&insight_config()).unwrap();
        assert_eq!(settings.stall_after, time::Duration::hours(36));
    }

    /// A year of hours is the ceiling, and past it the start fails by name.
    /// `as i64` used to carry the number straight into
    /// `time::Duration::hours`, which panics on overflow -- a TOML typo
    /// became a backtrace with no field name in it.
    #[test]
    fn a_stall_deadline_out_of_range_is_a_named_error_not_a_panic() {
        let mut config = insight_config();
        config.stall_after_hours = super::MAX_STALL_AFTER_HOURS as u64 + 1;
        let err = watch_settings(&config).unwrap_err().to_string();
        assert!(err.contains("stall_after_hours"), "got: {err}");

        // The value that used to panic: `u64::MAX` is negative as an `i64`,
        // and `Duration::hours` refuses it either way.
        config.stall_after_hours = u64::MAX;
        assert!(watch_settings(&config).is_err());

        // And the ceiling itself is still accepted.
        config.stall_after_hours = super::MAX_STALL_AFTER_HOURS as u64;
        assert!(watch_settings(&config).is_ok());
    }

    #[test]
    fn the_budget_the_file_and_the_languages_are_carried_over_unchanged() {
        let settings = watch_settings(&insight_config()).unwrap();
        assert_eq!(settings.max_searches_per_day, 7);
        assert_eq!(
            settings.notices_file,
            Path::new("/var/lib/signal-seerr/notices.json")
        );
        assert_eq!(
            settings
                .profile_languages
                .get("Dual Language, then German (1080p)"),
            Some(&vec!["German".to_string(), "English".to_string()]),
        );
    }
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
