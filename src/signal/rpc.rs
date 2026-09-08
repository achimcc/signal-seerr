use crate::model::Aci;
use serde::Deserialize;

#[derive(Debug)]
pub struct Incoming {
    pub from: Aci,
    pub text: String,
}

#[derive(Debug)]
pub enum Frame {
    Response {
        id: String,
        result: Result<serde_json::Value, String>,
    },
    Notification {
        note: Box<Notification>,
    },
    /// Anything we do not model. Logged at debug level and dropped -- a bot
    /// that dies on an unknown frame dies on the next signal-cli release.
    Other,
}

#[derive(Debug, Deserialize)]
pub struct Notification {
    pub envelope: Envelope,
    #[allow(dead_code)]
    pub account: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub source_uuid: Option<String>,
    pub data_message: Option<DataMessage>,
}

#[derive(Debug, Deserialize)]
pub struct DataMessage {
    pub message: Option<String>,
}

impl Notification {
    pub fn into_incoming(self) -> Option<Incoming> {
        let from = Aci(self.envelope.source_uuid?);
        let text = self.envelope.data_message?.message?;
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(Incoming {
            from,
            text: text.to_string(),
        })
    }
}

pub fn parse_line(line: &str) -> anyhow::Result<Frame> {
    let value: serde_json::Value = serde_json::from_str(line)?;

    if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
        let result = if let Some(err) = value.get("error") {
            Err(err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string())
        } else {
            Ok(value
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        };
        return Ok(Frame::Response {
            id: id.to_string(),
            result,
        });
    }

    if value.get("method").and_then(|m| m.as_str()) == Some("receive") {
        if let Some(params) = value.get("params") {
            match serde_json::from_value::<Notification>(params.clone()) {
                Ok(note) => {
                    return Ok(Frame::Notification {
                        note: Box::new(note),
                    });
                }
                Err(e) => {
                    // Anything else that fails to parse becomes `Frame::Other`
                    // without comment -- an unrelated method we do not model
                    // is routine. This one case is not: signal-cli reshaping
                    // the envelope is exactly the kind of upstream change
                    // that should announce itself instead of going quiet.
                    tracing::warn!(
                        error = %e,
                        "cannot parse a receive envelope -- signal-cli's shape may have changed"
                    );
                }
            }
        }
    }

    Ok(Frame::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_is_told_apart_from_a_notification() {
        let response = r#"{"jsonrpc":"2.0","id":"7","result":{"timestamp":1}}"#;
        assert!(matches!(parse_line(response).unwrap(), Frame::Response { id, .. } if id == "7"));

        let note = r#"{"jsonrpc":"2.0","method":"receive","params":{
            "envelope":{"sourceUuid":"aaaa-bbbb","dataMessage":{"message":"blade runner"}},
            "account":"+490000"}}"#;
        assert!(matches!(
            parse_line(note).unwrap(),
            Frame::Notification { .. }
        ));
    }

    #[test]
    fn an_error_response_carries_its_message() {
        let line =
            r#"{"jsonrpc":"2.0","id":"7","error":{"code":-32602,"message":"Unregistered user"}}"#;
        match parse_line(line).unwrap() {
            Frame::Response { id, result } => {
                assert_eq!(id, "7");
                assert!(result.unwrap_err().contains("Unregistered user"));
            }
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn an_incoming_text_message_is_extracted() {
        let note: Notification = serde_json::from_str(
            r#"{"envelope":{"sourceUuid":"aaaa-bbbb","dataMessage":{"message":" blade runner "}},
                "account":"+490000"}"#,
        )
        .unwrap();
        let got = note.into_incoming().expect("a text message");
        assert_eq!(got.from.0, "aaaa-bbbb");
        assert_eq!(got.text, "blade runner", "text is trimmed");
    }

    #[test]
    fn a_receipt_without_text_is_not_an_incoming_message() {
        // Read receipts, typing indicators and delivery reports all arrive on
        // the same stream. Treating one as an empty message would answer
        // "I cannot make sense of that" to somebody who said nothing.
        let note: Notification = serde_json::from_str(
            r#"{"envelope":{"sourceUuid":"aaaa-bbbb","receiptMessage":{"isDelivery":true}},
                "account":"+490000"}"#,
        )
        .unwrap();
        assert!(note.into_incoming().is_none());
    }

    #[test]
    fn a_message_with_only_whitespace_is_ignored() {
        let note: Notification = serde_json::from_str(
            r#"{"envelope":{"sourceUuid":"a","dataMessage":{"message":"   "}},"account":"+4900"}"#,
        )
        .unwrap();
        assert!(note.into_incoming().is_none());
    }
}
