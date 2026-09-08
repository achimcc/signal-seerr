use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Locale {
    De,
    En,
}

impl Locale {
    /// Authentik stores a POSIX-ish locale ("de", "de-DE", "" for unset).
    /// Anything we do not translate falls back to English.
    pub fn from_authentik(raw: &str) -> Locale {
        if raw.split(['-', '_']).next() == Some("de") {
            Locale::De
        } else {
            Locale::En
        }
    }
}

pub struct Catalogue {
    de: BTreeMap<String, String>,
    en: BTreeMap<String, String>,
}

impl Catalogue {
    /// The catalogues are compiled in: a missing file at runtime would be a
    /// silent half-working bot, and there is nothing to configure here.
    pub fn load() -> Catalogue {
        Catalogue {
            de: flatten(include_str!("../i18n/de.toml")),
            en: flatten(include_str!("../i18n/en.toml")),
        }
    }

    fn table(&self, locale: Locale) -> &BTreeMap<String, String> {
        match locale {
            Locale::De => &self.de,
            Locale::En => &self.en,
        }
    }

    pub fn keys(&self, locale: Locale) -> BTreeSet<String> {
        self.table(locale).keys().cloned().collect()
    }

    /// Returns the key itself when it is unknown. Loud enough to notice in a
    /// chat, quiet enough not to crash a running bot over a typo.
    pub fn text(&self, locale: Locale, key: &str, args: &[(&str, &str)]) -> String {
        let Some(template) = self.table(locale).get(key) else {
            tracing::warn!(key, "unknown i18n key");
            return key.to_string();
        };
        let mut out = template.clone();
        for (name, value) in args {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }
}

/// "greeting.title" from a nested TOML table.
fn flatten(raw: &str) -> BTreeMap<String, String> {
    // `Value::from_str` (i.e. `raw.parse()`) only parses a single TOML value in
    // this crate version, not a whole document -- `toml::from_str` is the
    // entry point for documents, see the crate's `de` module docs.
    let value: toml::Value = toml::from_str(raw).expect("catalogue is not valid TOML");
    let mut out = BTreeMap::new();
    walk(&value, String::new(), &mut out);
    out
}

fn walk(value: &toml::Value, prefix: String, out: &mut BTreeMap<String, String>) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let next = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                walk(v, next, out);
            }
        }
        toml::Value::String(s) => {
            out.insert(prefix, s.clone());
        }
        other => panic!("catalogue value at {prefix} is {other:?}, expected a string"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_catalogues_carry_the_same_keys() {
        let c = Catalogue::load();
        assert_eq!(
            c.keys(Locale::De),
            c.keys(Locale::En),
            "de.toml and en.toml must define exactly the same keys"
        );
    }

    #[test]
    fn placeholders_are_substituted() {
        let c = Catalogue::load();
        let s = c.text(
            Locale::De,
            "request.placed",
            &[("title", "Andor"), ("id", "17")],
        );
        assert!(s.contains("Andor"), "got: {s}");
        assert!(s.contains("17"), "got: {s}");
        assert!(!s.contains('{'), "no placeholder may survive: {s}");
    }

    #[test]
    fn an_unknown_key_yields_the_key_itself() {
        let c = Catalogue::load();
        assert_eq!(c.text(Locale::De, "nope.nothing", &[]), "nope.nothing");
    }

    #[test]
    fn authentik_locale_maps_to_english_by_default() {
        assert!(matches!(Locale::from_authentik("de"), Locale::De));
        assert!(matches!(Locale::from_authentik("de-DE"), Locale::De));
        assert!(matches!(Locale::from_authentik(""), Locale::En));
        assert!(matches!(Locale::from_authentik("fr"), Locale::En));
    }

    #[test]
    fn locale_keeps_its_json_shape() {
        // Task 7 persists a Locale inside a state file that must survive a
        // restart. A silently changed representation would make stored state
        // unreadable, and nothing would say so.
        assert_eq!(serde_json::to_string(&Locale::De).unwrap(), "\"De\"");
        assert_eq!(serde_json::to_string(&Locale::En).unwrap(), "\"En\"");
        assert_eq!(
            serde_json::from_str::<Locale>("\"En\"").unwrap(),
            Locale::En
        );
    }
}
