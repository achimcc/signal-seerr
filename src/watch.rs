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
use crate::insight::{classify, reason_from, Evidence};
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
}

#[derive(Debug, Default, PartialEq)]
pub struct RoundReport {
    pub wishes: usize,
    pub notices_sent: usize,
    pub searches: usize,
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

        let mut searched_this_round = false;
        for wish in &wishes {
            let queue = match wish.kind {
                MediaKind::Movie => &movie_queue,
                MediaKind::Tv => &tv_queue,
            };
            let queue_item = queue.iter().find(|item| Some(item.arr_id) == wish.arr_id);
            let outcome = self
                .consider(wish, queue_item, now, !searched_this_round)
                .await;
            searched_this_round |= outcome.searched;
            report.searches += usize::from(outcome.searched);
            report.notices_sent += usize::from(outcome.sent);
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
        let (seen_unreleased, mut title, reason, mut released_seen, told, last_notice) = {
            let mut notices = self.notices_mut();
            let note = notices.note_mut(wish.id, now);
            (
                note.seen_unreleased,
                note.title.clone(),
                note.reason.clone(),
                note.released_seen,
                note.told.clone(),
                note.last_notice,
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

        // A queue entry is evidence enough on its own. For a series it is
        // the ONLY evidence: there is no "movie" in Sonarr, and the
        // interactive search is a Radarr call.
        let mut movie = None;
        let mut last_event = None;
        if wish.kind == MediaKind::Movie && queue_item.is_none() {
            if let Some(arr_id) = wish.arr_id {
                match self.insight.movie(arr_id).await {
                    Ok(found) => movie = Some(found),
                    Err(e) => {
                        // Half the evidence is worse than none: it would
                        // read as "still searching" and go out as a message.
                        tracing::warn!(error = %e, id = wish.id, "cannot ask radarr about this film");
                        return outcome;
                    }
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

                match self.insight.last_event(MediaKind::Movie, arr_id).await {
                    Ok(found) => last_event = found,
                    Err(e) => {
                        tracing::warn!(error = %e, id = wish.id, "cannot read this film's history");
                        return outcome;
                    }
                }
            }
        }

        let mut state = classify(
            wish,
            &Evidence {
                movie: movie.as_ref(),
                series: None,
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
        let since = std::cmp::max(wish.created_at, released_seen.unwrap_or(wish.created_at));
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

        let search_target = if matches!(state, WishState::Searching)
            && reason.is_none()
            && wish.kind == MediaKind::Movie
        {
            self.search.as_ref().zip(wish.arr_id)
        } else {
            None
        };
        if let Some((search, arr_id)) = search_target {
            if !may_search_now {
                return outcome;
            }
            // Counted BEFORE the call and written out at once: a search that
            // reached the indexers and then timed out has still been made,
            // and a budget that only counts successes is no budget.
            let snapshot = {
                let mut notices = self.notices_mut();
                if !notices.may_search(now, self.settings.max_searches_per_day) {
                    return outcome;
                }
                notices.count_search(now);
                notices.clone()
            };
            outcome.searched = true;
            self.persist(&snapshot);

            match search.releases(arr_id).await {
                Ok(releases) => {
                    let wanted = wish
                        .profile_name
                        .as_ref()
                        .and_then(|name| self.settings.profile_languages.get(name))
                        .map(|languages| languages.as_slice());
                    let found = reason_from(&releases, wanted);
                    let snapshot = {
                        let mut notices = self.notices_mut();
                        let note = notices.note_mut(wish.id, now);
                        note.reason = Some(found.clone());
                        note.searched_at = Some(now);
                        notices.clone()
                    };
                    // The result is on disk before anybody is told about it.
                    // If it could not be written, the message waits: one
                    // notice late is cheaper than a second indexer search.
                    if !self.persist(&snapshot) {
                        return outcome;
                    }
                    state = WishState::Unsuitable(found);
                }
                Err(e) => {
                    tracing::warn!(error = %e, id = wish.id, "the interactive search failed");
                    return outcome;
                }
            }
        }

        let title =
            title.unwrap_or_else(|| self.catalogue.text(entry.locale, "status.untitled", &[]));
        let text = self.notice_text(entry.locale, class, &title, &state, wish.id);
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

    /// The message for one class -- `WishState::notice_class` yields exactly
    /// three, and the last arm takes "unsuitable".
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
            _ => {
                let mut text = self.catalogue.text(
                    locale,
                    "notice.unsuitable",
                    &[
                        ("title", title),
                        ("state", &state_text(&self.catalogue, locale, state)),
                    ],
                );
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

/// The one line a finished round logs, from the single place that logs it.
/// Both ways out of `round` come through here -- the ordinary one and the
/// one where Seerr could not be reached -- because the line says the LOOP
/// ran, not that Seerr answered.
fn heartbeat(report: &RoundReport) {
    tracing::info!(
        wishes = report.wishes,
        notices_sent = report.notices_sent,
        searches = report.searches,
        "{}",
        HEARTBEAT
    );
}

fn rfc3339(now: OffsetDateTime) -> String {
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
