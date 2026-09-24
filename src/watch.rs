//! The loop that speaks up unasked: once per wish and kind of problem, and
//! never more often than that.
//!
//! A wish that gets stuck used to be silent for ever -- somebody asked, read
//! "it is on the list", and never heard again. One round of this module
//! gathers what Seerr and Radarr/Sonarr say about every open wish, decides
//! with `insight::classify` what state it is in, and tells the person who
//! asked -- once.
//!
//! The order in which the notices file is written is deliberate and is not
//! the obvious one (design §5.3): the REASON is saved before the message
//! goes out, `told` only after the message actually went. A crash between
//! sending and saving therefore repeats that one notice on the next round --
//! better twice than never, and better than an unsent message recorded as
//! sent. What must not happen twice is the indexer search, and the saved
//! reason is what prevents it.
//!
//! **This is the only module in the crate that is ever handed a
//! `ReleaseSearch`.** An interactive search hits every indexer the operator
//! has, so it is budgeted twice over: at most one SUCCESSFUL search per
//! wish, and at most `max_searches_per_day` across the whole household per
//! UTC day.
//!
//! Successful, not "ever": the bar is `reason.is_none()`, and a search that
//! failed writes no reason. So a failed attempt costs one of that day's
//! places and is made again the next day -- which is what anybody would want
//! from a search that never reached the indexers, and is NOT what an earlier
//! version of this comment said.
//!
//! The clock arrives as an argument. Nothing in here reads the time itself --
//! a module whose entire subject is deadlines has to be testable without
//! waiting a day for them.

use crate::arr::{Insight, QueueItem, ReleaseSearch};
use crate::dialog::state_text;
use crate::i18n::{Catalogue, Locale};
use crate::insight::{classify, reason_from, series_facts, Evidence};
use crate::model::{MediaKind, Reason, Wish, WishState};
use crate::notices::Notices;
use crate::seerr::Requests;
use crate::signal::Messenger;
use crate::state::State;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

/// The shortest gap between two unasked messages about the same wish,
/// whatever the classes involved. Deliberately not `stall_after`: an
/// operator may shorten the deadline to a couple of hours, and that must
/// make the bot notice sooner, not talk more.
const MIN_NOTICE_GAP: Duration = Duration::hours(24);

/// The message of the one line every round logs when it is done.
///
/// **This literal is a contract with another repository**: the guest check
/// in `homeserver` greps the journal for exactly this text to tell a
/// watcher that is running from one that has stopped. Changing the wording
/// breaks a check that lives somewhere this compiler cannot see, so the
/// string is a named constant, the macro interpolates it rather than
/// repeating it, and a test reads it back off a rendered log line.
pub const HEARTBEAT: &str = "watch: round complete";

#[derive(Clone, Debug)]
pub struct WatchSettings {
    /// How long a wish may sit in the same unhappy state before the person
    /// who asked is told about it.
    pub stall_after: Duration,
    pub max_searches_per_day: u32,
    pub notices_file: PathBuf,
    /// Seerr profile name -> the languages that profile wants, as Radarr
    /// spells them. Without an entry a search can still say "nothing
    /// exists", but never "only in Portuguese".
    pub profile_languages: BTreeMap<String, Vec<String>>,
    /// How long a FAILED hand-over sits before it is handed over once more,
    /// by this loop itself. `None`: never.
    pub retry_failed_after: Option<Duration>,
    /// After how long a recorded reason is searched for again -- once per
    /// interval, budgeted like a first search, told only on a changed class.
    /// `None`: never.
    pub refresh_reason_after: Option<Duration>,
}

#[derive(Debug, Default, PartialEq)]
pub struct RoundReport {
    pub wishes: usize,
    pub notices_sent: usize,
    pub searches: usize,
    /// Hand-overs retried this round (at most one per wish, ever).
    pub retries: usize,
    /// Reasons searched for again this round (at most one per round).
    pub refreshes: usize,
}

pub struct Watcher {
    pub seerr: Arc<dyn Requests>,
    pub insight: Arc<dyn Insight>,
    /// `None` when the operator has not switched the interactive search on.
    /// Then a stalled wish still gets a message -- just the general sentence
    /// instead of a reason.
    pub search: Option<Arc<dyn ReleaseSearch>>,
    pub messenger: Arc<dyn Messenger>,
    /// std, not tokio, on both locks: every critical section below is a
    /// handful of map operations with no await inside it.
    pub directory: Arc<std::sync::RwLock<State>>,
    pub notices: Arc<std::sync::RwLock<Notices>>,
    pub catalogue: Arc<Catalogue>,
    pub settings: WatchSettings,
}

/// What one wish cost and achieved. Kept separate from `RoundReport` so the
/// per-wish function can return early at a dozen places without having to
/// remember to touch a shared counter on each of them.
#[derive(Default)]
struct WishOutcome {
    searched: bool,
    sent: bool,
    retried: bool,
}

/// Where a reason search looks: a film, or one season of a series -- the
/// season of the oldest aired episode that is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchTarget {
    Movie(i64),
    Season(i64, u16),
}

impl Watcher {
    /// One pass over every open wish. Returns what it did, and logs exactly
    /// one [`HEARTBEAT`] line on the way out -- by both ways out, because
    /// that line says the loop ran, not that Seerr answered.
    pub async fn round(&self, now: OffsetDateTime) -> RoundReport {
        let wishes = match self.seerr.open_wishes().await {
            Ok(wishes) => wishes,
            Err(e) => {
                // Nothing is touched here, above all not `retain_only`: an
                // outage must not look like everybody withdrawing at once,
                // which would clear the record and repeat every notice.
                tracing::warn!(error = %e, "cannot ask seerr for the open wishes");
                // The heartbeat is a statement about the LOOP, not about
                // Seerr: the round did run, and it is the warning above
                // that says Seerr is down. Without this line a Seerr outage
                // would read, to a check that greps for it, exactly like a
                // stopped watcher.
                let report = RoundReport::default();
                heartbeat(&report);
                return report;
            }
        };
        let mut report = RoundReport {
            wishes: wishes.len(),
            ..RoundReport::default()
        };

        // Once per kind for the whole round, not once per wish: the queue is
        // a single list, and a household with thirty open wishes would
        // otherwise ask Radarr thirty times for the same answer. Sonarr is
        // only asked when there is a series to ask about -- it may not even
        // be configured.
        let movie_queue = self.queue(MediaKind::Movie).await;
        let tv_queue = if wishes.iter().any(|w| w.kind == MediaKind::Tv) {
            self.queue(MediaKind::Tv).await
        } else {
            Vec::new()
        };
        let queue_item_of = |wish: &Wish| {
            let queue = match wish.kind {
                MediaKind::Movie => &movie_queue,
                MediaKind::Tv => &tv_queue,
            };
            queue
                .iter()
                .find(|item| Some(item.arr_id) == wish.arr_id)
                .cloned()
        };

        let mut searched_this_round = false;
        for wish in &wishes {
            let queue_item = queue_item_of(wish);
            let outcome = self
                .consider(wish, queue_item.as_ref(), now, !searched_this_round)
                .await;
            searched_this_round |= outcome.searched;
            report.searches += usize::from(outcome.searched);
            report.notices_sent += usize::from(outcome.sent);
            report.retries += usize::from(outcome.retried);
        }

        // A refresh comes AFTER every first search of the round, and only if
        // none happened: somebody who has never heard anything goes before
        // somebody who already knows the reason.
        if !searched_this_round {
            if let Some(wish) = self.refresh_candidate(&wishes, now) {
                let queue_item = queue_item_of(&wish);
                let outcome = self.refresh(&wish, queue_item.as_ref(), now).await;
                report.searches += usize::from(outcome.searched);
                report.refreshes += usize::from(outcome.searched);
                report.notices_sent += usize::from(outcome.sent);
            }
        }

        let live_ids: Vec<i64> = wishes.iter().map(|w| w.id).collect();
        let snapshot = {
            let mut notices = self.notices_mut();
            notices.retain_only(&live_ids);
            notices.clone()
        };
        self.persist(&snapshot);

        heartbeat(&report);
        report
    }

    /// One wish: gather, classify, and decide whether anything is said.
    ///
    /// `may_search_now` is false once some other wish has used this round's
    /// single search. A wish that wanted one then waits for the next round
    /// rather than being sent a weaker sentence now and the real reason
    /// tomorrow.
    async fn consider(
        &self,
        wish: &Wish,
        queue_item: Option<&QueueItem>,
        now: OffsetDateTime,
        may_search_now: bool,
    ) -> WishOutcome {
        let mut outcome = WishOutcome::default();

        // From here on the wish has a note, whatever else happens: it is
        // what `retain_only` keeps and what the deadline is measured on.
        let (seen_unreleased, mut title, reason, mut released_seen, told, last_notice, retried_at) = {
            let mut notices = self.notices_mut();
            let note = notices.note_mut(wish.id, now);
            (
                note.seen_unreleased,
                note.title.clone(),
                note.reason.clone(),
                note.released_seen,
                note.told.clone(),
                note.last_notice,
                note.retried_at,
            )
        };

        // Seerr does not put a title on a request, so it is fetched once and
        // remembered. A lookup that fails costs the wish nothing: the
        // catalogue has a placeholder for a nameless one.
        if title.is_none() {
            match self.seerr.title_for(wish.kind, wish.tmdb_id).await {
                Ok(Some(found)) => {
                    self.notices_mut().note_mut(wish.id, now).title = Some(found.clone());
                    title = Some(found);
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, id = wish.id, "cannot ask seerr for a title"),
            }
        }

        // -- a failed hand-over: one retry, by this loop, before any evidence
        // is gathered (there is none to gather for a wish Radarr never got).
        //
        // RECORDED BEFORE THE CALL. Whatever Seerr answers -- 200, 500, a
        // timeout -- `retried_at` is on disk first, so there is never a
        // second attempt: a crash between the write and the call costs the
        // retry, a crash between the call and the write would have cost a
        // duplicate hand-over. Of the two, the missing retry is the one a
        // person can live with.
        if wish.request_status == 4 {
            if let Some(delay) = self.settings.retry_failed_after {
                if retried_at.is_none() && now - wish.created_at >= delay {
                    let snapshot = {
                        let mut notices = self.notices_mut();
                        notices.note_mut(wish.id, now).retried_at = Some(now);
                        notices.clone()
                    };
                    if !self.persist(&snapshot) {
                        return outcome;
                    }
                    match self.seerr.retry(wish.id).await {
                        Ok(()) => tracing::info!(id = wish.id, "watch: hand-over retried"),
                        Err(e) => {
                            tracing::warn!(error = %e, id = wish.id, "the retry of the hand-over failed")
                        }
                    }
                    outcome.retried = true;
                    // The next round sees what Seerr made of it.
                    return outcome;
                }
            }
        }

        // A queue entry is evidence enough on its own. Otherwise a film is
        // asked about in Radarr, a series in Sonarr -- and for a series the
        // episodes are the evidence: which have aired, which are on file.
        let mut movie = None;
        let mut facts = None;
        let mut last_event = None;
        if queue_item.is_none() {
            if let Some(arr_id) = wish.arr_id {
                match wish.kind {
                    MediaKind::Movie => match self.insight.movie(arr_id).await {
                        Ok(found) => movie = Some(found),
                        Err(e) => {
                            // Half the evidence is worse than none: it would
                            // read as "still searching" and go out as a message.
                            tracing::warn!(error = %e, id = wish.id, "cannot ask radarr about this film");
                            return outcome;
                        }
                    },
                    MediaKind::Tv => match self.insight.series(arr_id).await {
                        Ok(found) => facts = Some(series_facts(&found, &wish.seasons, now)),
                        Err(e) => {
                            tracing::warn!(error = %e, id = wish.id, "cannot ask sonarr about this series");
                            return outcome;
                        }
                    },
                }
                // RECORDED BEFORE THE NEXT CALL CAN FAIL, and that placement
                // is the whole point: this block sat below the history
                // lookup, which returns early on an error, so a Radarr that
                // answered `movie()` and then failed on the history threw
                // away an observation it had already made. The next round
                // would have no record that the film was ever unreleased --
                // and `seen_unreleased` is the only evidence for the
                // transition the clock below runs on.
                //
                // The deadline for a film nobody could have got yet runs
                // from the day it became available, not from the day
                // somebody asked.
                //
                // The clock is only ever moved forward on an OBSERVED
                // transition, and the evidence for one is `seen_unreleased`:
                // some round really did look and really did find the film
                // not out yet. Anything else -- a first round that saw a
                // queue entry and so never called `movie()`, or one that
                // gave up on a Radarr error -- leaves the base at
                // `created_at` (design §5.1: "bei einem beim Wunsch schon
                // erschienenen Film ist das createdAt"). Deriving it from
                // the note's mere existence looked equivalent and is not: in
                // both of those histories the wish has been stuck for days,
                // and reading the first sight of `is_available` as a
                // transition would cost the person another whole
                // `stall_after` in silence.
                //
                // A SERIES NEEDS NONE OF THIS: its release is not observed,
                // it is written down -- every episode carries its air date,
                // and the clock below reads the oldest missing one.
                match movie.as_ref().map(|m| m.is_available) {
                    Some(false) if !seen_unreleased => {
                        self.notices_mut().note_mut(wish.id, now).seen_unreleased = true;
                    }
                    Some(true) if released_seen.is_none() => {
                        let seen_at = if seen_unreleased {
                            now
                        } else {
                            wish.created_at
                        };
                        self.notices_mut().note_mut(wish.id, now).released_seen = Some(seen_at);
                        released_seen = Some(seen_at);
                    }
                    _ => {}
                }

                match self.insight.last_event(wish.kind, arr_id).await {
                    Ok(found) => last_event = found,
                    Err(e) => {
                        tracing::warn!(error = %e, id = wish.id, "cannot read this wish's history");
                        return outcome;
                    }
                }
            }
        }

        let mut state = classify(
            wish,
            &Evidence {
                movie: movie.as_ref(),
                series: facts.as_ref(),
                queue_item,
                last_event,
                known_reason: reason.as_ref(),
            },
            now,
        );

        // -- the notice rule, in order.

        let Some(class) = state.notice_class() else {
            return outcome;
        };
        // Where the clock starts. A failed hand-over: at the retry, and
        // only if there was one -- a first failure is never a notice, the
        // retry is the answer to it. A series: at the oldest missing
        // episode's air date. A film: at the observed release.
        let since = match (&state, facts.as_ref()) {
            (WishState::NotHandedOver, _) => match retried_at {
                Some(at) => at,
                None => return outcome,
            },
            (_, Some(facts)) => std::cmp::max(
                wish.created_at,
                facts
                    .missing_oldest
                    .map(|(air, _)| air)
                    .unwrap_or(wish.created_at),
            ),
            (_, None) => std::cmp::max(wish.created_at, released_seen.unwrap_or(wish.created_at)),
        };
        if now - since < self.settings.stall_after {
            return outcome;
        }
        if told.contains_key(class) {
            return outcome;
        }
        if last_notice.is_some_and(|last| now - last < MIN_NOTICE_GAP) {
            return outcome;
        }
        // Nobody to tell -- and `told` stays empty on purpose, so the day
        // they do link their Signal account they still hear about it.
        let Some(entry) = wish.requested_by.as_ref().and_then(|username| {
            self.directory
                .read()
                .expect("the mapping lock is never poisoned")
                .by_user(username)
                .cloned()
        }) else {
            tracing::debug!(id = wish.id, "requester has no signal name");
            return outcome;
        };

        let season = facts
            .as_ref()
            .and_then(|f| f.missing_oldest.map(|(_, s)| s));
        let search_target = if matches!(state, WishState::Searching) && reason.is_none() {
            self.search
                .as_ref()
                .zip(wish.arr_id)
                .and_then(|(search, arr_id)| match wish.kind {
                    MediaKind::Movie => Some((search, SearchTarget::Movie(arr_id))),
                    MediaKind::Tv => season.map(|s| (search, SearchTarget::Season(arr_id, s))),
                })
        } else {
            None
        };
        if let Some((search, target)) = search_target {
            if !may_search_now {
                return outcome;
            }
            let Some(attempt) = self.search_once(search.as_ref(), wish, target, now).await else {
                return outcome;
            };
            // Attempted, so counted for this round -- whether or not it came
            // back with an answer (a failed one is repeated tomorrow).
            outcome.searched = true;
            let Some(found) = attempt else {
                return outcome;
            };
            let snapshot = {
                let mut notices = self.notices_mut();
                let note = notices.note_mut(wish.id, now);
                note.reason = Some(found.clone());
                note.searched_at = Some(now);
                notices.clone()
            };
            // The result is on disk before anybody is told about it. If it
            // could not be written, the message waits: one notice late is
            // cheaper than a second indexer search.
            if !self.persist(&snapshot) {
                return outcome;
            }
            state = WishState::Unsuitable(found);
        } else if matches!(state, WishState::Searching) && reason.is_none() && self.search.is_some()
        {
            // A search was wanted and could not be aimed (a series whose
            // Sonarr id or missing season is unknown): the general sentence
            // is not sent in its place -- see `may_search_now`.
            if wish.arr_id.is_some() {
                return outcome;
            }
        }

        let title =
            title.unwrap_or_else(|| self.catalogue.text(entry.locale, "status.untitled", &[]));
        let text = self.notice_text(entry.locale, class, &title, &state, wish.id, season);
        match self.messenger.send(&entry.aci, &text).await {
            Ok(()) => {
                let snapshot = {
                    let mut notices = self.notices_mut();
                    let note = notices.note_mut(wish.id, now);
                    note.told.insert(class.to_string(), rfc3339(now));
                    note.last_notice = Some(now);
                    notices.clone()
                };
                self.persist(&snapshot);
                outcome.sent = true;
            }
            // Not recorded as told: the person did not get it. The next
            // round tries again -- without a second search, because the
            // reason is already on the note.
            Err(e) => tracing::warn!(error = %e, id = wish.id, "cannot deliver the notice"),
        }
        outcome
    }

    /// One budgeted interactive search. Counted BEFORE the call and written
    /// out at once: a search that reached the indexers and then timed out
    /// has still been made, and a budget that only counts successes is no
    /// budget. Outer `None`: the budget is spent, nothing was attempted.
    /// Inner `None`: attempted -- and counted -- but the search failed
    /// (logged); the caller says nothing and tomorrow's budget pays again.
    async fn search_once(
        &self,
        search: &dyn ReleaseSearch,
        wish: &Wish,
        target: SearchTarget,
        now: OffsetDateTime,
    ) -> Option<Option<Reason>> {
        let snapshot = {
            let mut notices = self.notices_mut();
            if !notices.may_search(now, self.settings.max_searches_per_day) {
                return None;
            }
            notices.count_search(now);
            notices.clone()
        };
        self.persist(&snapshot);

        let releases = match target {
            SearchTarget::Movie(id) => search.releases(id).await,
            SearchTarget::Season(id, season) => search.season_releases(id, season).await,
        };
        match releases {
            Ok(releases) => {
                let wanted = wish
                    .profile_name
                    .as_ref()
                    .and_then(|name| self.settings.profile_languages.get(name))
                    .map(|languages| languages.as_slice());
                Some(Some(reason_from(&releases, wanted)))
            }
            Err(e) => {
                tracing::warn!(error = %e, id = wish.id, "the interactive search failed");
                Some(None)
            }
        }
    }

    /// The one wish whose recorded reason is due for a second look: the one
    /// searched longest ago among those past `refresh_reason_after`. `None`
    /// when the refresh is off, nothing is due, or there is no search to do
    /// it with.
    fn refresh_candidate(&self, wishes: &[Wish], now: OffsetDateTime) -> Option<Wish> {
        let interval = self.settings.refresh_reason_after?;
        self.search.as_ref()?;
        let notices = self
            .notices
            .read()
            .expect("the notices lock is never poisoned");
        wishes
            .iter()
            .filter(|w| w.arr_id.is_some())
            .filter_map(|w| {
                let note = notices.note(w.id)?;
                note.reason.as_ref()?;
                let searched_at = note.searched_at?;
                (now - searched_at >= interval).then_some((searched_at, w))
            })
            .min_by_key(|(searched_at, _)| *searched_at)
            .map(|(_, w)| w.clone())
    }

    /// Searches once more for a wish whose reason is on record, and tells
    /// the person only if the CLASS changed -- "only in Portuguese" turning
    /// into "too big" means a German version exists now and is not taken,
    /// and that is something they can act on. The same class again moves
    /// `searched_at` and says nothing.
    async fn refresh(
        &self,
        wish: &Wish,
        queue_item: Option<&QueueItem>,
        now: OffsetDateTime,
    ) -> WishOutcome {
        let mut outcome = WishOutcome::default();
        let Some(search) = self.search.as_ref() else {
            return outcome;
        };
        let Some(arr_id) = wish.arr_id else {
            return outcome;
        };
        let old = {
            let notices = self
                .notices
                .read()
                .expect("the notices lock is never poisoned");
            notices.note(wish.id).and_then(|n| n.reason.clone())
        };
        let Some(old) = old else {
            return outcome;
        };
        // A series is searched by season, and which season is a question
        // for Sonarr's episode list -- the same one `consider` reads.
        let target = match wish.kind {
            MediaKind::Movie => SearchTarget::Movie(arr_id),
            MediaKind::Tv => match self.insight.series(arr_id).await {
                Ok(series) => match series_facts(&series, &wish.seasons, now).missing_oldest {
                    Some((_, season)) => SearchTarget::Season(arr_id, season),
                    // Nothing missing any more: there is nothing to refresh.
                    None => return outcome,
                },
                Err(e) => {
                    tracing::warn!(error = %e, id = wish.id, "cannot ask sonarr about this series");
                    return outcome;
                }
            },
        };
        let Some(attempt) = self.search_once(search.as_ref(), wish, target, now).await else {
            return outcome;
        };
        outcome.searched = true;
        let Some(new) = attempt else {
            return outcome;
        };
        let changed = !same_class(&old, &new);
        tracing::info!(id = wish.id, changed, "watch: reason refreshed");
        let snapshot = {
            let mut notices = self.notices_mut();
            let note = notices.note_mut(wish.id, now);
            note.searched_at = Some(now);
            if changed {
                note.reason = Some(new);
                // Forgotten on purpose: the ordinary notice path below may
                // say the new sentence -- once, like any first one.
                note.told.remove("unsuitable");
            }
            notices.clone()
        };
        if !self.persist(&snapshot) {
            return outcome;
        }
        if changed {
            // No search this time: the reason is on the note, `consider`
            // reads it and only decides whether and what to tell.
            let told = self.consider(wish, queue_item, now, false).await;
            outcome.sent = told.sent;
        }
        outcome
    }

    /// The message for one class -- `WishState::notice_class` yields exactly
    /// four, and the last arm takes "unsuitable".
    ///
    /// Nothing from a `Release` ever reaches this: only `reason_from`'s
    /// verdict does, which is built from structured fields alone.
    /// `rejections` carries the operator's own custom-format names and
    /// scores, and those are nobody else's business.
    fn notice_text(
        &self,
        locale: Locale,
        class: &str,
        title: &str,
        state: &WishState,
        id: i64,
        season: Option<u16>,
    ) -> String {
        match class {
            "download_failed" => {
                self.catalogue
                    .text(locale, "notice.download_failed", &[("title", title)])
            }
            "import_stuck" => {
                self.catalogue
                    .text(locale, "notice.import_stuck", &[("title", title)])
            }
            "not_handed_over" => {
                self.catalogue
                    .text(locale, "notice.not_handed_over", &[("title", title)])
            }
            _ => {
                let state_text = state_text(&self.catalogue, locale, state);
                let mut text = match season {
                    Some(season) => self.catalogue.text(
                        locale,
                        "notice.unsuitable_season",
                        &[
                            ("title", title),
                            ("season", &season.to_string()),
                            ("state", &state_text),
                        ],
                    ),
                    None => self.catalogue.text(
                        locale,
                        "notice.unsuitable",
                        &[("title", title), ("state", &state_text)],
                    ),
                };
                // The hint is about picking a different version, so it only
                // makes sense where a different version could exist.
                if matches!(state, WishState::Unsuitable(reason) if *reason != Reason::NothingExists)
                {
                    text.push_str("\n\n");
                    text.push_str(&self.catalogue.text(
                        locale,
                        "notice.unsuitable_hint",
                        &[("id", &id.to_string())],
                    ));
                }
                text
            }
        }
    }

    /// A queue that could not be read is treated as empty for this round --
    /// the wishes in it then fall back on their other evidence, which is the
    /// same thing that happens when the download really has finished.
    async fn queue(&self, kind: MediaKind) -> Vec<QueueItem> {
        match self.insight.queue(kind).await {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!(error = %e, ?kind, "cannot read the queue");
                Vec::new()
            }
        }
    }

    fn notices_mut(&self) -> std::sync::RwLockWriteGuard<'_, Notices> {
        self.notices
            .write()
            .expect("the notices lock is never poisoned")
    }

    /// Writes a snapshot taken under the lock, outside of it. Returns
    /// whether it worked: a failed write is loud but never aborts a round --
    /// the other wishes still deserve their notices.
    fn persist(&self, snapshot: &Notices) -> bool {
        match snapshot.save(&self.settings.notices_file) {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    path = %self.settings.notices_file.display(),
                    "cannot write the notices file"
                );
                false
            }
        }
    }
}

/// Whether two reasons are the same CLASS -- the thing a person would be
/// told again about. `OnlyInLanguages` compares as a set: the same two
/// languages in the other order are not news.
pub fn same_class(a: &Reason, b: &Reason) -> bool {
    match (a, b) {
        (Reason::OnlyInLanguages(x), Reason::OnlyInLanguages(y)) => {
            let mut x = x.clone();
            let mut y = y.clone();
            x.sort();
            y.sort();
            x == y
        }
        _ => a == b,
    }
}

/// The one line a finished round logs, from the single place that logs it.
/// Both ways out of `round` come through here -- the ordinary one and the
/// one where Seerr could not be reached -- because the line says the LOOP
/// ran, not that Seerr answered.
fn heartbeat(report: &RoundReport) {
    tracing::info!(
        wishes = report.wishes,
        notices_sent = report.notices_sent,
        searches = report.searches,
        retries = report.retries,
        refreshes = report.refreshes,
        "{}",
        HEARTBEAT
    );
}

fn rfc3339(now: OffsetDateTime) -> String {
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
