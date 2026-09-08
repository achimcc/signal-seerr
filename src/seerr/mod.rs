use crate::model::{Hit, MediaKind, SeerrUserId};
use crate::secret::Secret;
use anyhow::{bail, Result};
use async_trait::async_trait;

/// Seerr's MediaStatus, from dist/constants/media.js. 4 = PARTIALLY_AVAILABLE,
/// 5 = AVAILABLE; 2 (PENDING) and 3 (PROCESSING) mean somebody already asked.
const STATUS_MEANS_ALREADY: &[i64] = &[2, 3, 4, 5];

#[async_trait]
pub trait Requests: Send + Sync {
    async fn search(&self, query: &str, kind: Option<MediaKind>, page: u32) -> Result<Vec<Hit>>;
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
                .expect("a client with no TLS surprises"),
        }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.base))
            .header("X-Api-Key", self.key.expose())
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
        let response = self
            .get("/api/v1/search")
            .query(&[("query", query), ("page", &page.to_string())])
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
}
