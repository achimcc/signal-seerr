//! The watcher's round: once per wish and class, at most one interactive
//! indexer search per wish, and the notices file written before the message
//! goes out.
//!
//! Every test drives a fixed clock (`NOW`) rather than the real one -- the
//! whole module is about deadlines, and a test that had to wait 25 hours
//! would never be run.

use signal_seerr::arr::{
    ArrMovie, HistoryEvent, Insight, QueueItem, QueueState, Release, ReleaseSearch,
};
use signal_seerr::dialog::state_text;
use signal_seerr::i18n::{Catalogue, Locale};
use signal_seerr::model::{
    Aci, Hit, MediaKind, QualityProfile, Reason, Seasons, SeerrUserId, Wish, WishState,
};
use signal_seerr::notices::Notices;
use signal_seerr::seerr::Requests;
use signal_seerr::signal::Messenger;
use signal_seerr::state::{Entry, State};
use signal_seerr::watch::{RoundReport, WatchSettings, Watcher};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use time::{Duration, OffsetDateTime};

const NOW: OffsetDateTime = time::macros::datetime!(2026-09-22 12:00 UTC);
const TITLE: &str = "Der Wunsch";
const PROFILE: &str = "Deutsch";

fn hours_ago(hours: i64) -> OffsetDateTime {
    NOW - Duration::hours(hours)
}

// -- the fakes -------------------------------------------------------------

/// Only `open_wishes` and `title_for` are ever reached from a round; every
/// other method of the trait panics, so a round that started using one would
/// fail loudly instead of silently taking a default.
#[derive(Default)]
struct FakeSeerr {
    wishes: Mutex<Vec<Wish>>,
    /// `open_wishes` fails -- an outage, not an empty list.
    fail: AtomicBool,
    title: Option<String>,
}

#[async_trait::async_trait]
impl Requests for FakeSeerr {
    async fn open_wishes(&self) -> anyhow::Result<Vec<Wish>> {
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("seerr is down");
        }
        Ok(self.wishes.lock().unwrap().clone())
    }
    async fn title_for(&self, _kind: MediaKind, _tmdb_id: i64) -> anyhow::Result<Option<String>> {
        Ok(self.title.clone())
    }
    async fn search(&self, _q: &str, _k: Option<MediaKind>, _p: u32) -> anyhow::Result<Vec<Hit>> {
        unreachable!("not used by a watcher round")
    }
    async fn user_id(&self, _u: &str) -> anyhow::Result<Option<SeerrUserId>> {
        unreachable!("not used by a watcher round")
    }
    async fn quality_profiles(&self, _k: MediaKind) -> anyhow::Result<Vec<QualityProfile>> {
        unreachable!("not used by a watcher round")
    }
    async fn request(
        &self,
        _hit: &Hit,
        _seasons: Seasons,
        _as_user: SeerrUserId,
        _profile_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        unreachable!("not used by a watcher round")
    }
    async fn profile_of(&self, _request_id: i64) -> anyhow::Result<Option<i64>> {
        unreachable!("not used by a watcher round")
    }
    async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Wish>> {
        unreachable!("not used by a watcher round")
    }
    async fn withdraw(&self, _id: i64, _u: SeerrUserId) -> anyhow::Result<()> {
        unreachable!("not used by a watcher round")
    }
    async fn requester_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
        unreachable!("not used by a watcher round")
    }
    async fn title_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
        unreachable!("not used by a watcher round")
    }
}

/// Radarr/Sonarr, counting every call. This one DOES implement
/// `ReleaseSearch` as well -- `watch` is the single module that may be
/// handed one, and the counters are how a test proves it used the budget the
/// way it promised.
#[derive(Default)]
struct FakeArr {
    /// arr id -> what Radarr says about that film. Behind a lock so a test
    /// can let the world change between two rounds, which is what half of
    /// them are about.
    movies: Mutex<Vec<(i64, ArrMovie)>>,
    queue: Mutex<Vec<(MediaKind, QueueItem)>>,
    event: Option<HistoryEvent>,
    releases: Vec<Release>,
    releases_fail: bool,
    movie_calls: AtomicUsize,
    last_event_calls: AtomicUsize,
    release_calls: AtomicUsize,
    /// Subsumes a plain counter: which kind was asked matters as much as
    /// how often (a movie-only round must not touch Sonarr).
    queue_kinds: Mutex<Vec<MediaKind>>,
}

#[async_trait::async_trait]
impl Insight for FakeArr {
    async fn movie(&self, id: i64) -> anyhow::Result<ArrMovie> {
        self.movie_calls.fetch_add(1, Ordering::SeqCst);
        self.movies
            .lock()
            .unwrap()
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, movie)| movie.clone())
            .ok_or_else(|| anyhow::anyhow!("radarr knows no movie {id}"))
    }
    async fn queue(&self, kind: MediaKind) -> anyhow::Result<Vec<QueueItem>> {
        self.queue_kinds.lock().unwrap().push(kind);
        Ok(self
            .queue
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, item)| item.clone())
            .collect())
    }
    async fn last_event(&self, _kind: MediaKind, _id: i64) -> anyhow::Result<Option<HistoryEvent>> {
        self.last_event_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.event)
    }
}

#[async_trait::async_trait]
impl ReleaseSearch for FakeArr {
    async fn releases(&self, _movie_id: i64) -> anyhow::Result<Vec<Release>> {
        self.release_calls.fetch_add(1, Ordering::SeqCst);
        if self.releases_fail {
            anyhow::bail!("radarr's indexers are unreachable");
        }
        Ok(self.releases.clone())
    }
}

#[derive(Default)]
struct FakeMessenger {
    sent: Mutex<Vec<(Aci, String)>>,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl Messenger for FakeMessenger {
    async fn send(&self, to: &Aci, text: &str) -> anyhow::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("signal-cli is unreachable");
        }
        self.sent
            .lock()
            .unwrap()
            .push((to.clone(), text.to_string()));
        Ok(())
    }
}

// -- fixtures --------------------------------------------------------------

fn movie_wish(id: i64) -> Wish {
    Wish {
        id,
        kind: MediaKind::Movie,
        tmdb_id: 100_000 + id,
        request_status: 2,
        media_status: 3,
        arr_id: Some(400 + id),
        created_at: hours_ago(25),
        profile_name: Some(PROFILE.to_string()),
        requested_by: Some("robert".to_string()),
        download_percent: None,
    }
}

fn available() -> ArrMovie {
    ArrMovie {
        is_available: true,
        has_file: false,
        digital_release: None,
        physical_release: None,
    }
}

fn not_released() -> ArrMovie {
    ArrMovie {
        is_available: false,
        has_file: false,
        digital_release: Some(NOW + Duration::days(30)),
        physical_release: None,
    }
}

fn recorded_releases() -> Vec<Release> {
    serde_json::from_str::<Vec<Release>>(include_str!(
        "fixtures/radarr-release-all-rejected-language.json"
    ))
    .expect("the recording parses")
}

fn directory(with_entry: bool, locale: Locale) -> Arc<RwLock<State>> {
    let mut state = State::default();
    if with_entry {
        state.upsert(Entry {
            authentik_username: "robert".into(),
            signal_username: "robert.1".into(),
            aci: Aci("aaaa".into()),
            greeted: true,
            locale,
            groups: vec!["Medien".into()],
        });
    }
    Arc::new(RwLock::new(state))
}

/// One wired-up watcher plus the handles a test needs to look at afterwards.
struct Harness {
    watcher: Watcher,
    seerr: Arc<FakeSeerr>,
    arr: Arc<FakeArr>,
    messenger: Arc<FakeMessenger>,
    notices: Arc<RwLock<Notices>>,
    path: PathBuf,
    /// Held so the temporary directory outlives the notices file.
    _dir: tempfile::TempDir,
}

struct Setup {
    seerr: FakeSeerr,
    arr: FakeArr,
    search_on: bool,
    per_day: u32,
    stall_after: Duration,
    with_entry: bool,
    /// The language the one person in the directory reads. Only the two
    /// tests that check a message word for word ever change it.
    locale: Locale,
}

impl Default for Setup {
    fn default() -> Setup {
        Setup {
            seerr: FakeSeerr::default(),
            arr: FakeArr::default(),
            search_on: true,
            per_day: 5,
            stall_after: Duration::hours(24),
            with_entry: true,
            locale: Locale::De,
        }
    }
}

fn harness(setup: Setup) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notices.json");
    let seerr = Arc::new(setup.seerr);
    let arr = Arc::new(setup.arr);
    let messenger = Arc::new(FakeMessenger::default());
    let notices = Arc::new(RwLock::new(Notices::default()));
    let watcher = Watcher {
        seerr: seerr.clone(),
        insight: arr.clone(),
        search: setup
            .search_on
            .then(|| arr.clone() as Arc<dyn ReleaseSearch>),
        messenger: messenger.clone(),
        directory: directory(setup.with_entry, setup.locale),
        notices: notices.clone(),
        catalogue: Arc::new(Catalogue::load()),
        settings: WatchSettings {
            stall_after: setup.stall_after,
            max_searches_per_day: setup.per_day,
            notices_file: path.clone(),
            profile_languages: BTreeMap::from([(PROFILE.to_string(), vec!["German".to_string()])]),
        },
    };
    Harness {
        watcher,
        seerr,
        arr,
        messenger,
        notices,
        path,
        _dir: dir,
    }
}

impl Harness {
    fn sent(&self) -> Vec<(Aci, String)> {
        self.messenger.sent.lock().unwrap().clone()
    }

    fn set_wishes(&self, wishes: Vec<Wish>) {
        *self.seerr.wishes.lock().unwrap() = wishes;
    }

    /// A second watcher over the SAME notices file, with its record read
    /// back from disk -- a restart, in other words.
    fn after_restart(&self) -> Watcher {
        Watcher {
            seerr: self.seerr.clone(),
            insight: self.arr.clone(),
            search: Some(self.arr.clone()),
            messenger: self.messenger.clone(),
            directory: self.watcher.directory.clone(),
            notices: Arc::new(RwLock::new(Notices::load(&self.path).unwrap())),
            catalogue: Arc::new(Catalogue::load()),
            settings: self.watcher.settings.clone(),
        }
    }
}

fn expected_notice(state: &WishState, id: i64) -> String {
    let catalogue = Catalogue::load();
    let mut text = catalogue.text(
        Locale::De,
        "notice.unsuitable",
        &[
            ("title", TITLE),
            ("state", &state_text(&catalogue, Locale::De, state)),
        ],
    );
    if let WishState::Unsuitable(reason) = state {
        if *reason != Reason::NothingExists {
            text.push_str("\n\n");
            text.push_str(&catalogue.text(
                Locale::De,
                "notice.unsuitable_hint",
                &[("id", &id.to_string())],
            ));
        }
    }
    text
}

// -- the tests -------------------------------------------------------------

/// 1. The deadline is a deadline: an hour short of it, nothing happens at
///    all -- not even the search, which is the expensive half.
#[tokio::test]
async fn a_wish_an_hour_short_of_the_deadline_is_left_alone() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            ..Default::default()
        },
        ..Default::default()
    });
    let mut wish = movie_wish(1);
    wish.created_at = hours_ago(23);
    h.set_wishes(vec![wish]);

    let report = h.watcher.round(NOW).await;

    assert_eq!(
        report,
        RoundReport {
            wishes: 1,
            notices_sent: 0,
            searches: 0
        }
    );
    assert!(h.sent().is_empty());
    assert_eq!(h.arr.release_calls.load(Ordering::SeqCst), 0);
}

/// 2. Past the deadline: exactly one search, exactly one notice -- and the
///    message carries the title but NOTHING out of the rejections. Those
///    sentences name the operator's own custom formats and scores; they are
///    nobody else's business.
#[tokio::test]
async fn a_stalled_wish_is_told_once_and_no_rejection_text_leaks() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            releases: recorded_releases(),
            ..Default::default()
        },
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);

    let report = h.watcher.round(NOW).await;

    assert_eq!(
        report,
        RoundReport {
            wishes: 1,
            notices_sent: 1,
            searches: 1
        }
    );
    let sent = h.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, Aci("aaaa".into()));
    let text = &sent[0].1;
    assert!(text.contains(TITLE), "the title must be in it: {text}");
    for forbidden in ["Custom", "Not German", "score"] {
        assert!(
            !text.contains(forbidden),
            "the rejections must not leak ({forbidden}): {text}"
        );
    }
    // The two languages the recording actually carries, most frequent
    // first -- written out rather than computed with `reason_from`, which
    // would only assert that the code agrees with itself.
    assert_eq!(
        *text,
        expected_notice(
            &WishState::Unsuitable(Reason::OnlyInLanguages(vec![
                "Portuguese".into(),
                "Portuguese (Brazil)".into(),
            ])),
            1
        )
    );
}

/// 3. Once told is told: neither the next round nor a restart repeats it.
///    That is the whole point of writing the file.
#[tokio::test]
async fn a_told_wish_stays_quiet_across_the_next_round_and_a_restart() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            releases: recorded_releases(),
            ..Default::default()
        },
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);

    assert_eq!(h.watcher.round(NOW).await.notices_sent, 1);

    let second = h.watcher.round(NOW + Duration::hours(1)).await;
    assert_eq!(
        second,
        RoundReport {
            wishes: 1,
            notices_sent: 0,
            searches: 0
        }
    );

    // ... and again from what is on disk, with an empty in-memory record.
    let restarted = h.after_restart();
    let third = restarted.round(NOW + Duration::hours(2)).await;
    assert_eq!(
        third,
        RoundReport {
            wishes: 1,
            notices_sent: 0,
            searches: 0
        },
        "the record must have survived the restart"
    );
    assert_eq!(h.sent().len(), 1, "still exactly one message in total");
}

/// 4. A failed send must not count as told -- but it must not buy a second
///    interactive search either: the reason is already on the note.
#[tokio::test]
async fn a_failed_send_is_retried_next_round_without_a_second_search() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            releases: recorded_releases(),
            ..Default::default()
        },
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);
    h.messenger.fail.store(true, Ordering::SeqCst);

    let first = h.watcher.round(NOW).await;
    assert_eq!(first.notices_sent, 0);
    assert_eq!(first.searches, 1);
    assert!(
        h.notices.read().unwrap().note(1).unwrap().told.is_empty(),
        "a message that never arrived is not a message that was sent"
    );

    h.messenger.fail.store(false, Ordering::SeqCst);
    let second = h.watcher.round(NOW + Duration::hours(1)).await;
    assert_eq!(
        second,
        RoundReport {
            wishes: 1,
            notices_sent: 1,
            searches: 0
        }
    );
    assert_eq!(h.arr.release_calls.load(Ordering::SeqCst), 1);
    // And it is the REASON that goes out, not the general sentence: the
    // whole point of not searching twice is that the answer was kept.
    assert_eq!(
        h.sent()[0].1,
        expected_notice(
            &WishState::Unsuitable(Reason::OnlyInLanguages(vec![
                "Portuguese".into(),
                "Portuguese (Brazil)".into(),
            ])),
            1
        ),
        "the retry must carry the stored reason, not fall back to the general sentence"
    );
}

/// 5. A film that is not out yet is never "stalled" -- the clock starts the
///    day it becomes available, not the day somebody asked for it.
#[tokio::test]
async fn the_deadline_starts_when_a_film_becomes_available() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, not_released())]),
            ..Default::default()
        },
        search_on: false,
        ..Default::default()
    });
    let mut wish = movie_wish(1);
    wish.created_at = NOW - Duration::days(10);
    h.set_wishes(vec![wish]);

    assert_eq!(h.watcher.round(NOW).await.notices_sent, 0);

    // Radarr now says it is out. That is the moment the clock starts.
    let released_at = NOW + Duration::hours(1);
    *h.arr.movies.lock().unwrap() = vec![(401, available())];
    assert_eq!(h.watcher.round(released_at).await.notices_sent, 0);
    assert_eq!(
        h.watcher
            .round(released_at + Duration::hours(23))
            .await
            .notices_sent,
        0,
        "23 hours after release is still inside the deadline"
    );
    assert_eq!(
        h.watcher
            .round(released_at + Duration::hours(25))
            .await
            .notices_sent,
        1
    );
}

/// 6. Something that is downloading is not a problem, and a message about
///    it would be noise.
#[tokio::test]
async fn a_downloading_wish_is_never_announced() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            queue: Mutex::new(vec![(
                MediaKind::Movie,
                QueueItem {
                    arr_id: 401,
                    percent: 40,
                    state: QueueState::Downloading,
                },
            )]),
            ..Default::default()
        },
        ..Default::default()
    });
    let mut wish = movie_wish(1);
    wish.created_at = NOW - Duration::days(10);
    h.set_wishes(vec![wish]);

    let report = h.watcher.round(NOW).await;
    assert_eq!(report.notices_sent, 0);
    assert_eq!(report.searches, 0);
    assert_eq!(
        h.arr.movie_calls.load(Ordering::SeqCst),
        0,
        "a queue entry is evidence enough; no second lookup"
    );
}

/// 7. The budget: one interactive search per round, five a day, and the
///    counter turns over with the UTC calendar day.
#[tokio::test]
async fn one_search_per_round_five_a_day_and_a_fresh_budget_tomorrow() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new((1..=6).map(|id| (400 + id, available())).collect()),
            releases: recorded_releases(),
            ..Default::default()
        },
        per_day: 5,
        ..Default::default()
    });
    h.set_wishes((1..=6).map(movie_wish).collect());

    for round in 0..5 {
        let report = h.watcher.round(NOW + Duration::minutes(round)).await;
        assert_eq!(
            (report.searches, report.notices_sent),
            (1, 1),
            "round {round} must search once and tell one person"
        );
    }

    let sixth = h.watcher.round(NOW + Duration::minutes(5)).await;
    assert_eq!(
        (sixth.searches, sixth.notices_sent),
        (0, 0),
        "today's budget is used up, and the last wish waits for its search"
    );

    let tomorrow = h.watcher.round(NOW + Duration::days(1)).await;
    assert_eq!((tomorrow.searches, tomorrow.notices_sent), (1, 1));
    assert_eq!(h.arr.release_calls.load(Ordering::SeqCst), 6);
}

/// 8. With the search switched off there is nothing to wait for: the general
///    sentence goes out at the deadline, and no indexer is ever asked.
#[tokio::test]
async fn without_the_search_the_general_sentence_goes_out_right_away() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            ..Default::default()
        },
        search_on: false,
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);

    let report = h.watcher.round(NOW).await;
    assert_eq!(
        report,
        RoundReport {
            wishes: 1,
            notices_sent: 1,
            searches: 0
        }
    );
    assert_eq!(h.sent()[0].1, expected_notice(&WishState::Searching, 1));
}

/// 9. Somebody who never linked Signal cannot be told -- and nothing may be
///    recorded as told either, or they would stay silent for ever once they
///    do link.
#[tokio::test]
async fn a_requester_without_a_signal_name_is_never_told_and_nothing_is_recorded() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            releases: recorded_releases(),
            ..Default::default()
        },
        with_entry: false,
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);

    let report = h.watcher.round(NOW).await;
    assert_eq!(report.notices_sent, 0);
    assert_eq!(report.searches, 0, "no budget on somebody unreachable");
    assert!(h.sent().is_empty());
    assert!(h.notices.read().unwrap().note(1).unwrap().told.is_empty());
}

/// 10. A withdrawn wish has nothing left to be told about.
#[tokio::test]
async fn a_wish_gone_from_seerr_loses_its_note() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            releases: recorded_releases(),
            ..Default::default()
        },
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);
    h.watcher.round(NOW).await;
    assert!(h.notices.read().unwrap().note(1).is_some());

    h.set_wishes(vec![]);
    h.watcher.round(NOW + Duration::hours(1)).await;
    assert!(h.notices.read().unwrap().note(1).is_none());
}

/// 11. A series is judged by its queue entry alone. There is no such thing
///     as a "movie" in Sonarr, and an interactive search is a Radarr call --
///     neither may be attempted.
///
///     And with neither of them there is NO measurement behind "still
///     looking, nothing suitable so far", so a stalled series is told
///     nothing at all rather than a sentence the bot cannot back up. It
///     classifies as `Waiting`, which is never announced unasked.
#[tokio::test]
async fn a_stalled_series_costs_no_lookup_and_is_told_nothing() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        ..Default::default()
    });
    let mut wish = movie_wish(1);
    wish.kind = MediaKind::Tv;
    h.set_wishes(vec![wish]);

    let report = h.watcher.round(NOW).await;

    assert_eq!(h.arr.movie_calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.arr.release_calls.load(Ordering::SeqCst), 0);
    assert_eq!(report.searches, 0);
    assert_eq!(
        report.notices_sent, 0,
        "nothing was measured, so there is nothing to claim"
    );
    assert!(h.sent().is_empty());
    assert!(h.notices.read().unwrap().note(1).unwrap().told.is_empty());
    assert_eq!(
        *h.arr.queue_kinds.lock().unwrap(),
        vec![MediaKind::Movie, MediaKind::Tv]
    );
}

/// 12. Seerr being down is not everybody withdrawing their wishes. An empty
///     answer from a failed call would prune the whole record, and the next
///     round would repeat every notice to everybody.
#[tokio::test]
async fn a_seerr_outage_is_not_a_withdrawal() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            releases: recorded_releases(),
            ..Default::default()
        },
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);
    h.watcher.round(NOW).await;

    h.seerr.fail.store(true, Ordering::SeqCst);
    let report = h.watcher.round(NOW + Duration::hours(1)).await;

    assert_eq!(report, RoundReport::default());
    assert!(
        h.notices.read().unwrap().note(1).is_some(),
        "an outage must not look like a withdrawal"
    );
}

/// 13. A wish whose search has not happened yet says nothing at all. Telling
///     somebody the general sentence now and the real reason tomorrow would
///     be two messages about the same problem.
#[tokio::test]
async fn a_wish_waiting_for_its_search_is_not_told_anything_yet() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available()), (402, available())]),
            releases: recorded_releases(),
            ..Default::default()
        },
        per_day: 1,
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1), movie_wish(2)]);

    let first = h.watcher.round(NOW).await;
    assert_eq!((first.searches, first.notices_sent), (1, 1));

    // The budget is gone for today; the second wish waits rather than being
    // told a sentence that says less than the one coming tomorrow.
    let second = h.watcher.round(NOW + Duration::hours(1)).await;
    assert_eq!(
        (second.searches, second.notices_sent),
        (0, 0),
        "no message while the wish is still waiting for its search"
    );
    assert!(h.notices.read().unwrap().note(2).unwrap().told.is_empty());
}

/// 14. A search that fails still counted against the budget: the request may
///     well have reached the indexers before it timed out, and a budget that
///     only counts successes is no budget at all. Nothing is said about the
///     wish this round -- there is no reason to tell yet.
#[tokio::test]
async fn a_failed_search_costs_its_budget_and_says_nothing() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            releases_fail: true,
            ..Default::default()
        },
        per_day: 1,
        ..Default::default()
    });
    h.set_wishes(vec![movie_wish(1)]);

    let first = h.watcher.round(NOW).await;
    assert_eq!((first.searches, first.notices_sent), (1, 0));
    {
        let notices = h.notices.read().unwrap();
        let note = notices.note(1).unwrap();
        assert!(note.reason.is_none(), "a failed search knows no reason");
        assert!(note.searched_at.is_none());
        assert!(note.told.is_empty());
    }

    // And today's single search is gone: the retry waits for tomorrow.
    let second = h.watcher.round(NOW + Duration::hours(1)).await;
    assert_eq!((second.searches, second.notices_sent), (0, 0));
    assert_eq!(h.arr.release_calls.load(Ordering::SeqCst), 1);
}

// -- the clock is started by an OBSERVATION, never by a note ---------------
//
// `released_seen` exists so that a film nobody could have got yet is not
// called stalled. It may therefore only move the clock forward where a round
// has actually SEEN the film unavailable. "No note existed yet" is not that
// observation, and the two histories below are the ordinary ways the two
// come apart -- in both, a wish stuck for days would otherwise wait another
// whole `stall_after` before anybody heard a word.

/// 15. The commonest stall there is: grabbed, then the download fails. The
///     first round sees the queue entry, so `movie()` is never called and
///     nothing about availability is known. The next round must not read its
///     first sight of `is_available` as "it came out just now".
#[tokio::test]
async fn a_wish_first_seen_in_the_queue_does_not_restart_its_clock_afterwards() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        arr: FakeArr {
            movies: Mutex::new(vec![(401, available())]),
            queue: Mutex::new(vec![(
                MediaKind::Movie,
                QueueItem {
                    arr_id: 401,
                    percent: 30,
                    state: QueueState::Downloading,
                },
            )]),
            // Never reached in the first round -- a queue entry answers on
            // its own -- and the reason the second round has something to
            // say.
            event: Some(HistoryEvent::DownloadFailed),
            ..Default::default()
        },
        ..Default::default()
    });
    let mut wish = movie_wish(1);
    wish.created_at = NOW - Duration::days(3);
    h.set_wishes(vec![wish]);

    assert_eq!(h.watcher.round(NOW).await.notices_sent, 0);
    assert_eq!(
        h.arr.movie_calls.load(Ordering::SeqCst),
        0,
        "a queue entry answers on its own"
    );

    // The download failed; the entry is gone from the queue.
    h.arr.queue.lock().unwrap().clear();
    let report = h.watcher.round(NOW + Duration::hours(1)).await;

    assert_eq!(
        report.notices_sent, 1,
        "three days stuck -- the clock must not restart just because this is \
         the first look at the film itself"
    );
}

/// 16. The same defect by the other road: the first round could not reach
///     Radarr at all and gave up on the wish -- but the note exists from
///     then on, so "no note" stops telling the truth about what was seen.
#[tokio::test]
async fn a_wish_whose_first_lookup_failed_does_not_restart_its_clock_either() {
    let h = harness(Setup {
        seerr: FakeSeerr {
            title: Some(TITLE.into()),
            ..Default::default()
        },
        // No entry for 401, so `movie()` answers with an error.
        arr: FakeArr {
            event: Some(HistoryEvent::DownloadFailed),
            ..Default::default()
        },
        ..Default::default()
    });
    let mut wish = movie_wish(1);
    wish.created_at = NOW - Duration::days(3);
    h.set_wishes(vec![wish]);

    assert_eq!(h.watcher.round(NOW).await.notices_sent, 0);
    assert!(
        h.notices.read().unwrap().note(1).is_some(),
        "the note is written even when the round gives up on the wish"
    );

    // Radarr answers again.
    *h.arr.movies.lock().unwrap() = vec![(401, available())];
    let report = h.watcher.round(NOW + Duration::hours(1)).await;

    assert_eq!(
        report.notices_sent, 1,
        "one failed lookup must not cost the person a further day"
    );
}

// -- the wording of the unasked message ------------------------------------
//
// THE WORDING ITSELF IS THE SUBJECT HERE, so these two compare against the
// literal sentence rather than against the catalogue -- the catalogue would
// only assert that the code agrees with itself. `{state}` is a FRAGMENT
// built for the `/status` line, and putting it straight behind the title
// produced "\u{201e}Der Wunsch\" ich suche noch, bisher war nichts Passendes
// dabei." -- in both languages, and for the commonest case of the first
// rollout stage (`reason_search = false`, so `Searching`).

/// 17. The general sentence, word for word, in both languages.
#[tokio::test]
async fn the_searching_notice_reads_as_a_sentence_in_both_languages() {
    for (locale, expected) in [
        (
            Locale::De,
            "Zu \u{201e}Der Wunsch\": ich suche noch, bisher war nichts Passendes dabei. \
             Ich suche weiter und melde mich, wenn er da ist.",
        ),
        (
            Locale::En,
            "About \"Der Wunsch\": still looking, nothing suitable so far. \
             I keep looking and will tell you when it is here.",
        ),
    ] {
        let h = harness(Setup {
            seerr: FakeSeerr {
                title: Some(TITLE.into()),
                ..Default::default()
            },
            arr: FakeArr {
                movies: Mutex::new(vec![(401, available())]),
                ..Default::default()
            },
            search_on: false,
            locale,
            ..Default::default()
        });
        h.set_wishes(vec![movie_wish(1)]);

        assert_eq!(h.watcher.round(NOW).await.notices_sent, 1);
        assert_eq!(h.sent()[0].1, expected, "{locale:?}");
    }
}

/// 18. And the one reason that carries no "pick another version" hint, so
///     the whole message is that single sentence -- again word for word.
#[tokio::test]
async fn the_nothing_exists_notice_reads_as_a_sentence_in_both_languages() {
    for (locale, expected) in [
        (
            Locale::De,
            "Zu \u{201e}Der Wunsch\": bisher nirgends aufzutreiben. \
             Ich suche weiter und melde mich, wenn er da ist.",
        ),
        (
            Locale::En,
            "About \"Der Wunsch\": nowhere to be found so far. \
             I keep looking and will tell you when it is here.",
        ),
    ] {
        let h = harness(Setup {
            seerr: FakeSeerr {
                title: Some(TITLE.into()),
                ..Default::default()
            },
            arr: FakeArr {
                movies: Mutex::new(vec![(401, available())]),
                // An empty search result: `reason_from` answers
                // `NothingExists`.
                releases: Vec::new(),
                ..Default::default()
            },
            locale,
            ..Default::default()
        });
        h.set_wishes(vec![movie_wish(1)]);

        let report = h.watcher.round(NOW).await;
        assert_eq!((report.searches, report.notices_sent), (1, 1));
        assert_eq!(h.sent()[0].1, expected, "{locale:?}");
    }
}
