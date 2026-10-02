pub mod diff;

use crate::i18n::{Catalogue, Locale};
use crate::model::Aci;
use crate::secret::Secret;
use crate::signal::Messenger;
use crate::state::{Entry, State};
use anyhow::{bail, Result};
use diff::{AuthentikUser, Change};
use std::future::Future;
use std::path::Path;

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

/// 500 users a page: far past any household, and a bound on a pagination
/// that never ends.
const MAX_USER_PAGES: u64 = 100;

pub struct AuthentikClient {
    base: String,
    token: Secret,
    http: reqwest::Client,
}

impl AuthentikClient {
    /// The client without a `ca_file`: it trusts what the system trusts.
    /// `main` goes through `with_ca_file`; this is the short form for
    /// everything that has no certificate to name.
    pub fn new(base: &str, token: Secret) -> AuthentikClient {
        AuthentikClient::with_ca_file(base, token, None)
            .expect("an HTTP client that names no ca_file")
    }

    /// `ca_file` is `authentik_ca_file`: a PEM file of certificates that are
    /// the ONLY ones this client trusts -- the system's trust store is
    /// replaced for it, not added to (see `tls::client`). A file that is
    /// missing or cannot be used is an error naming the field and the path.
    /// `None` is the client as it always was.
    pub fn with_ca_file(
        base: &str,
        token: Secret,
        ca_file: Option<&Path>,
    ) -> Result<AuthentikClient> {
        let builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            // A redirect carries our own headers onwards. reqwest strips
            // `Authorization` only when the HOST changes, so a redirect
            // within the same host would take this bearer token along --
            // and whatever answered there would be read as the directory,
            // which decides who is greeted and who is told goodbye. Nothing
            // this bot calls in Authentik redirects, so refusing is free
            // (Audit 3, B158).
            .redirect(reqwest::redirect::Policy::none());
        Ok(AuthentikClient {
            base: base.trim_end_matches('/').to_string(),
            token,
            http: crate::tls::client(builder, ca_file, "authentik_ca_file")?,
        })
    }

    /// Every user, all pages of them.
    ///
    /// An EMPTY result is an error (Audit 3, B128): Authentik answers a token
    /// that lost its object permission with 200 and an empty list, and read
    /// as the truth that would say goodbye to everybody. So is an answer
    /// without `results` at all. A directory with not one user in it -- not
    /// even its own admin -- is never what is really there.
    pub async fn users(&self, fallback_locale: &str) -> Result<Vec<AuthentikUser>> {
        let mut users = Vec::new();
        let mut page: u64 = 1;
        loop {
            let response = self
                .http
                .get(format!("{}/api/v3/core/users/", self.base))
                .query(&[("page_size", "500"), ("page", &page.to_string())])
                .bearer_auth(self.token.expose())
                .send()
                .await?;
            if !response.status().is_success() {
                bail!("authentik answered {}", response.status());
            }
            let body: serde_json::Value = response.json().await?;
            if !body.get("results").is_some_and(|r| r.is_array()) {
                bail!("authentik's user list carries no `results`");
            }
            users.extend(parse_users(&body, fallback_locale));

            // `pagination.next` is the next page's NUMBER, 0 on the last.
            let next = body
                .get("pagination")
                .and_then(|p| p.get("next"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            if next == 0 {
                break;
            }
            if next <= page || next > MAX_USER_PAGES {
                bail!("authentik's pagination went from page {page} to {next}");
            }
            page = next;
        }
        if users.is_empty() {
            bail!(
                "authentik listed no users at all -- a permission problem, not everybody leaving"
            );
        }
        Ok(users)
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
///
/// Returns nothing: every failure path below (a resolve, a greeting, a
/// farewell) is logged and swallowed rather than propagated, on purpose --
/// see each arm -- so there is no outcome left for a caller to branch on.
pub async fn apply<F, Fut>(
    state: &mut State,
    changes: Vec<Change>,
    users: &[AuthentikUser],
    messenger: &dyn Messenger,
    catalogue: &Catalogue,
    resolve: F,
) where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Option<Aci>>>,
{
    let conflicts: Vec<(String, String)> = changes
        .iter()
        .filter_map(|c| match c {
            Change::Conflict {
                username,
                signal_username,
                ..
            } => Some((username.clone(), signal_username.clone())),
            _ => None,
        })
        .collect();
    state.keep_reported_conflicts(&conflicts);

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
            Change::Updated {
                username,
                groups,
                locale,
            } => {
                // In place: the mapping, the ACI and `greeted` stay as they
                // are -- this is not a new person, only new rights and a new
                // language. Nothing is sent; a group change is not news the
                // person needs in a chat.
                if let Some(mut entry) = state.by_user(&username).cloned() {
                    entry.groups = groups;
                    entry.locale = Locale::from_authentik(&locale);
                    state.upsert(entry);
                    tracing::info!(username, "groups or language updated");
                }
            }
            Change::Greet { username } => {
                // plan() only emits this when the entry already exists with
                // a matching name, so the lookup below should always
                // succeed; if the entry vanished between plan() and here
                // (a concurrent removal), there is simply nobody left to
                // greet.
                if let Some(entry) = state.by_user(&username).cloned() {
                    // Re-derived from `users`, same as Added/Rebound above,
                    // not read off the stored entry: somebody who changed
                    // their language between the failed send and this retry
                    // must get the current one, not the stale one.
                    let locale = users
                        .iter()
                        .find(|u| u.username == username)
                        .map(|u| Locale::from_authentik(&u.locale))
                        .unwrap_or(entry.locale);
                    greet(state, messenger, catalogue, &username, &entry.aci, locale).await;
                }
            }
            Change::Removed { username, aci } => {
                // The farewell is specified (design doc: transition table
                // and dialog section), not optional -- clearing the field is
                // how a person unsubscribes, and without it they get
                // silence, which reads as the bot being broken rather than
                // as a confirmed goodbye. Sent before the removal, but a
                // failed send must not block it: the person asked to be
                // forgotten, and that happens whether or not the goodbye
                // lands.
                let locale = state
                    .by_user(&username)
                    .map(|e| e.locale)
                    .unwrap_or(Locale::En);
                let text = catalogue.text(locale, "farewell.goodbye", &[]);
                if let Err(e) = messenger.send(&aci, &text).await {
                    tracing::warn!(username, error = %e, "cannot send farewell");
                }
                state.remove_user(&username);
                tracing::info!(username, "signal name cleared");
            }
            Change::Conflict {
                username,
                signal_username,
                held_by,
            } => {
                if state.conflict_reported(&username, &signal_username) {
                    continue;
                }
                // Loud, and once (Audit 3, B126): this is somebody locked
                // out of the bot -- possibly on purpose, by whoever entered
                // their name first -- not a passing hiccup.
                tracing::error!(
                    username,
                    signal_username,
                    held_by,
                    "signal name already taken -- telling the owner of the name"
                );
                // The stored ACI of the holder is what the NAME resolved to:
                // the phone that owns the Signal name. That is the one
                // person who can say which of the two accounts is theirs --
                // the claimant, if the holder squatted, cannot be reached
                // any other way.
                let Some(holder) = state.by_user(&held_by).cloned() else {
                    // Claimed by another fresh account in this very pass;
                    // next pass the holder is stored and this goes out.
                    continue;
                };
                let text = catalogue.text(
                    holder.locale,
                    "conflict.notice",
                    &[
                        ("claimant", username.as_str()),
                        ("signal", signal_username.as_str()),
                        ("holder", held_by.as_str()),
                    ],
                );
                match messenger.send(&holder.aci, &text).await {
                    Ok(()) => state.mark_conflict_reported(&username, &signal_username),
                    Err(e) => {
                        tracing::warn!(username, error = %e, "cannot tell about the conflict, will retry next pass")
                    }
                }
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::diff::plan;
    use super::*;
    use crate::state::State;
    use std::sync::{Arc, Mutex};

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
        .await;
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
        .await;
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
        .await;
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
        .await;
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
        .await;
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
        .await;
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
        apply(&mut state, changes, &users, &messenger, &catalogue, resolve).await;

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

    fn known(state: &mut State, name: &str, signal: &str, aci: &str) {
        state.upsert(Entry {
            authentik_username: name.into(),
            signal_username: signal.into(),
            aci: Aci(aci.into()),
            greeted: true,
            locale: Locale::De,
            groups: vec!["Medien".into()],
        });
    }

    #[tokio::test]
    async fn clearing_the_name_sends_a_farewell_and_forgets_the_person() {
        // Clearing the field is how a person unsubscribes. Without the
        // farewell they get silence, which looks exactly like the bot being
        // broken -- so the next thing they do is write to it and get
        // nothing back, because they are no longer known.
        let messenger = Arc::new(Sent::default());
        let mut state = State::default();
        let catalogue = Catalogue::load();
        known(&mut state, "robert", "robert.42", "aaaa");
        let users = vec![AuthentikUser {
            username: "robert".into(),
            signal_username: None,
            locale: "de".into(),
            groups: vec!["Medien".into()],
        }];
        let resolve = |_name: String| async { Ok(Some(Aci("unused".into()))) };

        let changes = plan(&state, &users);
        assert_eq!(
            changes,
            vec![Change::Removed {
                username: "robert".into(),
                aci: Aci("aaaa".into())
            }]
        );
        apply(
            &mut state,
            changes,
            &users,
            &*messenger,
            &catalogue,
            resolve,
        )
        .await;

        let sent = messenger.0.lock().unwrap();
        assert_eq!(sent.len(), 1, "exactly one farewell");
        assert_eq!(sent[0].0, "aaaa");
        assert!(state.by_user("robert").is_none(), "the person is forgotten");
    }

    /// Audit 3, B126 (B2-SS-4). Mallory enters Bob's Signal name before Bob
    /// does; Bob then only gets `Conflict`, which used to be a `warn!` every
    /// 30 s and nothing else -- Bob just saw a bot that never answered.
    ///
    /// The one ACI worth telling is the one the NAME resolves to: that is the
    /// phone that owns the Signal name, i.e. Bob, whichever Authentik account
    /// holds it here. It is told once per conflict, not once per pass, and
    /// again if the same conflict comes back after it had gone.
    #[tokio::test]
    async fn a_conflict_is_told_once_to_the_owner_of_the_signal_name() {
        let messenger = Arc::new(Sent::default());
        let mut state = State::default();
        let catalogue = Catalogue::load();
        known(&mut state, "mallory", "bob.42", "bbbb");
        let user = |name: &str, signal: Option<&str>| AuthentikUser {
            username: name.into(),
            signal_username: signal.map(Into::into),
            locale: "de".into(),
            groups: vec!["Medien".into()],
        };
        let conflicted = vec![user("mallory", Some("bob.42")), user("bob", Some("BOB.42"))];
        let resolve = |_name: String| async { Ok(Some(Aci("unused".into()))) };

        for _ in 0..3 {
            let changes = plan(&state, &conflicted);
            apply(
                &mut state,
                changes,
                &conflicted,
                &*messenger,
                &catalogue,
                resolve,
            )
            .await;
        }
        {
            let sent = messenger.0.lock().unwrap();
            assert_eq!(sent.len(), 1, "told once, not once per pass: {sent:?}");
            assert_eq!(sent[0].0, "bbbb", "told to the owner of the name");
            assert!(
                sent[0].1.contains("bob"),
                "names the claimant: {}",
                sent[0].1
            );
            assert!(
                sent[0].1.contains("mallory"),
                "names the holder: {}",
                sent[0].1
            );
        }

        // Bob gives up and clears the field: the conflict is gone.
        let calm = vec![user("mallory", Some("bob.42")), user("bob", None)];
        let changes = plan(&state, &calm);
        apply(&mut state, changes, &calm, &*messenger, &catalogue, resolve).await;
        assert_eq!(messenger.0.lock().unwrap().len(), 1);

        // And tries again: a new conflict, told again.
        let changes = plan(&state, &conflicted);
        apply(
            &mut state,
            changes,
            &conflicted,
            &*messenger,
            &catalogue,
            resolve,
        )
        .await;
        assert_eq!(messenger.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_person_is_forgotten_even_when_the_farewell_fails_to_send() {
        // The person asked to be forgotten by clearing the field; that must
        // happen whether or not the goodbye actually lands.
        let mut state = State::default();
        let catalogue = Catalogue::load();
        known(&mut state, "robert", "robert.42", "aaaa");
        let users = vec![AuthentikUser {
            username: "robert".into(),
            signal_username: None,
            locale: "de".into(),
            groups: vec!["Medien".into()],
        }];
        let resolve = |_name: String| async { Ok(Some(Aci("unused".into()))) };

        let changes = plan(&state, &users);
        apply(
            &mut state,
            changes,
            &users,
            &AlwaysFails,
            &catalogue,
            resolve,
        )
        .await;

        assert!(
            state.by_user("robert").is_none(),
            "forgotten regardless of the failed send"
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

    // The two tests below exercise the wiring `parse_users` itself does not
    // reach: the real HTTP call (bearer token, endpoint path, status
    // handling). Neither is asked for by name in the task brief -- the
    // brief's own tests stop at the pure functions -- but `AuthentikClient`
    // exists only for this task ("wires it to Authentik"), and unlike
    // `plan`/`apply`, nothing else in the test suite ever calls it.

    /// Audit 3, B128 (B2-SS-6). Authentik answers a token that lost its
    /// object permission with 200 and an EMPTY list -- and `plan` read that
    /// as "everybody left": a farewell to every friend, every mapping gone,
    /// and a fresh welcome for all of them once it healed. A directory
    /// without a single user is never the truth; it is an error.
    #[tokio::test]
    async fn an_empty_user_list_is_an_error_not_everybody_leaving() {
        for body in [
            serde_json::json!({ "pagination": { "next": 0, "count": 0 }, "results": [] }),
            serde_json::json!({ "detail": "something else entirely" }),
        ] {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body.clone()))
                .mount(&server)
                .await;
            let client = AuthentikClient::new(&server.uri(), Secret::from("t".to_string()));
            assert!(
                client.users("de").await.is_err(),
                "{body} was taken for an empty directory"
            );
        }
    }

    /// Only the first page used to be read. Past `page_size` accounts, the
    /// rest would have been "gone" -- the same mass farewell, by growth.
    #[tokio::test]
    async fn every_page_of_users_is_read() {
        let server = wiremock::MockServer::start().await;
        let page = |next: u32, name: &str| {
            serde_json::json!({
                "pagination": { "next": next },
                "results": [ { "username": name, "groups_obj": [] } ]
            })
        };
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::query_param("page", "2"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(0, "zweite")))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(page(2, "erste")))
            .mount(&server)
            .await;

        let client = AuthentikClient::new(&server.uri(), Secret::from("t".to_string()));
        let users = client.users("de").await.unwrap();
        let names: Vec<_> = users.iter().map(|u| u.username.as_str()).collect();
        assert_eq!(names, ["erste", "zweite"]);
    }

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

    /// Audit 3, B158. The client used to follow redirects, and the bearer
    /// token went along to wherever one pointed on the same host -- while
    /// whatever answered there was read as the directory. A 302 is now an
    /// answer like any other non-200: an error, and nothing is fetched from
    /// the place it names.
    #[tokio::test]
    async fn a_redirect_from_authentik_is_not_followed() {
        // What would be taken for the directory if the redirect were
        // followed -- so that following it shows as data, not as an error
        // that happens to look like the refusal.
        let somebody_elses_list = || {
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "pagination": { "next": 0 },
                "results": [ { "username": "mallory", "groups_obj": [] } ]
            }))
        };
        // Two ways out: to another host, and to another path on the same
        // one -- the second is where reqwest would have kept the token.
        for same_host in [false, true] {
            let elsewhere = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::any())
                .respond_with(somebody_elses_list())
                .expect(0)
                .mount(&elsewhere)
                .await;
            let server = wiremock::MockServer::start().await;
            let target = if same_host {
                "/somewhere/else/".to_string()
            } else {
                format!("{}/somewhere/else/", elsewhere.uri())
            };
            wiremock::Mock::given(wiremock::matchers::path("/api/v3/core/users/"))
                .respond_with(
                    wiremock::ResponseTemplate::new(302).insert_header("location", target.as_str()),
                )
                .expect(1)
                .mount(&server)
                .await;
            wiremock::Mock::given(wiremock::matchers::path("/somewhere/else/"))
                .respond_with(somebody_elses_list())
                .expect(0)
                .mount(&server)
                .await;

            let client =
                AuthentikClient::new(&server.uri(), Secret::from("secret-token".to_string()));
            let err = client
                .users("de")
                .await
                .expect_err("a redirect is not a user list")
                .to_string();
            assert!(err.contains("302"), "same_host={same_host}, got: {err}");
            // Checked here rather than left to `Drop`, so a failure names
            // which of the two ways out was taken.
            server.verify().await;
            elsewhere.verify().await;
        }
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
}
