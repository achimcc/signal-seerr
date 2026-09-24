use signal_seerr::arr::{ArrClient, HistoryEvent, Insight, QueueState, ReleaseSearch};
use signal_seerr::model::MediaKind;
use signal_seerr::secret::Secret;
use std::time::Duration;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> ArrClient {
    ArrClient::new(&server.uri(), Secret::from("k-e-y".to_string()), None)
}

/// The production deadlines are 20 s and 120 s; a test that waited that long
/// to watch one expire is a test nobody runs. These are the same two
/// deadlines, in milliseconds, so the ORDER between them -- the thing under
/// test -- is the same.
const SHORT: Duration = Duration::from_millis(150);
const LONG: Duration = Duration::from_secs(5);
/// Longer than `SHORT`, short enough that the whole file still runs in under
/// a second.
const SLOWER_THAN_SHORT: Duration = Duration::from_millis(400);

/// Every file this test reads is what a running Radarr instance actually
/// sent on 2026-09-21 (see `tests/fixtures/README.md`), never a body built by
/// hand.
const MOVIE_ANNOUNCED: &str = include_str!("fixtures/radarr-movie-announced.json");
const MOVIE_RELEASED_MISSING: &str = include_str!("fixtures/radarr-movie-released-missing.json");
const HISTORY_DOWNLOAD_FAILED: &str =
    include_str!("fixtures/radarr-history-movie-download-failed.json");
const HISTORY_EMPTY: &str = include_str!("fixtures/radarr-history-movie-empty.json");
const QUEUE_EMPTY: &str = include_str!("fixtures/radarr-queue-empty.json");
/// Recorded on 2026-09-22 while a real download was in progress (see
/// `tests/fixtures/README.md`) -- the first `trackedDownloadState` this
/// project has ever actually seen.
const QUEUE_DOWNLOADING: &str = include_str!("fixtures/radarr-queue-downloading.json");
const RELEASES_ALL_REJECTED: &str =
    include_str!("fixtures/radarr-release-all-rejected-language.json");
/// Recorded on 2026-09-24 from Sonarr 4.0.20.3014 (see the README) -- and the
/// one file among them that is NOT a recording says so in its name.
const SERIES_COMPLETE: &str = include_str!("fixtures/sonarr-series-complete.json");
const EPISODES_COMPLETE: &str = include_str!("fixtures/sonarr-episode-complete.json");
const EPISODES_TWO_MISSING: &str = include_str!("fixtures/sonarr-episode-two-missing.json");
const SEASON_RELEASES: &str = include_str!("fixtures/sonarr-release-season-all-rejected.json");
const QUEUE_IMPORT_BLOCKED: &str =
    include_str!("fixtures/radarr-queue-import-blocked.synthesised.json");

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

/// `percent` is computed here in the test, from the recording's own
/// `size`/`sizeleft` -- not hard-coded, so the production code cannot simply
/// copy a number this test happens to expect.
#[tokio::test]
async fn a_downloading_queue_entry_carries_its_percent_and_state() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/queue"))
        .and(query_param("pageSize", "200"))
        .and(query_param("includeMovie", "false"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(QUEUE_DOWNLOADING, "application/json"),
        )
        .mount(&server)
        .await;

    let queue = client(&server).queue(MediaKind::Movie).await.unwrap();
    assert_eq!(queue.len(), 1);

    let raw: serde_json::Value = serde_json::from_str(QUEUE_DOWNLOADING).unwrap();
    let record = &raw["records"][0];
    let size = record["size"].as_f64().unwrap();
    let sizeleft = record["sizeleft"].as_f64().unwrap();
    let expected_percent = ((size - sizeleft) / size * 100.0) as u8;

    assert_eq!(queue[0].percent, expected_percent);
    assert_eq!(queue[0].state, QueueState::Downloading);
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

/// A release list is the one answer whose UNREAD fields are dangerous:
/// `downloadUrl`, `guid` and `infoUrl` carry the operator's indexer keys and
/// tracker passkeys. So when the answer does not fit `Release`, serde's own
/// message -- which quotes the offending VALUE -- must not travel onwards;
/// `watch.rs` puts exactly this error into a `tracing::warn!`.
///
/// The body is the recording with ONE field flipped (`serde_json`, not a
/// hand-built answer): `rejected` becomes a string carrying a passkey-shaped
/// value, and the error must not repeat it.
#[tokio::test]
async fn a_release_answer_that_does_not_fit_is_an_error_without_the_offending_value() {
    let mut body: serde_json::Value = serde_json::from_str(RELEASES_ALL_REJECTED).unwrap();
    const SENTINEL: &str = "passkey-0123456789abcdef";
    body[0]["rejected"] = serde_json::Value::String(SENTINEL.to_string());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/release"))
        .and(query_param("movieId", "111"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json"))
        .mount(&server)
        .await;

    let err = client(&server).releases(111).await.unwrap_err().to_string();
    assert!(!err.contains(SENTINEL), "got: {err}");
    assert!(err.contains("/api/v3/release"), "got: {err}");
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

/// Measured on the live server on 2026-09-22: the first watch round after the
/// 0.3.0 deploy logged `radarr answered 200 OK for /api/v3/release?movieId=118
/// with a body this bot cannot read` after 23 seconds -- while the answer
/// recorded seconds later had exactly this fixture's shape and deserialises
/// fine. The client's deadline had run out; the message blamed the body.
/// A deadline that runs out must be called a deadline that runs out, or the
/// next person spends the evening looking at serde.
#[tokio::test]
async fn a_call_past_its_deadline_is_a_timeout_and_not_an_unreadable_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie/111"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(MOVIE_ANNOUNCED, "application/json")
                .set_delay(SLOWER_THAN_SHORT),
        )
        .mount(&server)
        .await;

    let err = client(&server)
        .with_deadlines(SHORT, LONG)
        .movie(111)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("did not answer within"), "got: {err}");
    assert!(!err.contains("cannot read"), "got: {err}");
}

/// Answers the headers at once and then stalls halfway through the body, so
/// reqwest's deadline expires while READING -- the measured shape, and the
/// one reqwest reports as `is_decode()` (`Response::do_bytes` wraps every
/// body failure, the deadline included, in `error::decode`). Whoever checks
/// `is_decode()` before `is_timeout()` gets the bug back.
///
/// The bytes are the recording's own; only the delivery is staged, which no
/// mock server offers.
async fn answers_then_stalls(body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 2048];
        let _ = socket.read(&mut request).await;
        // An honest Content-Length, and then half of what it promises.
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket
            .write_all(&body.as_bytes()[..body.len() / 2])
            .await
            .unwrap();
        socket.flush().await.unwrap();
        // The rest never comes, and the connection stays open.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    format!("http://{address}")
}

#[tokio::test]
async fn a_body_that_stops_halfway_is_a_timeout_and_not_an_unreadable_body() {
    let base = answers_then_stalls(RELEASES_ALL_REJECTED).await;
    let err = ArrClient::new(&base, Secret::from("k-e-y".to_string()), None)
        // The release deadline is the short one here: it is the one under test.
        .with_deadlines(LONG, SHORT)
        .releases(111)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("did not answer within"), "got: {err}");
    assert!(!err.contains("cannot read"), "got: {err}");
    assert!(err.contains("/api/v3/release"), "got: {err}");
}

/// An answer that never started is not an answer this bot could not read,
/// and it is not a deadline either. Port 1 on loopback refuses at once, so
/// this measures the wording rather than waiting for anything.
#[tokio::test]
async fn a_host_that_refuses_is_neither_a_timeout_nor_an_unreadable_body() {
    let err = ArrClient::new(
        "http://127.0.0.1:1",
        Secret::from("k-e-y".to_string()),
        None,
    )
    .with_deadlines(SHORT, SHORT)
    .movie(111)
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("could not be reached"), "got: {err}");
    assert!(!err.contains("cannot read"), "got: {err}");
    assert!(!err.contains("did not answer within"), "got: {err}");
}

/// The point of the whole change: an interactive search at every indexer is
/// allowed to take far longer than a lookup of one movie. Same server, same
/// delay, two deadlines -- the movie call gives up, the release search does
/// not.
#[tokio::test]
async fn a_release_search_gets_the_longer_deadline() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie/111"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(MOVIE_ANNOUNCED, "application/json")
                .set_delay(SLOWER_THAN_SHORT),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/release"))
        .and(query_param("movieId", "111"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(RELEASES_ALL_REJECTED, "application/json")
                .set_delay(SLOWER_THAN_SHORT),
        )
        .mount(&server)
        .await;

    let c = client(&server).with_deadlines(SHORT, LONG);
    assert!(c.movie(111).await.is_err(), "the short deadline holds");
    assert_eq!(c.releases(111).await.unwrap().len(), 37);
}

/// Nothing in `main` passes deadlines, so the constants are what a deployed
/// bot runs with -- and the release one is the number the proxy in front has
/// to allow.
#[tokio::test]
async fn the_release_deadline_is_two_minutes_and_the_others_twenty_seconds() {
    assert_eq!(
        signal_seerr::arr::RELEASE_DEADLINE,
        Duration::from_secs(120)
    );
    assert_eq!(
        signal_seerr::arr::STANDARD_DEADLINE,
        Duration::from_secs(20)
    );
}

// -- series evidence and the season search (2026-09-24) ---------------------

fn sonarr_client(server: &MockServer) -> ArrClient {
    ArrClient::new(
        "http://radarr.invalid",
        Secret::from("k-e-y".to_string()),
        Some((&server.uri(), Secret::from("s-o-n-a-r-r".to_string()))),
    )
}

async fn mount_series(server: &MockServer, episodes: &'static str) {
    Mock::given(method("GET"))
        .and(path("/api/v3/series/1"))
        .and(header("X-Api-Key", "s-o-n-a-r-r"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SERIES_COMPLETE, "application/json"))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/episode"))
        .and(query_param("seriesId", "1"))
        .and(header("X-Api-Key", "s-o-n-a-r-r"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(episodes, "application/json"))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_complete_series_has_every_episode_on_file() {
    let server = MockServer::start().await;
    mount_series(&server, EPISODES_COMPLETE).await;

    let series = sonarr_client(&server).series(1).await.unwrap();
    assert!(series.monitored);
    assert_eq!(series.monitored_seasons, vec![1]);
    assert_eq!(series.episodes.len(), 6);
    assert!(series.episodes.iter().all(|e| e.has_file && e.monitored));
    assert!(series.episodes.iter().all(|e| e.air_date.is_some()));
    assert_eq!(series.episodes[0].season, 1);
    assert_eq!(series.episodes[0].number, 1);
}

#[tokio::test]
async fn two_missing_episodes_and_one_unaired_are_read_as_such() {
    let server = MockServer::start().await;
    mount_series(&server, EPISODES_TWO_MISSING).await;

    let series = sonarr_client(&server).series(1).await.unwrap();
    let missing: Vec<u16> = series
        .episodes
        .iter()
        .filter(|e| !e.has_file)
        .map(|e| e.number)
        .collect();
    assert_eq!(missing, vec![4, 5, 6]);
    let unaired = series.episodes.iter().find(|e| e.number == 6).unwrap();
    assert_eq!(unaired.air_date.unwrap().year(), 2030);
}

#[tokio::test]
async fn season_releases_hits_release_with_series_and_season() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/release"))
        .and(query_param("seriesId", "1"))
        .and(query_param("seasonNumber", "1"))
        .and(header("X-Api-Key", "s-o-n-a-r-r"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SEASON_RELEASES, "application/json"))
        .mount(&server)
        .await;

    let releases = sonarr_client(&server).season_releases(1, 1).await.unwrap();
    assert_eq!(releases.len(), 80);
    assert!(releases.iter().all(|r| r.rejected));
    // The recording carries other series' releases; the client keeps them
    // (it is `reason_from` that drops them) -- and it reads the language
    // names off the `{id, name}` objects as it does for Radarr.
    assert!(releases
        .iter()
        .any(|r| r.languages.iter().any(|l| l == "German")));
    assert!(releases
        .iter()
        .any(|r| r.rejections.iter().any(|s| s == "Unknown Series")));
}

#[tokio::test]
async fn a_series_call_without_sonarr_configured_is_an_error() {
    let server = MockServer::start().await;
    let err = client(&server).series(1).await.unwrap_err();
    assert!(err.to_string().contains("sonarr is not configured"));
    let err = client(&server).season_releases(1, 1).await.unwrap_err();
    assert!(err.to_string().contains("sonarr is not configured"));
}

// -- a stuck import, from the source's enum ---------------------------------

#[tokio::test]
async fn a_stuck_import_is_import_stuck() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/queue"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(QUEUE_IMPORT_BLOCKED, "application/json"),
        )
        .mount(&server)
        .await;

    let queue = client(&server).queue(MediaKind::Movie).await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].state, QueueState::ImportStuck);
}

#[tokio::test]
async fn importing_and_an_unknown_value_stay_downloading() {
    // Built from the recorded downloading queue with the one value swapped
    // -- the same way the synthesised fixture was made, for the values the
    // table maps to "still downloading".
    for value in ["importing", "imported", "ignored", "somethingNew"] {
        let body = QUEUE_DOWNLOADING.replace("\"downloading\"", &format!("\"{value}\""));
        assert_ne!(
            body, QUEUE_DOWNLOADING,
            "the swap must have happened for {value}"
        );
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/queue"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .mount(&server)
            .await;
        let queue = client(&server).queue(MediaKind::Movie).await.unwrap();
        assert_eq!(queue[0].state, QueueState::Downloading, "{value}");
    }
}
