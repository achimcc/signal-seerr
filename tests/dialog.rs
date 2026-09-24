use signal_seerr::arr::{ArrEpisode, ArrMovie, ArrSeries, HistoryEvent, Insight, QueueItem};
use signal_seerr::dialog::Dialog;
use signal_seerr::directory::{Directory, Member};
use signal_seerr::i18n::{Catalogue, Locale};
use signal_seerr::model::*;
use signal_seerr::notices::Notices;
use signal_seerr::seerr::Requests;
use std::sync::{Arc, Mutex, RwLock};

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
    /// What `pending` answers.
    pub wishes: Vec<Wish>,
    /// Every `SeerrUserId` `pending` was asked with, in order. Identity is
    /// the point: a status answer must only ever be about the sender.
    pub pending_calls: Mutex<Vec<SeerrUserId>>,
    /// What `title_for` answers, and how often it was asked at all -- a
    /// title that was already remembered must not cost a lookup.
    pub title: Option<String>,
    pub title_for_calls: Mutex<usize>,
    /// Authentik username -> Seerr id. Empty means "everybody is 12", so
    /// every test written before this behaves as it always did.
    pub users: Vec<(String, i64)>,
}

#[async_trait::async_trait]
impl Requests for FakeSeerr {
    async fn retry(&self, _request_id: i64) -> anyhow::Result<()> {
        unreachable!("retry is the watcher's alone")
    }
    async fn title_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

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
    async fn user_id(&self, u: &str) -> anyhow::Result<Option<SeerrUserId>> {
        let id = self
            .users
            .iter()
            .find(|(name, _)| name == u)
            .map(|(_, id)| *id)
            .unwrap_or(12);
        Ok(Some(SeerrUserId(id)))
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
    async fn pending(&self, as_user: SeerrUserId) -> anyhow::Result<Vec<Wish>> {
        self.pending_calls.lock().unwrap().push(as_user);
        Ok(self.wishes.clone())
    }
    async fn open_wishes(&self) -> anyhow::Result<Vec<Wish>> {
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
    async fn title_for(&self, _kind: MediaKind, _tmdb_id: i64) -> anyhow::Result<Option<String>> {
        *self.title_for_calls.lock().unwrap() += 1;
        Ok(self.title.clone())
    }
}

/// A read-only Radarr/Sonarr stand-in. Note what it does NOT implement:
/// `ReleaseSearch`. `Dialog` is only ever handed `Insight`, so by type no
/// chat command can set off a search at every indexer -- a fake that offered
/// both would quietly give that property away.
#[derive(Default)]
pub struct FakeInsight {
    /// arr id -> what Radarr says about that film.
    pub movies: Vec<(i64, ArrMovie)>,
    pub queue: Vec<QueueItem>,
    pub event: Option<HistoryEvent>,
    /// Every call fails. An extra source being down must never cost a wish
    /// or a status answer.
    pub fail: bool,
    pub movie_calls: Mutex<Vec<i64>>,
    pub queue_calls: Mutex<Vec<MediaKind>>,
    /// arr id -> what Sonarr says about that series.
    pub series: Vec<(i64, ArrSeries)>,
    pub series_calls: Mutex<Vec<i64>>,
}

#[async_trait::async_trait]
impl Insight for FakeInsight {
    async fn movie(&self, id: i64) -> anyhow::Result<ArrMovie> {
        self.movie_calls.lock().unwrap().push(id);
        if self.fail {
            anyhow::bail!("radarr is down");
        }
        self.movies
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, movie)| movie.clone())
            .ok_or_else(|| anyhow::anyhow!("radarr knows no movie {id}"))
    }
    async fn series(&self, id: i64) -> anyhow::Result<ArrSeries> {
        self.series_calls.lock().unwrap().push(id);
        if self.fail {
            anyhow::bail!("sonarr is down");
        }
        self.series
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, series)| series.clone())
            .ok_or_else(|| anyhow::anyhow!("sonarr knows no series {id}"))
    }
    async fn queue(&self, kind: MediaKind) -> anyhow::Result<Vec<QueueItem>> {
        self.queue_calls.lock().unwrap().push(kind);
        if self.fail {
            anyhow::bail!("radarr is down");
        }
        Ok(self.queue.clone())
    }
    async fn last_event(&self, _kind: MediaKind, _id: i64) -> anyhow::Result<Option<HistoryEvent>> {
        if self.fail {
            anyhow::bail!("radarr is down");
        }
        Ok(self.event)
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
        None,
        None,
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
        None,
        None,
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
        async fn retry(&self, _request_id: i64) -> anyhow::Result<()> {
            unreachable!("retry is the watcher's alone")
        }
        async fn title_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
            Ok(None)
        }

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
        async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Wish>> {
            Ok(vec![Wish {
                id: 1849,
                kind: MediaKind::Movie,
                tmdb_id: 4321,
                request_status: 1,
                media_status: 3,
                arr_id: None,
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                profile_name: None,
                requested_by: None,
                download_percent: None,
                seasons: Vec::new(),
            }])
        }
        async fn open_wishes(&self) -> anyhow::Result<Vec<Wish>> {
            Ok(vec![])
        }
        async fn withdraw(&self, _i: i64, _u: SeerrUserId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn requester_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn title_for(
            &self,
            _kind: MediaKind,
            _tmdb_id: i64,
        ) -> anyhow::Result<Option<String>> {
            Ok(Some("Blade Runner 2049".into()))
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
        None,
        None,
    );

    let out = d.handle(&aci, "/status").await;
    assert!(out[0].contains("1849"), "got: {}", out[0]);
    assert!(out[0].contains("Blade Runner 2049"), "got: {}", out[0]);
    // The wish above is the ordinary fresh one: request_status 1 (pending)
    // and no `externalServiceId` yet, because Seerr writes that on the
    // hand-over a moment later. It is WAITING, not failed -- this used to
    // answer "konnte ich nicht eintragen", which reads as "your wish is
    // gone" to somebody who asked for the film seconds ago.
    assert!(out[0].contains("wartet"), "got: {}", out[0]);
    assert!(
        !out[0].contains("nicht eintragen"),
        "a wish on its way must not read as a failure: {}",
        out[0]
    );
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
        async fn retry(&self, _request_id: i64) -> anyhow::Result<()> {
            unreachable!("retry is the watcher's alone")
        }
        async fn title_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
            Ok(None)
        }

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
        async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Wish>> {
            Ok(vec![])
        }
        async fn open_wishes(&self) -> anyhow::Result<Vec<Wish>> {
            Ok(vec![])
        }
        async fn title_for(
            &self,
            _kind: MediaKind,
            _tmdb_id: i64,
        ) -> anyhow::Result<Option<String>> {
            Ok(None)
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
        None,
        None,
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
        None,
        None,
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

// ===========================================================================
// /status tells the truth (Task 7)
//
// Every expected sentence is read OUT OF THE CATALOGUE rather than written
// out here: these are the texts a wording change is most likely to touch,
// and a test that pins the literal sentence is a trap for that change (see
// CLAUDE.md). What is under test is which STATE a wish is rendered as, not
// how that state happens to read this week.
// ===========================================================================

/// The fixed "now" every status test is driven with, so a wish's age is a
/// property of the test and not of the day it runs on.
fn now() -> time::OffsetDateTime {
    time::macros::datetime!(2026-09-21 12:00:00 UTC)
}

/// An open film wish: handed over to Radarr (`arr_id`), approved, not yet
/// available.
fn wish(id: i64, arr_id: Option<i64>) -> Wish {
    Wish {
        id,
        kind: MediaKind::Movie,
        tmdb_id: 4321,
        request_status: 2,
        media_status: 3,
        arr_id,
        created_at: now(),
        profile_name: None,
        requested_by: None,
        download_percent: None,
        seasons: Vec::new(),
    }
}

fn arr_movie(is_available: bool) -> ArrMovie {
    ArrMovie {
        is_available,
        has_file: false,
        digital_release: None,
        physical_release: None,
    }
}

fn silvia() -> Member {
    Member {
        authentik_username: "silvia".into(),
        ..member()
    }
}

/// A dialog that knows `aaaa` (robert) and `eeee` (silvia), with the two
/// optional neighbours this task adds and a fixed clock.
fn status_dialog(
    seerr: FakeSeerr,
    insight: Option<Arc<dyn Insight>>,
    notices: Option<Notices>,
) -> Dialog<FakeSeerr, FakeDirectory> {
    let dir = FakeDirectory(vec![
        (Aci("aaaa".into()), member()),
        (Aci("eeee".into()), silvia()),
    ]);
    Dialog::new(
        seerr,
        dir,
        Catalogue::load(),
        settings_url(),
        operator_name(),
        Vec::new(),
        insight,
        notices.map(|n| Arc::new(RwLock::new(n))),
    )
    .with_clock(now())
}

/// (a) A film that is not out yet is said to be not out yet -- where the bot
/// used to answer "wird geholt" for every open wish alike, including one
/// nobody could fetch at all.
///
/// Two open wishes, and the queue is read ONCE for both: the queue is a
/// property of the whole *arr, not of a wish, and asking per wish would turn
/// a five-line status into five round trips.
#[tokio::test]
async fn status_names_a_film_that_is_not_released_yet() {
    let seerr = FakeSeerr {
        wishes: vec![wish(1849, Some(42)), wish(1850, Some(43))],
        title: Some("Blade Runner 2049".into()),
        ..Default::default()
    };
    let insight = Arc::new(FakeInsight {
        movies: vec![(42, arr_movie(false)), (43, arr_movie(false))],
        ..Default::default()
    });
    let mut d = status_dialog(seerr, Some(insight.clone()), None);

    let out = d.handle(&Aci("aaaa".into()), "/status").await.join("\n");

    let catalogue = Catalogue::load();
    assert!(
        out.contains(&catalogue.text(Locale::De, "status.not_released", &[])),
        "got: {out}"
    );
    // The sentence status.fetching used to hold, now gone from both
    // catalogues. Written out because the key it came from no longer exists.
    assert!(
        !out.contains("wird geholt"),
        "the old blanket answer must be gone: {out}"
    );
    assert_eq!(
        insight.queue_calls.lock().unwrap().len(),
        1,
        "the queue is read once per /status, not once per wish"
    );
}

/// (b) Identity: a status answer is only ever about the sender, and a number
/// after the command does not turn it into a lookup of somebody else's wish.
#[tokio::test]
async fn status_only_ever_asks_about_the_senders_own_wishes() {
    let seerr = FakeSeerr {
        wishes: vec![wish(24, Some(5))],
        title: Some("Arrival".into()),
        users: vec![("robert".into(), 12), ("silvia".into(), 77)],
        ..Default::default()
    };
    let insight = Arc::new(FakeInsight {
        movies: vec![(5, arr_movie(true))],
        ..Default::default()
    });
    let mut d = status_dialog(seerr, Some(insight.clone()), None);
    let b = Aci("eeee".into());

    d.handle(&b, "/status").await;
    assert_eq!(
        d.seerr_ref().pending_calls.lock().unwrap().as_slice(),
        [SeerrUserId(77)],
        "B's own Seerr id, and nobody else's"
    );

    // 24 is a wish id, not B's. Today "/status 24" is not the /status
    // command at all (the match is on the whole message) and ends up a
    // search for that text -- whatever it is treated as, it must not become
    // a lookup: no second `pending`, and no question to Radarr about 24.
    d.handle(&b, "/status 24").await;
    assert_eq!(
        d.seerr_ref().pending_calls.lock().unwrap().as_slice(),
        [SeerrUserId(77)],
        "a number after /status must not fetch anybody's wish list again"
    );
    assert_eq!(
        insight.movie_calls.lock().unwrap().as_slice(),
        [5],
        "only the arr id of B's own wish was ever asked about"
    );
}

/// (c) What is listed: everything still open, plus what became available
/// within the last seven days -- a wish that arrived a month ago is not news.
#[tokio::test]
async fn status_lists_open_wishes_and_only_recently_available_ones() {
    let old = time::Duration::days(30);
    let recent = time::Duration::days(1);
    let seerr = FakeSeerr {
        wishes: vec![
            Wish {
                created_at: now() - old,
                ..wish(1, None)
            },
            Wish {
                media_status: 5,
                created_at: now() - old,
                ..wish(2, None)
            },
            Wish {
                media_status: 5,
                created_at: now() - recent,
                ..wish(3, None)
            },
        ],
        title: Some("Arrival".into()),
        ..Default::default()
    };
    let mut d = status_dialog(seerr, None, None);

    let out = d.handle(&Aci("aaaa".into()), "/status").await.join("\n");

    let ids: Vec<&str> = out
        .lines()
        .map(|line| line.split(':').next().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["1", "3"],
        "the open one and the fresh one, not the month-old arrival: {out}"
    );
}

/// (d) The title comes from what is already remembered when there is one,
/// and only then from Seerr -- a lookup per line is what the notices file
/// exists to spare.
#[tokio::test]
async fn a_remembered_title_is_used_and_costs_no_lookup() {
    let mut notices = Notices::default();
    notices.note_mut(1849, now()).title = Some("Arrival".into());
    let seerr = FakeSeerr {
        wishes: vec![wish(1849, None), wish(1850, None)],
        title: Some("Blade Runner 2049".into()),
        ..Default::default()
    };
    let mut d = status_dialog(seerr, None, Some(notices));

    let out = d.handle(&Aci("aaaa".into()), "/status").await.join("\n");

    assert!(out.contains("Arrival"), "got: {out}");
    assert!(out.contains("Blade Runner 2049"), "got: {out}");
    assert_eq!(
        *d.seerr_ref().title_for_calls.lock().unwrap(),
        1,
        "only the wish with no remembered title may cost a lookup"
    );
}

/// (e) Without any Radarr insight the bot says nothing about a search it
/// knows nothing about: the wish is waiting, not "being looked for".
#[tokio::test]
async fn without_insight_the_bot_does_not_claim_to_be_searching() {
    let seerr = FakeSeerr {
        wishes: vec![wish(1849, Some(42))],
        title: Some("Arrival".into()),
        ..Default::default()
    };
    let mut d = status_dialog(seerr, None, None);

    let out = d.handle(&Aci("aaaa".into()), "/status").await.join("\n");

    let catalogue = Catalogue::load();
    assert!(
        out.contains(&catalogue.text(Locale::De, "status.waiting", &[])),
        "got: {out}"
    );
    assert!(
        !out.contains(&catalogue.text(Locale::De, "status.searching", &[])),
        "a claim about a search nobody measured: {out}"
    );
}

/// (e2) Without any Radarr insight, Seerr's OWN account of a download
/// (`media.downloadStatus`) is still shown: it is the one piece of download
/// evidence that exists without `[insight]` configured at all, and it must
/// not be drowned out by the plain "waiting" this whole task removed from
/// every other case.
#[tokio::test]
async fn without_insight_seerrs_own_download_percent_is_shown() {
    let seerr = FakeSeerr {
        wishes: vec![Wish {
            download_percent: Some(54),
            ..wish(1849, Some(42))
        }],
        title: Some("Arrival".into()),
        ..Default::default()
    };
    let mut d = status_dialog(seerr, None, None);

    let out = d.handle(&Aci("aaaa".into()), "/status").await.join("\n");

    let catalogue = Catalogue::load();
    assert!(
        out.contains(&catalogue.text(Locale::De, "status.downloading", &[("percent", "54")])),
        "got: {out}"
    );
    assert!(
        !out.contains(&catalogue.text(Locale::De, "status.waiting", &[])),
        "seerr's own account of a download must win over the plain wait: {out}"
    );
}

/// (f) The confirmation says so when the film is not out yet -- the one
/// question everybody asks a week later, answered at the moment of asking.
#[tokio::test]
async fn placing_a_film_that_is_not_out_yet_says_so_in_the_confirmation() {
    let seerr = FakeSeerr {
        hits: vec![movie(1, "Blade Runner 2049")],
        wishes: vec![wish(1849, Some(7))],
        ..Default::default()
    };
    let insight = Arc::new(FakeInsight {
        movies: vec![(7, arr_movie(false))],
        ..Default::default()
    });
    let mut d = status_dialog(seerr, Some(insight), None);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    let out = d.handle(&aci, "1").await.join("\n");

    let expected = Catalogue::load().text(
        Locale::De,
        "request.placed_not_released",
        &[("title", "Blade Runner 2049"), ("id", "1849")],
    );
    assert_eq!(out, expected);
}

/// ... and a wish never fails, waits or changes its wording because that
/// extra question could not be answered.
#[tokio::test]
async fn a_failing_lookup_leaves_the_ordinary_confirmation() {
    let seerr = FakeSeerr {
        hits: vec![movie(1, "Blade Runner 2049")],
        wishes: vec![wish(1849, Some(7))],
        ..Default::default()
    };
    let insight = Arc::new(FakeInsight {
        fail: true,
        ..Default::default()
    });
    let mut d = status_dialog(seerr, Some(insight), None);
    let aci = Aci("aaaa".into());

    d.handle(&aci, "blade runner").await;
    let out = d.handle(&aci, "1").await.join("\n");

    let expected = Catalogue::load().text(
        Locale::De,
        "request.placed",
        &[("title", "Blade Runner 2049"), ("id", "1849")],
    );
    assert_eq!(out, expected);
    assert_eq!(
        d.seerr_ref().placed.lock().unwrap().len(),
        1,
        "the wish itself went out all the same"
    );
}

// -- series and Declined in /status (0.5.0) ----------------------------------

fn tv_wish(id: i64, arr_id: i64, seasons: Vec<u16>) -> Wish {
    let mut w = wish(id, Some(arr_id));
    w.kind = MediaKind::Tv;
    w.seasons = seasons;
    w
}

fn episode(season: u16, number: u16, days_from_now: i64, has_file: bool) -> ArrEpisode {
    ArrEpisode {
        season,
        number,
        air_date: Some(now() + time::Duration::days(days_from_now)),
        has_file,
        monitored: true,
    }
}

/// A series with a gap is "still looking"; one that is complete so far
/// names the next air date; one with nothing aired names the first. Each
/// line comes from Sonarr's episode list, read through `Insight::series` --
/// and never through anything that could search.
#[tokio::test]
async fn status_reads_a_series_off_its_episodes() {
    let seerr = FakeSeerr {
        wishes: vec![
            tv_wish(1, 42, vec![1]),
            tv_wish(2, 43, vec![1]),
            tv_wish(3, 44, vec![1]),
        ],
        title: Some("Die Serie".into()),
        ..Default::default()
    };
    let with_gap = ArrSeries {
        monitored: true,
        monitored_seasons: vec![1],
        episodes: vec![episode(1, 1, -30, true), episode(1, 2, -20, false)],
    };
    let complete_so_far = ArrSeries {
        monitored: true,
        monitored_seasons: vec![1],
        episodes: vec![episode(1, 1, -30, true), episode(1, 2, 7, false)],
    };
    let not_aired = ArrSeries {
        monitored: true,
        monitored_seasons: vec![1],
        episodes: vec![episode(1, 1, 14, false)],
    };
    let insight = Arc::new(FakeInsight {
        series: vec![(42, with_gap), (43, complete_so_far), (44, not_aired)],
        ..Default::default()
    });
    let mut d = status_dialog(seerr, Some(insight.clone()), None);

    let out = d.handle(&Aci("aaaa".into()), "/status").await.join("\n");
    let c = Catalogue::load();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(
        lines[0].ends_with(&c.text(Locale::De, "status.searching", &[])),
        "{out}"
    );
    assert!(lines[1].contains("die nächste kommt am"), "{out}");
    assert!(lines[2].contains("die erste Folge kommt am"), "{out}");
    assert_eq!(*insight.series_calls.lock().unwrap(), vec![42, 43, 44]);
    assert!(insight.movie_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn status_calls_a_declined_wish_declined_without_asking_anybody() {
    let mut declined = wish(1, Some(42));
    declined.request_status = 3;
    let seerr = FakeSeerr {
        wishes: vec![declined],
        title: Some("Der Wunsch".into()),
        ..Default::default()
    };
    let insight = Arc::new(FakeInsight::default());
    let mut d = status_dialog(seerr, Some(insight.clone()), None);

    let out = d.handle(&Aci("aaaa".into()), "/status").await.join("\n");
    let c = Catalogue::load();
    assert!(
        out.ends_with(&c.text(Locale::De, "status.declined", &[])),
        "{out}"
    );
    assert!(insight.movie_calls.lock().unwrap().is_empty());
    assert!(insight.queue_calls.lock().unwrap().is_empty());
}
