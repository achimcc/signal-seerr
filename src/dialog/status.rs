//! Putting one `WishState` into words, in one place.
//!
//! Separate from `dialog/mod.rs` because the watcher (which tells somebody
//! unasked) and `/status` (which answers when asked) have to say the same
//! thing about the same state -- two copies of this mapping would drift, and
//! the one nobody re-reads is the one that gets it wrong.

use crate::i18n::{Catalogue, Locale};
use crate::model::{Reason, WishState};

/// The sentence fragment that goes into `status.line`'s `{state}`.
pub fn state_text(catalogue: &Catalogue, locale: Locale, state: &WishState) -> String {
    match state {
        WishState::Available => catalogue.text(locale, "status.available", &[]),
        WishState::NotHandedOver => catalogue.text(locale, "status.not_handed_over", &[]),
        WishState::ImportStuck => catalogue.text(locale, "status.import_stuck", &[]),
        WishState::Downloading { percent } => catalogue.text(
            locale,
            "status.downloading",
            &[("percent", &percent.to_string())],
        ),
        WishState::NotReleased { date: None } => catalogue.text(locale, "status.not_released", &[]),
        WishState::NotReleased { date: Some(date) } => catalogue.text(
            locale,
            "status.not_released_date",
            &[("date", &render_date(locale, *date))],
        ),
        WishState::DownloadFailed => catalogue.text(locale, "status.download_failed", &[]),
        WishState::Searching => catalogue.text(locale, "status.searching", &[]),
        WishState::Unsuitable(reason) => reason_text(catalogue, locale, reason),
    }
}

/// Why nothing suitable was found -- in everyday words, and never with a
/// custom format's, a release's or an indexer's own name in them: those are
/// the operator's business, and this goes to whoever asked for the film.
fn reason_text(catalogue: &Catalogue, locale: Locale, reason: &Reason) -> String {
    match reason {
        Reason::NothingExists => catalogue.text(locale, "status.nothing_exists", &[]),
        Reason::OnlyInLanguages(languages) => {
            let named: Vec<String> = languages
                .iter()
                .map(|name| language_name(catalogue, locale, name))
                .collect();
            catalogue.text(
                locale,
                "status.only_in_languages",
                &[("languages", &named.join(" / "))],
            )
        }
        Reason::TooLarge => catalogue.text(locale, "status.too_large", &[]),
        Reason::TooSmall => catalogue.text(locale, "status.too_small", &[]),
        Reason::WrongQuality => catalogue.text(locale, "status.wrong_quality", &[]),
        Reason::Otherwise => catalogue.text(locale, "status.otherwise", &[]),
    }
}

/// Radarr's own English language name, translated where the catalogue has a
/// name for it and passed through where it has not.
///
/// A quiet lookup on purpose: `Catalogue::text` answers an unknown key with
/// the key itself and a warning, which is right for a typo in a message and
/// wrong here -- Radarr knows far more languages than the `[language]` table
/// names, and a miss is an ordinary outcome, not a fault.
fn language_name(catalogue: &Catalogue, locale: Locale, name: &str) -> String {
    catalogue
        .lookup(locale, &format!("language.{name}"))
        .unwrap_or(name)
        .to_string()
}

/// Numerically in both languages -- 14.11.2026 here, 2026-11-14 there. A
/// month NAME would need a table of its own per locale, and would be the
/// only place in this bot that had one.
fn render_date(locale: Locale, date: time::Date) -> String {
    let day = date.day();
    let month = u8::from(date.month());
    let year = date.year();
    match locale {
        Locale::De => format!("{day:02}.{month:02}.{year}"),
        Locale::En => format!("{year}-{month:02}-{day:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_date_is_rendered_the_way_each_language_writes_one() {
        let date = time::macros::date!(2026 - 11 - 14);
        assert_eq!(render_date(Locale::De, date), "14.11.2026");
        assert_eq!(render_date(Locale::En, date), "2026-11-14");
    }

    #[test]
    fn a_language_the_catalogue_does_not_name_is_passed_through_unchanged() {
        // Radarr knows several hundred; the catalogue names seventeen. The
        // rest must arrive as Radarr's own English name -- not as the raw
        // key "language.Icelandic", which is what a plain `text` lookup
        // would have produced.
        let c = Catalogue::load();
        assert_eq!(language_name(&c, Locale::De, "German"), "Deutsch");
        assert_eq!(language_name(&c, Locale::De, "Icelandic"), "Icelandic");
    }

    #[test]
    fn two_languages_are_joined_and_translated() {
        let c = Catalogue::load();
        let state = WishState::Unsuitable(Reason::OnlyInLanguages(vec![
            "Portuguese".into(),
            "Spanish".into(),
        ]));
        let text = state_text(&c, Locale::De, &state);
        assert!(text.contains("Portugiesisch / Spanisch"), "got: {text}");
        assert!(!text.contains('{'), "no placeholder may survive: {text}");
    }

    #[test]
    fn every_state_says_something_other_than_its_own_key() {
        // A missing catalogue entry makes `text` answer with the key itself,
        // which reads as "status.import_stuck" in a chat and is easy to ship
        // unnoticed -- this is the guard for the whole table at once.
        let c = Catalogue::load();
        let states = [
            WishState::Available,
            WishState::NotHandedOver,
            WishState::ImportStuck,
            WishState::Downloading { percent: 40 },
            WishState::NotReleased { date: None },
            WishState::NotReleased {
                date: Some(time::macros::date!(2026 - 11 - 14)),
            },
            WishState::DownloadFailed,
            WishState::Searching,
            WishState::Unsuitable(Reason::NothingExists),
            WishState::Unsuitable(Reason::OnlyInLanguages(vec!["German".into()])),
            WishState::Unsuitable(Reason::TooLarge),
            WishState::Unsuitable(Reason::TooSmall),
            WishState::Unsuitable(Reason::WrongQuality),
            WishState::Unsuitable(Reason::Otherwise),
        ];
        for locale in [Locale::De, Locale::En] {
            for state in &states {
                let text = state_text(&c, locale, state);
                assert!(
                    !text.starts_with("status."),
                    "{state:?} fell through to its key: {text}"
                );
                assert!(!text.contains('{'), "{state:?} left a placeholder: {text}");
            }
        }
    }
}
