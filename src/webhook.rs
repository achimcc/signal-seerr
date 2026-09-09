use crate::i18n::Catalogue;
use crate::secret::Secret;
use crate::seerr::Requests;
use crate::signal::Messenger;
use crate::state::State;
use axum::body::Bytes;
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct Payload {
    pub notification_type: String,
    /// Seerr's own one-liner, e.g. "Blade Runner 2049 (2017)".
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub request: Option<RequestPart>,
}

#[derive(Debug, Deserialize)]
pub struct RequestPart {
    /// Filled from {{request_id}}. Deliberately the ONLY thing we take from
    /// the payload about who asked: {{requestedBy_username}} sounds right and
    /// is not -- it maps to request.requestedBy.displayName, which the person
    /// can change in their own Seerr profile. Measured at the shipped
    /// package. The name comes from Seerr's API instead.
    ///
    /// A STRING on the wire, not a number: the payload template writes
    /// `"request_id":"{{request_id}}"`, quotes included, because a bare
    /// `{{request_id}}` would leave `{"request_id":}` -- broken JSON --
    /// behind whenever there is no request to substitute. So Seerr quotes it,
    /// and this field has to accept that. It also accepts a number, because
    /// nothing forces an operator to quote it in their own template.
    #[serde(rename = "request_id", default, deserialize_with = "loose_id")]
    pub request_id: Option<i64>,
}

/// Reads an id that may arrive as a number, as a quoted number, as an empty
/// string (Seerr's substitution for "there is no request") or as null.
///
/// Anything else is `None` rather than an error, on purpose: a payload this
/// bot cannot make sense of must be accepted and dropped, never answered with
/// a 4xx that makes Seerr retry it for ever.
fn loose_id<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

#[derive(Clone)]
pub struct WebhookState {
    pub messenger: Arc<dyn Messenger>,
    pub seerr: Arc<dyn Requests>,
    /// std, not tokio: the critical section is a `find` over a handful of
    /// entries with no await inside it.
    pub directory: Arc<std::sync::RwLock<State>>,
    pub catalogue: Arc<Catalogue>,
    pub token: Arc<Secret>,
    pub jellyfin_url: String,
}

pub fn router(state: WebhookState) -> Router {
    Router::new()
        .route("/seerr", post(handle))
        .with_state(state)
}

async fn handle(
    AxumState(state): AxumState<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let offered = headers
        .get("X-Webhook-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // `Secret::matches` and not `!=`: a plain comparison returns at the first
    // differing byte and leaks the token's prefix to anyone who can time the
    // answer. Cheap to get right here, awkward to retrofit later.
    if !state.token.matches(offered) {
        return StatusCode::UNAUTHORIZED;
    }

    // Parsed only now, deliberately: `Json<Payload>` as an extractor would
    // have deserialised the body before this function even started running
    // -- axum runs extractors in declaration order, ahead of the handler
    // body -- so untrusted input would be parsed before it was authenticated.
    // Taking the raw bytes and parsing by hand keeps the token check first.
    let Ok(payload) = serde_json::from_slice::<Payload>(&body) else {
        // A malformed body from an authenticated caller is a real 400: Seerr
        // sent us something we do not understand, and retrying will not help.
        tracing::warn!("webhook body did not parse");
        return StatusCode::BAD_REQUEST;
    };

    let key = match payload.notification_type.as_str() {
        "MEDIA_AVAILABLE" => "available.ready",
        "MEDIA_FAILED" => "available.failed",
        // Everything else is accepted and dropped. A 4xx would make Seerr
        // retry for ever over an event we chose not to relay.
        other => {
            tracing::debug!(event = other, "event not relayed");
            return StatusCode::OK;
        }
    };

    let Some(request_id) = payload.request.and_then(|r| r.request_id) else {
        tracing::info!("webhook without a request id");
        return StatusCode::OK;
    };

    // Ask Seerr who asked. The payload cannot tell us -- see RequestPart.
    let username = match state.seerr.requester_of(request_id).await {
        Ok(Some(name)) => name,
        Ok(None) => {
            tracing::info!(request_id, "seerr names no requester for this id");
            return StatusCode::OK;
        }
        Err(e) => {
            tracing::warn!(error = %e, request_id, "cannot ask seerr who requested this");
            return StatusCode::OK;
        }
    };

    let entry = {
        let directory = state
            .directory
            .read()
            .expect("the mapping lock is never poisoned");
        directory.by_user(&username).cloned()
    };
    let Some(entry) = entry else {
        tracing::info!(username, "requester has no signal name");
        return StatusCode::OK;
    };

    let text = state.catalogue.text(
        entry.locale,
        key,
        &[("title", &payload.subject), ("url", &state.jellyfin_url)],
    );
    if let Err(e) = state.messenger.send(&entry.aci, &text).await {
        tracing::warn!(error = %e, username, "cannot deliver the availability notice");
    }
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::model::{Aci, Hit, MediaKind, Pending, Seasons, SeerrUserId};
    use crate::state::Entry;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Mutex;
    use tower::ServiceExt;

    fn body(event: &str, request_id: i64) -> Body {
        // The payload carries an ID and nothing else about the person. See
        // the note under Step 4 for why the name in it cannot be trusted.
        Body::from(
            serde_json::json!({
                "notification_type": event,
                "subject": "Blade Runner 2049 (2017)",
                "request": { "request_id": request_id }
            })
            .to_string(),
        )
    }

    /// `requester_of` answers a fixed table; every other method is unused by
    /// this handler and panics if it is ever called.
    struct FakeSeerr;

    #[async_trait::async_trait]
    impl Requests for FakeSeerr {
        async fn search(
            &self,
            _q: &str,
            _kind: Option<MediaKind>,
            _page: u32,
        ) -> anyhow::Result<Vec<Hit>> {
            unreachable!("not used by the webhook handler")
        }
        async fn user_id(&self, _u: &str) -> anyhow::Result<Option<SeerrUserId>> {
            unreachable!("not used by the webhook handler")
        }
        async fn quality_profiles(
            &self,
            _kind: MediaKind,
        ) -> anyhow::Result<Vec<crate::model::QualityProfile>> {
            unreachable!("not used by the webhook handler")
        }
        async fn request(
            &self,
            _hit: &Hit,
            _seasons: Seasons,
            _as_user: SeerrUserId,
            _profile_id: Option<i64>,
        ) -> anyhow::Result<i64> {
            unreachable!("not used by the webhook handler")
        }
        async fn profile_of(&self, _request_id: i64) -> anyhow::Result<Option<i64>> {
            unreachable!("not used by the webhook handler")
        }
        async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Pending>> {
            unreachable!("not used by the webhook handler")
        }
        async fn withdraw(&self, _id: i64, _u: SeerrUserId) -> anyhow::Result<()> {
            unreachable!("not used by the webhook handler")
        }
        async fn requester_of(&self, request_id: i64) -> anyhow::Result<Option<String>> {
            Ok(match request_id {
                // "robert" is the requester behind the request id used by the
                // "reaches the person who asked" and "unsubscribed event"
                // tests, and holds a signal name in `test_app`'s state.
                1849 => Some("robert".to_string()),
                // 4242 belongs to a Seerr account with no matching entry in
                // our state -- e.g. somebody who never linked Signal.
                4242 => Some("konrad".to_string()),
                // 9001 is "silvia", whose account is set to English -- used to
                // check the notice goes out in the requester's own locale,
                // not a hardcoded one.
                9001 => Some("silvia".to_string()),
                _ => None,
            })
        }
    }

    fn entry(user: &str, aci: &str, locale: Locale) -> Entry {
        Entry {
            authentik_username: user.into(),
            signal_username: format!("{user}.1"),
            aci: Aci(aci.into()),
            greeted: true,
            locale,
            groups: vec!["Medien".into()],
        }
    }

    /// What `SharedMessenger` recorded: the aci a message was sent to, and its text.
    type SentLog = Arc<Mutex<Vec<(String, String)>>>;

    fn test_app() -> (Router, SentLog) {
        let sent: SentLog = Arc::new(Mutex::new(Vec::new()));
        let messenger = Arc::new(SharedMessenger { sent: sent.clone() });

        let mut state = State::default();
        state.upsert(entry("robert", "aaaa", Locale::De));
        state.upsert(entry("silvia", "eeee", Locale::En));

        let webhook_state = WebhookState {
            messenger,
            seerr: Arc::new(FakeSeerr),
            directory: Arc::new(std::sync::RwLock::new(state)),
            catalogue: Arc::new(Catalogue::load()),
            token: Arc::new(Secret::from("t-o-k-e-n".to_string())),
            jellyfin_url: "https://jellyfin.example.org".to_string(),
        };
        (router(webhook_state), sent)
    }

    /// A `Messenger` that appends every send to a shared, externally visible
    /// log, so the test can assert on what the handler tried to deliver.
    struct SharedMessenger {
        sent: SentLog,
    }

    #[async_trait::async_trait]
    impl Messenger for SharedMessenger {
        async fn send(&self, to: &Aci, text: &str) -> anyhow::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((to.0.clone(), text.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused_before_anything_is_read() {
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "wrong")
                    .header("content-type", "application/json")
                    .body(body("MEDIA_AVAILABLE", 1849))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn media_available_reaches_the_person_who_asked() {
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    .body(body("MEDIA_AVAILABLE", 1849))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "aaaa");
        assert!(
            sent[0].1.contains("Blade Runner 2049"),
            "got: {}",
            sent[0].1
        );
        assert!(
            sent[0].1.contains("jellyfin"),
            "the link must be there: {}",
            sent[0].1
        );
    }

    #[tokio::test]
    async fn media_available_uses_the_requesters_locale_not_a_hardcoded_one() {
        // "silvia" (request id 9001) is set to English in `test_app`'s state.
        // `Entry.locale` exists so the notice can be worded without a second
        // network round trip -- this must actually be read, not ignored in
        // favour of a fixed language.
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    .body(body("MEDIA_AVAILABLE", 9001))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "eeee");
        let expected = Catalogue::load().text(
            Locale::En,
            "available.ready",
            &[
                ("title", "Blade Runner 2049 (2017)"),
                ("url", "https://jellyfin.example.org"),
            ],
        );
        assert_eq!(sent[0].1, expected, "must be the ENGLISH catalogue text");
    }

    #[tokio::test]
    async fn media_failed_uses_the_requesters_locale_too() {
        // The two notification branches share one `text()` call in the
        // handler; this guards against a future refactor giving them two
        // separate calls and fixing only one.
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    .body(body("MEDIA_FAILED", 9001))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let expected = Catalogue::load().text(
            Locale::En,
            "available.failed",
            &[
                ("title", "Blade Runner 2049 (2017)"),
                ("url", "https://jellyfin.example.org"),
            ],
        );
        assert_eq!(sent[0].1, expected, "must be the ENGLISH catalogue text");
    }

    #[tokio::test]
    async fn an_unsubscribed_event_is_accepted_and_ignored() {
        // Seerr sends every event type it is configured for. Answering 4xx
        // would make Seerr retry for ever over something we chose not to relay.
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    .body(body("MEDIA_PENDING", 1849))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_wrong_token_with_an_unparseable_body_is_still_refused() {
        // The body is deliberately not JSON at all. Authentication must
        // happen before any attempt is made to parse it -- a `Json<Payload>`
        // extractor would run before `handle`'s body and answer 400/415 on
        // its own, making this 401 branch unreachable for exactly the
        // requests that matter most (an attacker who does not know the
        // token and sends garbage).
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "wrong")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_correct_token_with_an_unparseable_body_is_a_bad_request() {
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(sent.lock().unwrap().is_empty());
    }

    /// A `Messenger` that always fails, to check the webhook response does
    /// not depend on delivery succeeding.
    struct AlwaysFailingMessenger;

    #[async_trait::async_trait]
    impl Messenger for AlwaysFailingMessenger {
        async fn send(&self, _to: &Aci, _text: &str) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("signal-cli is unreachable"))
        }
    }

    #[tokio::test]
    async fn a_failed_delivery_still_answers_200() {
        // Seerr cannot fix a Signal-side delivery failure by retrying the
        // webhook -- 200 here means "received", not "delivered", and a 5xx
        // would start a retry storm over something retrying cannot fix.
        let mut state = State::default();
        state.upsert(entry("robert", "aaaa", Locale::De));
        let webhook_state = WebhookState {
            messenger: Arc::new(AlwaysFailingMessenger),
            seerr: Arc::new(FakeSeerr),
            directory: Arc::new(std::sync::RwLock::new(state)),
            catalogue: Arc::new(Catalogue::load()),
            token: Arc::new(Secret::from("t-o-k-e-n".to_string())),
            jellyfin_url: "https://jellyfin.example.org".to_string(),
        };
        let app = router(webhook_state);

        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    .body(body("MEDIA_AVAILABLE", 1849))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_requester_without_a_signal_name_is_accepted_and_dropped() {
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    .body(body("MEDIA_AVAILABLE", 4242))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(sent.lock().unwrap().is_empty());
    }

    /// Seerr sends `request_id` as a STRING, and this is the payload it
    /// really put on the wire on 2026-09-09 at 11:02:49 -- the template in
    /// `req-01.nix` reads `"request_id":"{{request_id}}"`, quotes included,
    /// because a bare `{{request_id}}` would leave `{"request_id":}` behind
    /// whenever there is no request. So the quotes are not a mistake in the
    /// template; the mistake was reading them as a number.
    ///
    /// Every other test in this file builds the body with `serde_json::json!`
    /// and an i64, so all of them agreed with each other and none of them
    /// agreed with Seerr. The bot answered 400, logged "webhook body did not
    /// parse", and the person who asked for the film was told nothing at all
    /// -- not even that something had gone wrong.
    #[tokio::test]
    async fn seerrs_own_payload_carries_the_request_id_as_a_string() {
        let (app, sent) = test_app();
        let response = app
            .oneshot(
                Request::post("/seerr")
                    .header("X-Webhook-Token", "t-o-k-e-n")
                    .header("content-type", "application/json")
                    // Written out by hand, NOT via json!(): the quoting is
                    // the thing under test.
                    .body(Body::from(
                        r#"{"notification_type":"MEDIA_AVAILABLE","subject":"Gilbert Grape (1993)","request":{"request_id":"1849"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a quoted request id is what Seerr actually sends"
        );
        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            1,
            "the requester must be told the film is there"
        );
    }
}
