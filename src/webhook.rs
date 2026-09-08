use crate::i18n::{Catalogue, Locale};
use crate::secret::Secret;
use crate::seerr::Requests;
use crate::signal::Messenger;
use crate::state::State;
use axum::extract::State as AxumState;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
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
    #[serde(rename = "request_id")]
    pub request_id: Option<i64>,
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
    Json(payload): Json<Payload>,
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
        Locale::De,
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
        async fn request(
            &self,
            _hit: &Hit,
            _seasons: Seasons,
            _as_user: SeerrUserId,
        ) -> anyhow::Result<i64> {
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
                _ => None,
            })
        }
    }

    fn entry(user: &str, aci: &str) -> Entry {
        Entry {
            authentik_username: user.into(),
            signal_username: format!("{user}.1"),
            aci: Aci(aci.into()),
            greeted: true,
            locale: Locale::De,
            groups: vec!["Medien".into()],
        }
    }

    /// What `SharedMessenger` recorded: the aci a message was sent to, and its text.
    type SentLog = Arc<Mutex<Vec<(String, String)>>>;

    fn test_app() -> (Router, SentLog) {
        let sent: SentLog = Arc::new(Mutex::new(Vec::new()));
        let messenger = Arc::new(SharedMessenger { sent: sent.clone() });

        let mut state = State::default();
        state.upsert(entry("robert", "aaaa"));

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
}
