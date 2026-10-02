//! `authentik_ca_file`, `seerr_ca_file` and `[insight] ca_file` on the wire
//! (Audit 3, B158): each of the three clients against a TLS listener whose
//! certificate no public CA has ever seen. What `[treff] ca_file` got in
//! `tests/treff_tls.rs`, for the other three doors a credential goes to.
//!
//! Every refusal below is also a statement about the credential: the
//! handshake fails BEFORE a request is written, so nothing arrives at the
//! door -- no token, no key.

mod support;

use signal_seerr::arr::{ArrClient, Insight};
use signal_seerr::directory::AuthentikClient;
use signal_seerr::model::MediaKind;
use signal_seerr::secret::Secret;
use signal_seerr::seerr::{Requests, SeerrClient};
use support::{chain, file_with, refused_in_the_handshake, Door};

/// One page, one user: the least Authentik can say that `users` accepts.
/// The shape is the one `src/directory/mod.rs` tests against; nothing here
/// is about the wire format.
const ONE_USER: &str =
    r#"{"pagination":{"next":0},"results":[{"username":"robert","groups_obj":[]}]}"#;

/// What a running Radarr, Sonarr and Seerr actually sent (see
/// `tests/fixtures/README.md`).
const MOVIE_ANNOUNCED: &str = include_str!("fixtures/radarr-movie-announced.json");
const SONARR_QUEUE_EMPTY: &str = include_str!("fixtures/sonarr-queue-empty.json");
const REQUEST_ALL: &str = include_str!("fixtures/seerr-request-all.json");

fn secret(value: &str) -> Secret {
    Secret::from(value.to_string())
}

// -- Authentik ---------------------------------------------------------------

#[tokio::test]
async fn with_its_ca_file_authentik_is_read_through_a_self_signed_door() {
    let door = Door::answering("127.0.0.1", ONE_USER).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "authentik.pem", &door.cert_pem);
    let client =
        AuthentikClient::with_ca_file(&door.base, secret("a-token"), Some(&ca)).expect("client");

    let users = client.users("de").await.expect("read");

    assert_eq!(users.len(), 1);
    assert_eq!(users[0].username, "robert");
    let arrived = door.arrived();
    assert_eq!(arrived.len(), 1, "got: {arrived:?}");
    assert_eq!(arrived[0].authorization.as_deref(), Some("Bearer a-token"));
}

/// `authentik_ca_file` replaces the trust store, it does not open it: a door
/// with some OTHER self-signed certificate stays refused.
#[tokio::test]
async fn authentik_behind_another_certificate_than_the_ca_file_names_is_refused() {
    let other = Door::answering("127.0.0.1", ONE_USER).await;
    let door = Door::answering("127.0.0.1", ONE_USER).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "other.pem", &other.cert_pem);
    let client =
        AuthentikClient::with_ca_file(&door.base, secret("a-token"), Some(&ca)).expect("client");

    let err = client.users("de").await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

/// Without `authentik_ca_file` nothing vouches for the door, and the token
/// stays home.
#[tokio::test]
async fn without_a_ca_file_a_self_signed_authentik_is_refused() {
    let door = Door::answering("127.0.0.1", ONE_USER).await;
    let client =
        AuthentikClient::with_ca_file(&door.base, secret("a-token"), None).expect("client");

    let err = client.users("de").await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

// -- Radarr and Sonarr -------------------------------------------------------

#[tokio::test]
async fn with_its_ca_file_radarr_is_read_through_a_self_signed_door() {
    let door = Door::answering("127.0.0.1", MOVIE_ANNOUNCED).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "arr.pem", &door.cert_pem);
    let client = ArrClient::with_ca_file(&door.base, secret("r-a-d-a-r-r"), None, Some(&ca))
        .expect("client");

    let movie = client.movie(111).await.expect("read");

    assert!(!movie.is_available);
    let arrived = door.arrived();
    assert_eq!(arrived.len(), 1, "got: {arrived:?}");
    assert_eq!(arrived[0].request_line, "GET /api/v3/movie/111 HTTP/1.1");
    assert_eq!(arrived[0].api_key.as_deref(), Some("r-a-d-a-r-r"));
}

/// Radarr and Sonarr share one client, so `[insight] ca_file` is one file
/// for both doors -- with both certificates in it when each has its own.
#[tokio::test]
async fn one_ca_file_vouches_for_radarr_and_sonarr_alike() {
    let radarr = Door::answering("127.0.0.1", MOVIE_ANNOUNCED).await;
    let sonarr = Door::answering("127.0.0.1", SONARR_QUEUE_EMPTY).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(
        &dir,
        "arr.pem",
        &format!("{}{}", radarr.cert_pem, sonarr.cert_pem),
    );
    let client = ArrClient::with_ca_file(
        &radarr.base,
        secret("r-a-d-a-r-r"),
        Some((&sonarr.base, secret("s-o-n-a-r-r"))),
        Some(&ca),
    )
    .expect("client");

    client.movie(111).await.expect("radarr");
    let queue = client.queue(MediaKind::Tv).await.expect("sonarr");

    assert!(queue.is_empty());
    assert_eq!(radarr.arrived().len(), 1);
    let at_sonarr = sonarr.arrived();
    assert_eq!(at_sonarr.len(), 1, "got: {at_sonarr:?}");
    assert_eq!(at_sonarr[0].api_key.as_deref(), Some("s-o-n-a-r-r"));
}

/// A `ca_file` that names Radarr's certificate only does not let Sonarr's
/// through: the file is the whole trust store for both.
#[tokio::test]
async fn a_sonarr_the_ca_file_does_not_name_is_refused() {
    let radarr = Door::answering("127.0.0.1", MOVIE_ANNOUNCED).await;
    let sonarr = Door::answering("127.0.0.1", SONARR_QUEUE_EMPTY).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "radarr-only.pem", &radarr.cert_pem);
    let client = ArrClient::with_ca_file(
        &radarr.base,
        secret("r-a-d-a-r-r"),
        Some((&sonarr.base, secret("s-o-n-a-r-r"))),
        Some(&ca),
    )
    .expect("client");

    client.movie(111).await.expect("radarr");
    let err = client
        .queue(MediaKind::Tv)
        .await
        .expect_err("must be refused");

    assert!(
        err.to_string().contains("could not be reached"),
        "got: {}",
        chain(&err)
    );
    assert!(sonarr.arrived().is_empty(), "got: {:?}", sonarr.arrived());
}

#[tokio::test]
async fn radarr_behind_another_certificate_than_the_ca_file_names_is_refused() {
    let other = Door::answering("127.0.0.1", MOVIE_ANNOUNCED).await;
    let door = Door::answering("127.0.0.1", MOVIE_ANNOUNCED).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "other.pem", &other.cert_pem);
    let client = ArrClient::with_ca_file(&door.base, secret("r-a-d-a-r-r"), None, Some(&ca))
        .expect("client");

    let err = client.movie(111).await.expect_err("must be refused");

    // The arr client words its own errors (`send_failure`), so the reqwest
    // error is not there to downcast: the wording says it never got as far
    // as an answer, and the door says nothing arrived.
    assert!(
        err.to_string().contains("could not be reached"),
        "got: {}",
        chain(&err)
    );
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

#[tokio::test]
async fn without_a_ca_file_a_self_signed_radarr_is_refused() {
    let door = Door::answering("127.0.0.1", MOVIE_ANNOUNCED).await;
    let client =
        ArrClient::with_ca_file(&door.base, secret("r-a-d-a-r-r"), None, None).expect("client");

    let err = client.movie(111).await.expect_err("must be refused");

    assert!(
        err.to_string().contains("could not be reached"),
        "got: {}",
        chain(&err)
    );
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

// -- Seerr -------------------------------------------------------------------

#[tokio::test]
async fn with_its_ca_file_seerr_is_read_through_a_self_signed_door() {
    let door = Door::answering("127.0.0.1", REQUEST_ALL).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "seerr.pem", &door.cert_pem);
    let client =
        SeerrClient::with_ca_file(&door.base, secret("s-e-e-r-r"), Some(&ca)).expect("client");

    let wishes = client.open_wishes().await.expect("read");

    assert!(!wishes.is_empty(), "the recording has open wishes");
    let arrived = door.arrived();
    assert_eq!(arrived.len(), 1, "got: {arrived:?}");
    assert_eq!(arrived[0].api_key.as_deref(), Some("s-e-e-r-r"));
}

#[tokio::test]
async fn seerr_behind_another_certificate_than_the_ca_file_names_is_refused() {
    let other = Door::answering("127.0.0.1", REQUEST_ALL).await;
    let door = Door::answering("127.0.0.1", REQUEST_ALL).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "other.pem", &other.cert_pem);
    let client =
        SeerrClient::with_ca_file(&door.base, secret("s-e-e-r-r"), Some(&ca)).expect("client");

    let err = client.open_wishes().await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

#[tokio::test]
async fn without_a_ca_file_a_self_signed_seerr_is_refused() {
    let door = Door::answering("127.0.0.1", REQUEST_ALL).await;
    let client = SeerrClient::with_ca_file(&door.base, secret("s-e-e-r-r"), None).expect("client");

    let err = client.open_wishes().await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

// -- What stops the start ----------------------------------------------------

/// Each constructor names ITS OWN config field and the path, so the line in
/// the journal says which of the four `ca_file`s to look at.
#[test]
fn a_missing_ca_file_stops_each_client_and_names_its_own_field() {
    let missing = std::path::Path::new("/nonexistent/door.pem");
    let refusals = [
        (
            "authentik_ca_file",
            AuthentikClient::with_ca_file("https://192.0.2.10:9443", secret("t"), Some(missing))
                .err(),
        ),
        (
            "seerr_ca_file",
            SeerrClient::with_ca_file("https://192.0.2.20:5055", secret("k"), Some(missing)).err(),
        ),
        (
            "insight.ca_file",
            ArrClient::with_ca_file("https://192.0.2.30:7878", secret("k"), None, Some(missing))
                .err(),
        ),
    ];
    for (field, refusal) in refusals {
        let got =
            chain(&refusal.unwrap_or_else(|| panic!("{field}: the client must not be built")));
        assert!(got.starts_with(&format!("{field}: ")), "got: {got}");
        assert!(got.contains("/nonexistent/door.pem"), "got: {field}: {got}");
    }
}
