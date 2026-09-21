use signal_seerr::arr::{ArrClient, HistoryEvent, Insight, ReleaseSearch};
use signal_seerr::model::MediaKind;
use signal_seerr::secret::Secret;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> ArrClient {
    ArrClient::new(&server.uri(), Secret::from("k-e-y".to_string()), None)
}

/// Every file this test reads is what a running Radarr instance actually
/// sent on 2026-09-21 (see `tests/fixtures/README.md`), never a body built by
/// hand.
const MOVIE_ANNOUNCED: &str = include_str!("fixtures/radarr-movie-announced.json");
const MOVIE_RELEASED_MISSING: &str = include_str!("fixtures/radarr-movie-released-missing.json");
const HISTORY_DOWNLOAD_FAILED: &str =
    include_str!("fixtures/radarr-history-movie-download-failed.json");
const HISTORY_EMPTY: &str = include_str!("fixtures/radarr-history-movie-empty.json");
const QUEUE_EMPTY: &str = include_str!("fixtures/radarr-queue-empty.json");
const RELEASES_ALL_REJECTED: &str =
    include_str!("fixtures/radarr-release-all-rejected-language.json");

#[tokio::test]
async fn an_announced_movie_is_not_available() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie/111"))
        .and(header("X-Api-Key", "k-e-y"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(MOVIE_ANNOUNCED, "application/json"))
        .mount(&server)
        .await;

    let movie = client(&server).movie(111).await.unwrap();
    assert!(!movie.is_available);
}

#[tokio::test]
async fn a_released_movie_without_a_file_is_available_but_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie/111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(MOVIE_RELEASED_MISSING, "application/json"),
        )
        .mount(&server)
        .await;

    let movie = client(&server).movie(111).await.unwrap();
    assert!(movie.is_available);
    assert!(!movie.has_file);
}

/// The mapping table from the brief, applied to whatever eventType the
/// recording's own newest entry (by `date`) actually carries -- the test
/// does not assume it already knows the answer from the file's name.
fn expected_event(raw: &str) -> HistoryEvent {
    let value: serde_json::Value = serde_json::from_str(raw).unwrap();
    let mut entries: Vec<(time::OffsetDateTime, String)> = value
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let date = time::OffsetDateTime::parse(
                e["date"].as_str().unwrap(),
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap();
            (date, e["eventType"].as_str().unwrap().to_string())
        })
        .collect();
    // Stable, descending: two entries share the same second in this
    // recording (an import right after a replaced file's deletion), and the
    // higher `id` -- the true tiebreak -- already sorts first in the
    // recording's own order, which a stable sort preserves.
    entries.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
    match entries.first().unwrap().1.as_str() {
        "grabbed" => HistoryEvent::Grabbed,
        "downloadFailed" => HistoryEvent::DownloadFailed,
        "downloadFolderImported" => HistoryEvent::Imported,
        _ => HistoryEvent::Other,
    }
}

#[tokio::test]
async fn last_event_is_the_newest_entry_by_date_mapped_through_the_table() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/history/movie"))
        .and(query_param("movieId", "111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(HISTORY_DOWNLOAD_FAILED, "application/json"),
        )
        .mount(&server)
        .await;

    let event = client(&server)
        .last_event(MediaKind::Movie, 111)
        .await
        .unwrap();
    assert_eq!(event, Some(expected_event(HISTORY_DOWNLOAD_FAILED)));
}

#[tokio::test]
async fn last_event_is_none_when_nothing_was_ever_grabbed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/history/movie"))
        .and(query_param("movieId", "222"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HISTORY_EMPTY, "application/json"))
        .mount(&server)
        .await;

    let event = client(&server)
        .last_event(MediaKind::Movie, 222)
        .await
        .unwrap();
    assert_eq!(event, None);
}

#[tokio::test]
async fn an_empty_queue_is_an_empty_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/queue"))
        .and(query_param("pageSize", "200"))
        .and(query_param("includeMovie", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(QUEUE_EMPTY, "application/json"))
        .mount(&server)
        .await;

    let queue = client(&server).queue(MediaKind::Movie).await.unwrap();
    assert!(queue.is_empty());
}

#[tokio::test]
async fn releases_deserialises_only_the_three_safe_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/release"))
        .and(query_param("movieId", "111"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(RELEASES_ALL_REJECTED, "application/json"),
        )
        .mount(&server)
        .await;

    let releases = client(&server).releases(111).await.unwrap();
    assert_eq!(releases.len(), 37);
    assert!(releases.iter().all(|r| r.rejected), "all are rejected");
    assert!(
        releases.iter().all(|r| !r.languages.is_empty()),
        "every entry carries a language name"
    );
}

/// `base` may carry a path part -- `http://host:7870/radarr` -- and it must
/// survive: `Url::join` would drop it.
#[tokio::test]
async fn a_path_part_in_the_base_url_is_kept() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/radarr/api/v3/movie/111"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(MOVIE_ANNOUNCED, "application/json"))
        .mount(&server)
        .await;

    let c = ArrClient::new(
        &format!("{}/radarr", server.uri()),
        Secret::from("k-e-y".to_string()),
        None,
    );
    let movie = c.movie(111).await.unwrap();
    assert!(!movie.is_available);
}

/// Seerr's settings endpoints leak API keys of Radarr and Sonarr in the
/// clear when their error bodies are echoed (see `src/seerr/mod.rs`); the
/// same applies here, so an error body must never appear in the message.
#[tokio::test]
async fn a_non_200_answer_is_an_error_without_the_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie/111"))
        .respond_with(ResponseTemplate::new(500).set_body_string("radarr-api-key-in-the-body"))
        .mount(&server)
        .await;

    let err = client(&server).movie(111).await.unwrap_err().to_string();
    assert!(!err.contains("radarr-api-key-in-the-body"), "got: {err}");
}

/// There is no Sonarr recording (`tests/fixtures/README.md`), so Sonarr's
/// paths are checked for the URL and header they hit, with a Radarr-shaped
/// recording as the body where one is needed.
#[tokio::test]
async fn sonarr_queue_asks_for_series_not_movies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/queue"))
        .and(query_param("pageSize", "200"))
        .and(query_param("includeSeries", "false"))
        .and(header("X-Api-Key", "s-o-n-a-r-r"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(QUEUE_EMPTY, "application/json"))
        .mount(&server)
        .await;

    let c = ArrClient::new(
        "http://radarr.invalid",
        Secret::from("k-e-y".to_string()),
        Some((&server.uri(), Secret::from("s-o-n-a-r-r".to_string()))),
    );
    let queue = c.queue(MediaKind::Tv).await.unwrap();
    assert!(queue.is_empty());
}

#[tokio::test]
async fn sonarr_history_is_keyed_by_series_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/history/series"))
        .and(query_param("seriesId", "222"))
        .and(header("X-Api-Key", "s-o-n-a-r-r"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HISTORY_EMPTY, "application/json"))
        .mount(&server)
        .await;

    let c = ArrClient::new(
        "http://radarr.invalid",
        Secret::from("k-e-y".to_string()),
        Some((&server.uri(), Secret::from("s-o-n-a-r-r".to_string()))),
    );
    let event = c.last_event(MediaKind::Tv, 222).await.unwrap();
    assert_eq!(event, None);
}

/// Asking `queue`/`last_event` for a series when no Sonarr was configured is
/// a configuration error, not a silent empty answer -- an empty list here
/// would look like "nothing downloading" when really nobody can tell.
#[tokio::test]
async fn a_series_query_without_sonarr_configured_is_an_error() {
    let server = MockServer::start().await;
    let c = client(&server); // sonarr: None
    assert!(c.queue(MediaKind::Tv).await.is_err());
    assert!(c.last_event(MediaKind::Tv, 1).await.is_err());
}
