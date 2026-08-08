//! The domain vocabulary: People, Circles, Collections and Items.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Person's stable identity: `p-` followed by a 26-character Crockford ULID.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PersonId(String);

impl PersonId {
    /// Mint a new Identity — local, random, never derived from the Device.
    pub fn generate() -> Self {
        Self(format!("p-{}", ulid::Ulid::generate().to_string().to_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The short form shown in attributions: `p-` plus six characters.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(8)]
    }
}

/// Read an id back from something wallsync itself wrote.
///
/// Validates nothing on purpose: rejecting an id a peer's newer wallsync minted
/// would drop that Person's attribution rather than show it.
impl From<String> for PersonId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl fmt::Display for PersonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PersonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PersonId({})", self.0)
    }
}

/// An Item's stable identity: a bare ULID, minted once at import.
///
/// Deliberately *not* the content hash — an Item survives a move, a rename or a
/// re-encode, and only its binding to bytes changes.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ItemId(String);

impl ItemId {
    pub fn generate() -> Self {
        Self(ulid::Ulid::generate().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Read an Item id back from local state wallsync wrote; validates nothing, as
/// [`PersonId`]'s does not.
impl From<String> for ItemId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Member's declared capability in a Circle — a policy, not an enforcement.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The Circle's Steward: the Member whose Device is its sole admission gate.
    Admin,
    Member,
}

/// One Device's assertion of which Person it speaks for.
///
/// Written only by the Device it names — that single-writer rule is what keeps
/// it conflict-free on a transport with no coordinator, and it is why the record
/// is keyed by Device even though every product fact keys on the Person inside.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MembershipClaim {
    pub schema: u32,
    /// The Sync Engine's identity for this Device. wallsync mints no second ID space.
    pub device: String,
    pub person: PersonId,
    pub display_name: String,
    /// The single freshness field. Every tie-break reads "newest `asserted`".
    pub asserted: String,
    /// Absent until this Member leaves the Circle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left_at: Option<String>,
}

/// A Person, as derived by folding every Membership claim that names them.
#[derive(Clone, Debug)]
pub struct Person {
    pub id: PersonId,
    pub display_name: String,
    /// v0.1 always holds exactly one; plural from day one so the second Device
    /// lands without a migration.
    pub devices: Vec<String>,
}

/// Whether another Member's Device is reachable from *this* Device, right now.
///
/// Never called "online": this is one Device's own live view of one connection,
/// not a claim about a Person. `Unknown` is a real answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Presence {
    Connected,
    NotConnected,
    Unknown,
}

impl Presence {
    /// The wire spelling used by `--json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Connected => "connected",
            Presence::NotConnected => "not_connected",
            Presence::Unknown => "unknown",
        }
    }
}

/// One piece of content: the bytes plus everything wallsync knows about them.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: ItemId,
    pub title: String,
    pub added_by: PersonId,
    pub added_at: String,
    /// The path its bytes currently occupy, if they have arrived on this Device.
    pub path: Option<std::path::PathBuf>,
    pub hash: Option<String>,
    pub bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_id_is_prefixed_and_short_form_is_eight_chars() {
        let id = PersonId::generate();
        assert!(id.as_str().starts_with("p-"), "{id} must be self-describing");
        assert_eq!(id.as_str().len(), 28, "p- plus a 26-character ULID");
        assert_eq!(id.short().len(), 8, "p- plus six characters");
    }

    #[test]
    fn person_id_is_never_mistakable_for_an_item_id() {
        assert!(!ItemId::generate().as_str().starts_with("p-"));
    }

    #[test]
    fn presence_wire_spelling_never_says_online() {
        for p in [Presence::Connected, Presence::NotConnected, Presence::Unknown] {
            assert!(!p.as_str().contains("online"));
        }
    }
}
