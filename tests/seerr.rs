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

    let hits = client(&server).search("blade runner", None, 1).await.unwrap();

    // The person is dropped: you cannot request a director.
    assert_eq!(hits.len(), 2, "people must be filtered out");
    assert_eq!(hits[0].title, "Blade Runner 2049");
    assert_eq!(hits[0].year, Some(2017));
    assert!(matches!(hits[0].kind, MediaKind::Movie));
    assert!(!hits[0].already);
    assert_eq!(hits[1].title, "Andor");
    assert!(hits[1].already, "status 5 is AVAILABLE, so it is already there");
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
