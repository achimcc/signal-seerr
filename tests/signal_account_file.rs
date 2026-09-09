use signal_seerr::config::{Config, Secrets};
use signal_seerr::signal::SignalClient;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// `Config` no longer carries the operator's phone number directly (see
/// CLAUDE.md, "No secret in `settings`") -- only a path to a file that
/// holds it, the same shape as the other three credentials. This proves the
/// whole chain still delivers the right value: a config naming a file whose
/// contents are a number ends up with exactly that number in the "account"
/// field of the JSON-RPC request signal-cli receives, not a stale, default
/// or literal one.
#[tokio::test]
async fn the_number_in_signal_account_file_reaches_signal_cli() {
    let dir = tempfile::tempdir().unwrap();
    let account_path = dir.path().join("account");
    // A trailing newline, the way a systemd credential or an operator's
    // editor would leave one -- read_one() must still trim it.
    std::fs::write(&account_path, "+491234567890\n").unwrap();

    // Built through the real config file, not a struct literal: `for_test`
    // is `#[cfg(test)]` inside the crate and unavailable from here, and
    // going through `Config::load` is closer to how the value actually
    // arrives in production anyway.
    let toml = include_str!("../config.example.toml")
        .replace(
            "signal_account_file = \"/run/credentials/signal-seerr.service/signal-account\"",
            &format!("signal_account_file = \"{}\"", account_path.display()),
        )
        // The other three credentials are not what this test is about; point
        // them at a file that always reads as empty rather than one that
        // does not exist at all.
        .replace(
            "authentik_token_file = \"/run/credentials/signal-seerr.service/authentik-token\"",
            "authentik_token_file = \"/dev/null\"",
        )
        .replace(
            "seerr_key_file = \"/run/credentials/signal-seerr.service/seerr-key\"",
            "seerr_key_file = \"/dev/null\"",
        )
        .replace(
            "webhook_token_file = \"/run/credentials/signal-seerr.service/webhook-token\"",
            "webhook_token_file = \"/dev/null\"",
        );
    let config_path = dir.path().join("c.toml");
    std::fs::write(&config_path, toml).unwrap();
    let cfg = Config::load(&config_path).unwrap();
    let secrets = Secrets::read(&cfg).unwrap();

    let socket_path = dir.path().join("signal-cli.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        let request_line = lines.next_line().await.unwrap().unwrap();
        let request: serde_json::Value = serde_json::from_str(&request_line).unwrap();

        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {},
        });
        write_half
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();

        request
    });

    let (client, _rx) = SignalClient::connect(&socket_path, secrets.signal_account.expose())
        .await
        .unwrap();
    client
        .call("getUserStatus", serde_json::json!({}))
        .await
        .unwrap();

    let request = server_task.await.unwrap();
    assert_eq!(
        request["params"]["account"],
        serde_json::json!("+491234567890"),
        "the account signal-cli sees must be the trimmed contents of signal_account_file"
    );
}

/// A missing signal_account_file must fail startup, not fall back to some
/// empty or hard-coded account -- the same guarantee `Secrets::read` already
/// gives for the other three credentials, exercised here from the outside
/// (`Config::load`, not a struct literal), since that is how it actually
/// fails in production.
#[test]
fn a_config_naming_a_missing_signal_account_file_fails_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let toml = include_str!("../config.example.toml").replace(
        "signal_account_file = \"/run/credentials/signal-seerr.service/signal-account\"",
        "signal_account_file = \"/nonexistent/signal-account\"",
    );
    let config_path = dir.path().join("c.toml");
    std::fs::write(&config_path, toml).unwrap();

    let cfg = Config::load(&config_path).expect("the config itself still parses");
    let err = Secrets::read(&cfg).unwrap_err().to_string();
    assert!(err.contains("/nonexistent/signal-account"), "got: {err}");
}
