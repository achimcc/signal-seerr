//! treff's internal door as Audit 3, B145 leaves it: TLS on 127.0.0.1 with a
//! certificate made up on the spot and signed by nobody but itself. wiremock
//! speaks no TLS, hence the few lines of HTTP by hand.
//!
//! Since B158 the same door stands in for Authentik, Seerr and the *arr
//! (`Door::answering`): they get a `ca_file` of their own.
//!
//! No key and no certificate is kept in this repository -- `rcgen` makes
//! both afresh for every door.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;

/// What got through the handshake and arrived as a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrived {
    pub request_line: String,
    pub authorization: Option<String>,
    /// `X-Api-Key`, which is how Seerr, Radarr and Sonarr take a credential.
    pub api_key: Option<String>,
    pub body: String,
}

/// What treff says to an event it took.
const CREATED: &str = "HTTP/1.1 201 Created\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

pub struct Door {
    /// treff's events endpoint behind this door.
    pub url: String,
    /// The door itself, `https://127.0.0.1:<port>` -- for a client that
    /// appends its own path.
    pub base: String,
    /// The door's certificate, as it would sit in `[treff] ca_file`.
    pub cert_pem: String,
    arrived: Arc<Mutex<Vec<Arrived>>>,
}

impl Door {
    /// `name` is what the certificate is made out to -- the door itself
    /// always listens on 127.0.0.1. Self-signed, and not a CA.
    pub async fn made_out_to(name: &str) -> Door {
        Door::open(name, rcgen::IsCa::NoCa, CREATED.to_string()).await
    }

    /// The same door for a client that READS what comes back: every request
    /// is answered `200` with `body` as JSON.
    pub async fn answering(name: &str, body: &str) -> Door {
        Door::open(
            name,
            rcgen::IsCa::NoCa,
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
        .await
    }

    /// The same door, but its certificate says `CA:TRUE` about itself --
    /// what `openssl req -x509` writes unless told otherwise.
    pub async fn made_out_to_as_a_ca(name: &str) -> Door {
        Door::open(
            name,
            rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained),
            CREATED.to_string(),
        )
        .await
    }

    /// `response` is what every request gets back, head and body, as it
    /// goes on the wire.
    async fn open(name: &str, is_ca: rcgen::IsCa, response: String) -> Door {
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).expect("params");
        params.is_ca = is_ca;
        let signing_key = rcgen::KeyPair::generate().expect("key");
        let cert = params.self_signed(&signing_key).expect("certificate");
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key.into())
        .expect("server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let port = listener.local_addr().expect("address").port();
        let arrived = Arc::new(Mutex::new(Vec::new()));
        let response = Arc::new(response);
        tokio::spawn({
            let arrived = arrived.clone();
            async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    let arrived = arrived.clone();
                    let response = response.clone();
                    tokio::spawn(async move {
                        // A client that refuses the certificate ends
                        // here: the handshake fails, nothing arrives.
                        let Ok(mut tls) = acceptor.accept(tcp).await else {
                            return;
                        };
                        let Some(request) = read_request(&mut tls).await else {
                            return;
                        };
                        arrived.lock().unwrap().push(request);
                        let _ = tls.write_all(response.as_bytes()).await;
                        let _ = tls.shutdown().await;
                    });
                }
            }
        });
        Door {
            url: format!("https://127.0.0.1:{port}/internal/events"),
            base: format!("https://127.0.0.1:{port}"),
            cert_pem: cert.pem(),
            arrived,
        }
    }

    /// Every request that got through a handshake so far.
    pub fn arrived(&self) -> Vec<Arrived> {
        self.arrived.lock().unwrap().clone()
    }
}

/// One HTTP/1.1 request with a `content-length` body, or `None` if the
/// connection ended first.
async fn read_request<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Option<Arrived> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => return None,
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    let length: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        match stream.read(&mut chunk).await {
            Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => return None,
        }
    }
    Some(Arrived {
        request_line: head.lines().next().unwrap_or_default().to_string(),
        authorization: header("authorization"),
        api_key: header("x-api-key"),
        body: String::from_utf8_lossy(&buf[head_end..head_end + length]).into_owned(),
    })
}

pub fn file_with(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write");
    path
}

pub fn event() -> signal_seerr::bell::BellEvent {
    signal_seerr::bell::BellEvent {
        handle: "robert".into(),
        kind: "film_available",
        title: "Blade Runner 2049 (2017)".into(),
        link: Some("https://jellyfin.example.org".into()),
        source_key: "seerr:1849".into(),
    }
}

pub fn token() -> signal_seerr::secret::Secret {
    signal_seerr::secret::Secret::from("events-token".to_string())
}

/// The whole chain of an error, causes included -- `to_string()` shows only
/// the outermost line.
pub fn chain(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

/// Refused in the TLS handshake, as opposed to answered with a "no".
pub fn refused_in_the_handshake(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_connect())
}
