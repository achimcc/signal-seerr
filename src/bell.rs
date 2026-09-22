//! A second exit for the availability notice: the bell in treff.
//!
//! treff (the forum on the same server) shows a person's news in a bell — on
//! the forum and on the server's start page. The bot already knows who asked
//! for a film and what it is called when Seerr says it is there or failed;
//! this hands that on. Optional: without a `[treff]` section in the
//! configuration nothing here runs.
//!
//! It is sent whether or not the person has a Signal name — somebody who
//! never linked Signal still has a bell — and a failure here never stops the
//! Signal message, nor the other way round.

use crate::secret::Secret;

/// One event, as treff's `POST /internal/events` takes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BellEvent {
    /// The Seerr user's Jellyfin name, which is the Authentik username and
    /// therefore the person's handle in treff.
    pub handle: String,
    /// `film_available` or `film_failed`.
    pub kind: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// `seerr:<request id>` — with `kind`, what makes a webhook that arrives
    /// twice one entry in treff.
    pub source_key: String,
}

#[async_trait::async_trait]
pub trait Bell: Send + Sync {
    async fn ring(&self, event: &BellEvent) -> anyhow::Result<()>;
}

/// treff over HTTP, with its events token.
pub struct TreffBell {
    client: reqwest::Client,
    url: String,
    token: Secret,
}

impl TreffBell {
    pub fn new(url: String, token: Secret) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            // Never follow a redirect: the token must not travel anywhere
            // the operator did not configure.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { client, url, token })
    }

    async fn once(&self, event: &BellEvent) -> anyhow::Result<()> {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(self.token.expose())
            .json(event)
            .send()
            .await?;
        // 201: new. 200: already there — a webhook that arrived twice. Both
        // mean the bell has it; anything else is treff saying no, and its own
        // words are not repeated into a log (they are its vocabulary).
        match response.status().as_u16() {
            200 | 201 => Ok(()),
            code => anyhow::bail!("treff answered {code}"),
        }
    }
}

#[async_trait::async_trait]
impl Bell for TreffBell {
    /// Once, and once more after two seconds: treff restarting for a deploy
    /// is the common failure, and it is short.
    async fn ring(&self, event: &BellEvent) -> anyhow::Result<()> {
        match self.once(event).await {
            Ok(()) => Ok(()),
            Err(first) => {
                tracing::info!(error = %first, "treff did not take the event, trying once more");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                self.once(event).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn event() -> BellEvent {
        BellEvent {
            handle: "robert".into(),
            kind: "film_available",
            title: "Blade Runner 2049 (2017)".into(),
            link: Some("https://jellyfin.example.org".into()),
            source_key: "seerr:1849".into(),
        }
    }

    #[tokio::test]
    async fn an_event_goes_to_treff_with_the_token_and_the_fields() {
        let treff = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/events"))
            .and(header("authorization", "Bearer events-token"))
            .and(body_json(serde_json::json!({
                "handle": "robert",
                "kind": "film_available",
                "title": "Blade Runner 2049 (2017)",
                "link": "https://jellyfin.example.org",
                "source_key": "seerr:1849",
            })))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&treff)
            .await;
        let bell = TreffBell::new(
            format!("{}/internal/events", treff.uri()),
            Secret::from("events-token".to_string()),
        )
        .expect("client");
        bell.ring(&event()).await.expect("taken");
    }

    /// 200 is "already there" — a webhook that arrived twice. Not an error.
    #[tokio::test]
    async fn already_there_is_fine() {
        let treff = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&treff)
            .await;
        let bell = TreffBell::new(treff.uri(), Secret::from("t".to_string())).expect("client");
        bell.ring(&event()).await.expect("fine");
    }

    /// A refusal is an error, after one more try.
    #[tokio::test]
    async fn a_refusal_is_an_error_after_one_more_try() {
        let treff = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .expect(2)
            .mount(&treff)
            .await;
        let bell = TreffBell::new(treff.uri(), Secret::from("t".to_string())).expect("client");
        assert!(bell.ring(&event()).await.is_err());
    }
}
