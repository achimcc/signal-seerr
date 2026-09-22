//! Pure decision logic, without I/O: turning what Seerr and Radarr/Sonarr
//! say about a wish into one state a person can understand (`classify`), and
//! turning a recorded interactive search into a reason class (`reason_from`).
//! Callers gather the `Evidence` themselves; nothing in this module makes a
//! request.

use crate::arr::{ArrMovie, HistoryEvent, QueueItem, QueueState, Release};
use crate::model::{Reason, Wish, WishState};

/// Radarr's own fixed sentences in `Release::rejections`, recorded from a
/// running 3.2.0 instance on 2026-09-21 (see `tests/fixtures/README.md`).
/// Anything else falls through to `Reason::Otherwise` on purpose -- a wrong
/// precise reason is worse than a general one.
const TOO_LARGE_PATTERN: &str = "larger than maximum allowed";
const TOO_SMALL_PATTERN: &str = "smaller than minimum allowed";
const WRONG_QUALITY_PATTERN: &str = "is not wanted in profile";

/// What is currently known about a wish, gathered by the caller. `movie` is
/// `None` for a series, and for a movie where no Radarr insight is
/// configured; `classify` never produces `WishState::NotReleased` in that
/// case.
pub struct Evidence<'a> {
    pub movie: Option<&'a ArrMovie>,
    pub queue_item: Option<&'a QueueItem>,
    pub last_event: Option<HistoryEvent>,
    pub known_reason: Option<&'a Reason>,
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
    if wish.media_status == 4 {
        return WishState::PartlyAvailable;
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
    // a film Radarr was actually asked about. Without film evidence -- a
    // series, or any wish with no `[insight]` configured -- nothing was
    // measured, and the honest answer is that it is on the list.
    if ev.movie.is_some() {
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
    if releases.is_empty() {
        return Reason::NothingExists;
    }
    if releases.iter().any(|r| !r.rejected) {
        // Something acceptable already exists; the fetch is only pending.
        return Reason::Otherwise;
    }
    let candidates: Vec<&Release> = match wanted_languages {
        Some(wanted) => {
            let matching: Vec<&Release> =
                releases.iter().filter(|r| carries_any(r, wanted)).collect();
            if matching.is_empty() {
                return Reason::OnlyInLanguages(languages_by_frequency(releases));
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

/// The languages actually offered, most common first, capped at the two
/// most frequent -- `Reason::OnlyInLanguages` names at most two, never the
/// whole spread of a search that missed on every count. `"Portuguese
/// (Brazil)"` is counted separately from `"Portuguese"` -- that is how
/// Radarr sends it, and merging them would misrepresent what was searched.
fn languages_by_frequency(releases: &[Release]) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for language in releases.iter().flat_map(|r| &r.languages) {
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
        }
    }

    fn no_evidence() -> Evidence<'static> {
        Evidence {
            movie: None,
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
            WishState::PartlyAvailable
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
}
