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
}
