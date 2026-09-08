use crate::model::Aci;
use crate::state::State;

#[derive(Clone, Debug, PartialEq)]
pub struct AuthentikUser {
    pub username: String,
    pub signal_username: Option<String>,
    pub locale: String,
    pub groups: Vec<String>,
}

impl AuthentikUser {
    /// The field as entered, with surrounding whitespace gone. An empty field
    /// is no field: Authentik stores "" rather than removing the attribute.
    pub fn signal_name(&self) -> Option<&str> {
        let raw = self.signal_username.as_deref()?.trim();
        (!raw.is_empty()).then_some(raw)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// Resolve, store, greet.
    Added {
        username: String,
        signal_username: String,
    },
    /// The person entered a different name. Resolve afresh, forget the old
    /// ACI, greet again -- the new name may well be a different human.
    Rebound {
        username: String,
        signal_username: String,
    },
    /// Field cleared, or the whole account gone. Say goodbye once, then forget.
    Removed { username: String, aci: Aci },
    /// Somebody else already holds that Signal name. Refuse rather than
    /// overwrite: an overwrite would silently move a stranger's chat onto this
    /// account.
    Conflict {
        username: String,
        signal_username: String,
        held_by: String,
    },
    /// The mapping already exists and the name hasn't changed, but the
    /// welcome never went out -- a prior send failed. Retry it; nothing else
    /// about the entry changes.
    Greet { username: String },
}

pub fn plan(state: &State, users: &[AuthentikUser]) -> Vec<Change> {
    let mut changes = Vec::new();
    // Names claimed during THIS pass, so two fresh accounts asking for one
    // name do not both get it. Kept in their original case and compared with
    // eq_ignore_ascii_case below -- the same idiom the "already correct"
    // check a few lines down uses, rather than a second, lowercased notion
    // of equality.
    let mut claimed: Vec<(String, String)> = state
        .iter()
        .map(|e| (e.signal_username.clone(), e.authentik_username.clone()))
        .collect();

    for user in users {
        let Some(wanted) = user.signal_name() else {
            if let Some(existing) = state.by_user(&user.username) {
                changes.push(Change::Removed {
                    username: user.username.clone(),
                    aci: existing.aci.clone(),
                });
            }
            continue;
        };

        let existing = state.by_user(&user.username);
        if let Some(e) = existing {
            if e.signal_username.eq_ignore_ascii_case(wanted) {
                if !e.greeted {
                    changes.push(Change::Greet {
                        username: user.username.clone(),
                    });
                }
                continue;
            }
        }

        if let Some((_, holder)) = claimed
            .iter()
            .find(|(name, holder)| name.eq_ignore_ascii_case(wanted) && holder != &user.username)
        {
            changes.push(Change::Conflict {
                username: user.username.clone(),
                signal_username: wanted.to_string(),
                held_by: holder.clone(),
            });
            continue;
        }

        claimed.retain(|(_, holder)| holder != &user.username);
        claimed.push((wanted.to_string(), user.username.clone()));

        changes.push(if existing.is_some() {
            Change::Rebound {
                username: user.username.clone(),
                signal_username: wanted.to_string(),
            }
        } else {
            Change::Added {
                username: user.username.clone(),
                signal_username: wanted.to_string(),
            }
        });
    }

    // Accounts that vanished from Authentik entirely.
    for entry in state.iter() {
        if !users.iter().any(|u| u.username == entry.authentik_username) {
            changes.push(Change::Removed {
                username: entry.authentik_username.clone(),
                aci: entry.aci.clone(),
            });
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Locale;
    use crate::model::Aci;
    use crate::state::{Entry, State};

    fn user(name: &str, signal: Option<&str>) -> AuthentikUser {
        AuthentikUser {
            username: name.into(),
            signal_username: signal.map(|s| s.into()),
            locale: "de".into(),
            groups: vec!["Medien".into()],
        }
    }

    fn known(state: &mut State, name: &str, signal: &str, aci: &str) {
        state.upsert(Entry {
            authentik_username: name.into(),
            signal_username: signal.into(),
            aci: Aci(aci.into()),
            greeted: true,
            locale: Locale::De,
            groups: vec!["Medien".into()],
        });
    }

    #[test]
    fn a_new_name_is_added() {
        let state = State::default();
        let got = plan(&state, &[user("robert", Some("robert.42"))]);
        assert_eq!(
            got,
            vec![Change::Added {
                username: "robert".into(),
                signal_username: "robert.42".into()
            }]
        );
    }

    #[test]
    fn an_unchanged_name_produces_nothing() {
        let mut state = State::default();
        known(&mut state, "robert", "robert.42", "aaaa");
        assert!(plan(&state, &[user("robert", Some("robert.42"))]).is_empty());
    }

    fn known_but_not_greeted(state: &mut State, name: &str, signal: &str, aci: &str) {
        state.upsert(Entry {
            authentik_username: name.into(),
            signal_username: signal.into(),
            aci: Aci(aci.into()),
            greeted: false,
            locale: Locale::De,
            groups: vec!["Medien".into()],
        });
    }

    #[test]
    fn an_unfinished_greeting_is_retried() {
        // The mapping is already right (name matches), but a prior send
        // failed and `greeted` never flipped. plan() must not treat this as
        // "nothing changed" -- silence here would be permanent.
        let mut state = State::default();
        known_but_not_greeted(&mut state, "robert", "robert.42", "aaaa");
        let got = plan(&state, &[user("robert", Some("robert.42"))]);
        assert_eq!(
            got,
            vec![Change::Greet {
                username: "robert".into()
            }]
        );
    }

    #[test]
    fn a_changed_name_is_rebound() {
        let mut state = State::default();
        known(&mut state, "robert", "robert.42", "aaaa");
        let got = plan(&state, &[user("robert", Some("robert.99"))]);
        assert_eq!(
            got,
            vec![Change::Rebound {
                username: "robert".into(),
                signal_username: "robert.99".into()
            }]
        );
    }

    #[test]
    fn a_cleared_field_says_goodbye() {
        let mut state = State::default();
        known(&mut state, "robert", "robert.42", "aaaa");
        let got = plan(&state, &[user("robert", None)]);
        assert_eq!(
            got,
            vec![Change::Removed {
                username: "robert".into(),
                aci: Aci("aaaa".into())
            }]
        );
    }

    #[test]
    fn a_deleted_account_says_goodbye_too() {
        // The account is gone from Authentik entirely, not just the field.
        let mut state = State::default();
        known(&mut state, "robert", "robert.42", "aaaa");
        let got = plan(&state, &[]);
        assert_eq!(
            got,
            vec![Change::Removed {
                username: "robert".into(),
                aci: Aci("aaaa".into())
            }]
        );
    }

    #[test]
    fn a_name_already_bound_elsewhere_is_a_conflict_not_a_silent_takeover() {
        let mut state = State::default();
        known(&mut state, "robert", "robert.42", "aaaa");
        let got = plan(
            &state,
            &[
                user("robert", Some("robert.42")),
                user("konrad", Some("ROBERT.42")),
            ],
        );
        assert_eq!(
            got,
            vec![Change::Conflict {
                username: "konrad".into(),
                signal_username: "ROBERT.42".into(),
                held_by: "robert".into(),
            }],
            "case must not open a back door"
        );
    }

    #[test]
    fn two_new_accounts_claiming_one_name_yield_one_add_and_one_conflict() {
        let state = State::default();
        let got = plan(
            &state,
            &[user("a", Some("shared.1")), user("b", Some("shared.1"))],
        );
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0], Change::Added { .. }));
        assert!(matches!(got[1], Change::Conflict { .. }));
    }

    #[test]
    fn whitespace_around_a_name_is_ignored() {
        let mut state = State::default();
        known(&mut state, "robert", "robert.42", "aaaa");
        assert!(plan(&state, &[user("robert", Some("  robert.42  "))]).is_empty());
    }

    #[test]
    fn an_empty_field_counts_as_no_name() {
        let state = State::default();
        assert!(plan(&state, &[user("robert", Some("   "))]).is_empty());
    }
}
