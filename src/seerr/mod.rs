use crate::model::{Hit, MediaKind, QualityProfile, Seasons, SeerrUserId, Wish};
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
    /// The quality profiles of whichever *arr serves this kind of media,
    /// in the order that *arr lists them. The caller picks by NAME.
    async fn quality_profiles(&self, kind: MediaKind) -> Result<Vec<QualityProfile>>;
    async fn request(
        &self,
        hit: &Hit,
        seasons: Seasons,
        as_user: SeerrUserId,
        profile_id: Option<i64>,
    ) -> Result<i64>;
    /// Which profile the request ACTUALLY carries, read back from Seerr.
    /// `None` means Seerr named none, so the *arr's own default applies.
    async fn profile_of(&self, request_id: i64) -> Result<Option<i64>>;
    /// This one person's own requests.
    async fn pending(&self, as_user: SeerrUserId) -> Result<Vec<Wish>>;
    /// Every wish in the household that is not yet available, across
    /// everybody -- `pending` above is scoped to one person and cannot
    /// answer this.
    async fn open_wishes(&self) -> Result<Vec<Wish>>;
    async fn withdraw(&self, id: i64, as_user: SeerrUserId) -> Result<()>;
    /// Who asked for this request — the Authentik username, read from Seerr
    /// rather than taken from a webhook payload. See the note in Task 13.
    async fn requester_of(&self, request_id: i64) -> Result<Option<String>>;

    /// The title Seerr itself holds for a request. The webhook body carries a
    /// `subject`, but that is text an authenticated caller chooses; it lands
    /// in a message to a person. Audit finding B43c (2026-09-20).
    async fn title_of(&self, request_id: i64) -> Result<Option<String>>;

    /// The title Seerr holds for a `tmdb_id` directly -- `Wish` itself never
    /// carries one; see the note on `title_of`.
    async fn title_for(&self, kind: MediaKind, tmdb_id: i64) -> Result<Option<String>>;
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
    async fn quality_profiles(&self, kind: MediaKind) -> Result<Vec<QualityProfile>> {
        (**self).quality_profiles(kind).await
    }
    async fn request(
        &self,
        hit: &Hit,
        seasons: Seasons,
        as_user: SeerrUserId,
        profile_id: Option<i64>,
    ) -> Result<i64> {
        (**self).request(hit, seasons, as_user, profile_id).await
    }
    async fn profile_of(&self, request_id: i64) -> Result<Option<i64>> {
        (**self).profile_of(request_id).await
    }
    async fn pending(&self, as_user: SeerrUserId) -> Result<Vec<Wish>> {
        (**self).pending(as_user).await
    }
    async fn open_wishes(&self) -> Result<Vec<Wish>> {
        (**self).open_wishes().await
    }
    async fn withdraw(&self, id: i64, as_user: SeerrUserId) -> Result<()> {
        (**self).withdraw(id, as_user).await
    }
    async fn requester_of(&self, request_id: i64) -> Result<Option<String>> {
        (**self).requester_of(request_id).await
    }
    async fn title_of(&self, request_id: i64) -> Result<Option<String>> {
        (**self).title_of(request_id).await
    }
    async fn title_for(&self, kind: MediaKind, tmdb_id: i64) -> Result<Option<String>> {
        (**self).title_for(kind, tmdb_id).await
    }
}

/// Which *arr serves this kind of media. Their id spaces are separate, so
/// this is also the answer to "whose numbers am I holding".
fn arr_of(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movie => "radarr",
        MediaKind::Tv => "sonarr",
    }
}

/// Parses one entry of `results` from `/api/v1/user/{id}/requests` or
/// `/api/v1/request` -- the same shape either way, measured on 2026-09-21
/// (`tests/fixtures/README.md`). `None` means the entry could not be
/// understood; the caller logs it rather than dropping it in silence, since
/// silent dropping is exactly the bug this task removes.
fn wish_from(r: &serde_json::Value) -> Option<Wish> {
    let media = r.get("media")?;
    Some(Wish {
        id: r.get("id")?.as_i64()?,
        kind: match r.get("type")?.as_str()? {
            "tv" => MediaKind::Tv,
            _ => MediaKind::Movie,
        },
        tmdb_id: media.get("tmdbId")?.as_i64()?,
        request_status: r.get("status")?.as_i64()?,
        media_status: media.get("status")?.as_i64()?,
        arr_id: media.get("externalServiceId").and_then(|v| v.as_i64()),
        created_at: time::OffsetDateTime::parse(
            r.get("createdAt")?.as_str()?,
            &time::format_description::well_known::Rfc3339,
        )
        .ok()?,
        profile_name: r
            .get("profileName")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        requested_by: r
            .get("requestedBy")
            .and_then(|u| u.get("jellyfinUsername"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        download_percent: download_percent_from(media),
        seasons: r
            .get("seasons")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|season| season.get("seasonNumber")?.as_u64())
                    .filter_map(|n| u16::try_from(n).ok())
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Seerr's own account of a download, from `media.downloadStatus` --
/// recorded 2026-09-22 (`seerr-user-requests-downloading.json`, see
/// `tests/fixtures/README.md`): an array, one entry per download, each with
/// `status`, `size`, `sizeLeft` and fields never read here (`title`,
/// `downloadId`, `estimatedCompletionTime`, `timeLeft`) so those never end up
/// deserialised, logged, or in a message.
///
/// The first entry whose `status` is `"downloading"` wins; a household is
/// never shown two competing percentages for one wish, and Seerr has not
/// been observed sending more than one entry at a time. `size == 0` answers
/// 0 rather than dividing by it.
fn download_percent_from(media: &serde_json::Value) -> Option<u8> {
    let entry = media
        .get("downloadStatus")?
        .as_array()?
        .iter()
        .find(|d| d.get("status").and_then(|v| v.as_str()) == Some("downloading"))?;
    let size = entry.get("size").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let size_left = entry
        .get("sizeLeft")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    Some(if size > 0.0 {
        ((size - size_left) / size * 100.0) as u8
    } else {
        0
    })
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
                // A redirect carries our own headers onwards. reqwest strips
                // `Authorization` when the host changes; `X-Api-Key` and
                // `X-API-User` are not headers it knows about, so they would
                // be sent to wherever the redirect points -- and that key is
                // a Seerr administrator. Nothing this bot calls redirects,
                // so refusing is free.
                .redirect(reqwest::redirect::Policy::none())
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

    /// Two calls, because the profiles hang off ONE server and Seerr can
    /// hold several: the list says which is the default, the detail carries
    /// that server's profiles. Neither answer contains an `apiKey` -- the
    /// route that would is `/settings/radarr`, and it needs ADMIN.
    async fn quality_profiles(&self, kind: MediaKind) -> Result<Vec<QualityProfile>> {
        let service = arr_of(kind);
        let servers = self
            .json(
                self.get(&format!("/api/v1/service/{service}"))
                    .send()
                    .await?,
            )
            .await?;
        let servers = servers
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("seerr did not list any {service} server"))?;
        // The default is the one FLAGGED default -- not the first, and not
        // id 0. Falling back to the first only when none is flagged.
        let server = servers
            .iter()
            .find(|s| s.get("isDefault").and_then(|d| d.as_bool()) == Some(true))
            .or_else(|| servers.first())
            .ok_or_else(|| anyhow::anyhow!("seerr has no {service} server configured"))?;
        let server_id = server
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("the {service} server carries no id"))?;

        let detail = self
            .json(
                self.get(&format!("/api/v1/service/{service}/{server_id}"))
                    .send()
                    .await?,
            )
            .await?;
        Ok(detail
            .get("profiles")
            .and_then(|p| p.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|p| {
                Some(QualityProfile {
                    id: p.get("id")?.as_i64()?,
                    name: p.get("name")?.as_str()?.to_string(),
                })
            })
            .collect())
    }

    async fn profile_of(&self, request_id: i64) -> Result<Option<i64>> {
        let body = self
            .json(
                self.get(&format!("/api/v1/request/{request_id}"))
                    .send()
                    .await?,
            )
            .await?;
        Ok(body.get("profileId").and_then(|v| v.as_i64()))
    }

    async fn request(
        &self,
        hit: &Hit,
        seasons: Seasons,
        as_user: SeerrUserId,
        profile_id: Option<i64>,
    ) -> Result<i64> {
        let mut body = serde_json::json!({
            "mediaId": hit.tmdb_id,
            "mediaType": match hit.kind { MediaKind::Movie => "movie", MediaKind::Tv => "tv" },
        });
        // OMITTED, not null, when nobody chose: Seerr then takes the *arr's
        // own default. A `"profileId": null` is a value, and a value has to
        // be interpreted by the other side.
        if let Some(id) = profile_id {
            body["profileId"] = serde_json::json!(id);
        }
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

    async fn pending(&self, as_user: SeerrUserId) -> Result<Vec<Wish>> {
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
                let wish = wish_from(r);
                if wish.is_none() {
                    tracing::warn!(id = ?r.get("id"), "request entry did not parse");
                }
                wish
            })
            .collect())
    }

    /// The household-wide list a reminder needs, not scoped to one person --
    /// unlike `pending`, this walks `/api/v1/request` page by page (Seerr
    /// caps `take` well below the size this collection can reach) and then
    /// drops what is already available.
    async fn open_wishes(&self) -> Result<Vec<Wish>> {
        // `skip` COUNTS WHAT WAS DELIVERED, not what was asked for. Seerr
        // caps `take` at its own maximum, so a server that answers 100 with
        // fewer than 100 entries would, with `page * 100`, leave a gap: the
        // wishes in it would simply never be looked at, and a wish nobody
        // looks at is silent for ever -- the very defect this loop exists to
        // remove.
        let mut wishes = Vec::new();
        let mut skip: usize = 0;
        let mut pages_read: i64 = 0;
        loop {
            let response = self
                .get(&format!(
                    "/api/v1/request?take=100&skip={skip}&filter=all&sort=added"
                ))
                .send()
                .await?;
            let body = self.json(response).await?;
            let results = body
                .get("results")
                .and_then(|r| r.as_array())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            for r in results {
                match wish_from(r) {
                    Some(w) => wishes.push(w),
                    None => tracing::warn!(id = ?r.get("id"), "request entry did not parse"),
                }
            }
            let pages = body
                .get("pageInfo")
                .and_then(|p| p.get("pages"))
                .and_then(|p| p.as_i64())
                .unwrap_or(1);
            pages_read += 1;
            // An empty page ends it whatever `pages` claims -- otherwise a
            // `pages` that is too large (or a `skip` the server ignores)
            // turns into a loop that never gets anywhere.
            if results.is_empty() || pages_read >= pages {
                break;
            }
            skip += results.len();
        }
        wishes.retain(|w| w.media_status != 5);
        Ok(wishes)
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

    /// TWO CALLS, AND THE FIRST ONE ALONE DOES NOT DO IT: the request object
    /// carries `media.tmdbId` and `media.mediaType`, but NO title -- measured
    /// at the running instance on 2026-09-20. This first call gets the
    /// `tmdb_id` and kind; `title_for` makes the second.
    async fn title_of(&self, request_id: i64) -> Result<Option<String>> {
        let response = self
            .get(&format!("/api/v1/request/{request_id}"))
            .send()
            .await?;
        let body = self.json(response).await?;
        let media = body.get("media");
        let tmdb = media.and_then(|m| m.get("tmdbId")).and_then(|v| v.as_i64());
        let kind = match media
            .and_then(|m| m.get("mediaType"))
            .and_then(|v| v.as_str())
        {
            Some("tv") => MediaKind::Tv,
            _ => MediaKind::Movie,
        };
        let Some(tmdb) = tmdb else {
            return Ok(None);
        };
        self.title_for(kind, tmdb).await
    }

    /// The title lives behind `/api/v1/movie/{tmdb}` or `/api/v1/tv/{tmdb}`,
    /// where a movie calls it `title` and a series `name`. `Wish` itself
    /// never carries a title -- Seerr does not send one (see the note on
    /// `Wish` in `model.rs`).
    async fn title_for(&self, kind: MediaKind, tmdb_id: i64) -> Result<Option<String>> {
        let path = match kind {
            MediaKind::Tv => format!("/api/v1/tv/{tmdb_id}"),
            MediaKind::Movie => format!("/api/v1/movie/{tmdb_id}"),
        };
        let detail = self.json(self.get(&path).send().await?).await?;
        Ok(detail
            .get("title")
            .or_else(|| detail.get("name"))
            .and_then(|t| t.as_str())
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.to_string()))
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
        async fn quality_profiles(&self, _kind: MediaKind) -> Result<Vec<QualityProfile>> {
            Ok(vec![])
        }
        async fn request(
            &self,
            _hit: &Hit,
            _seasons: Seasons,
            _as_user: SeerrUserId,
            _profile_id: Option<i64>,
        ) -> Result<i64> {
            Ok(1849)
        }
        async fn profile_of(&self, _request_id: i64) -> Result<Option<i64>> {
            Ok(None)
        }
        async fn pending(&self, _as_user: SeerrUserId) -> Result<Vec<Wish>> {
            Ok(vec![Wish {
                id: 1,
                kind: MediaKind::Movie,
                tmdb_id: 42,
                request_status: 1,
                media_status: 3,
                arr_id: None,
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                profile_name: None,
                requested_by: Some("canned-requester".into()),
                download_percent: None,
                seasons: Vec::new(),
            }])
        }
        async fn open_wishes(&self) -> Result<Vec<Wish>> {
            Ok(vec![])
        }
        async fn withdraw(&self, _id: i64, _as_user: SeerrUserId) -> Result<()> {
            Ok(())
        }
        async fn title_of(&self, _request_id: i64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn title_for(&self, _kind: MediaKind, _tmdb_id: i64) -> Result<Option<String>> {
            Ok(Some("canned title".into()))
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
                    None,
                )
                .await
                .unwrap(),
            1849
        );
        let pending = client.pending(SeerrUserId(7)).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].requested_by.as_deref(), Some("canned-requester"));
        assert_eq!(client.open_wishes().await.unwrap(), vec![]);
        assert_eq!(
            client.title_for(MediaKind::Movie, 42).await.unwrap(),
            Some("canned title".to_string())
        );
        assert!(client.withdraw(1, SeerrUserId(7)).await.is_ok());
        assert_eq!(
            client.requester_of(1).await.unwrap(),
            Some("canned-requester".to_string())
        );
    }
}
