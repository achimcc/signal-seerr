/// A Signal account id. Stable across username changes, which is exactly why
/// it and not the username is what we store: usernames are released and can
/// be taken by somebody else.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Aci(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeerrUserId(pub i64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Movie,
    Tv,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub tmdb_id: i64,
    pub kind: MediaKind,
    pub title: String,
    pub year: Option<u16>,
    pub rating: Option<f32>,
    /// 0 for movies.
    pub seasons: u16,
    /// Already available or already requested -- searcharr's "Already Added!".
    pub already: bool,
}

/// One quality profile as the *arr behind Seerr knows it.
///
/// The `id` is only ever meaningful together with the service it came from:
/// Radarr and Sonarr keep separate id spaces, and on 2026-09-09 both happened
/// to run 7..11 -- so a number taken from the wrong side looks entirely
/// plausible and picks a different profile. Everything that matches a profile
/// matches on `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualityProfile {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seasons {
    /// A movie: the field is not sent at all.
    NotApplicable,
    All,
    Only(Vec<u16>),
}

/// One request as Seerr's own wire format actually carries it -- not what
/// somebody believed it sends. Seerr never sends a title here; see
/// `Requests::title_for`.
#[derive(Clone, Debug, PartialEq)]
pub struct Wish {
    pub id: i64,
    pub kind: MediaKind,
    pub tmdb_id: i64,
    /// Seerr's MediaRequestStatus: 1 pending, 2 approved, 3 declined, 4 failed, 5 completed.
    pub request_status: i64,
    /// Seerr's MediaStatus: 3 processing, 4 partially available, 5 available.
    pub media_status: i64,
    /// `media.externalServiceId` -- the movie's id in Radarr / the series' in Sonarr.
    pub arr_id: Option<i64>,
    pub created_at: time::OffsetDateTime,
    pub profile_name: Option<String>,
    /// `requestedBy.jellyfinUsername` -- the one identity source (see CLAUDE.md).
    pub requested_by: Option<String>,
    /// How far along Seerr itself says a download is, from
    /// `media.downloadStatus[].{size,sizeLeft}` where an entry's `status` is
    /// `"downloading"` (recorded 2026-09-22, see `tests/fixtures/README.md`).
    /// This is Seerr's OWN account of the download, independent of Radarr's
    /// queue -- the only download evidence available where no `[insight]`
    /// is configured at all.
    pub download_percent: Option<u8>,
}

/// Why a wish's search never found a suitable release. Comes only from
/// structured fields (Radarr's `languages`, a fixed sentence in
/// `rejections`), never from a custom format's own name -- see
/// `insight::reason_from`. Stored on disk (the notices file), hence serde.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "class", content = "detail", rename_all = "snake_case")]
pub enum Reason {
    NothingExists,
    OnlyInLanguages(Vec<String>),
    TooLarge,
    TooSmall,
    WrongQuality,
    Otherwise,
}

/// One state a person can be told about, derived from a wish and what is
/// currently known about it in Radarr/Sonarr. See `insight::classify`.
#[derive(Clone, Debug, PartialEq)]
pub enum WishState {
    Available,
    NotHandedOver,
    /// Seerr's MediaStatus 4: some of it is there, the rest is not. Only a
    /// series can be in this state.
    PartlyAvailable,
    ImportStuck,
    Downloading {
        percent: u8,
    },
    NotReleased {
        date: Option<time::Date>,
    },
    DownloadFailed,
    Unsuitable(Reason),
    Searching,
    /// Nothing is known beyond what Seerr says: no film evidence was
    /// gathered at all, because there is none to gather (a series -- Sonarr
    /// has no "movie" and no interactive search here) or because no
    /// `[insight]` is configured.
    ///
    /// Deliberately NOT `Searching`. "Still looking, nothing suitable so
    /// far" would be a claim about a measurement nobody made, and answering
    /// that for a wish nobody could fetch is the exact defect this whole
    /// feature exists to remove.
    Waiting,
}

impl WishState {
    /// The key under which "already told" is remembered; None = never
    /// announced unasked.
    ///
    /// `Waiting` and `PartlyAvailable` are None on purpose: the first has no
    /// measurement behind it (see the variant), and the second is not a
    /// problem -- part of the series is there and the rest is on its way.
    pub fn notice_class(&self) -> Option<&'static str> {
        match self {
            WishState::Searching | WishState::Unsuitable(_) => Some("unsuitable"),
            WishState::DownloadFailed => Some("download_failed"),
            WishState::ImportStuck => Some("import_stuck"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A state the bot cannot back with a measurement is never announced
    /// unasked. `Waiting` means "no film evidence was gathered at all" (a
    /// series, or a wish with no insight configured) and `PartlyAvailable`
    /// means part of it is already there -- neither is a problem somebody
    /// needs to be woken up about, and the unasked "nothing suitable so
    /// far" would be a claim about a search that never happened.
    #[test]
    fn the_two_states_without_film_evidence_are_never_announced_unasked() {
        assert_eq!(WishState::Waiting.notice_class(), None);
        assert_eq!(WishState::PartlyAvailable.notice_class(), None);
    }

    #[test]
    fn reason_only_in_languages_round_trips_through_json() {
        let reason = Reason::OnlyInLanguages(vec!["Portuguese".to_string()]);
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(
            serde_json::from_str::<Reason>(&json).unwrap(),
            reason,
            "round trip of {json}"
        );
    }

    #[test]
    fn reason_unit_variant_round_trips_through_json() {
        let reason = Reason::NothingExists;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(
            serde_json::from_str::<Reason>(&json).unwrap(),
            reason,
            "round trip of {json}"
        );
    }
}
