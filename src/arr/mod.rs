//! A read-only Radarr/Sonarr client, and the separate, deliberately
//! non-`Insight` trait for an interactive release search.
//!
//! Two traits, not one: `Insight` is what the dialog module is ever handed --
//! read-only, and by its very type unable to trigger a search at every
//! indexer. `ReleaseSearch` is that search, and only `watch` (a later task)
//! is ever given something that implements it.

use crate::model::MediaKind;
use crate::secret::Secret;
use anyhow::{bail, Result};
use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq)]
pub struct ArrMovie {
    pub is_available: bool,
    pub has_file: bool,
    pub digital_release: Option<time::OffsetDateTime>,
    pub physical_release: Option<time::OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueState {
    Downloading,
    ImportStuck,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueItem {
    pub arr_id: i64,
    pub percent: u8,
    pub state: QueueState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryEvent {
    Grabbed,
    DownloadFailed,
    Imported,
    Other,
}

/// Deliberately three fields. Titles, indexers, URLs are never deserialised,
/// so they cannot end up in a message or a log line -- and `get` decodes the
/// answer straight into `Vec<Release>`, so they are not built into a
/// `serde_json::Value` on the way either.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Release {
    pub rejected: bool,
    #[serde(default)]
    pub rejections: Vec<String>,
    #[serde(default, deserialize_with = "language_names")]
    pub languages: Vec<String>,
}

/// The wire format carries `languages` as an array of `{id, name}` objects;
/// only the name is ever kept.
fn language_names<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    struct Language {
        name: String,
    }
    let languages: Vec<Language> = serde::Deserialize::deserialize(deserializer)?;
    Ok(languages.into_iter().map(|l| l.name).collect())
}

#[async_trait]
pub trait Insight: Send + Sync {
    async fn movie(&self, id: i64) -> Result<ArrMovie>;
    async fn queue(&self, kind: MediaKind) -> Result<Vec<QueueItem>>;
    async fn last_event(&self, kind: MediaKind, id: i64) -> Result<Option<HistoryEvent>>;
}

/// NOT part of `Insight`, on purpose: this is an interactive search at every
/// indexer. Only `watch` is ever handed something that implements it.
#[async_trait]
pub trait ReleaseSearch: Send + Sync {
    async fn releases(&self, movie_id: i64) -> Result<Vec<Release>>;
}

/// Which of `trackedDownloadState`'s values means a download in progress, as
/// opposed to an import stuck, mapped only from what a running instance has
/// actually sent -- this project never guesses a wire value.
///
/// `"downloading"` is recorded (`radarr-queue-downloading.json`, see
/// `tests/fixtures/README.md`) and means `QueueState::Downloading`.
/// `QueueState::ImportStuck` stays unreachable: which value
/// `trackedDownloadState` carries for a stuck import has **not** been
/// recorded yet, so nothing here claims to know it. Every other value --
/// including one never seen at all -- keeps the same answer, `Downloading`,
/// for the same reason: there is nothing recorded that says otherwise.
fn queue_state(tracked: Option<&str>) -> QueueState {
    match tracked {
        Some("downloading") => QueueState::Downloading,
        // No recording tells the two apart yet -- see the doc comment above.
        _ => QueueState::Downloading,
    }
}

fn map_event_type(event_type: &str) -> HistoryEvent {
    match event_type {
        "grabbed" => HistoryEvent::Grabbed,
        "downloadFailed" => HistoryEvent::DownloadFailed,
        "downloadFolderImported" => HistoryEvent::Imported,
        _ => HistoryEvent::Other,
    }
}

fn parse_date(value: Option<&serde_json::Value>) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(
        value?.as_str()?,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()
}

pub struct ArrClient {
    radarr: (String, Secret),
    sonarr: Option<(String, Secret)>,
    http: reqwest::Client,
}

impl ArrClient {
    pub fn new(radarr_url: &str, radarr_key: Secret, sonarr: Option<(&str, Secret)>) -> ArrClient {
        ArrClient {
            radarr: (radarr_url.to_string(), radarr_key),
            sonarr: sonarr.map(|(url, key)| (url.to_string(), key)),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                // A redirect carries our own headers onwards. reqwest strips
                // `Authorization` when the host changes; `X-Api-Key` is not a
                // header it knows about, so it is sent to wherever the
                // redirect points -- and this one is full write access to
                // Radarr. Nothing this bot calls redirects, so refusing is
                // free.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect(
                    "could not build the HTTP client -- rustls-platform-verifier reads the \
                     system certificate store as soon as a Client exists, so this fails \
                     wherever that store is missing; point SSL_CERT_FILE at a CA bundle",
                ),
        }
    }

    /// Which service serves this kind of media, and under which name --
    /// `queue` and `last_event` need both, `movie` and `releases` only ever
    /// mean Radarr, since there is no such thing as a "movie" in Sonarr.
    fn service(&self, kind: MediaKind) -> Result<(&str, &Secret, &'static str)> {
        match kind {
            MediaKind::Movie => Ok((self.radarr.0.as_str(), &self.radarr.1, "radarr")),
            MediaKind::Tv => self
                .sonarr
                .as_ref()
                .map(|(url, key)| (url.as_str(), key, "sonarr"))
                .ok_or_else(|| anyhow::anyhow!("sonarr is not configured")),
        }
    }

    /// Every call goes through here so that no branch can forget to look at
    /// the status. `base` may carry a path part (`http://host:7870/radarr`),
    /// so the URL is built with `format!`, not `Url::join`, which would drop
    /// it. An error body is never echoed -- it can carry a release's or a
    /// series' name.
    ///
    /// GENERIC IN `T`, AND THAT IS THE WHOLE POINT FOR `/release`: the answer
    /// is decoded straight into the caller's type, so the fields this bot
    /// does not name are never built into a `serde_json::Value` tree at all.
    /// A release list carries `downloadUrl`, `guid` and `infoUrl`, and those
    /// carry the operator's indexer keys and tracker passkeys; with a `Value`
    /// in between, every one of them sat in memory no matter what was read
    /// out of it afterwards.
    ///
    /// A DESERIALISATION FAILURE IS REPORTED WITHOUT SERDE'S OWN TEXT for the
    /// same reason: serde names the offending VALUE ("invalid type: string
    /// \"...\""), and this error travels by `?` into a `tracing::warn!` in
    /// `watch.rs`. The path and the status say enough to find the fault.
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        base: &str,
        key: &Secret,
        service: &str,
        path: &str,
    ) -> Result<T> {
        let url = format!("{}{path}", base.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .header("X-Api-Key", key.expose())
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            bail!("{service} answered {status} for {path}");
        }
        match response.json::<T>().await {
            Ok(body) => Ok(body),
            Err(_) => {
                bail!("{service} answered {status} for {path} with a body this bot cannot read")
            }
        }
    }
}

#[async_trait]
impl Insight for ArrClient {
    async fn movie(&self, id: i64) -> Result<ArrMovie> {
        let (base, key, service) = self.service(MediaKind::Movie)?;
        let body: serde_json::Value = self
            .get(base, key, service, &format!("/api/v3/movie/{id}"))
            .await?;
        Ok(ArrMovie {
            is_available: body
                .get("isAvailable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            has_file: body
                .get("hasFile")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            digital_release: parse_date(body.get("digitalRelease")),
            physical_release: parse_date(body.get("physicalRelease")),
        })
    }

    async fn queue(&self, kind: MediaKind) -> Result<Vec<QueueItem>> {
        let (base, key, service) = self.service(kind)?;
        let (path, id_field) = match kind {
            MediaKind::Movie => ("/api/v3/queue?pageSize=200&includeMovie=false", "movieId"),
            MediaKind::Tv => ("/api/v3/queue?pageSize=200&includeSeries=false", "seriesId"),
        };
        let body: serde_json::Value = self.get(base, key, service, path).await?;
        let records = body
            .get("records")
            .and_then(|r| r.as_array())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        Ok(records
            .iter()
            .filter_map(|r| {
                let arr_id = r.get(id_field)?.as_i64()?;
                let size = r.get("size").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let sizeleft = r.get("sizeleft").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let percent = if size > 0.0 {
                    ((size - sizeleft) / size * 100.0) as u8
                } else {
                    0
                };
                let tracked = r.get("trackedDownloadState").and_then(|v| v.as_str());
                Some(QueueItem {
                    arr_id,
                    percent,
                    state: queue_state(tracked),
                })
            })
            .collect())
    }

    async fn last_event(&self, kind: MediaKind, id: i64) -> Result<Option<HistoryEvent>> {
        let (base, key, service) = self.service(kind)?;
        let path = match kind {
            MediaKind::Movie => format!("/api/v3/history/movie?movieId={id}"),
            MediaKind::Tv => format!("/api/v3/history/series?seriesId={id}"),
        };
        let body: serde_json::Value = self.get(base, key, service, &path).await?;
        let mut entries: Vec<(time::OffsetDateTime, &str)> = body
            .as_array()
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|e| {
                let date = parse_date(e.get("date"))?;
                let event_type = e.get("eventType")?.as_str()?;
                Some((date, event_type))
            })
            .collect();
        // Stable, descending: when two entries share the same second (an
        // import right after a replaced file's deletion), the truly later
        // one already comes first in the recording's own order, and a
        // stable sort keeps that.
        entries.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
        Ok(entries
            .first()
            .map(|(_, event_type)| map_event_type(event_type)))
    }
}

#[async_trait]
impl ReleaseSearch for ArrClient {
    async fn releases(&self, movie_id: i64) -> Result<Vec<Release>> {
        let (base, key, service) = self.service(MediaKind::Movie)?;
        // `Vec<Release>` straight off the wire: `Release` names three fields,
        // and serde drops the rest as it reads them. No `serde_json::Value`
        // holds the answer in between (see `get`).
        self.get(
            base,
            key,
            service,
            &format!("/api/v3/release?movieId={movie_id}"),
        )
        .await
    }
}
