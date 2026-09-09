use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub signal_socket: PathBuf,
    pub signal_account_file: PathBuf,
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
    /// Where a person goes to enter their Signal name -- substituted into
    /// error.unknown_sender. Required, no default: an operator who never
    /// thought about it would otherwise ship somebody else's URL.
    pub settings_url: String,
    /// Who to ask when a group is missing -- substituted into
    /// error.not_allowed. Same reasoning as settings_url.
    pub operator_name: String,
    /// The quality profiles to offer, by NAME, in the order they are
    /// offered in.
    ///
    /// By name and never by id: Radarr and Sonarr keep separate id spaces,
    /// and the same number means a different profile on each side. By
    /// configuration and not in Seerr's own order: otherwise "3" means
    /// something else the week somebody adds a profile, and whoever learned
    /// to type it gets a different film.
    ///
    /// Empty (the default) means "offer whatever Seerr lists" -- and when
    /// Seerr lists nothing, the request simply carries no profile, exactly
    /// as it did before this question existed. A wish must never fail
    /// because a question could not be asked.
    #[serde(default)]
    pub quality_profiles: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("cannot parse config {}", path.display()))?;
        reject_missing_scheme("authentik_url", &cfg.authentik_url)?;
        reject_missing_scheme("seerr_url", &cfg.seerr_url)?;
        // Plain http:// is this deployment's deliberate choice today (see
        // the design doc's zone note), not a mistake -- so this is a
        // warning, not a rejection. But the day somebody wonders whether a
        // token crosses the wire in the clear, the answer belongs in the
        // journal rather than in memory.
        for (field, value) in plain_http_fields(&cfg) {
            tracing::warn!(
                field,
                value,
                "credentials for this endpoint cross the network unencrypted (http://)"
            );
        }
        Ok(cfg)
    }

    #[cfg(test)]
    pub fn for_test() -> Config {
        Config {
            signal_socket: "/tmp/socket".into(),
            signal_account_file: "/dev/null".into(),
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
            settings_url: "https://example.invalid/account".into(),
            operator_name: "the operator".into(),
            quality_profiles: Vec::new(),
        }
    }
}

/// A missing scheme is a misconfiguration, not a choice: `192.0.2.10:9000`
/// would otherwise reach reqwest in a shape nobody meant to send. Only the
/// two endpoints this process actually connects to are checked --
/// jellyfin_url never leaves a message to a person.
fn reject_missing_scheme(field: &str, value: &str) -> Result<()> {
    if !value.starts_with("http://") && !value.starts_with("https://") {
        bail!("{field} must start with http:// or https://, got {value:?}");
    }
    Ok(())
}

/// The authentik_url/seerr_url fields that are `http://` rather than
/// `https://`, for `load()`'s startup warning. A pure function so the
/// decision of what counts as unencrypted is tested directly, without
/// capturing `tracing` output.
fn plain_http_fields(cfg: &Config) -> Vec<(&'static str, &str)> {
    [
        ("authentik_url", cfg.authentik_url.as_str()),
        ("seerr_url", cfg.seerr_url.as_str()),
    ]
    .into_iter()
    .filter(|(_, v)| v.starts_with("http://"))
    .collect()
}

#[derive(Debug)]
pub struct Secrets {
    pub signal_account: Secret,
    pub authentik_token: Secret,
    pub seerr_key: Secret,
    pub webhook_token: Secret,
}

impl Secrets {
    pub fn read(config: &Config) -> Result<Secrets> {
        Ok(Secrets {
            signal_account: read_one(&config.signal_account_file)?,
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
    fn a_url_missing_a_scheme_is_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let body = with_field_value(
            include_str!("../config.example.toml"),
            "seerr_url",
            "192.0.2.20:5055",
        );
        let p = write(&dir, "c.toml", &body);
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("seerr_url"), "got: {err}");
    }

    #[test]
    fn an_http_url_is_accepted_not_rejected() {
        // Plain HTTP is this deployment's deliberate choice (see the design
        // doc's zone note), not a mistake -- the guard above is only against
        // a missing scheme, never against http:// itself.
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "c.toml", include_str!("../config.example.toml"));
        let cfg = Config::load(&p).expect("http:// must still load");
        assert!(cfg.seerr_url.starts_with("http://"));
    }

    #[test]
    fn plain_http_fields_names_every_unencrypted_endpoint() {
        let cfg = Config {
            authentik_url: "http://192.0.2.10:9000".into(),
            seerr_url: "https://192.0.2.20:5055".into(),
            ..Config::for_test()
        };
        let flagged: Vec<_> = plain_http_fields(&cfg)
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        assert_eq!(flagged, vec!["authentik_url"]);
    }

    #[test]
    fn plain_http_fields_is_empty_when_both_are_encrypted() {
        let cfg = Config {
            authentik_url: "https://a.example.invalid".into(),
            seerr_url: "https://b.example.invalid".into(),
            ..Config::for_test()
        };
        assert!(plain_http_fields(&cfg).is_empty());
    }

    /// Replaces the value of a `field = "..."` line, whitespace around `=`
    /// notwithstanding, without disturbing the rest of the file -- brittle
    /// exact-string matching would break on the next reformat of the
    /// example config.
    fn with_field_value(base: &str, field: &str, value: &str) -> String {
        base.lines()
            .map(|l| {
                if l.split('=').next().map(str::trim) == Some(field) {
                    format!("{field} = \"{value}\"")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Drops a `field = "..."` line entirely, for the "this field is
    /// required" tests below.
    fn without_field(base: &str, field: &str) -> String {
        base.lines()
            .filter(|l| l.split('=').next().map(str::trim) != Some(field))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_config_missing_settings_url_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let body = without_field(include_str!("../config.example.toml"), "settings_url");
        let p = write(&dir, "c.toml", &body);
        let err = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(err.contains("settings_url"), "got: {err}");
    }

    #[test]
    fn a_config_missing_operator_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let body = without_field(include_str!("../config.example.toml"), "operator_name");
        let p = write(&dir, "c.toml", &body);
        let err = format!("{:#}", Config::load(&p).unwrap_err());
        assert!(err.contains("operator_name"), "got: {err}");
    }

    #[test]
    fn secrets_are_read_from_files_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        // A credential file written by systemd ends in a newline more often
        // than not; a trailing \n in an API key yields a 401 that looks like a
        // wrong key.
        let account = write(&dir, "account", "+491234567890\n");
        let tok = write(&dir, "tok", "abc123\n");
        let key = write(&dir, "key", "def456");
        let hook = write(&dir, "hook", "ghi789\n\n");
        let cfg = Config {
            signal_account_file: account,
            authentik_token_file: tok,
            seerr_key_file: key,
            webhook_token_file: hook,
            ..Config::for_test()
        };
        let s = Secrets::read(&cfg).unwrap();
        assert_eq!(s.signal_account.expose(), "+491234567890");
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

    #[test]
    fn a_missing_signal_account_file_is_an_error_naming_the_path() {
        // The phone number moved behind a *_file option specifically so it
        // never sits in `settings` in plain text (see the module doc); a
        // config that still names a file that is not there must fail
        // loudly at startup, the same way the other three secrets do, not
        // fall back to some empty or guessed value.
        let cfg = Config {
            signal_account_file: "/nonexistent/signal-account".into(),
            ..Config::for_test()
        };
        let err = Secrets::read(&cfg).unwrap_err().to_string();
        assert!(err.contains("/nonexistent/signal-account"), "got: {err}");
    }
}
