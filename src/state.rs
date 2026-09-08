use crate::model::Aci;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub authentik_username: String,
    pub signal_username: String,
    /// Pinned once and never re-resolved: a released username can be taken by
    /// somebody else, and re-resolving would then serve the wrong person
    /// while looking entirely correct.
    pub aci: Aci,
    /// Whether the welcome has gone out. Persisted so a restart does not greet
    /// everybody again.
    pub greeted: bool,
    /// The person's language, from Authentik's locale setting. Kept here so
    /// the dialog can answer in it without a network round trip on every
    /// message.
    pub locale: crate::i18n::Locale,
    /// Their Authentik group names. Same reason: whether somebody may request
    /// anything is decided on every message, and it must not cost a lookup.
    pub groups: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    entries: Vec<Entry>,
}

impl State {
    pub fn load(path: &Path) -> Result<State> {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("cannot parse {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let temp = path.with_extension("json.new");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("cannot write {}", temp.display()))?;
        std::fs::rename(&temp, path)
            .with_context(|| format!("cannot rename onto {}", path.display()))?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn by_aci(&self, aci: &Aci) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.aci == aci)
    }

    pub fn by_user(&self, authentik_username: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.authentik_username == authentik_username)
    }

    pub fn signal_name_taken_by(&self, signal_username: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.signal_username.eq_ignore_ascii_case(signal_username))
    }

    pub fn upsert(&mut self, entry: Entry) {
        self.entries
            .retain(|e| e.authentik_username != entry.authentik_username);
        self.entries.push(entry);
    }

    pub fn remove_user(&mut self, authentik_username: &str) -> Option<Entry> {
        let at = self
            .entries
            .iter()
            .position(|e| e.authentik_username == authentik_username)?;
        Some(self.entries.remove(at))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::model::Aci;

    fn entry(user: &str, signal: &str, aci: &str) -> Entry {
        Entry {
            authentik_username: user.into(),
            signal_username: signal.into(),
            aci: Aci(aci.into()),
            greeted: false,
            locale: Locale::De,
            groups: vec!["Medien".into()],
        }
    }

    #[test]
    fn a_missing_file_is_an_empty_state_not_an_error() {
        // First start: there is no file yet. Failing here would mean the bot
        // never comes up on a fresh machine.
        let dir = tempfile::tempdir().unwrap();
        let s = State::load(&dir.path().join("nope.json")).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn it_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = State::default();
        let original = entry("robert", "robert.42", "aaaa");
        s.upsert(original.clone());
        s.save(&path).unwrap();

        let back = State::load(&path).unwrap();
        // Compares every field, not just the two used to look it up -- a
        // serde rename typo on `locale` or `groups` must not survive a
        // save/load cycle unnoticed.
        assert_eq!(back.by_aci(&Aci("aaaa".into())).unwrap(), &original);
        assert_eq!(back.by_user("robert").unwrap(), &original);
    }

    #[test]
    fn upsert_replaces_the_entry_for_that_account() {
        let mut s = State::default();
        s.upsert(entry("robert", "robert.42", "aaaa"));
        s.upsert(entry("robert", "robert.99", "bbbb"));
        assert_eq!(s.len(), 1);
        assert!(
            s.by_aci(&Aci("aaaa".into())).is_none(),
            "the old aci is gone"
        );
        assert_eq!(s.by_user("robert").unwrap().signal_username, "robert.99");
    }

    #[test]
    fn a_truncated_file_is_reported_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = State::load(&path).unwrap_err().to_string();
        assert!(err.contains("state.json"), "got: {err}");
    }

    #[test]
    fn saving_leaves_no_half_written_file_behind() {
        // Written to a sibling temp file and renamed. A crash mid-write would
        // otherwise leave a truncated state, and every greeting would be sent
        // a second time.
        //
        // This does not by itself distinguish an atomic write from a plain
        // one -- both leave the directory looking the same on success. It is
        // kept as a guard against stray temp files;
        // `a_failed_save_leaves_the_previous_state_intact` below is the test
        // that actually forces a failure and checks nothing was damaged.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = State::default();
        s.upsert(entry("a", "a.1", "x"));
        s.save(&path).unwrap();
        s.save(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "state.json")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn a_failed_save_leaves_the_previous_state_intact() {
        // This is what write-then-rename buys: a save that fails must not damage
        // what was already on disk. A direct write to the target would truncate it
        // before failing, and the next start would greet everybody a second time.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let mut first = State::default();
        first.upsert(entry("robert", "robert.42", "aaaa"));
        first.save(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Occupy the sibling name the atomic write needs, with a directory --
        // nothing can be written onto that.
        std::fs::create_dir(path.with_extension("json.new")).unwrap();

        let mut second = State::default();
        second.upsert(entry("konrad", "konrad.7", "bbbb"));
        assert!(
            second.save(&path).is_err(),
            "the save must fail rather than quietly succeed"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed save must leave the previous state untouched"
        );
    }

    #[test]
    fn signal_name_taken_by_finds_a_hit_case_insensitively_and_misses_cleanly() {
        // Signal treats usernames case-insensitively; a case-sensitive
        // comparison here would let a second account claim a name the first
        // already holds.
        let mut s = State::default();
        s.upsert(entry("robert", "robert.42", "aaaa"));

        assert_eq!(
            s.signal_name_taken_by("ROBERT.42")
                .unwrap()
                .authentik_username,
            "robert"
        );
        assert!(s.signal_name_taken_by("konrad.7").is_none());
    }

    #[test]
    fn remove_user_takes_the_entry_out_and_reports_absence() {
        let mut s = State::default();
        s.upsert(entry("robert", "robert.42", "aaaa"));

        let removed = s.remove_user("robert").unwrap();
        assert_eq!(removed.signal_username, "robert.42");
        assert!(s.by_user("robert").is_none());
        assert!(s.remove_user("robert").is_none());
    }

    #[test]
    fn iter_yields_every_entry() {
        let mut s = State::default();
        s.upsert(entry("robert", "robert.42", "aaaa"));
        s.upsert(entry("konrad", "konrad.7", "bbbb"));

        let mut names: Vec<_> = s.iter().map(|e| e.authentik_username.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["konrad", "robert"]);
    }
}
