use crate::directory::Directory;
use crate::i18n::{Catalogue, Locale};
use crate::model::{Aci, Hit, MediaKind, PendingState, QualityProfile, Seasons, SeerrUserId};
use crate::seerr::Requests;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const PAGE: usize = 5;
/// After this, a bare "2" is a search for "2" again.
const RESULTS_LIVE: Duration = Duration::from_secs(600);
/// How long a stranger is left alone after being told once.
const STRANGER_QUIET: Duration = Duration::from_secs(3600);

/// Strips a known command prefix (matched case-insensitively against
/// `lowered`) and returns the ORIGINAL-case remainder, trimmed.
///
/// The prefix's own byte length is used to slice `text` -- never a length
/// derived from `lowered`. Lowercasing is not byte-length preserving ('ẞ' is
/// 3 bytes and lowercases to the 2-byte 'ß'; 'İ' is 2 bytes and lowercases to
/// the 3-byte 'i̇'), so a length taken from the lowercased copy can land a
/// slice off a UTF-8 character boundary (panic) or make the byte count run
/// backwards (subtraction overflow, also a panic) -- reachable from a single
/// message containing such a character after the command word.
fn strip_command<'a>(text: &'a str, lowered: &str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes
        .iter()
        .find(|p| lowered.starts_with(**p))
        .map(|p| text[p.len()..].trim())
}

/// Drops every stranger entry older than `STRANGER_QUIET` as of `now`. A
/// free function taking `now` explicitly (rather than calling
/// `Instant::now()` itself) so a test can drive it with a synthetic,
/// already-old `Instant` -- `Instant` supports `Duration` subtraction, so
/// this needs neither a real sleep nor a paused clock.
fn prune_stale_strangers(told: &mut HashMap<Aci, Instant>, now: Instant) {
    told.retain(|_, at| now.duration_since(*at) < STRANGER_QUIET);
}

#[derive(Clone, Debug)]
pub enum Conversation {
    Idle,
    Results {
        query: String,
        kind: Option<MediaKind>,
        page: u32,
        hits: Vec<Hit>,
        at: Instant,
    },
    Seasons {
        hit: Hit,
        at: Instant,
    },
    /// The profile question, the last step before the wish goes out (§4.2).
    /// It carries the seasons already answered, so a series does not have to
    /// be asked twice.
    Profile {
        hit: Hit,
        seasons: Seasons,
        choices: Vec<QualityProfile>,
        at: Instant,
    },
}

pub struct Dialog<R: Requests, D: Directory> {
    seerr: R,
    directory: D,
    catalogue: Catalogue,
    /// Where a person goes to enter their Signal name -- substituted into
    /// error.unknown_sender. Deployment configuration, not translatable
    /// prose: see the note on Config::settings_url.
    settings_url: String,
    /// Who to ask when a group is missing -- substituted into
    /// error.not_allowed. Same reasoning as settings_url.
    operator_name: String,
    /// The profile names to offer, IN THE ORDER THEY ARE OFFERED IN.
    ///
    /// Deployment configuration, and deliberately not Seerr's own ordering:
    /// otherwise "3" means something else the week somebody adds a profile,
    /// and the person who learned to type it gets a different film. Names
    /// that no *arr knows are dropped with a line in the journal; an empty
    /// list means "offer whatever Seerr lists, in Seerr's order".
    quality_profiles: Vec<String>,
    conversations: HashMap<Aci, Conversation>,
    told_strangers: HashMap<Aci, Instant>,
}

impl<R: Requests, D: Directory> Dialog<R, D> {
    pub fn new(
        seerr: R,
        directory: D,
        catalogue: Catalogue,
        settings_url: String,
        operator_name: String,
        quality_profiles: Vec<String>,
    ) -> Self {
        Dialog {
            seerr,
            directory,
            catalogue,
            settings_url,
            operator_name,
            quality_profiles,
            conversations: HashMap::new(),
            told_strangers: HashMap::new(),
        }
    }

    /// Lets a test look at what got placed through the fake `Requests`
    /// without `Dialog` itself exposing anything about `R`. Task 11 needs
    /// this to check what selecting a digit or answering the seasons
    /// question actually sent to Seerr.
    pub fn seerr_ref(&self) -> &R {
        &self.seerr
    }

    /// Test seam: inserts `conversation` as though it had been created
    /// `age` ago, so a test can exercise the `RESULTS_LIVE` expiry directly
    /// instead of asserting a property that a missing conversation would
    /// already satisfy on its own. `#[cfg(test)]` means this never ships --
    /// and, being compiled only for the crate's own unit tests, it is not
    /// reachable from `tests/dialog.rs` either, which is why the two tests
    /// that use it live next to this method instead.
    #[cfg(test)]
    fn insert_conversation_aged(
        &mut self,
        from: &Aci,
        mut conversation: Conversation,
        age: Duration,
    ) {
        let at = Instant::now()
            .checked_sub(age)
            .expect("age must not exceed how long this process has been up");
        match &mut conversation {
            Conversation::Idle => {}
            Conversation::Results { at: a, .. }
            | Conversation::Seasons { at: a, .. }
            | Conversation::Profile { at: a, .. } => *a = at,
        }
        self.conversations.insert(from.clone(), conversation);
    }

    pub async fn handle(&mut self, from: &Aci, text: &str) -> Vec<String> {
        let Some(member) = self.directory.lookup(from) else {
            return self.tell_stranger_once(from);
        };
        let locale = member.locale;
        if !member.allowed {
            return vec![self.catalogue.text(
                locale,
                "error.not_allowed",
                &[("operator", self.operator_name.as_str())],
            )];
        }

        let text = text.trim();
        let lowered = text.to_lowercase();

        // /abbruch and /hilfe are the only two ways OUT of a stuck
        // conversation, so they must work no matter what is open: answering
        // "which seasons?" to someone who typed /abbruch (wants out) or
        // /hilfe (is stuck and asking what they can type) would trap them
        // in exactly the question they are trying to escape or understand.
        // Every other command below is deliberately swallowed by an open
        // seasons question instead -- do not "tidy" these two back down into
        // that general case.
        match lowered.as_str() {
            "/hilfe" | "/help" => return vec![self.catalogue.text(locale, "help.body", &[])],
            "/abbruch" | "/cancel" => {
                self.conversations.remove(from);
                return vec![];
            }
            _ => {}
        }

        // An open seasons question swallows the next message, whatever it is
        // -- /status, /weg, "m", a bare title, all of it -- barring the two
        // escapes handled above.
        if let Some(Conversation::Seasons { hit, at }) = self.conversations.get(from).cloned() {
            if at.elapsed() <= RESULTS_LIVE {
                return self.place_series(from, locale, hit, text).await;
            }
            self.conversations.remove(from);
        }

        // And so does an open profile question, for the same reason.
        if let Some(Conversation::Profile {
            hit,
            seasons,
            choices,
            at,
        }) = self.conversations.get(from).cloned()
        {
            if at.elapsed() <= RESULTS_LIVE {
                return self
                    .answer_profile(from, locale, hit, seasons, &choices, text)
                    .await;
            }
            self.conversations.remove(from);
        }

        if let Ok(choice) = text.parse::<usize>() {
            if let Some(Conversation::Results { hits, at, .. }) =
                self.conversations.get(from).cloned()
            {
                if at.elapsed() <= RESULTS_LIVE {
                    return self.choose(from, locale, &hits, choice).await;
                }
                // Stale list: fall through, so "2" searches for "2" again.
                self.conversations.remove(from);
            }
        }

        match lowered.as_str() {
            "m" | "mehr" | "more" => return self.next_page(from, locale).await,
            "/status" => return self.status(from, locale).await,
            _ => {}
        }

        if let Some(query) = strip_command(text, &lowered, &["/film ", "/movie "]) {
            return self
                .search(from, locale, query, Some(MediaKind::Movie), 1)
                .await;
        }
        if let Some(query) = strip_command(text, &lowered, &["/serie ", "/series "]) {
            return self
                .search(from, locale, query, Some(MediaKind::Tv), 1)
                .await;
        }

        if lowered == "/weg"
            || lowered == "/withdraw"
            || lowered.starts_with("/weg ")
            || lowered.starts_with("/withdraw ")
        {
            return self.withdraw(from, locale, text).await;
        }

        self.search(from, locale, text, None, 1).await
    }

    async fn choose(
        &mut self,
        from: &Aci,
        locale: Locale,
        hits: &[Hit],
        choice: usize,
    ) -> Vec<String> {
        let Some(hit) = choice.checked_sub(1).and_then(|i| hits.get(i)).cloned() else {
            return vec![self.catalogue.text(locale, "error.not_understood", &[])];
        };
        if hit.already {
            self.conversations.remove(from);
            return vec![self
                .catalogue
                .text(locale, "request.already", &[("title", &hit.title)])];
        }

        if matches!(hit.kind, MediaKind::Tv) {
            let question =
                self.catalogue
                    .text(locale, "request.seasons_question", &[("title", &hit.title)]);
            self.conversations.insert(
                from.clone(),
                Conversation::Seasons {
                    hit,
                    at: Instant::now(),
                },
            );
            return vec![question];
        }

        self.ask_profile(from, locale, hit, Seasons::NotApplicable)
            .await
    }

    async fn place_series(
        &mut self,
        from: &Aci,
        locale: Locale,
        hit: Hit,
        answer: &str,
    ) -> Vec<String> {
        let lowered = answer.trim().to_lowercase();
        let seasons = if lowered == "alle" || lowered == "all" {
            Seasons::All
        } else {
            let wanted: Option<Vec<u16>> = lowered
                .split_whitespace()
                .map(|t| t.parse::<u16>().ok())
                .collect();
            match wanted {
                // A season that does not exist would be accepted by Seerr and
                // then sit in the list for ever, waiting for something that
                // is never coming.
                Some(list)
                    if !list.is_empty() && list.iter().all(|s| *s >= 1 && *s <= hit.seasons) =>
                {
                    Seasons::Only(list)
                }
                _ => {
                    return vec![self.catalogue.text(
                        locale,
                        "request.seasons_question",
                        &[("title", &hit.title)],
                    )]
                }
            }
        };
        self.ask_profile(from, locale, hit, seasons).await
    }

    /// The lookup `place`, `status`, and `withdraw` all need before they can
    /// do anything: the directory entry for `from`, and the Seerr account
    /// that goes with it. Before this existed, two of those three copies
    /// collapsed "no directory entry" (silent, `vec![]`) into "no Seerr
    /// account or Seerr unreachable" (`error.seerr_down`, logged) instead of
    /// keeping them apart the way `place`'s copy did -- an inconsistency
    /// that cost nothing today but would have bitten whoever next copied the
    /// "wrong" one of the three.
    async fn seerr_user(&mut self, from: &Aci, locale: Locale) -> Result<SeerrUserId, Vec<String>> {
        let Some(member) = self.directory.lookup(from) else {
            return Err(vec![]);
        };
        match self.seerr.user_id(&member.authentik_username).await {
            Ok(Some(id)) => Ok(id),
            Ok(None) => {
                tracing::warn!(user = member.authentik_username, "no seerr account");
                Err(vec![self.catalogue.text(locale, "error.seerr_down", &[])])
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot look up the seerr account");
                Err(vec![self.catalogue.text(locale, "error.seerr_down", &[])])
            }
        }
    }

    /// Asks which profile -- or places the wish straight away when there is
    /// nothing to choose between.
    ///
    /// A wish must never fail because a question could not be asked: if Seerr
    /// cannot be reached for the list, or the list is empty, or none of the
    /// configured names exists, the request goes out carrying no `profileId`
    /// and the *arr applies its own default. That is exactly what happened
    /// before this question existed.
    async fn ask_profile(
        &mut self,
        from: &Aci,
        locale: Locale,
        hit: Hit,
        seasons: Seasons,
    ) -> Vec<String> {
        let available = match self.seerr.quality_profiles(hit.kind).await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the quality profiles");
                Vec::new()
            }
        };
        let choices = self.order_profiles(available);
        if choices.is_empty() {
            return self.place(from, locale, &hit, seasons, None, &[]).await;
        }

        let question = self.render_profiles(&hit, &choices, locale);
        self.conversations.insert(
            from.clone(),
            Conversation::Profile {
                hit,
                seasons,
                choices,
                at: Instant::now(),
            },
        );
        vec![question]
    }

    /// Puts the offered profiles into the configured order, dropping the ones
    /// no *arr knows. An empty configuration means "take Seerr's own list".
    fn order_profiles(&self, available: Vec<QualityProfile>) -> Vec<QualityProfile> {
        if self.quality_profiles.is_empty() {
            return available;
        }
        let mut ordered = Vec::new();
        for wanted in &self.quality_profiles {
            match available.iter().find(|p| &p.name == wanted) {
                Some(found) => ordered.push(found.clone()),
                // Named in the configuration, unknown to the *arr. Dropping
                // it silently would renumber the list under the people who
                // learned it, so it is said out loud -- once per request,
                // which is cheap, and in the journal, where it belongs.
                None => tracing::warn!(profile = wanted, "no *arr knows this profile name"),
            }
        }
        ordered
    }

    fn render_profiles(&self, hit: &Hit, choices: &[QualityProfile], locale: Locale) -> String {
        let mut out =
            self.catalogue
                .text(locale, "request.profile_question", &[("title", &hit.title)]);
        out.push('\n');
        for (index, profile) in choices.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", index + 1, profile.name));
        }
        out
    }

    async fn answer_profile(
        &mut self,
        from: &Aci,
        locale: Locale,
        hit: Hit,
        seasons: Seasons,
        choices: &[QualityProfile],
        answer: &str,
    ) -> Vec<String> {
        let picked = answer
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| choices.get(i));
        let Some(profile) = picked else {
            // Repeat the question rather than fall through to a search for
            // "9" -- the same rule the seasons question follows.
            return vec![self.render_profiles(&hit, choices, locale)];
        };
        let id = profile.id;
        self.place(from, locale, &hit, seasons, Some(id), choices)
            .await
    }

    async fn place(
        &mut self,
        from: &Aci,
        locale: Locale,
        hit: &Hit,
        seasons: Seasons,
        profile_id: Option<i64>,
        // The list this conversation was just offered -- the confirmation
        // names the profile from here rather than asking Seerr again.
        choices: &[QualityProfile],
    ) -> Vec<String> {
        let user = match self.seerr_user(from, locale).await {
            Ok(id) => id,
            Err(message) => return message,
        };

        self.conversations.remove(from);
        let placed = self.seerr.request(hit, seasons, user, profile_id).await;
        let id = match placed {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "cannot place the request");
                return vec![self.catalogue.text(locale, "error.seerr_down", &[])];
            }
        };

        // Read back what the request CARRIES, rather than trusting what we
        // sent: an OverrideRule can replace a `profileId` silently
        // (MediaRequest.js:259-263). There are none today -- a measurement,
        // not a property of Seerr.
        let kept = match self.seerr.profile_of(id).await {
            Ok(kept) => kept,
            Err(e) => {
                tracing::warn!(error = %e, request = id, "cannot read back the profile");
                profile_id
            }
        };
        if kept != profile_id {
            tracing::warn!(
                sent = ?profile_id,
                kept = ?kept,
                request = id,
                "seerr kept a different profile than the one asked for"
            );
        }

        let id_text = id.to_string();
        let name = kept.and_then(|k| choices.iter().find(|p| p.id == k).map(|p| p.name.clone()));
        match name {
            Some(name) => vec![self.catalogue.text(
                locale,
                "request.placed_with_profile",
                &[
                    ("title", hit.title.as_str()),
                    ("id", id_text.as_str()),
                    ("profile", name.as_str()),
                ],
            )],
            None => vec![self.catalogue.text(
                locale,
                "request.placed",
                &[("title", &hit.title), ("id", &id.to_string())],
            )],
        }
    }

    async fn status(&mut self, from: &Aci, locale: Locale) -> Vec<String> {
        let user = match self.seerr_user(from, locale).await {
            Ok(id) => id,
            Err(message) => return message,
        };
        match self.seerr.pending(user).await {
            Ok(list) if list.is_empty() => {
                vec![self.catalogue.text(locale, "status.empty", &[])]
            }
            Ok(list) => vec![list
                .iter()
                .map(|p| {
                    let state_key = match p.state {
                        PendingState::Waiting => "status.waiting",
                        PendingState::Fetching => "status.fetching",
                        PendingState::Available => "status.available",
                    };
                    let state = self.catalogue.text(locale, state_key, &[]);
                    self.catalogue.text(
                        locale,
                        "status.line",
                        &[
                            ("id", &p.id.to_string()),
                            ("title", &p.title),
                            ("state", &state),
                        ],
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")],
            Err(e) => {
                tracing::warn!(error = %e, "cannot list requests");
                vec![self.catalogue.text(locale, "error.seerr_down", &[])]
            }
        }
    }

    async fn withdraw(&mut self, from: &Aci, locale: Locale, text: &str) -> Vec<String> {
        let Some(id) = text
            .split_whitespace()
            .nth(1)
            .and_then(|t| t.parse::<i64>().ok())
        else {
            return vec![self.catalogue.text(locale, "error.not_understood", &[])];
        };
        let user = match self.seerr_user(from, locale).await {
            Ok(id) => id,
            Err(message) => return message,
        };
        match self.seerr.withdraw(id, user).await {
            Ok(()) => {
                vec![self
                    .catalogue
                    .text(locale, "request.withdrawn", &[("id", &id.to_string())])]
            }
            Err(e) => {
                tracing::info!(error = %e, id, "cannot withdraw");
                vec![self
                    .catalogue
                    .text(locale, "request.unknown_id", &[("id", &id.to_string())])]
            }
        }
    }

    fn tell_stranger_once(&mut self, from: &Aci) -> Vec<String> {
        let now = Instant::now();
        // `conversations` is bounded by the size of the household; this map
        // is not -- every wrong number that ever writes leaves an entry that
        // is refreshed but never otherwise removed. Prune opportunistically
        // rather than running a timer for it.
        prune_stale_strangers(&mut self.told_strangers, now);
        if let Some(when) = self.told_strangers.get(from) {
            if now.duration_since(*when) < STRANGER_QUIET {
                return vec![];
            }
        }
        self.told_strangers.insert(from.clone(), now);
        // A stranger has no Authentik account and therefore no locale. German
        // is the house language; the English half of the sentence is in the
        // same string.
        vec![self.catalogue.text(
            Locale::De,
            "error.unknown_sender",
            &[("url", self.settings_url.as_str())],
        )]
    }

    async fn search(
        &mut self,
        from: &Aci,
        locale: Locale,
        query: &str,
        kind: Option<MediaKind>,
        page: u32,
    ) -> Vec<String> {
        let hits = match self.seerr.search(query, kind, page).await {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(error = %e, "search failed");
                return vec![self.catalogue.text(locale, "error.seerr_down", &[])];
            }
        };
        if hits.is_empty() {
            return vec![self.catalogue.text(
                locale,
                if page > 1 {
                    "search.no_more"
                } else {
                    "search.none"
                },
                &[],
            )];
        }

        let listing = self.render(&hits, locale);
        self.conversations.insert(
            from.clone(),
            Conversation::Results {
                query: query.to_string(),
                kind,
                page,
                hits,
                at: Instant::now(),
            },
        );
        vec![listing]
    }

    async fn next_page(&mut self, from: &Aci, locale: Locale) -> Vec<String> {
        let Some(Conversation::Results {
            query,
            kind,
            page,
            at,
            ..
        }) = self.conversations.get(from).cloned()
        else {
            return vec![self.catalogue.text(locale, "error.not_understood", &[])];
        };
        if at.elapsed() > RESULTS_LIVE {
            self.conversations.remove(from);
            return vec![self.catalogue.text(locale, "error.not_understood", &[])];
        }
        self.search(from, locale, &query, kind, page + 1).await
    }

    fn render(&self, hits: &[Hit], locale: Locale) -> String {
        let mut out = String::new();
        for (index, hit) in hits.iter().take(PAGE).enumerate() {
            out.push_str(&format!("{}. {}", index + 1, hit.title));
            if let Some(year) = hit.year {
                out.push_str(&format!(" ({year})"));
            }
            out.push_str(" · ");
            out.push_str(&self.catalogue.text(
                locale,
                match hit.kind {
                    MediaKind::Movie => "search.kind_movie",
                    MediaKind::Tv => "search.kind_tv",
                },
                &[],
            ));
            if let Some(rating) = hit.rating {
                out.push_str(&format!(" · ★ {rating:.1}"));
            }
            if hit.already {
                out.push_str(" · ");
                out.push_str(&self.catalogue.text(locale, "search.already", &[]));
            }
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&self.catalogue.text(locale, "search.footer", &[]));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_command_uses_the_prefixs_own_length_not_the_lowered_remainders() {
        // Regression for the crash this function replaces: slicing by
        // `text.len() - lowered_remainder.len()` breaks when lowercasing
        // changes byte length. `strip_command` must not reproduce that.
        assert_eq!(
            strip_command("/film ẞ", "/film ß", &["/film "]),
            Some("ẞ"),
            "the original character, not the lowercased one"
        );
        assert_eq!(
            strip_command("/FILM Blade Runner", "/film blade runner", &["/film "]),
            Some("Blade Runner")
        );
        assert_eq!(
            strip_command("blade runner", "blade runner", &["/film "]),
            None
        );
    }

    #[test]
    fn stale_stranger_entries_are_pruned_and_fresh_ones_survive() {
        let now = Instant::now();
        let mut told = HashMap::new();
        told.insert(
            Aci("old".into()),
            now - STRANGER_QUIET - Duration::from_secs(1),
        );
        told.insert(Aci("fresh".into()), now);

        prune_stale_strangers(&mut told, now);

        assert_eq!(told.len(), 1, "the stale entry must be gone");
        assert!(told.contains_key(&Aci("fresh".into())));
        assert!(!told.contains_key(&Aci("old".into())));
    }

    // --- Backdated-conversation expiry -------------------------------
    //
    // `a_digit_with_no_open_list_is_a_search` (tests/dialog.rs) never opens
    // a conversation at all, so it proves the *empty* case, not expiry --
    // delete the `at.elapsed() <= RESULTS_LIVE` guards entirely and every
    // test in the suite still passes. These two use `insert_conversation_aged`
    // to plant a conversation that is provably past `RESULTS_LIVE` and check
    // that it is treated as gone.

    use crate::directory::Member;
    use crate::model::{Pending, SeerrUserId};
    use std::sync::Mutex;

    #[derive(Default)]
    struct ExpirySeerr {
        placed: Mutex<Vec<(i64, Seasons, SeerrUserId)>>,
        queries: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Requests for ExpirySeerr {
        async fn search(
            &self,
            q: &str,
            _kind: Option<MediaKind>,
            _page: u32,
        ) -> anyhow::Result<Vec<Hit>> {
            self.queries.lock().unwrap().push(q.to_string());
            Ok(vec![])
        }
        async fn user_id(&self, _u: &str) -> anyhow::Result<Option<SeerrUserId>> {
            Ok(Some(SeerrUserId(12)))
        }
        async fn quality_profiles(
            &self,
            _kind: MediaKind,
        ) -> anyhow::Result<Vec<crate::model::QualityProfile>> {
            Ok(vec![])
        }
        async fn request(
            &self,
            hit: &Hit,
            seasons: Seasons,
            as_user: SeerrUserId,
            _profile_id: Option<i64>,
        ) -> anyhow::Result<i64> {
            self.placed
                .lock()
                .unwrap()
                .push((hit.tmdb_id, seasons, as_user));
            Ok(1849)
        }
        async fn profile_of(&self, _request_id: i64) -> anyhow::Result<Option<i64>> {
            Ok(None)
        }
        async fn pending(&self, _u: SeerrUserId) -> anyhow::Result<Vec<Pending>> {
            Ok(vec![])
        }
        async fn withdraw(&self, _id: i64, _u: SeerrUserId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn requester_of(&self, _id: i64) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
    }

    struct ExpiryDirectory;
    impl Directory for ExpiryDirectory {
        fn lookup(&self, _aci: &Aci) -> Option<Member> {
            Some(Member {
                authentik_username: "robert".into(),
                locale: Locale::De,
                allowed: true,
            })
        }
    }

    fn expiry_hit() -> Hit {
        Hit {
            tmdb_id: 1,
            kind: MediaKind::Movie,
            title: "A".into(),
            year: None,
            rating: None,
            seasons: 0,
            already: false,
        }
    }

    #[tokio::test]
    async fn a_digit_against_a_backdated_results_list_is_a_fresh_search() {
        let mut d = Dialog::new(
            ExpirySeerr::default(),
            ExpiryDirectory,
            Catalogue::load(),
            "https://example.invalid".to_string(),
            "the operator".to_string(),
            Vec::new(),
        );
        let aci = Aci("aaaa".into());
        d.insert_conversation_aged(
            &aci,
            Conversation::Results {
                query: "old".into(),
                kind: None,
                page: 1,
                hits: vec![expiry_hit()],
                at: Instant::now(),
            },
            RESULTS_LIVE + Duration::from_secs(1),
        );

        d.handle(&aci, "2").await;

        assert_eq!(
            d.seerr_ref().queries.lock().unwrap().as_slice(),
            ["2"],
            "a backdated list must not be chosen from -- it must search for '2'"
        );
        assert!(
            d.seerr_ref().placed.lock().unwrap().is_empty(),
            "must not have placed anything from a stale list"
        );
    }

    #[tokio::test]
    async fn a_seasons_answer_against_a_backdated_question_does_not_place_a_request() {
        let mut d = Dialog::new(
            ExpirySeerr::default(),
            ExpiryDirectory,
            Catalogue::load(),
            "https://example.invalid".to_string(),
            "the operator".to_string(),
            Vec::new(),
        );
        let aci = Aci("aaaa".into());
        let series = Hit {
            kind: MediaKind::Tv,
            seasons: 2,
            ..expiry_hit()
        };
        d.insert_conversation_aged(
            &aci,
            Conversation::Seasons {
                hit: series,
                at: Instant::now(),
            },
            RESULTS_LIVE + Duration::from_secs(1),
        );

        d.handle(&aci, "alle").await;

        assert!(
            d.seerr_ref().placed.lock().unwrap().is_empty(),
            "a backdated seasons question must not still accept an answer"
        );
    }
}
