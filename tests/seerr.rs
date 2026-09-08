use signal_seerr::model::{MediaKind, SeerrUserId};
use signal_seerr::secret::Secret;
use signal_seerr::seerr::{Requests, SeerrClient};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> SeerrClient {
    SeerrClient::new(&server.uri(), Secret::from("k-e-y".to_string()))
}

#[tokio::test]
async fn search_maps_movies_and_series() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("query", "blade runner"))
        .and(header("X-Api-Key", "k-e-y"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1, "totalPages": 2, "totalResults": 3,
            "results": [
                { "id": 335984, "mediaType": "movie", "title": "Blade Runner 2049",
                  "releaseDate": "2017-10-04", "voteAverage": 8.0 },
                { "id": 4321, "mediaType": "tv", "name": "Andor",
                  "firstAirDate": "2022-09-21", "voteAverage": 8.4,
                  "mediaInfo": { "status": 5 } },
                { "id": 999, "mediaType": "person", "name": "Denis Villeneuve" }
            ]
        })))
        .mount(&server)
        .await;

    let hits = client(&server)
        .search("blade runner", None, 1)
        .await
        .unwrap();

    // The person is dropped: you cannot request a director.
    assert_eq!(hits.len(), 2, "people must be filtered out");
    assert_eq!(hits[0].title, "Blade Runner 2049");
    assert_eq!(hits[0].year, Some(2017));
    assert!(matches!(hits[0].kind, MediaKind::Movie));
    assert!(!hits[0].already);
    assert_eq!(hits[1].title, "Andor");
    assert!(
        hits[1].already,
        "status 5 is AVAILABLE, so it is already there"
    );
}

#[tokio::test]
async fn search_can_be_narrowed_to_one_kind() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1, "totalPages": 1, "totalResults": 2,
            "results": [
                { "id": 1, "mediaType": "movie", "title": "A", "releaseDate": "2001-01-01" },
                { "id": 2, "mediaType": "tv", "name": "B", "firstAirDate": "2002-01-01" }
            ]
        })))
        .mount(&server)
        .await;

    let hits = client(&server)
        .search("x", Some(MediaKind::Movie), 1)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "A");
}

#[tokio::test]
async fn a_missing_release_date_is_not_an_error() {
    // An announced-but-undated film has no releaseDate. Treating that as a
    // parse failure would make the whole search fail over one entry.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1, "totalPages": 1, "totalResults": 1,
            "results": [ { "id": 7, "mediaType": "movie", "title": "Untitled" } ]
        })))
        .mount(&server)
        .await;

    let hits = client(&server).search("x", None, 1).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].year, None);
}

#[tokio::test]
async fn a_server_error_is_an_error_not_an_empty_list() {
    // Exit code is not the result: an empty list would read as "nothing found"
    // and send somebody looking for a film that is right there.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    assert!(client(&server).search("x", None, 1).await.is_err());
}

use signal_seerr::model::{Hit, Seasons};

#[tokio::test]
async fn a_request_is_placed_in_the_name_of_the_asker() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/request"))
        .and(header("X-Api-Key", "k-e-y"))
        // This header is the whole point of the mapping chain: without it the
        // request lands on the API key's own account and Seerr shows every
        // wish as the operator's.
        .and(header("X-API-User", "12"))
        .and(wiremock::matchers::body_partial_json(serde_json::json!({
            "mediaType": "movie", "mediaId": 335984
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 1849, "status": 1
        })))
        .mount(&server)
        .await;

    let hit = Hit {
        tmdb_id: 335984,
        kind: MediaKind::Movie,
        title: "Blade Runner 2049".into(),
        year: Some(2017),
        rating: Some(8.0),
        seasons: 0,
        already: false,
    };
    let id = client(&server)
        .request(&hit, Seasons::NotApplicable, SeerrUserId(12))
        .await
        .unwrap();
    assert_eq!(id, 1849);
}

#[tokio::test]
async fn a_series_request_carries_its_seasons() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/request"))
        .and(wiremock::matchers::body_partial_json(serde_json::json!({
            "mediaType": "tv", "seasons": [1, 2]
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 7 })))
        .mount(&server)
        .await;

    let hit = Hit {
        tmdb_id: 4321,
        kind: MediaKind::Tv,
        title: "Andor".into(),
        year: Some(2022),
        rating: None,
        seasons: 2,
        already: false,
    };
    let id = client(&server)
        .request(&hit, Seasons::Only(vec![1, 2]), SeerrUserId(12))
        .await
        .unwrap();
    assert_eq!(id, 7);
}

#[tokio::test]
async fn seerr_user_is_found_by_its_jellyfin_username() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pageInfo": { "pages": 1, "results": 2 },
            "results": [
                { "id": 1, "jellyfinUsername": "alice", "displayName": "Alice" },
                { "id": 12, "jellyfinUsername": "bob", "displayName": "Bob" }
            ]
        })))
        .mount(&server)
        .await;

    let c = client(&server);
    assert_eq!(c.user_id("bob").await.unwrap(), Some(SeerrUserId(12)));
    assert_eq!(c.user_id("nobody").await.unwrap(), None);
}

#[tokio::test]
async fn withdrawing_somebody_elses_request_is_refused_before_it_is_sent() {
    // /weg takes a number the person read off a chat message. Nothing stops
    // them typing a neighbour's number, and Seerr would honour it: the API key
    // is an administrator. The check belongs here, not in Seerr.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/request/1849"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1849, "requestedBy": { "id": 99 }
        })))
        .mount(&server)
        .await;

    let err = client(&server)
        .withdraw(1849, SeerrUserId(12))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not yours"), "got: {err}");
}

use signal_seerr::model::{Pending, PendingState};

#[tokio::test]
async fn pending_maps_status_and_falls_back_to_the_series_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/12/requests"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pageInfo": { "pages": 1, "results": 4 },
            "results": [
                { "id": 1, "media": { "title": "Blade Runner 2049", "status": 5 } },
                { "id": 2, "media": { "name": "Andor", "status": 3 } },
                { "id": 3, "media": { "name": "The Bear", "status": 4 } },
                { "id": 4, "media": { "title": "Untitled Film", "status": 1 } }
            ]
        })))
        .mount(&server)
        .await;

    let items = client(&server).pending(SeerrUserId(12)).await.unwrap();

    assert_eq!(
        items,
        vec![
            Pending {
                id: 1,
                title: "Blade Runner 2049".into(),
                state: PendingState::Available
            },
            Pending {
                id: 2,
                title: "Andor".into(),
                state: PendingState::Fetching
            },
            Pending {
                id: 3,
                title: "The Bear".into(),
                state: PendingState::Fetching
            },
            Pending {
                id: 4,
                title: "Untitled Film".into(),
                state: PendingState::Waiting
            },
        ]
    );
}

#[tokio::test]
async fn requester_of_reads_the_jellyfin_username() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/request/1849"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1849,
            "requestedBy": { "jellyfinUsername": "alice", "displayName": "Alice Example" }
        })))
        .mount(&server)
        .await;

    let who = client(&server).requester_of(1849).await.unwrap();
    assert_eq!(who, Some("alice".to_string()));
}

#[tokio::test]
async fn requester_of_ignores_the_editable_display_name() {
    // {{requestedBy_username}} in Seerr's webhook payload maps to
    // displayName, which the person can change in their own profile --
    // identity has to come from jellyfinUsername instead, even when Seerr
    // sends an empty one alongside a populated displayName. If this ever
    // drifted back to reading displayName, this is the test that would say
    // so.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/request/1850"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1850,
            "requestedBy": { "jellyfinUsername": "", "displayName": "Somebody Else" }
        })))
        .mount(&server)
        .await;

    let who = client(&server).requester_of(1850).await.unwrap();
    assert_eq!(who, None, "must not fall back to the editable display name");
}
