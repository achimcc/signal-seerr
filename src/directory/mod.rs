pub mod diff;

use crate::i18n::{Catalogue, Locale};
use crate::model::Aci;
use crate::secret::Secret;
use crate::signal::{Messenger, SignalClient};
use crate::state::{Entry, State};
use anyhow::{bail, Result};
use diff::{plan, AuthentikUser, Change};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub authentik_username: String,
    pub locale: Locale,
    /// In the media group. False means "we know who you are, but not this".
    pub allowed: bool,
}

pub trait Directory: Send + Sync {
    fn lookup(&self, aci: &Aci) -> Option<Member>;
}

pub struct AuthentikClient {
    base: String,
    token: Secret,
    http: reqwest::Client,
}

impl AuthentikClient {
    pub fn new(base: &str, token: Secret) -> AuthentikClient {
        AuthentikClient {
            base: base.trim_end_matches('/').to_string(),
            token,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("a client with no TLS surprises"),
        }
    }

    pub async fn users(&self, fallback_locale: &str) -> Result<Vec<AuthentikUser>> {
        let response = self
            .http
            .get(format!("{}/api/v3/core/users/", self.base))
            .query(&[("page_size", "500")])
            .bearer_auth(self.token.expose())
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("authentik answered {}", response.status());
        }
        Ok(parse_users(&response.json().await?, fallback_locale))
    }
}

fn parse_users(body: &serde_json::Value, fallback_locale: &str) -> Vec<AuthentikUser> {
    body.get("results")
        .and_then(|r| r.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter_map(|u| {
            Some(AuthentikUser {
                username: u.get("username")?.as_str()?.to_string(),
                signal_username: u
                    .get("attributes")
                    .and_then(|a| a.get("signal_username"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string()),
                locale: u
                    .get("attributes")
                    .and_then(|a| a.get("settings"))
                    .and_then(|s| s.get("locale"))
                    .and_then(|l| l.as_str())
                    .filter(|l| !l.is_empty())
                    .unwrap_or(fallback_locale)
                    .to_string(),
                groups: u
                    .get("groups_obj")
                    .and_then(|g| g.as_array())
                    .map(|v| {
                        v.iter()
                            .filter_map(|g| Some(g.get("name")?.as_str()?.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Carries out what `plan` decided. The resolver is a parameter rather than a
/// SignalClient so the whole pass is testable without a socket.
pub async fn apply<F, Fut>(
    state: &mut State,
    changes: Vec<Change>,
    users: &[AuthentikUser],
    messenger: &dyn Messenger,
    catalogue: &Catalogue,
    resolve: F,
) -> Result<()>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Option<Aci>>>,
{
    for change in changes {
        match change {
            Change::Added {
                username,
                signal_username,
            }
            | Change::Rebound {
                username,
                signal_username,
            } => {
                let user = users.iter().find(|u| u.username == username);
                let locale = user
                    .map(|u| Locale::from_authentik(&u.locale))
                    .unwrap_or(Locale::En);
                let groups = user.map(|u| u.groups.clone()).unwrap_or_default();

                match resolve(signal_username.clone()).await {
                    Ok(Some(aci)) => {
                        // Stored before the greeting is attempted: the
                        // mapping must exist -- so the person can already
                        // use the bot, and so a later name change plans
                        // correctly -- even if the send below fails.
                        // `greeted` starts false and `greet` below only
                        // flips it once send() actually succeeds, which
                        // leaves a failure retriable rather than stranded.
                        state.upsert(Entry {
                            authentik_username: username.clone(),
                            signal_username,
                            aci: aci.clone(),
                            greeted: false,
                            locale,
                            groups,
                        });
                        greet(state, messenger, catalogue, &username, &aci, locale).await;
                    }
                    Ok(None) => {
                        // A typo. Nothing is stored, so the next pass produces
                        // the same Added and does the same nothing -- no
                        // message, no retry storm.
                        tracing::info!(username, "the signal name does not resolve");
                    }
                    Err(e) => {
                        tracing::warn!(username, error = %e, "cannot resolve the signal name")
                    }
                }
            }
            Change::Greet { username } => {
                // plan() only emits this when the entry already exists with
                // a matching name, so the lookup below should always
                // succeed; if the entry vanished between plan() and here
                // (a concurrent removal), there is simply nobody left to
                // greet.
                if let Some(entry) = state.by_user(&username).cloned() {
                    let locale = entry.locale;
                    let aci = entry.aci.clone();
                    greet(state, messenger, catalogue, &username, &aci, locale).await;
                }
            }
            Change::Removed { username, aci: _ } => {
                state.remove_user(&username);
                tracing::info!(username, "signal name cleared");
            }
            Change::Conflict {
                username,
                signal_username,
                held_by,
            } => {
                tracing::warn!(
                    username,
                    signal_username,
                    held_by,
                    "signal name already taken"
                );
            }
        }
    }
    Ok(())
}

/// Sends the welcome message and marks the entry greeted on success. A
/// failed send is logged and left alone -- the entry keeps `greeted: false`,
/// so the *next* pass's `plan()` emits `Change::Greet` again and this same
/// function is what actually delivers it then. One unreachable recipient
/// must not stall, or worse silence forever, everyone else in the pass, so
/// this never propagates the send error to its caller.
async fn greet(
    state: &mut State,
    messenger: &dyn Messenger,
    catalogue: &Catalogue,
    username: &str,
    aci: &Aci,
    locale: Locale,
) {
    let text = format!(
        "{}\n\n{}",
        catalogue.text(locale, "greeting.title", &[("name", username)]),
        catalogue.text(locale, "help.body", &[])
    );
    match messenger.send(aci, &text).await {
        Ok(()) => {
            if let Some(entry) = state.remove_user(username) {
                state.upsert(Entry {
                    greeted: true,
                    ..entry
                });
            }
        }
        Err(e) => {
            tracing::warn!(username, error = %e, "cannot greet, will retry next pass");
        }
    }
}

/// Wires the pure decision (`plan`) and its side effects (`apply`) to the two
/// real endpoints: Authentik over HTTP, Signal over signal-cli's socket.
///
/// One call is one poll. The pieces it composes -- `AuthentikClient`'s
/// parsing (`parse_users`), `plan`, and `apply` -- each have their own unit
/// tests above that do not need a socket or an HTTP server; `run_once` itself
/// is thin wiring, exercised end to end once the poll loop (a later task)
/// calls it against the real daemons.
pub struct Reconciler {
    authentik: AuthentikClient,
    signal: Arc<SignalClient>,
    catalogue: Catalogue,
    state: State,
    state_path: PathBuf,
    fallback_locale: String,
}

impl Reconciler {
    pub fn new(
        authentik: AuthentikClient,
        signal: Arc<SignalClient>,
        catalogue: Catalogue,
        state: State,
        state_path: PathBuf,
        fallback_locale: String,
    ) -> Reconciler {
        Reconciler {
            authentik,
            signal,
            catalogue,
            state,
            state_path,
            fallback_locale,
        }
    }

    /// One pass: read Authentik, decide what changed, resolve and greet,
    /// persist. `apply` logs and carries on past a single resolve or send
    /// failure rather than aborting the pass (see `greet`), so it currently
    /// never fails partway through. State is still saved unconditionally
    /// before the outcome is propagated, so if a future change to `apply`
    /// ever does introduce a pass-ending error, whatever it already applied
    /// is not lost along with it.
    pub async fn run_once(&mut self) -> Result<()> {
        let users = self.authentik.users(&self.fallback_locale).await?;
        let changes = plan(&self.state, &users);

        let signal_for_resolve = self.signal.clone();
        let resolve = move |name: String| {
            let signal = signal_for_resolve.clone();
            async move { signal.resolve_username(&name).await }
        };

        let outcome = apply(
            &mut self.state,
            changes,
            &users,
            self.signal.as_ref(),
            &self.catalogue,
            resolve,
        )
        .await;

        // `apply` mutates `self.state` change by change and can stop early
        // (a send failure propagates via `?`, see `apply`'s Added/Rebound
        // arm). Saving unconditionally here means a send failure costs
        // only the greeting it belongs to, not the ones already applied
        // earlier in this same pass.
        self.state.save(&self.state_path)?;
        outcome?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Sent(Mutex<Vec<(String, String)>>);

    #[async_trait::async_trait]
    impl Messenger for Sent {
        async fn send(&self, to: &Aci, text: &str) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push((to.0.clone(), text.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_added_user_is_greeted_exactly_once() {
        let messenger = Arc::new(Sent::default());
        let mut state = State::default();
        let catalogue = Catalogue::load();

        let users = vec![AuthentikUser {
            username: "robert".into(),
            signal_username: Some("robert.42".into()),
            locale: "de".into(),
            groups: vec!["Medien".into()],
        }];

        // The resolver is a closure so this test needs no signal-cli.
        let resolve = |_name: String| async { Ok(Some(Aci("aaaa".into()))) };

        // `plan` is computed into a local before the call: `apply(&mut
        // state, plan(&state, ...), ...)` would borrow `state` both
        // mutably (the first argument) and immutably (inside the second)
        // at once -- that is not the two-phase-borrow-friendly shape a
        // method-call receiver gets, so a plain function call rejects it.
        let changes = plan(&state, &users);
        apply(
            &mut state,
            changes,
            &users,
            &*messenger,
            &catalogue,
            resolve,
        )
        .await
        .unwrap();
        assert_eq!(messenger.0.lock().unwrap().len(), 1);
        assert!(state.by_user("robert").unwrap().greeted);

        // Second pass: nothing changed, so nothing is sent.
        let changes = plan(&state, &users);
        apply(
            &mut state,
            changes,
            &users,
            &*messenger,
            &catalogue,
            resolve,
        )
        .await
        .unwrap();
        assert_eq!(messenger.0.lock().unwrap().len(), 1, "greeted twice");
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_not_stored() {
        let messenger = Arc::new(Sent::default());
        let mut state = State::default();
        let catalogue = Catalogue::load();
        let users = vec![AuthentikUser {
            username: "robert".into(),
            signal_username: Some("typo.99".into()),
            locale: "de".into(),
            groups: vec!["Medien".into()],
        }];
        let resolve = |_name: String| async { Ok(None) };

        let changes = plan(&state, &users);
        apply(
            &mut state,
            changes,
            &users,
            &*messenger,
            &catalogue,
            resolve,
        )
        .await
        .unwrap();
        assert!(
            state.by_user("robert").is_none(),
            "an unresolvable name must not be pinned"
        );
        assert!(messenger.0.lock().unwrap().is_empty(), "nobody to greet");

        // And it must not be retried every 30 seconds for ever: the second
        // pass sees the same unresolvable name and does the same nothing,
        // without an extra message.
        let changes = plan(&state, &users);
        apply(
            &mut state,
            changes,
            &users,
            &*messenger,
            &catalogue,
            resolve,
        )
        .await
        .unwrap();
        assert!(messenger.0.lock().unwrap().is_empty());
    }

    #[derive(Default)]
    struct AlwaysFails;

    #[async_trait::async_trait]
    impl Messenger for AlwaysFails {
        async fn send(&self, _to: &Aci, _text: &str) -> Result<()> {
            anyhow::bail!("signal-cli is unreachable")
        }
    }

    #[tokio::test]
    async fn a_failing_send_leaves_the_person_retriable_and_the_next_pass_greets_them() {
        // This is the property the design's whole poll-over-webhook argument
        // depends on: nothing is missed for good, a later pass catches up.
        // A send failure here must not be the one place that promise breaks.
        let mut state = State::default();
        let catalogue = Catalogue::load();
        let users = vec![AuthentikUser {
            username: "robert".into(),
            signal_username: Some("robert.42".into()),
            locale: "de".into(),
            groups: vec!["Medien".into()],
        }];
        let resolve = |_name: String| async { Ok(Some(Aci("aaaa".into()))) };

        // First pass: the send fails.
        let changes = plan(&state, &users);
        apply(
            &mut state,
            changes,
            &users,
            &AlwaysFails,
            &catalogue,
            resolve,
        )
        .await
        .unwrap();
        assert_eq!(
            state.by_user("robert").map(|e| e.greeted),
            Some(false),
            "the mapping exists, but the welcome never went out"
        );

        // Second pass: plan() must ask for the greeting again ...
        let changes = plan(&state, &users);
        assert_eq!(
            changes,
            vec![Change::Greet {
                username: "robert".into()
            }]
        );

        // ... and this time, with a working messenger, it goes through.
        let messenger = Arc::new(Sent::default());
        apply(
            &mut state,
            changes,
            &users,
            &*messenger,
            &catalogue,
            resolve,
        )
        .await
        .unwrap();
        assert_eq!(messenger.0.lock().unwrap().len(), 1);
        assert!(state.by_user("robert").unwrap().greeted);
    }

    #[derive(Default)]
    struct FailsForOneAci(String);

    #[async_trait::async_trait]
    impl Messenger for FailsForOneAci {
        async fn send(&self, to: &Aci, _text: &str) -> Result<()> {
            if to.0 == self.0 {
                anyhow::bail!("unreachable: {}", to.0);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn one_failing_recipient_does_not_block_the_others_in_the_same_pass() {
        let mut state = State::default();
        let catalogue = Catalogue::load();
        let users = vec![
            AuthentikUser {
                username: "a".into(),
                signal_username: Some("a.1".into()),
                locale: "de".into(),
                groups: vec![],
            },
            AuthentikUser {
                username: "b".into(),
                signal_username: Some("b.1".into()),
                locale: "de".into(),
                groups: vec![],
            },
        ];
        let resolve = |name: String| async move { Ok(Some(Aci(format!("aci-{name}")))) };
        // "a" resolves to an ACI the messenger refuses; "b" resolves fine.
        let messenger = FailsForOneAci("aci-a.1".to_string());

        let changes = plan(&state, &users);
        apply(&mut state, changes, &users, &messenger, &catalogue, resolve)
            .await
            .unwrap();

        assert_eq!(
            state.by_user("a").map(|e| e.greeted),
            Some(false),
            "a is retriable, not lost"
        );
        assert_eq!(
            state.by_user("b").map(|e| e.greeted),
            Some(true),
            "b must still be greeted despite a's failure"
        );
    }

    #[test]
    fn users_are_read_out_of_authentiks_page() {
        let body = serde_json::json!({
            "pagination": { "next": 0 },
            "results": [
                { "username": "robert", "attributes": { "signal_username": "robert.42" },
                  "groups_obj": [ { "name": "Medien" } ] },
                { "username": "konrad", "attributes": {}, "groups_obj": [] },
                { "username": "svc-ldap", "attributes": { "signal_username": "" },
                  "groups_obj": [ { "name": "LDAP-Suche" } ] }
            ]
        });
        let got = parse_users(&body, "de");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].signal_name(), Some("robert.42"));
        assert!(got[0].groups.contains(&"Medien".to_string()));
        assert_eq!(got[1].signal_name(), None, "a missing attribute is no name");
        assert_eq!(got[2].signal_name(), None, "an empty attribute is no name");
    }

    #[test]
    fn the_locale_falls_back_when_authentik_has_none() {
        let body = serde_json::json!({
            "results": [ { "username": "a", "attributes": { "settings": {} } } ]
        });
        assert_eq!(parse_users(&body, "de")[0].locale, "de");
    }

    #[test]
    fn the_locale_is_read_from_the_settings_attribute() {
        // Authentik's user settings flow writes the locale into
        // attributes.settings.locale, not into a top-level field.
        let body = serde_json::json!({
            "results": [ { "username": "a", "attributes": { "settings": { "locale": "en" } } } ]
        });
        assert_eq!(parse_users(&body, "de")[0].locale, "en");
    }

    // The tests below exercise the wiring `parse_users` itself does not
    // reach: the real HTTP call (bearer token, endpoint path, status
    // handling) and the real signal-cli socket, end to end through
    // `Reconciler::run_once`. Neither is asked for by name in the task
    // brief -- the brief's own tests stop at the pure functions -- but both
    // pieces exist only for this task ("wires it to Authentik and to
    // Signal"), and unlike `plan`/`apply`, nothing else in the test suite
    // ever calls them.

    #[tokio::test]
    async fn authentik_client_sends_a_bearer_token_and_parses_the_answer() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v3/core/users/"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer secret-token",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        { "username": "robert", "attributes": { "signal_username": "robert.42" },
                          "groups_obj": [ { "name": "Medien" } ] }
                    ]
                })),
            )
            .mount(&server)
            .await;

        let client = AuthentikClient::new(&server.uri(), Secret::from("secret-token".to_string()));
        let users = client.users("de").await.unwrap();

        assert_eq!(users.len(), 1);
        assert_eq!(users[0].username, "robert");
        assert_eq!(users[0].signal_name(), Some("robert.42"));
    }

    #[tokio::test]
    async fn authentik_client_reports_a_non_success_status() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = AuthentikClient::new(&server.uri(), Secret::from("secret-token".to_string()));
        let err = client.users("de").await.unwrap_err().to_string();
        assert!(err.contains("503"), "got: {err}");
    }

    #[tokio::test]
    async fn run_once_greets_a_new_user_end_to_end() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let authentik = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v3/core/users/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        { "username": "robert", "attributes": { "signal_username": "robert.42" },
                          "groups_obj": [ { "name": "Medien" } ] }
                    ]
                })),
            )
            .mount(&authentik)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("signal-cli.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        // A minimal stand-in for signal-cli: answers exactly the two calls
        // one greeting round trip makes, getUserStatus then send.
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();

            let request = lines.next_line().await.unwrap().unwrap();
            let id = serde_json::from_str::<serde_json::Value>(&request).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string();
            let answer = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": [ { "recipient": "u:robert.42", "number": null,
                               "uuid": "aaaa-bbbb", "isRegistered": true } ]
            });
            write_half
                .write_all(format!("{answer}\n").as_bytes())
                .await
                .unwrap();

            let request = lines.next_line().await.unwrap().unwrap();
            let id = serde_json::from_str::<serde_json::Value>(&request).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string();
            let answer = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "timestamp": 1 }
            });
            write_half
                .write_all(format!("{answer}\n").as_bytes())
                .await
                .unwrap();
        });

        let (signal, _rx) = SignalClient::connect(&socket_path, "+490000")
            .await
            .unwrap();
        let authentik_client =
            AuthentikClient::new(&authentik.uri(), Secret::from("secret-token".to_string()));
        let state_path = dir.path().join("state.json");

        let mut reconciler = Reconciler::new(
            authentik_client,
            signal,
            Catalogue::load(),
            State::default(),
            state_path.clone(),
            "de".to_string(),
        );

        reconciler.run_once().await.unwrap();
        server_task.await.unwrap();

        let state = State::load(&state_path).unwrap();
        let entry = state.by_user("robert").expect("robert was greeted");
        assert!(entry.greeted);
        assert_eq!(entry.aci, Aci("aaaa-bbbb".into()));
        assert_eq!(entry.groups, vec!["Medien".to_string()]);
    }
}
