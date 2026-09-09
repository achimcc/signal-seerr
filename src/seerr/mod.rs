use crate::model::{Hit, MediaKind, Pending, PendingState, Seasons, SeerrUserId};
use crate::secret::Secret;
use anyhow::{bail, Result};
use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};

/// Seerr's MediaStatus, from dist/constants/media.js. 4 = PARTIALLY_AVAILABLE,
/// 5 = AVAILABLE; 2 (PENDING) and 3 (PROCESSING) mean somebody already asked.
const STATUS_MEANS_ALREADY: &[i64] = &[2, 3, 4, 5];

#[async_trait]
pub trait Requests: Send + Sync {
    async fn search(&self, query: &str, kind: Option<MediaKind>, page: u32) -> Result<Vec<Hit>>;
    async fn user_id(&self, authentik_username: &str) -> Result<Option<SeerrUserId>>;
    async fn request(&self, hit: &Hit, seasons: Seasons, as_user: SeerrUserId) -> Result<i64>;
    async fn pending(&self, as_user: SeerrUserId) -> Result<Vec<Pending>>;
    async fn withdraw(&self, id: i64, as_user: SeerrUserId) -> Result<()>;
    /// Who asked for this request — the Authentik username, read from Seerr
    /// rather than taken from a webhook payload. See the note in Task 13.
    async fn requester_of(&self, request_id: i64) -> Result<Option<String>>;
}

/// Lets an `Arc<SeerrClient>` satisfy `R: Requests` directly, so the same
/// `Arc` that is shared into the webhook (as `Arc<dyn Requests>`, via the
/// ordinary unsized coercion) can also be moved into `Dialog`, which is
/// generic over `R: Requests` and takes it by value. Without this, the
/// client would have to be duplicated or `Dialog` given its own connection.
#[async_trait]
impl<T: Requests + ?Sized> Requests for std::sync::Arc<T> {
    async fn search(&self, query: &str, kind: Option<MediaKind>, page: u32) -> Result<Vec<Hit>> {
        (**self).search(query, kind, page).await
    }
    async fn user_id(&self, authentik_username: &str) -> Result<Option<SeerrUserId>> {
        (**self).user_id(authentik_username).await
    }
    async fn request(&self, hit: &Hit, seasons: Seasons, as_user: SeerrUserId) -> Result<i64> {
        (**self).request(hit, seasons, as_user).await
    }
    async fn pending(&self, as_user: SeerrUserId) -> Result<Vec<Pending>> {
        (**self).pending(as_user).await
    }
    async fn withdraw(&self, id: i64, as_user: SeerrUserId) -> Result<()> {
        (**self).withdraw(id, as_user).await
    }
    async fn requester_of(&self, request_id: i64) -> Result<Option<String>> {
        (**self).requester_of(request_id).await
    }
}

pub struct SeerrClient {
    base: String,
    key: Secret,
    http: reqwest::Client,
}

impl SeerrClient {
    pub fn new(base: &str, key: Secret) -> SeerrClient {
        SeerrClient {
            base: base.trim_end_matches('/').to_string(),
            key,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect(
                    "could not build the HTTP client -- rustls-platform-verifier reads the \
                     system certificate store as soon as a Client exists, so this fails \
                     wherever that store is missing; point SSL_CERT_FILE at a CA bundle",
                ),
        }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.base))
            .header("X-Api-Key", self.key.expose())
    }

    fn post(&self, path: &str, as_user: SeerrUserId) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base))
            .header("X-Api-Key", self.key.expose())
            // Seerr reads this in dist/middleware/auth.js and acts as that user.
            .header("X-API-User", as_user.0.to_string())
    }

    /// Every call goes through here so that no branch can forget to look at
    /// the status. An error body is never echoed: Seerr's settings endpoints
    /// return the API keys of Radarr and Sonarr in the clear.
    async fn json(&self, response: reqwest::Response) -> Result<serde_json::Value> {
        let status = response.status();
        if !status.is_success() {
            bail!("seerr answered {status}");
        }
        Ok(response.json().await?)
    }
}

#[async_trait]
impl Requests for SeerrClient {
    async fn search(&self, query: &str, kind: Option<MediaKind>, page: u32) -> Result<Vec<Hit>> {
        // Built by hand rather than with `.query()`: that serialises
        // form-urlencoded, so a space becomes `+`, and Seerr's OpenAPI
        // validator refuses a `query` carrying a reserved character with
        // 400 ("Parameter 'query' must be url encoded"). Percent-encoding
        // is what it accepts. Only this call takes text a person typed --
        // the `take=` parameters elsewhere have nothing to encode.
        let encoded = utf8_percent_encode(query, NON_ALPHANUMERIC);
        let response = self
            .get(&format!("/api/v1/search?query={encoded}&page={page}"))
            .send()
            .await?;
        let body = self.json(response).await?;

        let wanted = |m: &str| match kind {
            None => m == "movie" || m == "tv",
            Some(MediaKind::Movie) => m == "movie",
            Some(MediaKind::Tv) => m == "tv",
        };

        Ok(body
            .get("results")
            .and_then(|r| r.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|entry| {
                let media_type = entry.get("mediaType")?.as_str()?;
                if !wanted(media_type) {
                    return None;
                }
                let (kind, title_key, date_key) = if media_type == "movie" {
                    (MediaKind::Movie, "title", "releaseDate")
                } else {
                    (MediaKind::Tv, "name", "firstAirDate")
                };
                Some(Hit {
                    tmdb_id: entry.get("id")?.as_i64()?,
                    kind,
                    title: entry.get(title_key)?.as_str()?.to_string(),
                    year: entry
                        .get(date_key)
                        .and_then(|d| d.as_str())
                        .and_then(|d| d.get(0..4))
                        .and_then(|y| y.parse().ok()),
                    rating: entry
                        .get("voteAverage")
                        .and_then(|v| v.as_f64())
                        .map(|v| v as f32),
                    seasons: entry
                        .get("seasonCount")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u16,
                    already: entry
                        .get("mediaInfo")
                        .and_then(|m| m.get("status"))
                        .and_then(|s| s.as_i64())
                        .is_some_and(|s| STATUS_MEANS_ALREADY.contains(&s)),
                })
            })
            .collect())
    }

    async fn user_id(&self, authentik_username: &str) -> Result<Option<SeerrUserId>> {
        // Seerr's accounts are created on first Jellyfin login, and Jellyfin
        // authenticates against Authentik through the LDAP outpost -- so the
        // jellyfinUsername IS the Authentik username.
        let response = self
            .get("/api/v1/user")
            .query(&[("take", "500")])
            .send()
            .await?;
        let body = self.json(response).await?;
        Ok(body
            .get("results")
            .and_then(|r| r.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .find(|u| {
                u.get("jellyfinUsername").and_then(|n| n.as_str()) == Some(authentik_username)
            })
            .and_then(|u| u.get("id")?.as_i64())
            .map(SeerrUserId))
    }

    async fn request(&self, hit: &Hit, seasons: Seasons, as_user: SeerrUserId) -> Result<i64> {
        let mut body = serde_json::json!({
            "mediaId": hit.tmdb_id,
            "mediaType": match hit.kind { MediaKind::Movie => "movie", MediaKind::Tv => "tv" },
        });
        match seasons {
            Seasons::NotApplicable => {}
            Seasons::All => body["seasons"] = serde_json::Value::String("all".into()),
            Seasons::Only(list) => body["seasons"] = serde_json::json!(list),
        }

        let response = self
            .post("/api/v1/request", as_user)
            .json(&body)
            .send()
            .await?;
        let answer = self.json(response).await?;
        answer
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("seerr accepted the request but named no id"))
    }

    async fn pending(&self, as_user: SeerrUserId) -> Result<Vec<Pending>> {
        let response = self
            .get(&format!("/api/v1/user/{}/requests", as_user.0))
            .query(&[("take", "50")])
            .send()
            .await?;
        let body = self.json(response).await?;
        Ok(body
            .get("results")
            .and_then(|r| r.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|r| {
                let media = r.get("media")?;
                Some(Pending {
                    id: r.get("id")?.as_i64()?,
                    title: media
                        .get("title")
                        .or_else(|| media.get("name"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    state: match media.get("status").and_then(|s| s.as_i64()) {
                        Some(5) => PendingState::Available,
                        Some(3) | Some(4) => PendingState::Fetching,
                        _ => PendingState::Waiting,
                    },
                })
            })
            .collect())
    }

    async fn requester_of(&self, request_id: i64) -> Result<Option<String>> {
        // The webhook payload's {{requestedBy_username}} is NOT a username --
        // it maps to request.requestedBy.displayName, which the person can
        // change in their own Seerr profile. Measured at the running package.
        // So identity comes from here, through the same field user_id()
        // matches on: one identity source, not two.
        let response = self
            .get(&format!("/api/v1/request/{request_id}"))
            .send()
            .await?;
        let body = self.json(response).await?;
        Ok(body
            .get("requestedBy")
            .and_then(|u| u.get("jellyfinUsername"))
            .and_then(|n| n.as_str())
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string()))
    }

    async fn withdraw(&self, id: i64, as_user: SeerrUserId) -> Result<()> {
        // The API key is an administrator, so Seerr would delete anybody's
        // request. The owner check has to happen here.
        let response = self.get(&format!("/api/v1/request/{id}")).send().await?;
        let existing = self.json(response).await?;
        let owner = existing
            .get("requestedBy")
            .and_then(|u| u.get("id"))
            .and_then(|v| v.as_i64());
        if owner != Some(as_user.0) {
            bail!("request {id} is not yours");
        }

        let response = self
            .http
            .delete(format!("{}/api/v1/request/{id}", self.base))
            .header("X-Api-Key", self.key.expose())
            .header("X-API-User", as_user.0.to_string())
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("seerr answered {} when withdrawing {id}", response.status());
        }
        Ok(())
    }
}

#[cfg(test)]
mod arc_requests_tests {
    use super::*;
    use std::sync::Arc;

    /// Answers every method with a fixed, recognisable value -- there is no
    /// HTTP call for the blanket impl to make, only forwarding for it to get
    /// right or wrong.
    struct MockRequests;

    #[async_trait]
    impl Requests for MockRequests {
        async fn search(
            &self,
            _query: &str,
            _kind: Option<MediaKind>,
            _page: u32,
        ) -> Result<Vec<Hit>> {
            Ok(vec![Hit {
                tmdb_id: 42,
                kind: MediaKind::Movie,
                title: "canned title".into(),
                year: Some(2020),
                rating: Some(7.5),
                seasons: 0,
                already: false,
            }])
        }
        async fn user_id(&self, _authentik_username: &str) -> Result<Option<SeerrUserId>> {
            Ok(Some(SeerrUserId(7)))
        }
        async fn request(
            &self,
            _hit: &Hit,
            _seasons: Seasons,
            _as_user: SeerrUserId,
        ) -> Result<i64> {
            Ok(1849)
        }
        async fn pending(&self, _as_user: SeerrUserId) -> Result<Vec<Pending>> {
            Ok(vec![Pending {
                id: 1,
                title: "canned pending".into(),
                state: PendingState::Waiting,
            }])
        }
        async fn withdraw(&self, _id: i64, _as_user: SeerrUserId) -> Result<()> {
            Ok(())
        }
        async fn requester_of(&self, _request_id: i64) -> Result<Option<String>> {
            Ok(Some("canned-requester".to_string()))
        }
    }

    /// The failure mode this guards against: an `Arc<T>` impl that calls
    /// `self.search(...)` instead of `(**self).search(...)` recurses onto
    /// its own blanket impl forever -- a stack overflow on the very first
    /// request, and nothing in a wiring task would ever exercise it, since
    /// `main.rs` only ever moves the `Arc` around and never calls through
    /// it directly. `search` is checked in full; the other five follow the
    /// exact same one-line forwarding shape.
    #[tokio::test]
    async fn an_arc_forwards_search_to_the_wrapped_client_not_to_itself() {
        let client: Arc<dyn Requests> = Arc::new(MockRequests);
        let hits = client.search("anything", None, 1).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "canned title");
    }

    #[tokio::test]
    async fn an_arc_forwards_every_other_method_too() {
        let client: Arc<dyn Requests> = Arc::new(MockRequests);

        assert_eq!(
            client.user_id("robert").await.unwrap(),
            Some(SeerrUserId(7))
        );
        assert_eq!(
            client
                .request(
                    &Hit {
                        tmdb_id: 1,
                        kind: MediaKind::Movie,
                        title: "x".into(),
                        year: None,
                        rating: None,
                        seasons: 0,
                        already: false,
                    },
                    Seasons::NotApplicable,
                    SeerrUserId(7),
                )
                .await
                .unwrap(),
            1849
        );
        let pending = client.pending(SeerrUserId(7)).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "canned pending");
        assert!(client.withdraw(1, SeerrUserId(7)).await.is_ok());
        assert_eq!(
            client.requester_of(1).await.unwrap(),
            Some("canned-requester".to_string())
        );
    }
}
