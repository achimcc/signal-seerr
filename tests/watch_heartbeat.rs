//! The heartbeat line, read off a rendered log line.
//!
//! **This test lives in a file of its own on purpose, and it must stay the
//! only one in it.** `tracing` caches, globally and for the whole process,
//! whether any subscriber is interested in a given callsite -- and it caches
//! that the first time the callsite is hit. A test binary that runs other
//! rounds with no subscriber installed therefore decides, on whichever test
//! happens to go first, that nobody wants the heartbeat; a capturing test
//! next to them then sees nothing and fails about a third of the time.
//! Measured: alone it passes, inside `tests/watch.rs` it failed three runs
//! out of three. One test per process is what makes it deterministic.
//!
//! The wish list is deliberately empty. What is under test is the LINE --
//! its literal text, its fields, and the fact that both ways out of `round`
//! emit it -- not what a round does with a wish, which `tests/watch.rs`
//! covers at length.

use signal_seerr::arr::{ArrMovie, ArrSeries, HistoryEvent, Insight, QueueItem};
use signal_seerr::i18n::Catalogue;
use signal_seerr::model::{Aci, Hit, MediaKind, QualityProfile, Seasons, SeerrUserId, Wish};
use signal_seerr::notices::Notices;
use signal_seerr::seerr::Requests;
use signal_seerr::signal::Messenger;
use signal_seerr::state::State;
use signal_seerr::watch::{WatchSettings, Watcher, HEARTBEAT};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

#[test]
fn both_ways_out_of_a_round_log_the_line_the_health_check_greps_for() {
    assert_eq!(
        HEARTBEAT, "watch: round complete",
        "the guest check in the homeserver repository greps the journal for \
         exactly this text -- changing it breaks a check this compiler \
         cannot see"
    );

    let log = Arc::new(Mutex::new(Vec::new()));
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || SharedBuf(writer.clone()))
        .finish();

    // A current-thread runtime built inside the subscriber guard: the
    // default dispatcher is thread-local, and this keeps both rounds on the
    // thread that holds it.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let seerr = Arc::new(FakeSeerr::default());
    tracing::subscriber::with_default(subscriber, || {
        let dir = tempfile::tempdir().unwrap();
        let watcher = watcher(seerr.clone(), dir.path().join("notices.json"));
        runtime.block_on(async {
            watcher
                .round(time::macros::datetime!(2026-09-22 12:00 UTC))
                .await;
            // Now Seerr is unreachable. The round still ran, so the line
            // still goes out -- otherwise an outage reads, to a check that
            // greps for it, exactly like a stopped watcher.
            seerr.fail.store(true, Ordering::SeqCst);
            watcher
                .round(time::macros::datetime!(2026-09-22 13:00 UTC))
                .await;
        });
    });

    let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
    let beats: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("watch: round complete"))
        .collect();

    assert_eq!(
        beats.len(),
        2,
        "one line per round, the Seerr outage included -- got:\n{text}"
    );
    for line in &beats {
        assert!(
            line.contains("wishes=0")
                && line.contains("notices_sent=0")
                && line.contains("searches=0"),
            "the counters must survive the move into one function: {line}"
        );
    }
    assert_eq!(
        text.lines()
            .filter(|line| line.contains("cannot ask seerr for the open wishes"))
            .count(),
        1,
        "the outage says so in its own line, next to the heartbeat:\n{text}"
    );
}

fn watcher(seerr: Arc<FakeSeerr>, notices_file: std::path::PathBuf) -> Watcher {
    Watcher {
        seerr,
        insight: Arc::new(EmptyArr),
        search: None,
        messenger: Arc::new(NoMessenger),
        directory: Arc::new(RwLock::new(State::default())),
        notices: Arc::new(RwLock::new(Notices::default())),
        catalogue: Arc::new(Catalogue::load()),
        settings: WatchSettings {
            stall_after: time::Duration::hours(24),
            max_searches_per_day: 5,
            notices_file,
            profile_languages: BTreeMap::new(),
            retry_failed_after: None,
            refresh_reason_after: None,
        },
    }
}

/// Collects what the subscriber writes, so the test reads the line that was
/// rendered instead of trusting the macro to have said it.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeSeerr {
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl Requests for FakeSeerr {
    async fn retry(&self, _request_id: i64) -> anyhow::Result<()> {
        unreachable!("retry is the watcher's alone")
    }
    async fn open_wishes(&self) -> anyhow::Result<Vec<Wish>> {
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("seerr is down");
        }
        Ok(Vec::new())
    }
    async fn search(&self, _q: &str, _k: Option<MediaKind>, _p: u32) -> anyhow::Result<Vec<Hit>> {
        unreachable!("no wish, no lookup")
    }
    async fn user_id(&self, _u: &str) -> anyhow::Result<Option<SeerrUserId>> {
        unreachable!("no wish, no lookup")
    }
    async fn quality_profiles(&self, _k: MediaKind) -> anyhow::Result<Vec<QualityProfile>> {
        unreachable!("no wish, no lookup")
    }
    async fn request(
        &self,
        _hit: &Hit,
        _seasons: Seasons,
        _as_user: SeerrUserId,
        _profile_id: Option<i64>,
    ) -> anyhow::Result<i64> {
        unreachable!("no wish, no lookup")
    }
    async fn profile_of(&self, _request_id: i64) -> anyhow::Result<Option<i64>> {
        unreachable!("no wish, no lookup")
    }
    async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Wish>> {
        unreachable!("no wish, no lookup")
    }
    async fn withdraw(&self, _id: i64, _u: SeerrUserId) -> anyhow::Result<()> {
        unreachable!("no wish, no lookup")
    }
    async fn requester_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
        unreachable!("no wish, no lookup")
    }
    async fn title_of(&self, _request_id: i64) -> anyhow::Result<Option<String>> {
        unreachable!("no wish, no lookup")
    }
    async fn title_for(&self, _kind: MediaKind, _tmdb_id: i64) -> anyhow::Result<Option<String>> {
        unreachable!("no wish, no lookup")
    }
}

/// Answers the one call an empty round makes -- `queue(Movie)` -- and
/// nothing else.
struct EmptyArr;

#[async_trait::async_trait]
impl Insight for EmptyArr {
    async fn movie(&self, _id: i64) -> anyhow::Result<ArrMovie> {
        unreachable!("no wish to look up")
    }
    async fn series(&self, _id: i64) -> anyhow::Result<ArrSeries> {
        unreachable!("no wish to look up")
    }
    async fn queue(&self, _kind: MediaKind) -> anyhow::Result<Vec<QueueItem>> {
        Ok(Vec::new())
    }
    async fn last_event(&self, _kind: MediaKind, _id: i64) -> anyhow::Result<Option<HistoryEvent>> {
        unreachable!("no wish to look up")
    }
}

struct NoMessenger;

#[async_trait::async_trait]
impl Messenger for NoMessenger {
    async fn send(&self, _to: &Aci, _text: &str) -> anyhow::Result<()> {
        unreachable!("an empty round tells nobody anything")
    }
}
