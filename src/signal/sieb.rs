//! The sieve between the bot and signal-cli (homeserver audit 3, B94).
//!
//! signal-cli's JSON-RPC socket offers every command of the account on the
//! same line protocol: besides `send` and `getUserStatus` also `addDevice`,
//! `unregister`, `setPin`, `startChangeNumber`, `updateAccount`. Whoever
//! could open the socket could link a device of their own to the bot's
//! account — a take-over that outlives every repair of the bot. The sieve
//! owns the socket instead; the bot talks to the sieve, and only the methods
//! it names reach signal-cli. What signal-cli sends back (answers and the
//! `receive` notifications) passes unchanged.

use serde_json::{json, Value};

/// The methods the bot calls. Nothing else is forwarded.
pub const ALLOWED: &[&str] = &["send", "getUserStatus"];

/// Longer lines are refused rather than buffered without end.
pub const MAX_LINE: usize = 1 << 20;

/// `Ok(())` when `line` is a single JSON-RPC request for an allowed method;
/// otherwise the JSON-RPC error line to answer the client with (carrying its
/// `id`, when it had one). A batch (a JSON array) is refused as a whole.
pub fn check(line: &str, allowed: &[&str]) -> Result<(), String> {
    let refuse = |id: Value, why: &str| {
        json!({"jsonrpc": "2.0", "id": id,
               "error": {"code": -32601, "message": format!("signal-sieb: {why}")}})
        .to_string()
    };
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Err(refuse(Value::Null, "not a JSON object"));
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let Some(obj) = v.as_object() else {
        return Err(refuse(id, "only single requests, no batches"));
    };
    match obj.get("method").and_then(Value::as_str) {
        Some(m) if allowed.contains(&m) => Ok(()),
        Some(m) => Err(refuse(id, &format!("method {m:?} is not allowed"))),
        None => Err(refuse(id, "a request needs a method")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_methods_of_the_bot_pass() {
        assert!(check(
            r#"{"jsonrpc":"2.0","id":"1","method":"send","params":{}}"#,
            ALLOWED
        )
        .is_ok());
        assert!(check(
            r#"{"jsonrpc":"2.0","id":"2","method":"getUserStatus","params":{}}"#,
            ALLOWED
        )
        .is_ok());
    }

    #[test]
    fn account_commands_are_refused_with_the_callers_id() {
        for m in [
            "addDevice",
            "unregister",
            "setPin",
            "removePin",
            "startChangeNumber",
            "updateAccount",
            "listContacts",
            "trust",
            "removeDevice",
        ] {
            let line = format!(r#"{{"jsonrpc":"2.0","id":"x7","method":"{m}","params":{{}}}}"#);
            let err = check(&line, ALLOWED).unwrap_err();
            let v: Value = serde_json::from_str(&err).unwrap();
            assert_eq!(v["id"], "x7", "{m}");
            assert_eq!(v["error"]["code"], -32601, "{m}");
        }
    }

    #[test]
    fn batches_garbage_and_missing_methods_are_refused() {
        assert!(check(r#"[{"jsonrpc":"2.0","id":"1","method":"send"}]"#, ALLOWED).is_err());
        assert!(check("not json", ALLOWED).is_err());
        assert!(check(r#"{"jsonrpc":"2.0","id":"1"}"#, ALLOWED).is_err());
    }
}
