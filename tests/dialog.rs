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
    /// What `quality_profiles` answers. Empty by default, so every test
    /// written before the profile question behaves as it always did.
    pub profiles: Vec<QualityProfile>,
    /// The `profileId` that reached `request` -- the point of the whole
    /// question, and the thing a test has to be able to look at.
    pub asked_profile: Mutex<Option<i64>>,
    /// What `profile_of` reads back afterwards.
    pub readback: Option<i64>,
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
    async fn quality_profiles(&self, _kind: MediaKind) -> anyhow::Result<Vec<QualityProfile>> {
        Ok(self.profiles.clone())
    }
    async fn profile_of(&self, _request_id: i64) -> anyhow::Result<Option<i64>> {
        Ok(self.readback)
    }
    async fn request(
        &self,
        hit: &Hit,
        seasons: Seasons,
        as_user: SeerrUserId,
        profile_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        *self.asked_profile.lock().unwrap() = profile_id;
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
        Vec::new(),
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
        Vec::new(),
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

#[tokio::test]
async fn status_lists_what_is_still_on_its_way() {
    struct WithPending;
    #[async_trait::async_trait]
    impl Requests for WithPending {
        async fn search(
            &self,
            _q: &str,
            _k: Option<MediaKind>,
            _p: u32,
        ) -> anyhow::Result<Vec<Hit>> {
            Ok(vec![])
        }
        async fn user_id(&self, _u: &str) -> anyhow::Result<Option<SeerrUserId>> {
            Ok(Some(SeerrUserId(12)))
        }
        async fn quality_profiles(&self, _kind: MediaKind) -> anyhow::Result<Vec<QualityProfile>> {
            Ok(vec![])
        }
        async fn profile_of(&self, _request_id: i64) -> anyhow::Result<Option<i64>> {
            Ok(None)
        }
        async fn request(
            &self,
            _h: &Hit,
            _s: Seasons,
            _u: SeerrUserId,
            _p: Option<i64>,
        ) -> anyhow::Result<i64> {
            Ok(1)
        }
        async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Pending>> {
            Ok(vec![Pending {
                id: 1849,
                title: "Blade Runner 2049".into(),
                state: PendingState::Fetching,
            }])
        }
        async fn withdraw(&self, _i: i64, _u: SeerrUserId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn requester_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
    }
    let aci = Aci("aaaa".into());
    let dir = FakeDirectory(vec![(aci.clone(), member())]);
    let mut d = Dialog::new(
        WithPending,
        dir,
        Catalogue::load(),
        settings_url(),
        operator_name(),
        Vec::new(),
    );

    let out = d.handle(&aci, "/status").await;
    assert!(out[0].contains("1849"), "got: {}", out[0]);
    assert!(out[0].contains("Blade Runner 2049"), "got: {}", out[0]);
}

#[tokio::test]
async fn status_says_so_when_there_is_nothing() {
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "/status").await;
    assert!(out[0].contains("nichts unterwegs"), "got: {}", out[0]);
}

#[tokio::test]
async fn weg_withdraws_and_confirms() {
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "/weg 1849").await;
    assert!(out[0].contains("zurückgenommen"), "got: {}", out[0]);
}

#[tokio::test]
async fn weg_without_a_number_does_not_reach_seerr() {
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "/weg").await;
    assert!(out[0].contains("nichts anfangen"), "got: {}", out[0]);
}

#[tokio::test]
async fn weg_on_somebody_elses_request_reports_it_as_not_yours() {
    struct Refuses;
    #[async_trait::async_trait]
    impl Requests for Refuses {
        async fn search(
            &self,
            _q: &str,
            _k: Option<MediaKind>,
            _p: u32,
        ) -> anyhow::Result<Vec<Hit>> {
            Ok(vec![])
        }
        async fn user_id(&self, _u: &str) -> anyhow::Result<Option<SeerrUserId>> {
            Ok(Some(SeerrUserId(12)))
        }
        async fn quality_profiles(&self, _kind: MediaKind) -> anyhow::Result<Vec<QualityProfile>> {
            Ok(vec![])
        }
        async fn profile_of(&self, _request_id: i64) -> anyhow::Result<Option<i64>> {
            Ok(None)
        }
        async fn request(
            &self,
            _h: &Hit,
            _s: Seasons,
            _u: SeerrUserId,
            _p: Option<i64>,
        ) -> anyhow::Result<i64> {
            Ok(1)
        }
        async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Pending>> {
            Ok(vec![])
        }
        async fn withdraw(&self, id: i64, _u: SeerrUserId) -> anyhow::Result<()> {
            anyhow::bail!("request {id} is not yours")
        }
        async fn requester_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
    }
    let aci = Aci("aaaa".into());
    let dir = FakeDirectory(vec![(aci.clone(), member())]);
    let mut d = Dialog::new(
        Refuses,
        dir,
        Catalogue::load(),
        settings_url(),
        operator_name(),
        Vec::new(),
    );
    let out = d.handle(&aci, "/weg 1849").await;
    assert!(
        out[0].contains("1849"),
        "the number must be named back: {}",
        out[0]
    );
    assert!(
        !out[0].contains("zurückgenommen"),
        "claimed success: {}",
        out[0]
    );
}

#[tokio::test]
async fn help_is_the_same_text_as_the_greeting() {
    // One source. A second wording drifts, and the greeting is the one nobody
    // re-reads.
    let mut d = dialog(FakeSeerr::default(), true);
    let out = d.handle(&Aci("aaaa".into()), "/help").await;
    let catalogue = Catalogue::load();
    assert_eq!(out[0], catalogue.text(Locale::De, "help.body", &[]));
}

/// Sets up a `Dialog` mid-seasons-question: a series was picked and is now
/// waiting on an answer.
async fn dialog_mid_seasons_question(seasons: u16) -> (Dialog<FakeSeerr, FakeDirectory>, Aci) {
    let series = Hit {
        kind: MediaKind::Tv,
        seasons,
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
    (d, aci)
}

#[tokio::test]
async fn status_mid_seasons_question_re_asks_instead_of_listing() {
    let (mut d, aci) = dialog_mid_seasons_question(2).await;
    let out = d.handle(&aci, "/status").await;
    assert!(
        out[0].contains("Staffeln"),
        "must re-ask, not list: {}",
        out[0]
    );
}

#[tokio::test]
async fn abbruch_mid_seasons_question_clears_it_and_a_following_digit_is_a_fresh_search() {
    let (mut d, aci) = dialog_mid_seasons_question(2).await;
    let cancelled = d.handle(&aci, "/abbruch").await;
    assert!(cancelled.is_empty(), "got: {cancelled:?}");

    d.handle(&aci, "3").await;
    // A seasons answer never reaches `seerr.search` -- only a fresh search
    // does. Finding "3" in the recorded queries is proof the digit was
    // *not* read as an answer to the (supposedly cleared) seasons question.
    assert_eq!(
        d.seerr_ref().queries.lock().unwrap().as_slice(),
        ["andor", "3"],
        "the digit must reach seerr as a fresh search, not be read as a season"
    );
    assert!(
        placed(&d).is_empty(),
        "must not have placed anything for '3'"
    );
}

#[tokio::test]
async fn hilfe_mid_seasons_question_answers_and_leaves_the_question_open() {
    let (mut d, aci) = dialog_mid_seasons_question(3).await;
    let help = d.handle(&aci, "/hilfe").await;
    let catalogue = Catalogue::load();
    assert_eq!(help[0], catalogue.text(Locale::De, "help.body", &[]));

    // The question must still be open: an answer now completes the request.
    let done = d.handle(&aci, "alle").await;
    assert!(
        done[0].contains("eingetragen"),
        "the seasons question was lost: {}",
        done[0]
    );
}

#[tokio::test]
async fn season_zero_is_refused() {
    let (mut d, aci) = dialog_mid_seasons_question(2).await;
    let out = d.handle(&aci, "0").await;
    assert!(out[0].contains("Staffeln"), "asked again, got: {}", out[0]);
    assert!(placed(&d).is_empty());
}

#[tokio::test]
async fn a_season_number_equal_to_the_count_is_accepted() {
    let (mut d, aci) = dialog_mid_seasons_question(2).await;
    let out = d.handle(&aci, "2").await;
    assert!(out[0].contains("eingetragen"), "got: {}", out[0]);
    let list = placed(&d);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].1, Seasons::Only(vec![2]));
}

#[tokio::test]
async fn an_empty_seasons_answer_is_refused() {
    let (mut d, aci) = dialog_mid_seasons_question(2).await;
    let out = d.handle(&aci, "   ").await;
    assert!(out[0].contains("Staffeln"), "asked again, got: {}", out[0]);
    assert!(placed(&d).is_empty());
}

#[tokio::test]
async fn a_non_numeric_seasons_answer_is_refused() {
    let (mut d, aci) = dialog_mid_seasons_question(2).await;
    let out = d.handle(&aci, "eins").await;
    assert!(out[0].contains("Staffeln"), "asked again, got: {}", out[0]);
    assert!(placed(&d).is_empty());
}

// ===========================================================================
// Die Profilfrage (Entwurf §4.2 / §4.3)
// ===========================================================================

/// Two profiles under names that differ from the ids, so a mix-up shows.
fn zwei_profile() -> Vec<QualityProfile> {
    vec![
        QualityProfile {
            id: 7,
            name: "Dual Language, sonst Deutsch (1080p)".into(),
        },
        QualityProfile {
            id: 11,
            name: "Rarität, Originalsprache (auch SD)".into(),
        },
    ]
}

fn seerr_mit_profilen() -> FakeSeerr {
    FakeSeerr {
        hits: vec![movie(1, "Blade Runner 2049")],
        profiles: zwei_profile(),
        ..Default::default()
    }
}

/// Choosing a film no longer places the request straight away: the profile
/// question comes first, and NOTHING has reached Seerr until it is answered.
/// A request placed before the person picked would make the question a lie.
#[tokio::test]
async fn choosing_a_film_asks_for_the_profile_before_placing_anything() {
    let mut d = dialog(seerr_mit_profilen(), true);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    let answer = d.handle(&aci, "1").await.join("\n");

    assert!(
        answer.contains("Dual Language, sonst Deutsch (1080p)"),
        "the question must list the profiles, got: {answer}"
    );
    assert!(answer.contains("Rarität, Originalsprache (auch SD)"));
    assert!(
        d.seerr_ref().placed.lock().unwrap().is_empty(),
        "nothing may be requested before the profile is chosen"
    );
}

/// The digit picks by POSITION in the configured list, and what travels is
/// that entry's id. The two differ on purpose here (position 2 -> id 11): a
/// bot that sent the position would look right for the first entry alone.
#[tokio::test]
async fn the_answer_sends_that_profiles_id_not_its_position() {
    let mut d = dialog(seerr_mit_profilen(), true);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    d.handle(&aci, "1").await;
    d.handle(&aci, "2").await;

    assert_eq!(
        *d.seerr_ref().asked_profile.lock().unwrap(),
        Some(11),
        "position 2 is the profile with id 11"
    );
    assert_eq!(d.seerr_ref().placed.lock().unwrap().len(), 1);
}

/// With no profiles to offer -- none configured, or Seerr unreachable -- the
/// request still goes out, carrying no profileId, exactly as it did before
/// this question existed. A wish must never fail because a question could
/// not be asked.
#[tokio::test]
async fn without_profiles_the_request_goes_out_unasked() {
    let mut d = dialog(
        FakeSeerr {
            hits: vec![movie(1, "Blade Runner 2049")],
            profiles: vec![],
            ..Default::default()
        },
        true,
    );
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    let answer = d.handle(&aci, "1").await.join("\n");

    assert_eq!(d.seerr_ref().placed.lock().unwrap().len(), 1);
    assert_eq!(*d.seerr_ref().asked_profile.lock().unwrap(), None);
    assert!(
        answer.contains("Blade Runner 2049"),
        "the confirmation, not a question: {answer}"
    );
}

/// A series asks for seasons FIRST and the profile second (§4.3): somebody
/// who mistyped the title should notice it at the seasons question, before
/// answering a second one -- and the season choice is the one nobody can
/// make without the title in mind.
#[tokio::test]
async fn a_series_is_asked_for_seasons_first_then_the_profile() {
    let series = Hit {
        tmdb_id: 2,
        kind: MediaKind::Tv,
        title: "Andor".into(),
        year: Some(2022),
        rating: Some(8.4),
        seasons: 2,
        already: false,
    };
    let mut d = dialog(
        FakeSeerr {
            hits: vec![series],
            profiles: zwei_profile(),
            ..Default::default()
        },
        true,
    );
    let aci = Aci("aaaa".into());

    d.handle(&aci, "andor").await;
    let nach_wahl = d.handle(&aci, "1").await.join("\n");
    assert!(
        nach_wahl.contains("Staffeln"),
        "seasons come first: {nach_wahl}"
    );

    let nach_staffeln = d.handle(&aci, "alle").await.join("\n");
    assert!(
        nach_staffeln.contains("Rarität, Originalsprache (auch SD)"),
        "and the profile question follows: {nach_staffeln}"
    );
    assert!(
        d.seerr_ref().placed.lock().unwrap().is_empty(),
        "still nothing placed"
    );

    d.handle(&aci, "1").await;
    assert_eq!(*d.seerr_ref().asked_profile.lock().unwrap(), Some(7));
    assert_eq!(d.seerr_ref().placed.lock().unwrap().len(), 1);
}

/// /abbruch and /hilfe are the two ways OUT of any open question (§4.3), and
/// the profile question must not swallow them either: answering "which
/// profile?" to somebody who typed /abbruch traps them in the question they
/// are trying to leave.
#[tokio::test]
async fn cancel_and_help_still_get_through_an_open_profile_question() {
    let mut d = dialog(seerr_mit_profilen(), true);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    d.handle(&aci, "1").await;

    let hilfe = d.handle(&aci, "/hilfe").await.join("\n");
    assert!(hilfe.contains("/abbruch"), "help, not the question again");

    // /hilfe leaves the question OPEN -- asking what you can type must not
    // cost you your place.
    let weiter = d.handle(&aci, "1").await;
    assert_eq!(d.seerr_ref().placed.lock().unwrap().len(), 1, "{weiter:?}");

    // And /abbruch clears it.
    d.handle(&aci, "blade runner").await;
    d.handle(&aci, "1").await;
    let abbruch = d.handle(&aci, "/abbruch").await;
    assert!(abbruch.is_empty(), "cancel says nothing and clears");
}

/// An answer that is not one of the offered numbers repeats the question
/// rather than falling through to a search for "9".
#[tokio::test]
async fn a_number_outside_the_list_repeats_the_profile_question() {
    let mut d = dialog(seerr_mit_profilen(), true);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    d.handle(&aci, "1").await;
    let nochmal = d.handle(&aci, "9").await.join("\n");

    assert!(nochmal.contains("Rarität, Originalsprache (auch SD)"));
    assert!(d.seerr_ref().placed.lock().unwrap().is_empty());
}

/// What the bot reports is what Seerr says the request CARRIES, read back
/// after placing it -- not the bot's own intention. An `OverrideRule` can
/// replace a sent `profileId` silently (`MediaRequest.js:259-263`); there
/// are none today, and that is a measurement, not a property.
#[tokio::test]
async fn the_confirmation_names_the_profile_seerr_actually_kept() {
    let mut d = dialog(
        FakeSeerr {
            hits: vec![movie(1, "Blade Runner 2049")],
            profiles: zwei_profile(),
            // The bot asks for 7; Seerr answers that it kept 11.
            readback: Some(11),
            ..Default::default()
        },
        true,
    );
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    d.handle(&aci, "1").await;
    let bestaetigung = d.handle(&aci, "1").await.join("\n");

    assert_eq!(*d.seerr_ref().asked_profile.lock().unwrap(), Some(7));
    assert!(
        bestaetigung.contains("Rarität, Originalsprache (auch SD)"),
        "the confirmation must name what Seerr kept (11), not what we sent (7): {bestaetigung}"
    );
}

/// The configured order wins over Seerr's, and a configured name no *arr
/// knows is dropped rather than renumbering the list under the people who
/// learned it.
///
/// Seerr here lists the profiles in one order; the configuration asks for
/// the reverse, plus one name that does not exist. What the person sees must
/// be the configured order, without the phantom.
#[tokio::test]
async fn the_configured_order_wins_over_seerrs_own() {
    let aci = Aci("aaaa".into());
    let mut d = Dialog::new(
        FakeSeerr {
            hits: vec![movie(1, "Blade Runner 2049")],
            profiles: zwei_profile(),
            ..Default::default()
        },
        FakeDirectory(vec![(aci.clone(), member())]),
        Catalogue::load(),
        settings_url(),
        operator_name(),
        vec![
            "Rarität, Originalsprache (auch SD)".to_string(),
            "Gibt es nicht".to_string(),
            "Dual Language, sonst Deutsch (1080p)".to_string(),
        ],
    );

    d.handle(&aci, "blade runner").await;
    let frage = d.handle(&aci, "1").await.join("\n");

    let raritaet = frage.find("Rarität").expect("die Rarität fehlt");
    let dual = frage.find("Dual Language").expect("Dual Language fehlt");
    assert!(
        raritaet < dual,
        "die konfigurierte Reihenfolge gilt, nicht Seerrs: {frage}"
    );
    assert!(
        !frage.contains("Gibt es nicht"),
        "ein Name, den kein *arr kennt, steht nicht in der Liste: {frage}"
    );

    // Und "1" ist jetzt die Rarität -- id 11, nicht 7.
    d.handle(&aci, "1").await;
    assert_eq!(*d.seerr_ref().asked_profile.lock().unwrap(), Some(11));
}
