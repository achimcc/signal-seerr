//! `[treff] ca_file` REPLACES the system's trust store for the treff client,
//! it does not add to it.
//!
//! The one test here lives in a file of its own on purpose: it points
//! `SSL_CERT_FILE` at a certificate of its choosing, which is process-wide.
//! Every file under `tests/` is its own process, so nothing else builds a
//! client while the variable is being changed.

mod support;

use signal_seerr::bell::{Bell, TreffBell};
use support::{chain, event, file_with, refused_in_the_handshake, token, Door};

#[tokio::test]
async fn a_certificate_the_system_trusts_is_still_refused_once_ca_file_is_set() {
    let door = Door::made_out_to("127.0.0.1").await;
    let pinned = Door::made_out_to("127.0.0.1").await;
    let dir = tempfile::tempdir().unwrap();

    // The door's certificate becomes the whole "system" trust store.
    let system = file_with(&dir, "system.pem", &door.cert_pem);
    std::env::set_var("SSL_CERT_FILE", &system);
    std::env::remove_var("SSL_CERT_DIR");

    // The control, without which the refusal below would prove nothing: a
    // client WITHOUT ca_file reads that store and reaches the door.
    let plain = TreffBell::new(door.url.clone(), token(), None).expect("client");
    plain
        .ring(&event())
        .await
        .expect("the system trust store must vouch for the door");
    assert_eq!(door.arrived().len(), 1);

    // With ca_file naming some OTHER certificate, the same door is refused:
    // what the system trusts no longer counts for this client.
    let ca = file_with(&dir, "pinned.pem", &pinned.cert_pem);
    let bell = TreffBell::new(door.url.clone(), token(), Some(&ca)).expect("client");
    let err = bell.ring(&event()).await.expect_err("must be refused");
    assert!(refused_in_the_handshake(&err), "got: {}", chain(&err));
    assert_eq!(door.arrived().len(), 1, "got: {:?}", door.arrived());
}
