use signal_seerr::dialog::Dialog;
use signal_seerr::directory::{Directory, Member};
use signal_seerr::i18n::{Catalogue, Locale};
use signal_seerr::model::*;
use signal_seerr::seerr::Requests;
use std::sync::Mutex;

pub struct FakeDirectory(pub Vec<(Aci, Member)>);
impl Directory for FakeDirectory {
    fn lookup(&self, aci: &Aci) -> Option<Member> {
        self.0
            .iter()
            .find(|(a, _)| a == aci)
            .map(|(_, m)| m.clone())
    }
}

#[derive(Default)]
pub struct FakeSeerr {
    pub hits: Vec<Hit>,
    pub placed: Mutex<Vec<(i64, Seasons, SeerrUserId)>>,
    pub fail: bool,
    /// Every query string `Dialog` actually handed to `search`, in order --
    /// so a test can check what reached Seerr, not just what came back.
    pub queries: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Requests for FakeSeerr {
    async fn search(
        &self,
        q: &str,
        kind: Option<MediaKind>,
        page: u32,
    ) -> anyhow::Result<Vec<Hit>> {
        self.queries.lock().unwrap().push(q.to_string());
        if self.fail {
            anyhow::bail!("seerr is down");
        }
        let matching: Vec<Hit> = self
            .hits
            .iter()
            .filter(|h| kind.is_none_or(|k| k == h.kind))
            .cloned()
            .collect();
        Ok(matching
            .chunks(5)
            .nth(page as usize - 1)
            .map(|c| c.to_vec())
            .unwrap_or_default())
    }
    async fn user_id(&self, _u: &str) -> anyhow::Result<Option<SeerrUserId>> {
        Ok(Some(SeerrUserId(12)))
    }
    async fn request(
        &self,
        hit: &Hit,
        seasons: Seasons,
        as_user: SeerrUserId,
    ) -> anyhow::Result<i64> {
        self.placed
            .lock()
            .unwrap()
            .push((hit.tmdb_id, seasons, as_user));
        Ok(1849)
    }
    async fn pending(&self, _as_user: SeerrUserId) -> anyhow::Result<Vec<Pending>> {
        Ok(vec![])
    }
    async fn withdraw(&self, _id: i64, _as_user: SeerrUserId) -> anyhow::Result<()> {
        Ok(())
    }
    // Not exercised by any test in this file -- Task 13's territory -- but
    // required by the trait, which already carries it (Task 9).
    async fn requester_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

pub fn member() -> Member {
    Member {
        authentik_username: "robert".into(),
        locale: Locale::De,
        allowed: true,
    }
}

pub fn movie(id: i64, title: &str) -> Hit {
    Hit {
        tmdb_id: id,
        kind: MediaKind::Movie,
        title: title.into(),
        year: Some(2017),
        rating: Some(8.0),
        seasons: 0,
        already: false,
    }
}

/// The `Dialog::new` fixture used across this file. `settings_url` and
/// `operator_name` are deployment configuration (see src/config.rs), not
/// translatable prose, so `Dialog::new` takes them directly; these are the
/// same neutral placeholders `config.example.toml` uses, now that this
/// repository is going public and does not ship its own domain in tests.
fn settings_url() -> String {
    "https://example.invalid".to_string()
}

fn operator_name() -> String {
    "the operator".to_string()
}

fn dialog(seerr: FakeSeerr, known: bool) -> Dialog<FakeSeerr, FakeDirectory> {
    let aci = Aci("aaaa".into());
    let dir = FakeDirectory(if known { vec![(aci, member())] } else { vec![] });
    Dialog::new(
        seerr,
        dir,
        Catalogue::load(),
        settings_url(),
        operator_name(),
    )
}

#[tokio::test]
async fn a_bare_title_searches() {
    let mut d = dialog(
        FakeSeerr {
            hits: vec![movie(1, "Blade Runner 2049")],
            ..Default::default()
        },
        true,
    );
    let out = d.handle(&Aci("aaaa".into()), "blade runner").await;
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("1. Blade Runner 2049"), "got: {}", out[0]);
    assert!(out[0].contains("2017"), "got: {}", out[0]);
}

#[tokio::test]
async fn an_unknown_sender_is_told_where_to_go_and_then_left_alone() {
    let mut d = dialog(FakeSeerr::default(), false);
    let first = d.handle(&Aci("zzzz".into()), "hallo").await;
    assert_eq!(first.len(), 1);
    assert!(first[0].contains("example.invalid"), "got: {}", first[0]);

    // A bot that answers every wrong number is an amplifier.
    let second = d.handle(&Aci("zzzz".into()), "hallo?").await;
    assert!(second.is_empty(), "answered a stranger twice: {second:?}");
}

#[tokio::test]
async fn a_known_account_without_the_media_group_gets_a_different_sentence() {
    let aci = Aci("aaaa".into());
    let dir = FakeDirectory(vec![(
        aci.clone(),
        Member {
            allowed: false,
            ..member()
        },
    )]);
    let mut d = Dialog::new(
        FakeSeerr::default(),
        dir,
        Catalogue::load(),
        settings_url(),
        operator_name(),
    );
    let out = d.handle(&aci, "blade runner").await;
    // The human is right, the group is missing. The other sentence would send
    // them looking for a fault in themselves.
    assert!(!out[0].contains("example.invalid"), "got: {}", out[0]);
    assert!(out[0].contains("Gruppe"), "got: {}", out[0]);
}

#[tokio::test]
async fn nothing_found_says_so() {
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "asdfghjkl").await;
    assert!(out[0].contains("nichts"), "got: {}", out[0]);
}

#[tokio::test]
async fn seerr_being_down_is_not_reported_as_nothing_found() {
    let mut d = dialog(
        FakeSeerr {
            fail: true,
            ..Default::default()
        },
        true,
    );
    let out = d.handle(&Aci("aaaa".into()), "blade runner").await;
    assert!(out[0].contains("Wunschliste"), "got: {}", out[0]);
}

#[tokio::test]
async fn a_hit_that_is_already_there_is_marked() {
    let hit = Hit {
        already: true,
        ..movie(1, "Blade Runner")
    };
    let mut d = dialog(
        FakeSeerr {
            hits: vec![hit],
            ..Default::default()
        },
        true,
    );
    let out = d.handle(&Aci("aaaa".into()), "blade").await;
    assert!(out[0].contains("schon da"), "got: {}", out[0]);
}

#[tokio::test]
async fn m_shows_the_next_page_and_then_says_there_is_no_more() {
    let hits: Vec<Hit> = (1..=7).map(|i| movie(i, &format!("Film {i}"))).collect();
    let mut d = dialog(
        FakeSeerr {
            hits,
            ..Default::default()
        },
        true,
    );
    let aci = Aci("aaaa".into());

    let first = d.handle(&aci, "film").await;
    assert!(
        first[0].contains("5. Film 5"),
        "five per page: {}",
        first[0]
    );

    let second = d.handle(&aci, "m").await;
    assert!(second[0].contains("Film 6"), "got: {}", second[0]);
    assert!(
        second[0].contains("1. Film 6"),
        "numbering restarts per page: {}",
        second[0]
    );

    let third = d.handle(&aci, "m").await;
    assert!(
        third[0].contains("Mehr habe ich nicht"),
        "got: {}",
        third[0]
    );
}

#[tokio::test]
async fn film_and_serie_narrow_the_search() {
    let hits = vec![
        movie(1, "Andor the Movie"),
        Hit {
            kind: MediaKind::Tv,
            seasons: 2,
            ..movie(2, "Andor")
        },
    ];
    let mut d = dialog(
        FakeSeerr {
            hits,
            ..Default::default()
        },
        true,
    );
    let aci = Aci("aaaa".into());

    let only_films = d.handle(&aci, "/film andor").await;
    assert!(only_films[0].contains("Andor the Movie"));
    assert!(
        !only_films[0].contains("1. Andor\n"),
        "the series leaked in"
    );

    let only_series = d.handle(&aci, "/serie andor").await;
    assert!(only_series[0].contains("Andor"));
    assert!(
        !only_series[0].contains("Andor the Movie"),
        "the film leaked in"
    );
}

#[tokio::test]
async fn a_film_command_reaches_seerr_with_the_original_case_preserved() {
    let mut d = dialog(FakeSeerr::default(), true);
    d.handle(&Aci("aaaa".into()), "/film Blade Runner 2049")
        .await;
    assert_eq!(
        d.seerr_ref().queries.lock().unwrap().as_slice(),
        ["Blade Runner 2049"],
        "the prefix must be stripped, not the whole query lowercased"
    );
}

#[tokio::test]
async fn a_bare_title_reaches_seerr_unchanged() {
    let mut d = dialog(FakeSeerr::default(), true);
    d.handle(&Aci("aaaa".into()), "Blade Runner").await;
    assert_eq!(
        d.seerr_ref().queries.lock().unwrap().as_slice(),
        ["Blade Runner"]
    );
}

#[tokio::test]
async fn a_query_that_changes_byte_length_when_lowercased_does_not_panic() {
    // 'ẞ' (U+1E9E, capital sharp S) is 3 bytes in UTF-8 and lowercases to 'ß'
    // (U+00DF), which is 2. Slicing the original message by a length derived
    // from the *lowercased* copy -- `text[text.len() - rest.len()..]` -- can
    // therefore land off a UTF-8 character boundary and panic, or (for a
    // character whose lowercasing grows, such as 'İ') underflow the
    // subtraction and panic that way instead. Either way this must not crash
    // message handling from a single crafted message.
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "/film ẞ").await;
    assert_eq!(
        d.seerr_ref().queries.lock().unwrap().as_slice(),
        ["ẞ"],
        "the original character must reach Seerr unchanged, not the lowercased one"
    );
    assert!(!out.is_empty(), "must still answer");
}

#[tokio::test]
async fn m_with_no_open_list_says_it_does_not_understand() {
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "m").await;
    let expected = Catalogue::load().text(Locale::De, "error.not_understood", &[]);
    assert_eq!(out[0], expected);
}

#[tokio::test]
async fn seerr_ref_returns_the_backend_handed_to_new() {
    let seerr = FakeSeerr {
        fail: true,
        ..Default::default()
    };
    let d = dialog(seerr, true);
    assert!(
        d.seerr_ref().fail,
        "seerr_ref must expose the same backend new() was given, not a copy or a default"
    );
}

#[tokio::test]
async fn each_stranger_is_told_once_independently() {
    let mut d = dialog(FakeSeerr::default(), false);
    let a = Aci("stranger-a".into());
    let b = Aci("stranger-b".into());

    let first_a = d.handle(&a, "hi").await;
    let first_b = d.handle(&b, "hi").await;
    assert_eq!(first_a.len(), 1, "a must be told");
    assert_eq!(first_b.len(), 1, "a different stranger must be told too");

    let second_a = d.handle(&a, "hi again").await;
    let second_b = d.handle(&b, "hi again").await;
    assert!(second_a.is_empty(), "a was just told");
    assert!(second_b.is_empty(), "b was just told");
}

/// What `FakeSeerr::request` actually recorded -- a free function, not an
/// inherent `impl Dialog<..>` in this crate (that is only legal in the crate
/// that defines `Dialog`).
fn placed(d: &Dialog<FakeSeerr, FakeDirectory>) -> Vec<(i64, Seasons, SeerrUserId)> {
    d.seerr_ref().placed.lock().unwrap().clone()
}

#[tokio::test]
async fn a_digit_places_the_request_in_the_asker_s_name() {
    let seerr = FakeSeerr {
        hits: vec![movie(335984, "Blade Runner 2049")],
        ..Default::default()
    };
    let mut d = dialog(seerr, true);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    let out = d.handle(&aci, "1").await;

    assert!(out[0].contains("eingetragen"), "got: {}", out[0]);
    assert!(
        out[0].contains("1849"),
        "the withdrawal number must be named: {}",
        out[0]
    );
}

#[tokio::test]
async fn a_digit_outside_the_list_is_not_a_request() {
    let seerr = FakeSeerr {
        hits: vec![movie(1, "A")],
        ..Default::default()
    };
    let mut d = dialog(seerr, true);
    let aci = Aci("aaaa".into());
    d.handle(&aci, "a").await;
    let out = d.handle(&aci, "4").await;
    assert!(out[0].contains("nichts anfangen"), "got: {}", out[0]);
}

#[tokio::test]
async fn a_digit_with_no_open_list_is_a_search() {
    // Ten minutes on, "2" means a film called 2 again -- not entry two of a
    // list nobody can still see.
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "2").await;
    assert!(
        out[0].contains("nichts"),
        "expected a search, got: {}",
        out[0]
    );
}

#[tokio::test]
async fn a_series_is_asked_about_its_seasons_before_anything_is_placed() {
    let series = Hit {
        kind: MediaKind::Tv,
        seasons: 2,
        ..movie(4321, "Andor")
    };
    let seerr = FakeSeerr {
        hits: vec![series],
        ..Default::default()
    };
    let mut d = dialog(seerr, true);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "andor").await;
    let asked = d.handle(&aci, "1").await;
    assert!(asked[0].contains("Staffeln"), "got: {}", asked[0]);

    let done = d.handle(&aci, "alle").await;
    assert!(done[0].contains("eingetragen"), "got: {}", done[0]);
}

#[tokio::test]
async fn named_seasons_are_passed_through() {
    let series = Hit {
        kind: MediaKind::Tv,
        seasons: 3,
        ..movie(4321, "Andor")
    };
    let seerr = FakeSeerr {
        hits: vec![series],
        ..Default::default()
    };
    let mut d = dialog(seerr, true);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "andor").await;
    d.handle(&aci, "1").await;
    d.handle(&aci, "1 3").await;

    let list = placed(&d);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].1, Seasons::Only(vec![1, 3]));
    assert_eq!(
        list[0].2,
        SeerrUserId(12),
        "placed as the asker, not as the key owner"
    );
}

#[tokio::test]
async fn a_season_number_that_does_not_exist_is_refused() {
    let series = Hit {
        kind: MediaKind::Tv,
        seasons: 2,
        ..movie(4321, "Andor")
    };
    let seerr = FakeSeerr {
        hits: vec![series],
        ..Default::default()
    };
    let mut d = dialog(seerr, true);
    let aci = Aci("aaaa".into());
    d.handle(&aci, "andor").await;
    d.handle(&aci, "1").await;
    let out = d.handle(&aci, "1 9").await;
    assert!(out[0].contains("Staffeln"), "asked again, got: {}", out[0]);
    assert!(placed(&d).is_empty(), "placed a season that does not exist");
}
