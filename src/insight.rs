//! Pure decision logic, without I/O: turning what Seerr and Radarr/Sonarr
//! say about a wish into one state a person can understand (`classify`), and
//! turning a recorded interactive search into a reason class (`reason_from`).
//! Callers gather the `Evidence` themselves; nothing in this module makes a
//! request.

use crate::arr::{ArrMovie, ArrSeries, HistoryEvent, QueueItem, QueueState, Release};
use crate::model::{PartCounts, Reason, Wish, WishState};

/// Radarr's own fixed sentences in `Release::rejections`, recorded from a
/// running 3.2.0 instance on 2026-09-21 (see `tests/fixtures/README.md`).
/// Anything else falls through to `Reason::Otherwise` on purpose -- a wrong
/// precise reason is worse than a general one.
const TOO_LARGE_PATTERN: &str = "larger than maximum allowed";
const TOO_SMALL_PATTERN: &str = "smaller than minimum allowed";
const WRONG_QUALITY_PATTERN: &str = "is not wanted in profile";

/// Sonarr's season search fans out wide and comes back with releases that
/// belong to OTHER series -- 468 of the 662 in the recording of 2026-09-24
/// (`sonarr-release-season-all-rejected.json`), each rejected with one of
/// these two sentences. They say nothing about the series asked about, so
/// they are dropped before anything is judged. Radarr never sends either.
const FOREIGN_PATTERNS: [&str; 2] = ["Unknown Series", "matches an alias for series with TVDB ID"];

/// Two of the names Sonarr puts in `languages[].name` that are not
/// languages anything is "only" available in: `Unknown` (324 of 662 in the
/// same recording) and `Original`. Counted, they would have produced "so far
/// only in Unknown" as a sentence to a person.
const NOT_A_LANGUAGE: [&str; 2] = ["Unknown", "Original"];

/// What is currently known about a wish, gathered by the caller. `movie` is
/// `None` for a series, and for a movie where no Radarr insight is
/// configured; `classify` never produces `WishState::NotReleased` in that
/// case.
pub struct Evidence<'a> {
    pub movie: Option<&'a ArrMovie>,
    /// `None` for a movie, and for a series where no Sonarr insight is
    /// configured or the lookup failed. Then a series is judged from Seerr
    /// and the queue alone, as before.
    pub series: Option<&'a SeriesFacts>,
    pub queue_item: Option<&'a QueueItem>,
    pub last_event: Option<HistoryEvent>,
    pub known_reason: Option<&'a Reason>,
}

/// What the episodes of a series say, counted over the ones that COUNT: in
/// a wished season (or any monitored season, if no seasons were named),
/// with both season and episode monitored. Pure; built once by
/// `series_facts` and read by `classify` and by the watcher's clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesFacts {
    /// Counting episodes whose air date is known and past.
    pub aired: u32,
    /// Counting episodes on file.
    pub have: u32,
    /// The oldest aired-but-missing episode: when it aired, and its season.
    /// `None` when nothing is missing. The season is where a reason search
    /// would look; the date is where the stall clock starts.
    pub missing_oldest: Option<(time::OffsetDateTime, u16)>,
    /// The first counting episode still ahead, if Sonarr knows a date.
    pub next_air: Option<time::OffsetDateTime>,
}

/// Counts what is there and what is missing. An episode without an air
/// date is neither aired nor missing: Sonarr does not know when it comes,
/// so neither does this bot -- it only shows up nowhere, never as "missing
/// since 1970".
pub fn series_facts(series: &ArrSeries, wished: &[u16], now: time::OffsetDateTime) -> SeriesFacts {
    let counts = |season: u16| {
        series.monitored_seasons.contains(&season)
            && (wished.is_empty() || wished.contains(&season))
    };
    let mut facts = SeriesFacts {
        aired: 0,
        have: 0,
        missing_oldest: None,
        next_air: None,
    };
    for episode in series
        .episodes
        .iter()
        .filter(|e| e.monitored && counts(e.season))
    {
        let Some(air) = episode.air_date else {
            continue;
        };
        if air > now {
            if facts.next_air.is_none_or(|next| air < next) {
                facts.next_air = Some(air);
            }
            continue;
        }
        facts.aired += 1;
        if episode.has_file {
            facts.have += 1;
        } else if facts.missing_oldest.is_none_or(|(oldest, _)| air < oldest) {
            facts.missing_oldest = Some((air, episode.season));
        }
    }
    facts
}

/// Turns what Seerr and Radarr/Sonarr say about one wish into a single state
/// a person can be told about. Order matters: the first matching row wins.
pub fn classify(wish: &Wish, ev: &Evidence, now: time::OffsetDateTime) -> WishState {
    if wish.media_status == 5 {
        return WishState::Available;
    }
    // ONLY request status 4 (FAILED). A MISSING `arr_id` IS NOT A FAILURE:
    // Seerr fills `media.externalServiceId` when it hands the wish over,
    // which is a moment or two after the request is placed -- and that is
    // exactly when somebody types `/status` about the thing they just asked
    // for. Reading the empty field as "I couldn't put it on the list" told
    // them their wish had failed while it was on its way. Without an
    // `arr_id` the caller gathers no film evidence either, so such a wish
    // falls through to `Waiting`: on the list, nothing measured yet.
    // Declined BEFORE failed: a declined wish was never handed over, so
    // there is no hand-over that could have failed.
    if wish.request_status == 3 {
        return WishState::Declined;
    }
    if wish.request_status == 4 {
        return WishState::NotHandedOver;
    }
    if let Some(item) = ev.queue_item {
        return match item.state {
            QueueState::ImportStuck => WishState::ImportStuck,
            QueueState::Downloading => WishState::Downloading {
                percent: item.percent,
            },
        };
    }
    // Below the queue rows: something coming down right now says more than
    // "half of it is here".
    if let Some(facts) = ev.series {
        // With episode evidence, a series is judged on its episodes. Seerr's
        // own media status 4 is folded into the same rows below -- what the
        // episodes say is the finer measurement of the same thing.
        if facts.aired == 0 {
            return WishState::NotAired {
                date: facts.next_air.map(time::OffsetDateTime::date),
            };
        }
        if facts.missing_oldest.is_none() {
            // Every aired episode that counts is on file. Seerr still calls
            // it open, so more is to come -- or Seerr has not caught up.
            return WishState::PartlyAvailable {
                counts: Some(PartCounts {
                    have: facts.have,
                    aired: facts.aired,
                    next: facts.next_air.map(time::OffsetDateTime::date),
                }),
            };
        }
    } else if wish.media_status == 4 {
        return WishState::PartlyAvailable { counts: None };
    }
    if let Some(movie) = ev.movie {
        if !movie.is_available {
            return WishState::NotReleased {
                date: earliest_future_date(movie, now),
            };
        }
    }
    // No queue item at this point -- the row above already returned for one.
    if ev.last_event == Some(HistoryEvent::DownloadFailed) {
        return WishState::DownloadFailed;
    }
    if let Some(reason) = ev.known_reason {
        return WishState::Unsuitable(reason.clone());
    }
    // THE LAST TWO ROWS ARE THE WHOLE POINT OF `Waiting`. "Still looking,
    // nothing suitable so far" is only true where somebody looked: that is
    // a film Radarr was actually asked about, or a series whose episodes
    // Sonarr listed and some aired one is missing. Without either -- no
    // `[insight]`, no Sonarr, a failed lookup -- nothing was measured, and
    // the honest answer is that it is on the list.
    if ev.movie.is_some() || ev.series.is_some() {
        return WishState::Searching;
    }
    WishState::Waiting
}

/// The earlier of the two release dates that still lies in the future,
/// relative to `now`; `None` if there is none (both unset, or both already
/// past -- e.g. a release the Radarr sync has not caught up with yet).
fn earliest_future_date(movie: &ArrMovie, now: time::OffsetDateTime) -> Option<time::Date> {
    [movie.digital_release, movie.physical_release]
        .into_iter()
        .flatten()
        .filter(|date| *date > now)
        .min()
        .map(time::OffsetDateTime::date)
}

/// Turns one recorded interactive search into a single reason class, using
/// only the *structured* fields -- the free text in `rejections` never
/// becomes a class by itself, since it carries the operator's own
/// custom-format names, which a public bot must not repeat.
pub fn reason_from(releases: &[Release], wanted_languages: Option<&[String]>) -> Reason {
    // Other series' releases first (see `FOREIGN_PATTERNS`): a search that
    // returned nothing but strangers found nothing.
    let own: Vec<&Release> = releases.iter().filter(|r| !is_foreign(r)).collect();
    if own.is_empty() {
        return Reason::NothingExists;
    }
    let releases: Vec<Release> = own.into_iter().cloned().collect();
    let releases = releases.as_slice();
    if releases.iter().any(|r| !r.rejected) {
        // Something acceptable already exists; the fetch is only pending.
        return Reason::Otherwise;
    }
    let candidates: Vec<&Release> = match wanted_languages {
        Some(wanted) => {
            let matching: Vec<&Release> =
                releases.iter().filter(|r| carries_any(r, wanted)).collect();
            if matching.is_empty() {
                let offered = languages_by_frequency(releases);
                // Nothing but `Unknown`/`Original` on offer: that is not "only
                // in <language>", it is "in versions we don't take" -- never
                // an empty list in a sentence.
                return if offered.is_empty() {
                    Reason::Otherwise
                } else {
                    Reason::OnlyInLanguages(offered)
                };
            }
            matching
        }
        None => releases.iter().collect(),
    };
    if candidates.iter().all(|r| has_pattern(r, TOO_LARGE_PATTERN)) {
        return Reason::TooLarge;
    }
    if candidates.iter().all(|r| has_pattern(r, TOO_SMALL_PATTERN)) {
        return Reason::TooSmall;
    }
    if candidates
        .iter()
        .all(|r| has_pattern(r, WRONG_QUALITY_PATTERN))
    {
        return Reason::WrongQuality;
    }
    Reason::Otherwise
}

/// Whether `release` carries any of the `wanted` languages, compared
/// case-insensitively (ASCII only -- these are Radarr's own English names).
fn carries_any(release: &Release, wanted: &[String]) -> bool {
    release
        .languages
        .iter()
        .any(|lang| wanted.iter().any(|w| w.eq_ignore_ascii_case(lang)))
}

fn has_pattern(release: &Release, pattern: &str) -> bool {
    release.rejections.iter().any(|r| r.contains(pattern))
}

fn is_foreign(release: &Release) -> bool {
    FOREIGN_PATTERNS.iter().any(|p| has_pattern(release, p))
}

/// The languages actually offered, most common first, capped at the two
/// most frequent -- `Reason::OnlyInLanguages` names at most two, never the
/// whole spread of a search that missed on every count. `"Portuguese
/// (Brazil)"` is counted separately from `"Portuguese"` -- that is how
/// Radarr sends it, and merging them would misrepresent what was searched.
fn languages_by_frequency(releases: &[Release]) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for language in releases
        .iter()
        .flat_map(|r| &r.languages)
        .filter(|l| !NOT_A_LANGUAGE.iter().any(|n| n.eq_ignore_ascii_case(l)))
    {
        match counts.iter_mut().find(|(name, _)| name == language) {
            Some(entry) => entry.1 += 1,
            None => counts.push((language.clone(), 1)),
        }
    }
    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts.into_iter().map(|(name, _)| name).take(2).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaKind;
    use time::Duration;

    fn wish() -> Wish {
        Wish {
            id: 1,
            kind: MediaKind::Movie,
            tmdb_id: 100001,
            request_status: 2,
            media_status: 3,
            arr_id: Some(42),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            profile_name: None,
            requested_by: None,
            download_percent: None,
            seasons: Vec::new(),
        }
    }

    fn no_evidence() -> Evidence<'static> {
        Evidence {
            movie: None,
            series: None,
            queue_item: None,
            last_event: None,
            known_reason: None,
        }
    }

    // -- classify: one test per row of the order, plus the precedence cases
    // -- named in the brief.

    #[test]
    fn media_status_5_is_available_even_with_a_queue_item() {
        let mut w = wish();
        w.media_status = 5;
        let item = QueueItem {
            arr_id: 42,
            percent: 10,
            state: QueueState::Downloading,
        };
        let ev = Evidence {
            queue_item: Some(&item),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, time::OffsetDateTime::UNIX_EPOCH),
            WishState::Available
        );
    }

    #[test]
    fn request_status_4_is_not_handed_over() {
        let mut w = wish();
        w.request_status = 4;
        assert_eq!(
            classify(&w, &no_evidence(), time::OffsetDateTime::UNIX_EPOCH),
            WishState::NotHandedOver
        );
    }

    #[test]
    fn a_missing_arr_id_while_the_wish_is_being_handed_over_is_waiting() {
        // Seerr fills `externalServiceId` on the hand-over, a moment or two
        // after the request is placed -- and that is precisely when somebody
        // asks `/status` about what they just wished for. Answering "I
        // couldn't put it on the list" there tells them their wish failed
        // while it is on its way. Status 1 is pending, status 2 approved;
        // both are open, not failed.
        for status in [1, 2] {
            let mut w = wish();
            w.arr_id = None;
            w.request_status = status;
            assert_eq!(
                classify(&w, &no_evidence(), time::OffsetDateTime::UNIX_EPOCH),
                WishState::Waiting,
                "request_status {status}"
            );
        }
    }

    #[test]
    fn a_missing_arr_id_does_not_make_a_failed_wish_anything_else() {
        // The other half of the same rule: status 4 keeps its own answer
        // whether or not an id was ever written.
        let mut w = wish();
        w.arr_id = None;
        w.request_status = 4;
        assert_eq!(
            classify(&w, &no_evidence(), time::OffsetDateTime::UNIX_EPOCH),
            WishState::NotHandedOver
        );
    }

    #[test]
    fn import_stuck_beats_not_released() {
        let w = wish();
        let item = QueueItem {
            arr_id: 42,
            percent: 0,
            state: QueueState::ImportStuck,
        };
        let movie = ArrMovie {
            is_available: false,
            has_file: false,
            digital_release: None,
            physical_release: None,
        };
        let ev = Evidence {
            movie: Some(&movie),
            queue_item: Some(&item),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, time::OffsetDateTime::UNIX_EPOCH),
            WishState::ImportStuck
        );
    }

    #[test]
    fn downloading_carries_the_percent() {
        let w = wish();
        let item = QueueItem {
            arr_id: 42,
            percent: 40,
            state: QueueState::Downloading,
        };
        let ev = Evidence {
            queue_item: Some(&item),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, time::OffsetDateTime::UNIX_EPOCH),
            WishState::Downloading { percent: 40 }
        );
    }

    #[test]
    fn not_released_picks_the_earlier_future_date() {
        let w = wish();
        let now = time::OffsetDateTime::UNIX_EPOCH + Duration::days(100);
        let digital = time::OffsetDateTime::UNIX_EPOCH + Duration::days(200);
        let physical = time::OffsetDateTime::UNIX_EPOCH + Duration::days(150);
        let movie = ArrMovie {
            is_available: false,
            has_file: false,
            digital_release: Some(digital),
            physical_release: Some(physical),
        };
        let ev = Evidence {
            movie: Some(&movie),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, now),
            WishState::NotReleased {
                date: Some(physical.date())
            }
        );
    }

    #[test]
    fn not_released_with_both_dates_in_the_past_has_no_date() {
        let w = wish();
        let now = time::OffsetDateTime::UNIX_EPOCH + Duration::days(300);
        let digital = time::OffsetDateTime::UNIX_EPOCH + Duration::days(200);
        let physical = time::OffsetDateTime::UNIX_EPOCH + Duration::days(150);
        let movie = ArrMovie {
            is_available: false,
            has_file: false,
            digital_release: Some(digital),
            physical_release: Some(physical),
        };
        let ev = Evidence {
            movie: Some(&movie),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, now),
            WishState::NotReleased { date: None }
        );
    }

    #[test]
    fn without_a_movie_not_released_is_never_produced() {
        // A series, or a movie with no [insight] configured: `ev.movie` is
        // `None`, so classify falls straight through the NotReleased row.
        let mut w = wish();
        w.kind = MediaKind::Tv;
        assert_ne!(
            classify(&w, &no_evidence(), time::OffsetDateTime::UNIX_EPOCH),
            WishState::NotReleased { date: None }
        );
    }

    #[test]
    fn media_status_4_is_partly_available() {
        let mut w = wish();
        w.media_status = 4;
        assert_eq!(
            classify(&w, &no_evidence(), time::OffsetDateTime::UNIX_EPOCH),
            WishState::PartlyAvailable { counts: None }
        );
    }

    #[test]
    fn a_queue_item_beats_partly_available() {
        // Half a series being there says less than the part that is coming
        // down right now: the queue row stands above the media status.
        let mut w = wish();
        w.media_status = 4;
        let item = QueueItem {
            arr_id: 42,
            percent: 60,
            state: QueueState::Downloading,
        };
        let ev = Evidence {
            queue_item: Some(&item),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, time::OffsetDateTime::UNIX_EPOCH),
            WishState::Downloading { percent: 60 }
        );
    }

    #[test]
    fn a_series_with_an_empty_queue_is_waiting_not_searching() {
        // THE POINT OF `Waiting`. There is no film evidence for a series --
        // no `movie()` and no interactive search exist on the Sonarr side --
        // so "still looking, nothing suitable so far" would be a claim about
        // a measurement that was never made. That is the very defect this
        // whole feature removes; it must not be reintroduced by a default.
        let mut w = wish();
        w.kind = MediaKind::Tv;
        assert_eq!(
            classify(&w, &no_evidence(), time::OffsetDateTime::UNIX_EPOCH),
            WishState::Waiting
        );
    }

    #[test]
    fn a_film_without_configured_insight_is_waiting_too() {
        // Same reasoning for a movie when no [insight] is configured: the
        // caller gathers no evidence, so there is nothing to claim.
        assert_eq!(
            classify(&wish(), &no_evidence(), time::OffsetDateTime::UNIX_EPOCH),
            WishState::Waiting
        );
    }

    #[test]
    fn no_queue_item_and_download_failed() {
        let w = wish();
        let ev = Evidence {
            last_event: Some(HistoryEvent::DownloadFailed),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, time::OffsetDateTime::UNIX_EPOCH),
            WishState::DownloadFailed
        );
    }

    #[test]
    fn known_reason_is_unsuitable() {
        let w = wish();
        let reason = Reason::TooLarge;
        let ev = Evidence {
            known_reason: Some(&reason),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, time::OffsetDateTime::UNIX_EPOCH),
            WishState::Unsuitable(Reason::TooLarge)
        );
    }

    #[test]
    fn a_released_film_with_nothing_else_to_show_is_searching() {
        // Radarr was asked, it has the film, it is out, nothing is in the
        // queue and no attempt has failed: only NOW is "still looking"
        // something that was actually measured.
        let w = wish();
        let movie = ArrMovie {
            is_available: true,
            has_file: false,
            digital_release: None,
            physical_release: None,
        };
        let ev = Evidence {
            movie: Some(&movie),
            ..no_evidence()
        };
        assert_eq!(
            classify(&w, &ev, time::OffsetDateTime::UNIX_EPOCH),
            WishState::Searching
        );
    }

    // -- reason_from: constructed releases, plus the recorded search.

    fn release(rejected: bool, rejections: &[&str], languages: &[&str]) -> Release {
        Release {
            rejected,
            rejections: rejections.iter().map(|s| s.to_string()).collect(),
            languages: languages.iter().map(|s| s.to_string()).collect(),
        }
    }

    const RELEASES: &str =
        include_str!("../tests/fixtures/radarr-release-all-rejected-language.json");

    #[test]
    fn the_recorded_search_is_a_language_problem() {
        let releases: Vec<Release> = serde_json::from_str(RELEASES).unwrap();
        let wanted = vec!["German".to_string()];
        match reason_from(&releases, Some(&wanted)) {
            Reason::OnlyInLanguages(l) => {
                assert!(!l.is_empty() && l.len() <= 2);
                assert_eq!(l[0], "Portuguese");
            }
            other => panic!("expected a language reason, got {other:?}"),
        }
    }

    #[test]
    fn only_in_languages_caps_at_the_two_most_frequent() {
        // Three distinct non-wanted languages, none of them "German": five
        // releases in Portuguese, three in Spanish, one in French. The
        // reason may name at most two -- the two most frequent, in
        // frequency order -- never all three.
        let wanted = vec!["German".to_string()];
        let mut releases = Vec::new();
        for _ in 0..5 {
            releases.push(release(true, &[], &["Portuguese"]));
        }
        for _ in 0..3 {
            releases.push(release(true, &[], &["Spanish"]));
        }
        releases.push(release(true, &[], &["French"]));
        match reason_from(&releases, Some(&wanted)) {
            Reason::OnlyInLanguages(l) => {
                assert_eq!(l, vec!["Portuguese".to_string(), "Spanish".to_string()]);
            }
            other => panic!("expected a language reason, got {other:?}"),
        }
    }

    #[test]
    fn without_a_language_table_the_same_search_is_only_otherwise() {
        let releases: Vec<Release> = serde_json::from_str(RELEASES).unwrap();
        assert_eq!(reason_from(&releases, None), Reason::Otherwise);
    }

    #[test]
    fn empty_release_list_is_nothing_exists() {
        assert_eq!(reason_from(&[], None), Reason::NothingExists);
    }

    #[test]
    fn one_allowed_release_is_otherwise_even_with_a_language_mismatch() {
        // Something acceptable already exists -- the fetch is only pending,
        // never claim a language reason on top of that.
        let releases = vec![
            release(true, &[], &["Portuguese"]),
            release(false, &[], &["Portuguese"]),
        ];
        let wanted = vec!["German".to_string()];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::Otherwise);
    }

    #[test]
    fn all_wanted_language_releases_too_large_is_too_large() {
        let wanted = vec!["German".to_string()];
        let releases = vec![
            release(
                true,
                &["43.7 GB is larger than maximum allowed 12.8 GB (for Film A)"],
                &["German"],
            ),
            release(
                true,
                &["50.1 GB is larger than maximum allowed 12.8 GB (for Film A)"],
                &["German"],
            ),
            // Different language, unrelated rejection -- must be ignored,
            // since it does not carry the wanted language.
            release(
                true,
                &["Custom Formats Not German or English have score -35000 below Movie's profile minimum 10000"],
                &["Portuguese"],
            ),
        ];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::TooLarge);
    }

    #[test]
    fn all_wanted_language_releases_too_small_is_too_small() {
        let wanted = vec!["German".to_string()];
        let releases = vec![
            release(
                true,
                &["0.3 GB is smaller than minimum allowed 0.5 GB (for Film A)"],
                &["German"],
            ),
            release(
                true,
                &["0.2 GB is smaller than minimum allowed 0.5 GB (for Film A)"],
                &["German"],
            ),
        ];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::TooSmall);
    }

    #[test]
    fn all_wanted_language_releases_wrong_quality_is_wrong_quality() {
        let wanted = vec!["German".to_string()];
        let releases = vec![
            release(true, &["Bluray-480p is not wanted in profile"], &["German"]),
            release(true, &["BR-DISK is not wanted in profile"], &["German"]),
        ];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::WrongQuality);
    }

    #[test]
    fn mixed_patterns_among_wanted_language_releases_is_otherwise() {
        let wanted = vec!["German".to_string()];
        let releases = vec![
            release(
                true,
                &["43.7 GB is larger than maximum allowed 12.8 GB (for Film A)"],
                &["German"],
            ),
            release(true, &["Bluray-480p is not wanted in profile"], &["German"]),
        ];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::Otherwise);
    }

    #[test]
    fn without_a_language_table_patterns_are_judged_over_all_releases() {
        let releases = vec![
            release(
                true,
                &["43.7 GB is larger than maximum allowed 12.8 GB (for Film A)"],
                &["German"],
            ),
            release(
                true,
                &["50.1 GB is larger than maximum allowed 12.8 GB (for Film A)"],
                &["Portuguese"],
            ),
        ];
        assert_eq!(reason_from(&releases, None), Reason::TooLarge);
    }

    #[test]
    fn language_match_is_case_insensitive() {
        let wanted = vec!["german".to_string()];
        let releases = vec![release(
            true,
            &["Bluray-480p is not wanted in profile"],
            &["German"],
        )];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::WrongQuality);
    }

    // -- series: `series_facts` and the three new rows of `classify`.

    use crate::arr::{ArrEpisode, ArrSeries};

    fn episode(
        season: u16,
        number: u16,
        air: Option<time::OffsetDateTime>,
        has_file: bool,
    ) -> ArrEpisode {
        ArrEpisode {
            season,
            number,
            air_date: air,
            has_file,
            monitored: true,
        }
    }

    fn tv_wish(seasons: Vec<u16>) -> Wish {
        let mut w = wish();
        w.kind = MediaKind::Tv;
        w.seasons = seasons;
        w
    }

    const NOW: time::OffsetDateTime = time::macros::datetime!(2026-09-24 12:00 UTC);

    fn days(n: i64) -> Option<time::OffsetDateTime> {
        Some(NOW + Duration::days(n))
    }

    #[test]
    fn declined_comes_before_not_handed_over() {
        let mut w = wish();
        w.request_status = 3;
        assert_eq!(classify(&w, &no_evidence(), NOW), WishState::Declined);
        assert_eq!(WishState::Declined.notice_class(), None);
    }

    #[test]
    fn a_series_with_no_aired_episode_is_not_aired_with_the_first_air_date() {
        let series = ArrSeries {
            monitored: true,
            monitored_seasons: vec![1],
            episodes: vec![
                episode(1, 1, days(10), false),
                episode(1, 2, days(3), false),
            ],
        };
        let facts = series_facts(&series, &[1], NOW);
        assert_eq!(facts.aired, 0);
        let ev = Evidence {
            series: Some(&facts),
            ..no_evidence()
        };
        assert_eq!(
            classify(&tv_wish(vec![1]), &ev, NOW),
            WishState::NotAired {
                date: Some((NOW + Duration::days(3)).date())
            }
        );
    }

    #[test]
    fn a_series_with_all_aired_on_file_is_partly_available_with_counts_and_next() {
        let series = ArrSeries {
            monitored: true,
            monitored_seasons: vec![1],
            episodes: vec![
                episode(1, 1, days(-14), true),
                episode(1, 2, days(-7), true),
                episode(1, 3, days(7), false),
            ],
        };
        let facts = series_facts(&series, &[1], NOW);
        let ev = Evidence {
            series: Some(&facts),
            ..no_evidence()
        };
        // Even with Seerr saying 3 (processing): the episodes are the finer
        // measurement, and they say the rest is not out yet.
        assert_eq!(
            classify(&tv_wish(vec![1]), &ev, NOW),
            WishState::PartlyAvailable {
                counts: Some(PartCounts {
                    have: 2,
                    aired: 2,
                    next: Some((NOW + Duration::days(7)).date()),
                }),
            }
        );
        assert_eq!(
            WishState::PartlyAvailable { counts: None }.notice_class(),
            None
        );
    }

    #[test]
    fn a_series_with_a_missing_aired_episode_is_searching() {
        let series = ArrSeries {
            monitored: true,
            monitored_seasons: vec![1, 2],
            episodes: vec![
                episode(1, 1, days(-30), true),
                episode(2, 1, days(-20), false),
                episode(2, 2, days(-13), false),
            ],
        };
        let facts = series_facts(&series, &[1, 2], NOW);
        assert_eq!(facts.have, 1);
        assert_eq!(facts.aired, 3);
        // The OLDEST missing one: its air date starts the clock, its season
        // is where a search would look.
        assert_eq!(facts.missing_oldest, Some((NOW - Duration::days(20), 2)));
        let ev = Evidence {
            series: Some(&facts),
            ..no_evidence()
        };
        assert_eq!(
            classify(&tv_wish(vec![1, 2]), &ev, NOW),
            WishState::Searching
        );
        // ... and Seerr's own "partly available" does not override the
        // measured gap.
        let mut w = tv_wish(vec![1, 2]);
        w.media_status = 4;
        assert_eq!(classify(&w, &ev, NOW), WishState::Searching);
    }

    #[test]
    fn an_empty_wish_list_counts_every_monitored_season() {
        let series = ArrSeries {
            monitored: true,
            monitored_seasons: vec![1, 2],
            episodes: vec![
                episode(1, 1, days(-30), true),
                episode(2, 1, days(-20), false),
            ],
        };
        let facts = series_facts(&series, &[], NOW);
        assert_eq!((facts.have, facts.aired), (1, 2));
        // ... while naming season 1 alone leaves season 2 out entirely.
        let only_one = series_facts(&series, &[1], NOW);
        assert_eq!((only_one.have, only_one.aired), (1, 1));
        assert_eq!(only_one.missing_oldest, None);
    }

    #[test]
    fn an_episode_without_an_air_date_is_neither_aired_nor_missing() {
        let series = ArrSeries {
            monitored: true,
            monitored_seasons: vec![1],
            episodes: vec![episode(1, 1, days(-30), true), episode(1, 2, None, false)],
        };
        let facts = series_facts(&series, &[1], NOW);
        assert_eq!((facts.have, facts.aired), (1, 1));
        assert_eq!(facts.missing_oldest, None);
        assert_eq!(facts.next_air, None);
    }

    #[test]
    fn an_unmonitored_season_or_episode_does_not_count() {
        let mut unmonitored = episode(2, 1, days(-20), false);
        unmonitored.monitored = false;
        let series = ArrSeries {
            monitored: true,
            monitored_seasons: vec![1], // season 2 is not monitored at all
            episodes: vec![
                episode(1, 1, days(-30), true),
                episode(1, 2, days(-25), false),
                unmonitored,
                episode(2, 2, days(-10), false),
            ],
        };
        let facts = series_facts(&series, &[], NOW);
        assert_eq!((facts.have, facts.aired), (1, 2));
        assert_eq!(facts.missing_oldest, Some((NOW - Duration::days(25), 1)));
    }

    #[test]
    fn a_series_without_evidence_is_still_waiting_or_partly_available() {
        let w = tv_wish(vec![1]);
        assert_eq!(classify(&w, &no_evidence(), NOW), WishState::Waiting);
        let mut w4 = tv_wish(vec![1]);
        w4.media_status = 4;
        assert_eq!(
            classify(&w4, &no_evidence(), NOW),
            WishState::PartlyAvailable { counts: None }
        );
    }

    #[test]
    fn the_recorded_episodes_give_the_expected_facts() {
        let series: Vec<ArrEpisode> = {
            // Through the wire types of `arr`, the same way the client reads
            // them -- so the fixture, not a hand-built list, is what is judged.
            #[derive(serde::Deserialize)]
            struct E {
                #[serde(rename = "seasonNumber")]
                season: u16,
                #[serde(rename = "episodeNumber")]
                number: u16,
                #[serde(rename = "airDateUtc", with = "time::serde::rfc3339::option", default)]
                air: Option<time::OffsetDateTime>,
                #[serde(rename = "hasFile")]
                has_file: bool,
                monitored: bool,
            }
            let raw: Vec<E> = serde_json::from_str(include_str!(
                "../tests/fixtures/sonarr-episode-two-missing.json"
            ))
            .unwrap();
            raw.into_iter()
                .map(|e| ArrEpisode {
                    season: e.season,
                    number: e.number,
                    air_date: e.air,
                    has_file: e.has_file,
                    monitored: e.monitored,
                })
                .collect()
        };
        let series = ArrSeries {
            monitored: true,
            monitored_seasons: vec![1],
            episodes: series,
        };
        let facts = series_facts(&series, &[1], NOW);
        // Six episodes: 1-3 on file, 4 and 5 aired (2019) and missing, 6
        // moved to 2030.
        assert_eq!((facts.have, facts.aired), (3, 5));
        assert_eq!(facts.missing_oldest.map(|(_, s)| s), Some(1));
        assert_eq!(facts.next_air.map(|d| d.year()), Some(2030));
    }

    // -- reason_from: the season search's strangers and non-languages.

    const SEASON_RELEASES: &str =
        include_str!("../tests/fixtures/sonarr-release-season-all-rejected.json");

    #[test]
    fn foreign_releases_are_dropped_before_judging() {
        let releases: Vec<Release> = serde_json::from_str(SEASON_RELEASES).unwrap();
        let wanted = vec!["German".to_string()];
        // German releases exist and fall on the profile's own sentences
        // (custom format scores, "not wanted in profile" mixed) -- so the
        // verdict is the general one, and NOT "only in <language>", which
        // the 29 alias entries and 10 `Unknown Series` would have skewed.
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::Otherwise);
    }

    #[test]
    fn only_foreign_releases_is_nothing_exists() {
        let releases = vec![
            release(true, &["Unknown Series"], &["German"]),
            release(
                true,
                &["Series X matches an alias for series with TVDB ID: 0"],
                &["German"],
            ),
        ];
        assert_eq!(reason_from(&releases, None), Reason::NothingExists);
        let wanted = vec!["German".to_string()];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::NothingExists);
    }

    #[test]
    fn unknown_and_original_are_not_languages() {
        let wanted = vec!["German".to_string()];
        let releases = vec![
            release(true, &[], &["Unknown"]),
            release(true, &[], &["Unknown"]),
            release(true, &[], &["Original"]),
            release(true, &[], &["Spanish"]),
        ];
        assert_eq!(
            reason_from(&releases, Some(&wanted)),
            Reason::OnlyInLanguages(vec!["Spanish".to_string()])
        );
    }

    #[test]
    fn a_search_with_only_unknown_languages_is_otherwise_not_an_empty_list() {
        let wanted = vec!["German".to_string()];
        let releases = vec![
            release(true, &[], &["Unknown"]),
            release(true, &[], &["Original"]),
        ];
        assert_eq!(reason_from(&releases, Some(&wanted)), Reason::Otherwise);
    }
}
