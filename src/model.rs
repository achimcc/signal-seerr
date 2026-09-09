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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingState {
    Waiting,
    Fetching,
    Available,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub id: i64,
    pub title: String,
    pub state: PendingState,
}
