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
        .request(&hit, Seasons::NotApplicable, SeerrUserId(12), None)
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
        .request(&hit, Seasons::Only(vec![1, 2]), SeerrUserId(12), None)
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

/// Seerr's OpenAPI validator rejects a `query` whose value carries a
/// reserved character, and `+` is one: a form-urlencoded space (what
/// `reqwest`'s `.query()` produces) gets a 400, a percent-encoded one
/// (`%20`) gets a 200. Measured against the running instance on
/// 2026-09-09 -- "Der Vorleser" answered 400, "Der%20Vorleser" 200, which
/// meant every multi-word title failed and every single-word one worked.
///
/// The matcher reads the RAW query string on purpose. `query_param` in the
/// tests above decodes first, so `+` and `%20` look identical to it -- which
/// is exactly why none of them saw this.
#[tokio::test]
async fn a_space_in_the_query_is_percent_encoded_not_a_plus() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(|request: &wiremock::Request| {
            let raw = request.url.query().unwrap_or_default();
            raw.contains("query=blade%20runner") && !raw.contains('+')
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1, "totalPages": 1, "totalResults": 0, "results": []
        })))
        .mount(&server)
        .await;

    client(&server)
        .search("blade runner", None, 1)
        .await
        .expect("a space must travel as %20 -- Seerr answers 400 to a +");
}

/// The list comes from the DEFAULT server, and the default is the one
/// flagged as such -- not the first in the array, and not id 0. Measured on
/// 2026-09-09: `GET /api/v1/service/radarr` answers with `isDefault` per
/// entry and, notably, carries no `apiKey` (the leaking route would be
/// `/settings/radarr`, which needs ADMIN).
#[tokio::test]
async fn quality_profiles_come_from_the_server_flagged_default() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/service/radarr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 3, "name": "Ein anderer", "isDefault": false },
            { "id": 7, "name": "Radarr", "isDefault": true }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/service/radarr/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "server": { "id": 7 },
            "profiles": [
                { "id": 11, "name": "Rarität, Originalsprache (auch SD)" },
                { "id": 7, "name": "Dual Language, sonst Deutsch (1080p)" }
            ],
            "rootFolders": [],
            "tags": []
        })))
        .mount(&server)
        .await;

    let profiles = client(&server)
        .quality_profiles(MediaKind::Movie)
        .await
        .unwrap();

    assert_eq!(profiles.len(), 2);
    assert_eq!(profiles[0].id, 11);
    assert_eq!(profiles[0].name, "Rarität, Originalsprache (auch SD)");
    assert_eq!(profiles[1].id, 7);
}

/// Radarr and Sonarr keep SEPARATE id spaces, and today the numbers happen
/// to coincide (7..11 on both, measured 2026-09-09) -- which is exactly what
/// makes a number-based mapping look correct right up to the day a profile
/// is added or removed on one side. The mapping is by NAME; this test gives
/// the two services deliberately different numbers for the same name, so a
/// client that ever asked the wrong service would say so.
#[tokio::test]
async fn a_series_asks_sonarr_not_radarr_for_its_profile_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/service/sonarr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 0, "name": "Sonarr", "isDefault": true }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/service/sonarr/0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "server": { "id": 0 },
            "profiles": [ { "id": 42, "name": "Dual Language, sonst Deutsch (1080p)" } ],
            "rootFolders": [],
            "tags": []
        })))
        .mount(&server)
        .await;
    // Radarr answers with the SAME name under a different number. If the
    // client asked here for a series, the assertion below would catch it.
    Mock::given(method("GET"))
        .and(path("/api/v1/service/radarr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 0, "name": "Radarr", "isDefault": true }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/service/radarr/0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "server": { "id": 0 },
            "profiles": [ { "id": 7, "name": "Dual Language, sonst Deutsch (1080p)" } ],
            "rootFolders": [],
            "tags": []
        })))
        .mount(&server)
        .await;

    let profiles = client(&server)
        .quality_profiles(MediaKind::Tv)
        .await
        .unwrap();

    assert_eq!(profiles.len(), 1);
    assert_eq!(
        profiles[0].id, 42,
        "a series must carry Sonarr's number for that name, not Radarr's"
    );
}

/// A `profileId` reaches Seerr's request body under exactly that key --
/// `routes/request.js:248` -> `entity/MediaRequest.js:71`. And when nobody
/// chose one, the key must be ABSENT rather than null: Seerr then applies
/// the server's own default, which is what a request placed before v0.2 did.
#[tokio::test]
async fn a_chosen_profile_travels_as_profile_id_and_none_omits_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/request"))
        .and(wiremock::matchers::body_partial_json(
            serde_json::json!({ "profileId": 11 }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 55 })))
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
        .request(&hit, Seasons::NotApplicable, SeerrUserId(3), Some(11))
        .await
        .unwrap();
    assert_eq!(id, 55);

    let without = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/request"))
        .and(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body.get("profileId").is_none()
        })
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 56 })))
        .mount(&without)
        .await;

    let id = client(&without)
        .request(&hit, Seasons::NotApplicable, SeerrUserId(3), None)
        .await
        .expect("no choice means no key at all, not a null");
    assert_eq!(id, 56);
}
