//! `[treff] ca_file` on the wire: the bell against a TLS listener whose
//! certificate no public CA has ever seen.

mod support;

use signal_seerr::bell::{Bell, TreffBell};
use support::{chain, event, file_with, refused_in_the_handshake, token, Door};

#[tokio::test]
async fn with_the_ca_file_the_event_reaches_a_self_signed_door_with_the_token() {
    let door = Door::made_out_to("127.0.0.1").await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "treff.pem", &door.cert_pem);
    let bell = TreffBell::new(door.url.clone(), token(), Some(&ca)).expect("client");

    bell.ring(&event()).await.expect("taken");

    let arrived = door.arrived();
    assert_eq!(arrived.len(), 1, "got: {arrived:?}");
    assert_eq!(arrived[0].request_line, "POST /internal/events HTTP/1.1");
    assert_eq!(
        arrived[0].authorization.as_deref(),
        Some("Bearer events-token")
    );
    let body: serde_json::Value = serde_json::from_str(&arrived[0].body).expect("json");
    assert_eq!(body["source_key"], "seerr:1849");
}

/// "One or more certificates": the door's own may be the second one.
#[tokio::test]
async fn the_ca_file_may_hold_more_than_one_certificate() {
    let other = Door::made_out_to("127.0.0.1").await;
    let door = Door::made_out_to("127.0.0.1").await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(
        &dir,
        "bundle.pem",
        &format!("{}{}", other.cert_pem, door.cert_pem),
    );
    let bell = TreffBell::new(door.url.clone(), token(), Some(&ca)).expect("client");
    bell.ring(&event()).await.expect("taken");
    assert_eq!(door.arrived().len(), 1);
}

/// Without `ca_file` nothing vouches for the door, and the call fails in
/// the handshake -- before the token is written anywhere.
#[tokio::test]
async fn without_the_ca_file_a_self_signed_door_is_refused_and_the_token_stays_home() {
    let door = Door::made_out_to("127.0.0.1").await;
    let bell = TreffBell::new(door.url.clone(), token(), None).expect("client");

    let err = bell.ring(&event()).await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

/// `ca_file` replaces the trust store, it does not open it: a door with
/// some OTHER self-signed certificate stays refused.
#[tokio::test]
async fn the_ca_file_vouches_for_its_own_certificates_only() {
    let other = Door::made_out_to("127.0.0.1").await;
    let door = Door::made_out_to("127.0.0.1").await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "other.pem", &other.cert_pem);
    let bell = TreffBell::new(door.url.clone(), token(), Some(&ca)).expect("client");

    let err = bell.ring(&event()).await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

/// Trusting the certificate is not trusting whoever shows it: one made
/// out to another address is refused at 127.0.0.1, `ca_file` or not.
#[tokio::test]
async fn a_trusted_certificate_made_out_to_another_address_is_refused() {
    let door = Door::made_out_to("192.0.2.50").await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "treff.pem", &door.cert_pem);
    let bell = TreffBell::new(door.url.clone(), token(), Some(&ca)).expect("client");

    let err = bell.ring(&event()).await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}

/// The trap in making the certificate: `openssl req -x509` marks what it
/// writes as a CA unless told otherwise, and a CA certificate shown as the
/// door's own is refused -- named in `ca_file` or not. The README says how
/// to make one that works; this is what it rests on.
#[tokio::test]
async fn a_self_signed_certificate_that_calls_itself_a_ca_is_refused_as_the_doors_own() {
    let door = Door::made_out_to_as_a_ca("127.0.0.1").await;
    let dir = tempfile::tempdir().unwrap();
    let ca = file_with(&dir, "treff.pem", &door.cert_pem);
    let bell = TreffBell::new(door.url.clone(), token(), Some(&ca)).expect("client");

    let err = bell.ring(&event()).await.expect_err("must be refused");

    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert!(
        chain(&err).contains("CaUsedAsEndEntity"),
        "got: {}",
        chain(&err)
    );
    assert!(door.arrived().is_empty(), "got: {:?}", door.arrived());
}
