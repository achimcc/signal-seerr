use crate::secret::Secret;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
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
    /// Read-only Radarr/Sonarr access for `/status` and stall notices.
    /// Optional: without this section the bot behaves exactly as it did
    /// before insight existed.
    #[serde(default)]
    pub insight: Option<InsightConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("cannot parse config {}", path.display()))?;
        reject_missing_scheme("authentik_url", &cfg.authentik_url)?;
        reject_missing_scheme("seerr_url", &cfg.seerr_url)?;
        if let Some(insight) = &cfg.insight {
            validate_insight(insight)?;
            reject_missing_scheme("insight.radarr_url", &insight.radarr_url)?;
            if let Some(sonarr_url) = &insight.sonarr_url {
                reject_missing_scheme("insight.sonarr_url", sonarr_url)?;
            }
        }
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
            insight: None,
        }
    }
}

/// Read-only Radarr/Sonarr access for `/status` and stall notices. Absent by
/// default: an operator who never fills in this section keeps the bot
/// behaving exactly as it did before insight existed.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct InsightConfig {
    pub radarr_url: String,
    pub radarr_key_file: PathBuf,
    /// `sonarr_url` and `sonarr_key_file` are a pair -- Sonarr access is
    /// optional, but one without the other is a load error (see
    /// `validate_insight`).
    #[serde(default)]
    pub sonarr_url: Option<String>,
    #[serde(default)]
    pub sonarr_key_file: Option<PathBuf>,
    #[serde(default = "ten_minutes")]
    pub poll_seconds: u64,
    #[serde(default = "one_day")]
    pub stall_after_hours: u64,
    /// Off by default: an interactive search at somebody else's indexers is
    /// something an operator switches on explicitly, never a byproduct of
    /// turning insight on.
    #[serde(default)]
    pub reason_search: bool,
    #[serde(default = "five")]
    pub max_reason_searches_per_day: u32,
    pub notices_file: PathBuf,
    /// profile NAME -> the languages that profile REQUIRES. Only listed
    /// profiles ever get the "only in <language>" reason.
    #[serde(default)]
    pub profile_languages: BTreeMap<String, Vec<String>>,
}

fn ten_minutes() -> u64 {
    600
}

fn one_day() -> u64 {
    24
}

fn five() -> u32 {
    5
}

/// `sonarr_url` and `sonarr_key_file` must come as a pair -- one without the
/// other is a load error naming both fields, not a half-configured Sonarr
/// that fails later in a way nobody connects back to this section.
fn validate_insight(insight: &InsightConfig) -> Result<()> {
    match (&insight.sonarr_url, &insight.sonarr_key_file) {
        (Some(_), None) | (None, Some(_)) => {
            bail!(
                "insight.sonarr_url and insight.sonarr_key_file must both be set or both be absent"
            )
        }
        _ => Ok(()),
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
    let mut fields = vec![
        ("authentik_url", cfg.authentik_url.as_str()),
        ("seerr_url", cfg.seerr_url.as_str()),
    ];
    if let Some(insight) = &cfg.insight {
        fields.push(("insight.radarr_url", insight.radarr_url.as_str()));
        if let Some(sonarr_url) = &insight.sonarr_url {
            fields.push(("insight.sonarr_url", sonarr_url.as_str()));
        }
    }
    fields
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
    /// Read only when `[insight]` is configured.
    pub radarr_key: Option<Secret>,
    /// Read only when `[insight]` is configured AND names a `sonarr_url`.
    pub sonarr_key: Option<Secret>,
}

impl Secrets {
    pub fn read(config: &Config) -> Result<Secrets> {
        let (radarr_key, sonarr_key) = match &config.insight {
            Some(insight) => {
                let radarr_key = Some(read_one(&insight.radarr_key_file)?);
                let sonarr_key = insight
                    .sonarr_key_file
                    .as_ref()
                    .map(|p| read_one(p))
                    .transpose()?;
                (radarr_key, sonarr_key)
            }
            None => (None, None),
        };
        Ok(Secrets {
            signal_account: read_one(&config.signal_account_file)?,
            authentik_token: read_one(&config.authentik_token_file)?,
            seerr_key: read_one(&config.seerr_key_file)?,
            webhook_token: read_one(&config.webhook_token_file)?,
            radarr_key,
            sonarr_key,
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
        // The [insight] section is commented out in the shipped example --
        // without it, the bot behaves exactly as it did before insight
        // existed.
        assert!(cfg.insight.is_none());
    }

    /// Appends a real (uncommented) `[insight]` section to the example
    /// config, whose own copy stays commented out. `body` is the section's
    /// content, without the `[insight]` header itself.
    fn with_insight_section(body: &str) -> String {
        format!(
            "{}\n[insight]\n{body}",
            include_str!("../config.example.toml")
        )
    }

    /// A minimal, valid `[insight]` body: just the two required fields.
    const MINIMAL_INSIGHT: &str = "radarr_url = \"https://radarr.example.invalid\"\nradarr_key_file = \"/dev/null\"\nnotices_file = \"/tmp/notices.json\"\n";

    #[test]
    fn insight_section_parses_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(&dir, "c.toml", &with_insight_section(MINIMAL_INSIGHT));
        let cfg = Config::load(&p).expect("insight section must parse");
        let insight = cfg.insight.expect("insight must be Some");
        assert_eq!(insight.radarr_url, "https://radarr.example.invalid");
        assert_eq!(insight.radarr_key_file, Path::new("/dev/null"));
        assert!(insight.sonarr_url.is_none());
        assert!(insight.sonarr_key_file.is_none());
        assert_eq!(insight.poll_seconds, 600);
        assert_eq!(insight.stall_after_hours, 24);
        assert!(!insight.reason_search, "reason_search must default to off");
        assert_eq!(insight.max_reason_searches_per_day, 5);
        assert!(insight.profile_languages.is_empty());
    }

    #[test]
    fn sonarr_url_without_key_file_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("{MINIMAL_INSIGHT}sonarr_url = \"https://sonarr.example.invalid\"\n");
        let p = write(&dir, "c.toml", &with_insight_section(&body));
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("sonarr_url"), "got: {err}");
        assert!(err.contains("sonarr_key_file"), "got: {err}");
    }

    #[test]
    fn sonarr_key_file_without_url_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("{MINIMAL_INSIGHT}sonarr_key_file = \"/dev/null\"\n");
        let p = write(&dir, "c.toml", &with_insight_section(&body));
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("sonarr_url"), "got: {err}");
        assert!(err.contains("sonarr_key_file"), "got: {err}");
    }

    #[test]
    fn insight_radarr_url_missing_scheme_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let body = "radarr_url = \"192.0.2.30:7878\"\nradarr_key_file = \"/dev/null\"\nnotices_file = \"/tmp/notices.json\"\n";
        let p = write(&dir, "c.toml", &with_insight_section(body));
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("insight.radarr_url"), "got: {err}");
    }

    #[test]
    fn insight_sonarr_url_missing_scheme_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "{MINIMAL_INSIGHT}sonarr_url = \"192.0.2.40:8989\"\nsonarr_key_file = \"/dev/null\"\n"
        );
        let p = write(&dir, "c.toml", &with_insight_section(&body));
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("insight.sonarr_url"), "got: {err}");
    }

    #[test]
    fn plain_http_fields_names_insight_endpoints_too() {
        let cfg = Config {
            authentik_url: "https://a.example.invalid".into(),
            seerr_url: "https://b.example.invalid".into(),
            insight: Some(InsightConfig {
                radarr_url: "http://192.0.2.30:7878".into(),
                radarr_key_file: "/dev/null".into(),
                sonarr_url: Some("http://192.0.2.40:8989".into()),
                sonarr_key_file: Some("/dev/null".into()),
                poll_seconds: 600,
                stall_after_hours: 24,
                reason_search: false,
                max_reason_searches_per_day: 5,
                notices_file: "/tmp/notices.json".into(),
                profile_languages: Default::default(),
            }),
            ..Config::for_test()
        };
        let flagged: Vec<_> = plain_http_fields(&cfg)
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        assert_eq!(flagged, vec!["insight.radarr_url", "insight.sonarr_url"]);
    }

    #[test]
    fn profile_languages_parses_a_quoted_key_with_spaces_commas_and_parens() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "{MINIMAL_INSIGHT}\n[insight.profile_languages]\n\"Dual Language, then German (1080p)\" = [\"German\", \"English\"]\n"
        );
        let p = write(&dir, "c.toml", &with_insight_section(&body));
        let cfg = Config::load(&p).expect("profile_languages must parse");
        let insight = cfg.insight.expect("insight must be Some");
        assert_eq!(
            insight
                .profile_languages
                .get("Dual Language, then German (1080p)"),
            Some(&vec!["German".to_string(), "English".to_string()])
        );
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

    #[test]
    fn without_insight_radarr_and_sonarr_keys_are_absent() {
        let cfg = Config::for_test();
        let s = Secrets::read(&cfg).unwrap();
        assert!(s.radarr_key.is_none());
        assert!(s.sonarr_key.is_none());
    }

    #[test]
    fn with_insight_the_radarr_key_is_read_and_sonarr_only_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let radarr_key = write(&dir, "radarr-key", "radarr-secret\n");
        let cfg = Config {
            insight: Some(InsightConfig {
                radarr_url: "https://radarr.example.invalid".into(),
                radarr_key_file: radarr_key,
                sonarr_url: None,
                sonarr_key_file: None,
                poll_seconds: 600,
                stall_after_hours: 24,
                reason_search: false,
                max_reason_searches_per_day: 5,
                notices_file: "/tmp/notices.json".into(),
                profile_languages: Default::default(),
            }),
            ..Config::for_test()
        };
        let s = Secrets::read(&cfg).unwrap();
        assert_eq!(s.radarr_key.unwrap().expose(), "radarr-secret");
        assert!(s.sonarr_key.is_none());
    }

    #[test]
    fn with_insight_the_sonarr_key_is_read_too_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let radarr_key = write(&dir, "radarr-key", "radarr-secret");
        let sonarr_key = write(&dir, "sonarr-key", "sonarr-secret");
        let cfg = Config {
            insight: Some(InsightConfig {
                radarr_url: "https://radarr.example.invalid".into(),
                radarr_key_file: radarr_key,
                sonarr_url: Some("https://sonarr.example.invalid".into()),
                sonarr_key_file: Some(sonarr_key),
                poll_seconds: 600,
                stall_after_hours: 24,
                reason_search: false,
                max_reason_searches_per_day: 5,
                notices_file: "/tmp/notices.json".into(),
                profile_languages: Default::default(),
            }),
            ..Config::for_test()
        };
        let s = Secrets::read(&cfg).unwrap();
        assert_eq!(s.radarr_key.unwrap().expose(), "radarr-secret");
        assert_eq!(s.sonarr_key.unwrap().expose(), "sonarr-secret");
    }
}
