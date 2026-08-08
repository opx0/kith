//! This Person's Identity on this Device — minted locally, never escrowed, and
//! not recoverable from anywhere else.
//!
//! The Device half of the pair is not stored here: a Device's identity *is* the
//! Sync Engine's device id, so it is asked for, never recorded.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::PersonId;

const SCHEMA: u32 = 1;

/// One of exactly two files kith cannot rebuild from the synced tree.
#[derive(Debug, Serialize, Deserialize)]
pub struct Identity {
    pub schema: u32,
    pub person: PersonId,
    pub display_name: String,
    pub created: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("no data directory for this Person")]
    NoDataDir,
    #[error("an Identity already exists at {0} — kith has no rename in v0.1")]
    AlreadyExists(PathBuf),
    #[error("a display name is required")]
    NameRequired,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the Identity file is unreadable: {0}")]
    Malformed(String),
}

/// `$XDG_DATA_HOME/kith/identity.toml` — data, not state: losing it loses the
/// Person.
pub fn path() -> Result<PathBuf, IdentityError> {
    directories::BaseDirs::new()
        .map(|b| b.data_dir().join("kith/identity.toml"))
        .ok_or(IdentityError::NoDataDir)
}

pub fn load() -> Result<Option<Identity>, IdentityError> {
    let p = path()?;
    match std::fs::read_to_string(&p) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|e| IdentityError::Malformed(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Mint this Person and bind them to this Device.
///
/// Refuses to overwrite: replacing an Identity would orphan every attribution
/// that already names it.
pub fn create(display_name: &str, now: &str) -> Result<Identity, IdentityError> {
    let display_name = sanitise(display_name);
    if display_name.is_empty() {
        return Err(IdentityError::NameRequired);
    }

    let p = path()?;
    if p.exists() {
        return Err(IdentityError::AlreadyExists(p));
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let identity = Identity {
        schema: SCHEMA,
        person: PersonId::generate(),
        display_name,
        created: now.to_string(),
    };

    let text = toml::to_string_pretty(&identity).map_err(|e| IdentityError::Malformed(e.to_string()))?;
    write_private(&p, &text)?;
    Ok(identity)
}

/// Write 0600 — a shared machine should not hand this Person to the next
/// account over.
fn write_private(path: &PathBuf, text: &str) -> Result<(), IdentityError> {
    let tmp = path.with_extension("toml.kith-tmp");
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Strip the characters that could make the admission prompt lie: bidi
/// overrides, controls and newlines.
fn sanitise(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bidi_overrides_that_could_forge_an_admission_prompt() {
        assert_eq!(sanitise("Ana\u{202E}nimda"), "Ananimda");
        assert_eq!(sanitise("  Ben \n"), "Ben");
    }

    #[test]
    fn blank_names_are_refused_rather_than_stored() {
        assert!(sanitise("   ").is_empty());
        assert!(sanitise("\u{202E}").is_empty());
    }

    #[test]
    fn identity_round_trips_through_toml() {
        let id = Identity {
            schema: SCHEMA,
            person: PersonId::generate(),
            display_name: "Ana".into(),
            created: "2026-08-07T00:00:00Z".into(),
        };
        let text = toml::to_string_pretty(&id).unwrap();
        let back: Identity = toml::from_str(&text).unwrap();
        assert_eq!(back.person, id.person);
        assert_eq!(back.display_name, "Ana");
        assert_eq!(back.schema, SCHEMA);
    }
}
