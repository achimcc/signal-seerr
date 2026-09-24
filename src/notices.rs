//! The on-disk record of what the bot has already told whom about which
//! wish. Kept separate from `state.rs` (see its module doc): that mapping
//! rebuilds itself from Authentik after a loss, this one does not.
//!
//! An unreadable or unparsable file is an error that names the path, never
//! an empty record -- a silently emptied record would repeat every notice
//! to everybody. A missing file, by contrast, is an empty record: the first
//! start.

use crate::model::Reason;
use crate::state::write_atomically;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use time::{Date, OffsetDateTime};

const SCHEMA: u32 = 1;

/// Everything remembered about one wish: when it was first seen, when it
/// (last) turned out to be released, why it is unsuitable if it is, when it
/// was last searched for, and which notice classes have already gone out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub title: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub first_seen: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub released_seen: Option<OffsetDateTime>,
    /// Whether a round has ever seen this film as NOT yet available.
    ///
    /// It is what makes `released_seen` an observation rather than a guess:
    /// only a wish that was once seen unreleased can have "it came out" as
    /// the start of its stall clock. Without it the first sight of an
    /// available film looks like the transition, and the two commonest
    /// histories -- a first round that saw a queue entry, or one whose
    /// lookup failed -- would push the deadline a whole `stall_after` into
    /// the future.
    ///
    /// `#[serde(default)]` and no schema bump: an older file simply says
    /// "never seen unreleased", which is the safe reading.
    #[serde(default)]
    pub seen_unreleased: bool,
    #[serde(default)]
    pub reason: Option<Reason>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub searched_at: Option<OffsetDateTime>,
    /// class -> rfc3339 timestamp of when that class was last told.
    #[serde(default)]
    pub told: BTreeMap<String, String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_notice: Option<OffsetDateTime>,
    /// When the bot handed a FAILED wish to Radarr/Sonarr a second time
    /// (`Requests::retry`). Written BEFORE the call: whatever Seerr answers,
    /// there is never a third attempt. `#[serde(default)]`, no schema bump --
    /// an older file simply says "never retried".
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub retried_at: Option<OffsetDateTime>,
}

impl Note {
    fn new(first_seen: OffsetDateTime) -> Note {
        Note {
            title: None,
            first_seen,
            released_seen: None,
            seen_unreleased: false,
            reason: None,
            searched_at: None,
            told: BTreeMap::new(),
            last_notice: None,
            retried_at: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notices {
    schema: u32,
    requests: BTreeMap<i64, Note>,
    search_day: Option<Date>,
    searches_today: u32,
}

impl Default for Notices {
    fn default() -> Notices {
        Notices {
            schema: SCHEMA,
            requests: BTreeMap::new(),
            search_day: None,
            searches_today: 0,
        }
    }
}

impl Notices {
    pub fn load(path: &Path) -> Result<Notices> {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Notices::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
            Ok(raw) => {
                let parsed: Notices = serde_json::from_str(&raw)
                    .with_context(|| format!("cannot parse {}", path.display()))?;
                if parsed.schema != SCHEMA {
                    anyhow::bail!(
                        "unsupported notices schema {} in {}",
                        parsed.schema,
                        path.display()
                    );
                }
                Ok(parsed)
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        write_atomically(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn note(&self, request_id: i64) -> Option<&Note> {
        self.requests.get(&request_id)
    }

    /// The entry for `request_id`, created with `first_seen = now` if it did
    /// not exist yet; an existing entry is returned untouched.
    pub fn note_mut(&mut self, request_id: i64, now: OffsetDateTime) -> &mut Note {
        self.requests
            .entry(request_id)
            .or_insert_with(|| Note::new(now))
    }

    /// Drops every entry whose request id is not among `live_ids` -- a wish
    /// withdrawn or gone from Seerr has nothing left to be told about.
    pub fn retain_only(&mut self, live_ids: &[i64]) {
        self.requests.retain(|id, _| live_ids.contains(id));
    }

    /// Whether an indexer search may still run today, counted per UTC
    /// calendar day.
    ///
    /// Today's count is normalised BEFORE it is compared, not after: the
    /// fresh-record and turned-over-day branches used to answer "yes"
    /// without looking at `per_day` at all, so a budget of zero -- the one
    /// setting whose entire point is that nothing happens -- still bought
    /// one search a day.
    pub fn may_search(&self, now: OffsetDateTime, per_day: u32) -> bool {
        self.searches_on(today_utc(now)) < per_day
    }

    fn searches_on(&self, day: Date) -> u32 {
        if self.search_day == Some(day) {
            self.searches_today
        } else {
            0
        }
    }

    /// Counts one search against today's budget, resetting the counter if
    /// the UTC calendar day has turned over since the last one.
    pub fn count_search(&mut self, now: OffsetDateTime) {
        let today = today_utc(now);
        if self.search_day != Some(today) {
            self.search_day = Some(today);
            self.searches_today = 0;
        }
        self.searches_today += 1;
    }
}

fn today_utc(now: OffsetDateTime) -> Date {
    now.to_offset(time::UtcOffset::UTC).date()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_missing_file_is_an_empty_record_not_an_error() {
        // First start: there is no file yet. Failing here would mean the
        // bot never comes up on a fresh machine.
        let dir = tempfile::tempdir().unwrap();
        let n = Notices::load(&dir.path().join("nope.json")).unwrap();
        assert!(n.note(1).is_none());
    }

    #[test]
    fn it_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");

        let mut n = Notices::default();
        let note = n.note_mut(42, datetime!(2026-09-21 10:00:00 UTC));
        note.title = Some("Arrival".into());
        note.reason = Some(Reason::OnlyInLanguages(vec!["Portuguese".into()]));
        note.searched_at = Some(datetime!(2026-09-21 10:05:00 UTC));
        note.told
            .insert("unsuitable".into(), "2026-09-21T10:06:00Z".into());
        note.last_notice = Some(datetime!(2026-09-21 10:06:00 UTC));
        n.count_search(datetime!(2026-09-21 10:00:00 UTC));
        n.save(&path).unwrap();

        let back = Notices::load(&path).unwrap();
        // Compares every field, not just the ones a lookup uses -- a serde
        // rename typo on any of them must not survive a save/load cycle
        // unnoticed.
        assert_eq!(back.note(42).unwrap(), n.note(42).unwrap());
        assert!(back.may_search(datetime!(2026-09-21 12:00:00 UTC), 2));
        assert!(!back.may_search(datetime!(2026-09-21 12:00:00 UTC), 1));
    }

    #[test]
    fn a_truncated_file_is_reported_by_name_not_emptied() {
        // The decisive behaviour: an emptied record would repeat every
        // notice to everybody, so this must be an error, not Notices::default().
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = Notices::load(&path).unwrap_err().to_string();
        assert!(err.contains("notices.json"), "got: {err}");
    }

    #[test]
    fn an_unknown_schema_is_an_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        std::fs::write(
            &path,
            r#"{"schema":2,"requests":{},"search_day":null,"searches_today":0}"#,
        )
        .unwrap();
        let err = Notices::load(&path).unwrap_err().to_string();
        assert!(err.contains("notices.json"), "got: {err}");
        assert!(err.contains('2'), "got: {err}");
    }

    #[test]
    fn saving_leaves_no_half_written_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        let mut n = Notices::default();
        n.note_mut(1, datetime!(2026-09-21 10:00:00 UTC));
        n.save(&path).unwrap();
        n.save(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "notices.json")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn a_failed_save_leaves_the_previous_notices_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");

        let mut first = Notices::default();
        first.note_mut(1, datetime!(2026-09-21 10:00:00 UTC));
        first.save(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Occupy the sibling name the atomic write needs, with a directory
        // -- nothing can be written onto that.
        std::fs::create_dir(path.with_extension("json.new")).unwrap();

        let mut second = Notices::default();
        second.note_mut(2, datetime!(2026-09-21 10:00:00 UTC));
        assert!(
            second.save(&path).is_err(),
            "the save must fail rather than quietly succeed"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed save must leave the previous notices untouched"
        );
    }

    #[test]
    fn may_search_counts_per_utc_calendar_day_and_resets_on_the_next() {
        let mut n = Notices::default();
        let morning = datetime!(2026-09-21 08:00:00 UTC);
        let evening = datetime!(2026-09-21 23:00:00 UTC);
        let next_day = datetime!(2026-09-22 00:30:00 UTC);

        assert!(n.may_search(morning, 2));
        n.count_search(morning);
        assert!(n.may_search(evening, 2));
        n.count_search(evening);
        assert!(!n.may_search(evening, 2), "budget for today is used up");

        // A new UTC calendar day resets the counter, even minutes later.
        assert!(n.may_search(next_day, 2));
    }

    #[test]
    fn a_budget_of_zero_allows_no_search_at_all() {
        // The fresh-record and new-day branches used to answer "yes" without
        // looking at `per_day`, so an operator who switched the interactive
        // search off by setting the budget to zero still got one search a
        // day -- the one setting whose whole point is that nothing happens.
        let n = Notices::default();
        assert!(!n.may_search(datetime!(2026-09-21 08:00:00 UTC), 0));

        let mut used = Notices::default();
        used.count_search(datetime!(2026-09-21 08:00:00 UTC));
        assert!(
            !used.may_search(datetime!(2026-09-22 08:00:00 UTC), 0),
            "a new day resets the counter, it does not grant a search"
        );
    }

    #[test]
    fn a_note_written_before_seen_unreleased_existed_still_loads() {
        // The field arrived after the first deployments. A file without it
        // must load as "never seen unreleased" rather than failing, which
        // would take the whole loop down over a schema that did not change.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        std::fs::write(
            &path,
            r#"{"schema":1,"requests":{"7":{"title":"Arrival",
               "first_seen":"2026-09-21T10:00:00Z"}},
               "search_day":null,"searches_today":0}"#,
        )
        .unwrap();

        let back = Notices::load(&path).unwrap();
        let note = back.note(7).expect("the note must survive");
        assert_eq!(note.title.as_deref(), Some("Arrival"));
        assert!(!note.seen_unreleased);
    }

    #[test]
    fn retain_only_drops_entries_not_in_the_given_ids() {
        let mut n = Notices::default();
        n.note_mut(1, datetime!(2026-09-21 10:00:00 UTC));
        n.note_mut(2, datetime!(2026-09-21 10:00:00 UTC));
        n.note_mut(3, datetime!(2026-09-21 10:00:00 UTC));

        n.retain_only(&[1, 3]);

        assert!(n.note(1).is_some());
        assert!(n.note(2).is_none(), "withdrawn wish must be dropped");
        assert!(n.note(3).is_some());
    }
}
