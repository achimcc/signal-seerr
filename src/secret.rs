use std::fmt;

/// A string that must never reach a log line, an error message or a Debug
/// dump. Three API keys have leaked into a chat transcript in this project's
/// sibling repo; this type is the structural answer to that.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let s = Secret::from("hunter2-and-then-some".to_string());
        let shown = format!("{s:?}");
        assert!(!shown.contains("hunter2"), "leaked: {shown}");
        assert_eq!(shown, "<redacted>");
    }
}
