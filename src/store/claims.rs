//! Membership claims — one file per Device, at `.wallsync/members/<device-id>.toml`.
//!
//! **Single writer:** the filename is the key, and the Device it names is the only
//! Device that ever writes that file — so two Members never race on one path.
//!
//! Keyed by Device, attributed to Person: `person` lives inside the claim, and
//! folding claims by it is what turns Devices into People ([`derive_people`]). A
//! claim is a descriptor, not a record log, so rewriting one is the format working
//! as designed — and it is asserted, never proven, since nothing signs it.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::domain::{MembershipClaim, Person};
use crate::identity::Identity;
use crate::store::descriptors::write_atomic;

/// The claim schema this build writes.
///
/// A claim carrying a higher one is read best-effort and never rewritten, since
/// this build round-trips six fields and would silently drop anything else.
const SCHEMA: u32 = 1;

/// Where a Circle keeps its Membership claims, relative to the Circle root.
const MEMBERS_DIR: &str = ".wallsync/members";

fn members_dir(root: &Path) -> PathBuf {
    root.join(MEMBERS_DIR)
}

/// Publish this Device's Membership claim into a Circle, idempotently.
///
/// | Existing claim | Action |
/// |---|---|
/// | absent, or unreadable | write it |
/// | ours, same display name, no `left_at`, no conflict copies | nothing |
/// | ours, display name differs, or carries `left_at` | rewrite whole |
/// | names a different Person | refuse (`PermissionDenied`) |
///
/// `asserted` is deliberately not compared: it differs at every read, so
/// comparing it would rewrite the claim — and wake every Member's engine — on
/// every start. The refusal keeps two People behind one daemon from rewriting
/// each other's claim forever.
pub fn publish(root: &Path, device: &str, id: &Identity, now: &str) -> io::Result<()> {
    validate_device(device)?;

    let dir = members_dir(root);
    let copies = copies_of(&dir, device)?;

    // Every readable copy has to agree the Device is ours before we touch
    // anything: a copy naming somebody else is the shared-daemon collision.
    for copy in &copies {
        let Some(claim) = &copy.claim else { continue };
        if claim.person != id.person {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} already claims this Device for {} — refusing to publish {} over it",
                    copy.path.display(),
                    claim.person,
                    id.person
                ),
            ));
        }
        if claim.schema > SCHEMA {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} was written by a newer wallsync (schema {}) — reading it, never rewriting it",
                    copy.path.display(),
                    claim.schema
                ),
            ));
        }
    }

    let canonical = copies.iter().find(|c| c.canonical);
    let stale = match canonical.and_then(|c| c.claim.as_ref()) {
        Some(claim) => claim.display_name != id.display_name || claim.left_at.is_some(),
        None => true,
    };
    // Re-assert when a conflict copy exists, so the file we keep is demonstrably
    // the newest statement before the copy goes.
    let has_conflict_copy = copies.iter().any(|c| !c.canonical);

    if stale || has_conflict_copy {
        std::fs::create_dir_all(&dir)?;
        let claim = MembershipClaim {
            schema: SCHEMA,
            device: device.to_string(),
            person: id.person.clone(),
            display_name: id.display_name.clone(),
            asserted: now.to_string(),
            left_at: None,
        };
        write_atomic(&dir.join(format!("{device}.toml")), &claim)?;
    }

    // One we could not read is left where it is: unreadable evidence, not a duplicate.
    for copy in copies.iter().filter(|c| !c.canonical && c.claim.is_some()) {
        match std::fs::remove_file(&copy.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Stamp `left_at` into this Device's own claim — the Member leaving, in one write.
///
/// A tombstone rather than a deletion, so the claim survives to say *left* and to
/// keep this Person's name on the Items they added. `asserted` is refreshed in
/// the same write, or the departure would lose the freshness tie-break and simply
/// not take. `NotFound` when this Device published no claim here.
pub fn stamp_left(root: &Path, device: &str, now: &str) -> io::Result<()> {
    validate_device(device)?;

    let dir = members_dir(root);
    let path = dir.join(format!("{device}.toml"));
    let text = std::fs::read_to_string(&path)?;
    let mut claim: MembershipClaim = toml::from_str(&text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a readable Membership claim: {e}", path.display()),
        )
    })?;

    if claim.schema > SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{} was written by a newer wallsync (schema {}) — reading it, never rewriting it",
                path.display(),
                claim.schema
            ),
        ));
    }

    claim.left_at = Some(now.to_string());
    claim.asserted = now.to_string();
    write_atomic(&path, &claim)
}

/// Every Membership claim in a Circle, one per Device.
///
/// Conflict copies are absorbed by reading the newest `asserted`, a claim whose
/// `device` contradicts its filename is ignored, and one unreadable file is
/// skipped rather than allowed to blank a Members screen.
pub fn read_all(root: &Path) -> io::Result<Vec<MembershipClaim>> {
    let dir = members_dir(root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // A Circle whose first claim has not arrived yet is not a fault.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    // Keyed by writing Device, so a conflict copy competes only with the claim it
    // is a copy of. BTreeMap, so directory-entry order cannot decide anything.
    let mut newest: BTreeMap<String, (MembershipClaim, bool)> = BTreeMap::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(device) = claim_file_device(name) else { continue };

        let Some(claim) = read_claim(&entry.path()) else { continue };
        // The filename is the key, so a claim that disagrees with it is not a
        // claim about anything.
        if claim.device != device {
            continue;
        }

        let canonical = name == format!("{device}.toml");
        match newest.get(device) {
            Some(held) if !supersedes_copy((&claim, canonical), (&held.0, held.1)) => {}
            _ => {
                newest.insert(device.to_string(), (claim, canonical));
            }
        }
    }

    Ok(newest.into_values().map(|(claim, _)| claim).collect())
}

/// Fold claims into the People a Circle actually has, grouping by `person`.
///
/// The display name comes from the claim with the newest `asserted`, ties broken
/// by the smaller Device id; a Member has left only when *every* claim naming
/// them carries `left_at`, since one Device stopping is not the Person leaving.
pub fn derive_people(claims: &[MembershipClaim]) -> Vec<Person> {
    let mut by_person: BTreeMap<&str, Vec<&MembershipClaim>> = BTreeMap::new();
    for claim in claims {
        by_person.entry(claim.person.as_str()).or_default().push(claim);
    }

    let mut people: Vec<Person> = by_person
        .into_values()
        .filter(|group| group.iter().any(|c| c.left_at.is_none()))
        .map(|group| {
            let newest = group
                .iter()
                .copied()
                .reduce(|held, c| if supersedes(c, held) { c } else { held })
                .expect("a group exists because a claim is in it");

            let mut devices: Vec<String> = group.iter().map(|c| c.device.clone()).collect();
            devices.sort();
            devices.dedup();

            Person {
                id: newest.person.clone(),
                display_name: newest.display_name.clone(),
                devices,
            }
        })
        .collect();

    // Two People may share a display name, so the PersonId settles the order.
    people.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    people
}

/// One file that claims to be a Device's Membership claim, canonical or a copy.
struct Copy {
    path: PathBuf,
    /// `None` when the file could not be read; kept in the list anyway, because
    /// an unreadable copy is evidence we must not delete.
    claim: Option<MembershipClaim>,
    canonical: bool,
}

/// Every file in `.wallsync/members` that belongs to one Device: `<device>.toml` and
/// any `*.sync-conflict-*` copy of it.
fn copies_of(dir: &Path, device: &str) -> io::Result<Vec<Copy>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut copies = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if claim_file_device(name) != Some(device) {
            continue;
        }
        let path = entry.path();
        let claim = read_claim(&path);
        copies.push(Copy {
            canonical: name == format!("{device}.toml"),
            claim,
            path,
        });
    }
    Ok(copies)
}

/// The Device a claim filename names: the segment before the first `.`.
///
/// That is what makes a conflict copy readable as the same claim, and requiring
/// `.toml` is what keeps the `.toml.wallsync-tmp` staging file out of every read.
fn claim_file_device(name: &str) -> Option<&str> {
    if !name.ends_with(".toml") {
        return None;
    }
    match name.split('.').next() {
        Some("") | None => None,
        Some(device) => Some(device),
    }
}

fn read_claim(path: &Path) -> Option<MembershipClaim> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Newest `asserted` wins; ties go to the smaller Device id.
fn supersedes(candidate: &MembershipClaim, held: &MembershipClaim) -> bool {
    let (c, h) = (asserted_at(candidate), asserted_at(held));
    if c != h {
        return c > h;
    }
    candidate.device < held.device
}

/// The same rule between two copies of *one* Device's claim, where the Device id
/// cannot separate them: prefer the file the Device actually writes.
fn supersedes_copy(candidate: (&MembershipClaim, bool), held: (&MembershipClaim, bool)) -> bool {
    let (c, h) = (asserted_at(candidate.0), asserted_at(held.0));
    if c != h {
        return c > h;
    }
    candidate.1 && !held.1
}

/// `asserted` parsed for comparison, because `…:02Z` and `…:02.117Z` sort
/// backwards as strings.
///
/// `None` sorts oldest, so a claim wallsync cannot date never wins a freshness tie.
fn asserted_at(claim: &MembershipClaim) -> Option<jiff::Timestamp> {
    claim.asserted.parse::<jiff::Timestamp>().ok()
}

/// A Device id that cannot be a filename cannot be a claim.
///
/// A separator or a `..` would write a file naming some other Device, or none —
/// the one thing the single-writer rule exists to make impossible.
fn validate_device(device: &str) -> io::Result<()> {
    let usable = !device.is_empty()
        && device
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if usable {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{device:?} cannot name a Membership claim — a Device id is its own filename"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PersonId;

    const ANA_DEVICE: &str = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2";
    const BEN_DEVICE: &str = "K5J2FVL-B3QTXAO-7SWNDUE-HMR4YZI-6CPGA2N-XQTLB5V-JW3EOHY-RD6MSAK";

    /// A Circle root of our own, never the Person's home.
    fn circle(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("wallsync-claims-tests")
            .join(format!("{name}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(root.join(MEMBERS_DIR)).unwrap();
        root
    }

    fn identity(display_name: &str) -> Identity {
        Identity {
            schema: 1,
            person: PersonId::generate(),
            display_name: display_name.to_string(),
            created: "2026-08-07T09:00:00Z".to_string(),
        }
    }

    fn claim(
        device: &str,
        person: &PersonId,
        display_name: &str,
        asserted: &str,
        left_at: Option<&str>,
    ) -> MembershipClaim {
        MembershipClaim {
            schema: SCHEMA,
            device: device.to_string(),
            person: person.clone(),
            display_name: display_name.to_string(),
            asserted: asserted.to_string(),
            left_at: left_at.map(str::to_string),
        }
    }

    fn put(root: &Path, file: &str, claim: &MembershipClaim) {
        let path = root.join(MEMBERS_DIR).join(file);
        std::fs::write(&path, toml::to_string_pretty(claim).unwrap()).unwrap();
    }

    /// A Person is the *set* of claims naming them.
    #[test]
    fn two_devices_of_one_person_fold_to_one_person() {
        let ana = PersonId::generate();
        let people = derive_people(&[
            claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:02:11Z", None),
            claim(BEN_DEVICE, &ana, "Ana", "2026-08-07T09:40:00Z", None),
        ]);

        assert_eq!(people.len(), 1, "one Person, two Devices");
        assert_eq!(people[0].id, ana);
        assert_eq!(people[0].display_name, "Ana");
        assert_eq!(people[0].devices, vec![BEN_DEVICE.to_string(), ANA_DEVICE.to_string()]);
    }

    #[test]
    fn two_people_stay_two_people_and_sort_by_display_name() {
        let (ana, ben) = (PersonId::generate(), PersonId::generate());
        let people = derive_people(&[
            claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:02:11Z", None),
            claim(BEN_DEVICE, &ben, "Ben", "2026-08-07T09:40:00Z", None),
        ]);

        let names: Vec<&str> = people.iter().map(|p| p.display_name.as_str()).collect();
        assert_eq!(names, ["Ana", "Ben"]);
    }

    #[test]
    fn a_left_at_stamp_removes_the_person_from_the_circle() {
        let ana = PersonId::generate();
        let people = derive_people(&[claim(
            ANA_DEVICE,
            &ana,
            "Ana",
            "2026-08-07T09:02:11Z",
            Some("2026-09-01T18:20:00Z"),
        )]);

        assert!(people.is_empty(), "a Member who left is not a Member");
    }

    #[test]
    fn one_device_leaving_does_not_remove_a_person_who_has_another() {
        let ana = PersonId::generate();
        let people = derive_people(&[
            claim(
                ANA_DEVICE,
                &ana,
                "Ana",
                "2026-09-01T18:20:00Z",
                Some("2026-09-01T18:20:00Z"),
            ),
            claim(BEN_DEVICE, &ana, "Ana", "2026-08-07T09:02:11Z", None),
        ]);

        assert_eq!(people.len(), 1);
        assert_eq!(people[0].display_name, "Ana");
    }

    #[test]
    fn the_newest_assertion_wins_a_display_name_disagreement() {
        let ana = PersonId::generate();
        let people = derive_people(&[
            claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:02:11Z", None),
            claim(BEN_DEVICE, &ana, "Ana Ruiz", "2026-08-09T11:00:00.500Z", None),
        ]);

        assert_eq!(people[0].display_name, "Ana Ruiz");
    }

    #[test]
    fn freshness_is_a_timestamp_comparison_and_not_a_string_one() {
        let ana = PersonId::generate();
        let people = derive_people(&[
            claim(ANA_DEVICE, &ana, "whole second", "2026-08-07T09:02:02Z", None),
            claim(BEN_DEVICE, &ana, "a fraction later", "2026-08-07T09:02:02.117Z", None),
        ]);

        assert_eq!(people[0].display_name, "a fraction later");
    }

    #[test]
    fn a_tie_on_asserted_is_broken_by_the_smaller_device_id() {
        let ana = PersonId::generate();
        let both = "2026-08-07T09:02:11Z";
        let people = derive_people(&[
            claim(ANA_DEVICE, &ana, "from P56", both, None),
            claim(BEN_DEVICE, &ana, "from K5J", both, None),
        ]);

        assert!(BEN_DEVICE < ANA_DEVICE, "the fixtures make the tie-break visible");
        assert_eq!(people[0].display_name, "from K5J");
    }

    #[test]
    fn an_undatable_claim_never_wins_the_freshness_tie_break() {
        let ana = PersonId::generate();
        let people = derive_people(&[
            claim(ANA_DEVICE, &ana, "dated", "2026-08-07T09:02:11Z", None),
            claim(BEN_DEVICE, &ana, "whenever", "last tuesday", None),
        ]);

        assert_eq!(people[0].display_name, "dated");
    }

    #[test]
    fn publish_writes_a_claim_the_circle_can_read() {
        let root = circle("publish");
        let ana = identity("Ana");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();

        let claims = read_all(&root).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].schema, SCHEMA);
        assert_eq!(claims[0].device, ANA_DEVICE, "the claim names its own filename");
        assert_eq!(claims[0].person, ana.person);
        assert_eq!(claims[0].display_name, "Ana");
        assert_eq!(claims[0].asserted, "2026-08-07T09:02:11Z");
        assert!(claims[0].left_at.is_none());
        assert_eq!(derive_people(&claims).len(), 1);
    }

    /// Re-asserting on every start would wake every Member's engine for nothing.
    #[test]
    fn publishing_an_unchanged_claim_rewrites_nothing() {
        let root = circle("idempotent");
        let ana = identity("Ana");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();
        publish(&root, ANA_DEVICE, &ana, "2026-08-09T22:00:00Z").unwrap();

        assert_eq!(read_all(&root).unwrap()[0].asserted, "2026-08-07T09:02:11Z");
    }

    #[test]
    fn publishing_a_changed_display_name_rewrites_the_whole_claim() {
        let root = circle("rename");
        let mut ana = identity("Ana");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();

        ana.display_name = "Ana Ruiz".to_string();
        publish(&root, ANA_DEVICE, &ana, "2026-08-09T22:00:00Z").unwrap();

        let claims = read_all(&root).unwrap();
        assert_eq!(claims[0].display_name, "Ana Ruiz");
        assert_eq!(claims[0].asserted, "2026-08-09T22:00:00Z");
    }

    /// Two People behind one daemon: the second must never rewrite the first's claim.
    #[test]
    fn publish_refuses_a_claim_that_names_a_different_person() {
        let root = circle("contradiction");
        let ana = identity("Ana");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();

        let ben = identity("Ben");
        let refused = publish(&root, ANA_DEVICE, &ben, "2026-08-07T10:00:00Z").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);

        let claims = read_all(&root).unwrap();
        assert_eq!(claims[0].person, ana.person, "Ana's claim is untouched");
    }

    #[test]
    fn publish_refuses_a_device_id_that_could_name_another_file() {
        let root = circle("traversal");
        let ana = identity("Ana");

        for hostile in ["../../etc/passwd", "P56IOI7.MZJNU2Y", "", "with space", "a/b"] {
            let e = publish(&root, hostile, &ana, "2026-08-07T09:02:11Z").unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "accepted {hostile:?}");
        }
        assert!(read_all(&root).unwrap().is_empty(), "nothing was written");
    }

    #[test]
    fn publish_never_rewrites_a_claim_from_a_newer_wallsync() {
        let root = circle("newer-schema");
        let ana = identity("Ana");
        let mut future = claim(ANA_DEVICE, &ana.person, "Ana", "2026-08-07T09:02:11Z", None);
        future.schema = SCHEMA + 1;
        put(&root, &format!("{ANA_DEVICE}.toml"), &future);

        let e = publish(&root, ANA_DEVICE, &ana, "2026-08-09T22:00:00Z").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert_eq!(read_all(&root).unwrap()[0].schema, SCHEMA + 1, "left as found");
    }

    #[test]
    fn stamp_left_tombstones_the_claim_and_refreshes_asserted() {
        let root = circle("leave");
        let ana = identity("Ana");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();

        stamp_left(&root, ANA_DEVICE, "2026-09-01T18:20:00Z").unwrap();

        let claims = read_all(&root).unwrap();
        assert_eq!(claims.len(), 1, "a claim is tombstoned, never deleted");
        assert_eq!(claims[0].left_at.as_deref(), Some("2026-09-01T18:20:00Z"));
        assert_eq!(claims[0].asserted, "2026-09-01T18:20:00Z", "the departure is the newest word");
        assert_eq!(claims[0].person, ana.person, "attribution outlives Membership");
        assert!(derive_people(&claims).is_empty());
    }

    /// Publishing is this Device saying "I am here", so it clears its own tombstone.
    #[test]
    fn publishing_after_leaving_clears_the_tombstone() {
        let root = circle("rejoin");
        let ana = identity("Ana");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();
        stamp_left(&root, ANA_DEVICE, "2026-09-01T18:20:00Z").unwrap();

        publish(&root, ANA_DEVICE, &ana, "2026-09-04T08:00:00Z").unwrap();

        let claims = read_all(&root).unwrap();
        assert!(claims[0].left_at.is_none());
        assert_eq!(claims[0].asserted, "2026-09-04T08:00:00Z");
        assert_eq!(derive_people(&claims).len(), 1);
    }

    #[test]
    fn stamping_a_circle_this_device_never_published_into_is_not_silently_ok() {
        let root = circle("never-published");
        let e = stamp_left(&root, ANA_DEVICE, "2026-09-01T18:20:00Z").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_conflict_copy_is_absorbed_by_reading_the_newest_assertion() {
        let root = circle("conflict");
        let ana = identity("Ana");
        put(
            &root,
            &format!("{ANA_DEVICE}.toml"),
            &claim(ANA_DEVICE, &ana.person, "Ana", "2026-08-07T09:02:11Z", None),
        );
        put(
            &root,
            &format!("{ANA_DEVICE}.sync-conflict-20260807-143122-K5J2FVL.toml"),
            &claim(ANA_DEVICE, &ana.person, "Ana Ruiz", "2026-08-08T10:00:00Z", None),
        );

        let claims = read_all(&root).unwrap();
        assert_eq!(claims.len(), 1, "two copies of one claim are one Device");
        assert_eq!(claims[0].display_name, "Ana Ruiz");
        assert_eq!(derive_people(&claims).len(), 1);
    }

    #[test]
    fn the_owning_device_re_asserts_and_deletes_its_conflict_copy() {
        let root = circle("absorb");
        let ana = identity("Ana");
        let copy = format!("{ANA_DEVICE}.sync-conflict-20260807-143122-K5J2FVL.toml");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();
        put(
            &root,
            &copy,
            &claim(ANA_DEVICE, &ana.person, "Ana", "2026-08-08T10:00:00Z", None),
        );

        publish(&root, ANA_DEVICE, &ana, "2026-08-09T22:00:00Z").unwrap();

        assert!(!root.join(MEMBERS_DIR).join(&copy).exists(), "the copy is gone");
        assert_eq!(read_all(&root).unwrap()[0].asserted, "2026-08-09T22:00:00Z");
    }

    #[test]
    fn a_conflict_copy_naming_another_person_is_kept_as_evidence() {
        let root = circle("collision");
        let ana = identity("Ana");
        let ben = identity("Ben");
        let copy = format!("{ANA_DEVICE}.sync-conflict-20260807-143122-K5J2FVL.toml");
        publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();
        put(
            &root,
            &copy,
            &claim(ANA_DEVICE, &ben.person, "Ben", "2026-08-08T10:00:00Z", None),
        );

        let e = publish(&root, ANA_DEVICE, &ana, "2026-08-09T22:00:00Z").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(root.join(MEMBERS_DIR).join(&copy).exists(), "evidence is kept");
    }

    #[test]
    fn a_claim_whose_device_field_contradicts_its_filename_is_ignored() {
        let root = circle("mismatch");
        let ana = identity("Ana");
        put(
            &root,
            &format!("{BEN_DEVICE}.toml"),
            &claim(ANA_DEVICE, &ana.person, "Ana", "2026-08-07T09:02:11Z", None),
        );

        assert!(read_all(&root).unwrap().is_empty(), "the filename is the key");
    }

    #[test]
    fn one_unreadable_claim_does_not_cost_the_rest_of_the_circle() {
        let root = circle("malformed");
        let ana = identity("Ana");
        put(
            &root,
            &format!("{ANA_DEVICE}.toml"),
            &claim(ANA_DEVICE, &ana.person, "Ana", "2026-08-07T09:02:11Z", None),
        );
        std::fs::write(
            root.join(MEMBERS_DIR).join(format!("{BEN_DEVICE}.toml")),
            "person = ",
        )
        .unwrap();

        let claims = read_all(&root).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].display_name, "Ana");
    }

    #[test]
    fn staging_files_and_stray_names_are_not_claims() {
        assert_eq!(claim_file_device("P56IOI7-XZWICQ2.toml"), Some("P56IOI7-XZWICQ2"));
        assert_eq!(
            claim_file_device("P56IOI7-XZWICQ2.sync-conflict-20260807-143122-K5J2FVL.toml"),
            Some("P56IOI7-XZWICQ2")
        );
        assert_eq!(claim_file_device("P56IOI7-XZWICQ2.toml.wallsync-tmp"), None);
        assert_eq!(claim_file_device(".hidden.toml"), None);
        assert_eq!(claim_file_device("README.md"), None);
    }

    #[test]
    fn a_circle_with_no_claims_yet_reads_as_empty_rather_than_failing() {
        let root = std::env::temp_dir()
            .join("wallsync-claims-tests")
            .join(format!("absent-{}", ulid::Ulid::generate()));
        assert!(read_all(&root).unwrap().is_empty());
    }
}
