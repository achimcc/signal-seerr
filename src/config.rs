use crate::secret::Secret;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub signal_socket: PathBuf,
    pub signal_account: String,
    pub authentik_url: String,
    pub authentik_token_file: PathBuf,
    pub seerr_url: String,
    pub seerr_key_file: PathBuf,
    pub webhook_listen: SocketAddr,
    pub webhook_token_file: PathBuf,
    pub state_file: PathBuf,
    pub poll_seconds: u64,
    pub media_group: String,
    pub jellyfin_url: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("cannot parse config {}", path.display()))
    }

    #[cfg(test)]
    pub fn for_test() -> Config {
        Config {
            signal_socket: "/tmp/socket".into(),
            signal_account: "+490000".into(),
            authentik_url: "http://localhost:9000".into(),
            authentik_token_file: "/dev/null".into(),
            seerr_url: "http://localhost:5055".into(),
            seerr_key_file: "/dev/null".into(),
            webhook_listen: "127.0.0.1:0".parse().unwrap(),
            webhook_token_file: "/dev/null".into(),
            state_file: "/tmp/state.json".into(),
            poll_seconds: 30,
            media_group: "Medien".into(),
            jellyfin_url: "https://example.invalid".into(),
        }
    }
}

#[derive(Debug)]
pub struct Secrets {
    pub authentik_token: Secret,
    pub seerr_key: Secret,
    pub webhook_token: Secret,
}

impl Secrets {
    pub fn read(config: &Config) -> Result<Secrets> {
        Ok(Secrets {
            authentik_token: read_one(&config.authentik_token_file)?,
            seerr_key: read_one(&config.seerr_key_file)?,
            webhook_token: read_one(&config.webhook_token_file)?,
        })
    }
}

fn read_one(path: &Path) -> Result<Secret> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read secret {}", path.display()))?;
    Ok(Secret::from(raw.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn the_example_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "c.toml", include_str!("../config.example.toml"));
        let cfg = Config::load(&p).expect("example config must parse");
        assert_eq!(cfg.poll_seconds, 30);
        assert_eq!(cfg.media_group, "Medien");
    }

    #[test]
    fn secrets_are_read_from_files_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        // A credential file written by systemd ends in a newline more often
        // than not; a trailing \n in an API key yields a 401 that looks like a
        // wrong key.
        let tok = write(&dir, "tok", "abc123\n");
        let key = write(&dir, "key", "def456");
        let hook = write(&dir, "hook", "ghi789\n\n");
        let cfg = Config {
            authentik_token_file: tok,
            seerr_key_file: key,
            webhook_token_file: hook,
            ..Config::for_test()
        };
        let s = Secrets::read(&cfg).unwrap();
        assert_eq!(s.authentik_token.expose(), "abc123");
        assert_eq!(s.seerr_key.expose(), "def456");
        assert_eq!(s.webhook_token.expose(), "ghi789");
    }

    #[test]
    fn a_missing_secret_file_names_itself() {
        let cfg = Config {
            authentik_token_file: "/nonexistent/token".into(),
            ..Config::for_test()
        };
        let err = Secrets::read(&cfg).unwrap_err().to_string();
        assert!(err.contains("/nonexistent/token"), "got: {err}");
    }
}
