//! One way to build an HTTP client that trusts a door with a certificate of
//! its own.
//!
//! Every endpoint this bot sends a credential to may name a `ca_file`
//! (`authentik_ca_file`, `seerr_ca_file`, `insight.ca_file`,
//! `treff.ca_file`): a PEM file whose certificates are then the ONLY ones
//! that one client trusts. An internal door has no business being reachable
//! through any public CA, so naming the file REPLACES the system's trust
//! store for the client rather than adding to it. Without it, a client is
//! what it always was.
//!
//! Either way the name or address in the URL is checked against the
//! certificate: nothing here switches a verification off.

use anyhow::Context;
use std::path::Path;

/// The certificates in a `ca_file`. Missing, unreadable, not PEM, or PEM
/// without a single certificate in it are all errors: the operator asked for
/// a pinned door, and a client that quietly fell back to the system's trust
/// store -- or to none -- would be a different client from the one that was
/// configured.
///
/// `field` is the configuration field the path came from, as the operator
/// wrote it (`treff.ca_file`), so the message says which of the four to
/// look at.
fn read_ca_file(path: &Path, field: &str) -> anyhow::Result<Vec<reqwest::Certificate>> {
    let pem =
        std::fs::read(path).with_context(|| format!("{field}: cannot read {}", path.display()))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem)
        .with_context(|| format!("{field}: {} is not valid PEM", path.display()))?;
    if certs.is_empty() {
        anyhow::bail!(
            "{field}: {} holds no certificate (expected one or more \
             `-----BEGIN CERTIFICATE-----` blocks)",
            path.display()
        );
    }
    Ok(certs)
}

/// Builds `builder` into a client -- with `ca_file`, if there is one, as its
/// whole trust store. Everything else about the client (timeout, redirect
/// policy) is the caller's and is left as it came.
///
/// Every failure names `field` and the path. There is no fallback: a file
/// that cannot be used stops whoever asked, which in `main` is the start.
pub fn client(
    mut builder: reqwest::ClientBuilder,
    ca_file: Option<&Path>,
    field: &str,
) -> anyhow::Result<reqwest::Client> {
    if let Some(path) = ca_file {
        builder = builder.tls_certs_only(read_ca_file(path, field)?);
    }
    match (builder.build(), ca_file) {
        (Ok(client), _) => Ok(client),
        // The PEM frame held, what is inside it did not: reqwest only looks
        // into a certificate when it builds the trust store.
        (Err(e), Some(path)) => Err(anyhow::Error::new(e).context(format!(
            "{field}: {} holds a certificate that cannot be used as a trust anchor",
            path.display()
        ))),
        (Err(e), None) => Err(anyhow::Error::new(e).context(
            "could not build the HTTP client -- rustls-platform-verifier reads the system \
             certificate store as soon as a Client exists, so this fails wherever that \
             store is missing; point SSL_CERT_FILE at a CA bundle",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // What stops the start, for ANY field. `bell.rs` has the same four for
    // `treff.ca_file` through `TreffBell::new`; what a client then does on
    // the wire is in `tests/treff_tls.rs` and `tests/ca_file.rs`.

    const FIELD: &str = "some.ca_file";

    fn file_with(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("write");
        path
    }

    /// The whole chain of the refusal, causes included.
    fn refusal(ca: &Path) -> String {
        let err = client(reqwest::Client::builder(), Some(ca), FIELD)
            .expect_err("the client must not be built");
        format!("{err:#}")
    }

    #[test]
    fn without_a_ca_file_the_builder_is_built_as_it_came() {
        client(reqwest::Client::builder(), None, FIELD).expect("client");
    }

    #[test]
    fn a_missing_ca_file_names_the_field_and_the_path() {
        let got = refusal(Path::new("/nonexistent/door.pem"));
        assert!(got.starts_with("some.ca_file: "), "got: {got}");
        assert!(got.contains("/nonexistent/door.pem"), "got: {got}");
    }

    #[test]
    fn a_broken_pem_names_the_field_and_the_path() {
        let dir = tempfile::tempdir().unwrap();
        // A frame that opens and never closes.
        let ca = file_with(
            &dir,
            "broken.pem",
            "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIU\n",
        );
        let got = refusal(&ca);
        assert!(got.starts_with("some.ca_file: "), "got: {got}");
        assert!(got.contains("broken.pem"), "got: {got}");
    }

    /// Nothing that looks like PEM at all must not become "no extra trust,
    /// carry on".
    #[test]
    fn a_ca_file_without_a_certificate_names_the_field_and_the_path() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [("empty.pem", ""), ("words.pem", "not a certificate\n")] {
            let got = refusal(&file_with(&dir, name, body));
            assert!(got.starts_with("some.ca_file: "), "{name}: got: {got}");
            assert!(got.contains(name), "{name}: got: {got}");
        }
    }

    /// A well-formed frame around something that is no certificate.
    #[test]
    fn a_ca_file_whose_certificate_is_not_one_names_the_field_and_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let ca = file_with(
            &dir,
            "hollow.pem",
            "-----BEGIN CERTIFICATE-----\naGVsbG8gd29ybGQ=\n-----END CERTIFICATE-----\n",
        );
        let got = refusal(&ca);
        assert!(got.starts_with("some.ca_file: "), "got: {got}");
        assert!(got.contains("hollow.pem"), "got: {got}");
    }
}
