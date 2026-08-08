//! `kith list` and `kith status` — the two commands that only look.
//!
//! Local facts plus what the engine says, and nothing else. Presence is never
//! "online": `unknown` is a real answer, `not connected` would be a claim this
//! Device is in no position to make. Every caveat the human surface prints also
//! travels in the envelope's `notes[]`, so a script is told what a Person is told.
//!
//! Both verbs work with the Sync Engine down: `list` degrades to the tree and
//! exits 0, `status` prints the same local facts and exits 69.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::domain::{Item, MembershipClaim, Person, PersonId, Presence, Role};
use crate::engine::syncthing::{Credentials, SyncthingEngine};
use crate::engine::{CircleId, CircleRef, CircleStatus, PeerDevice, SyncEngine, SyncError};
use crate::identity::Identity;
use crate::provider::wallpaper::WallpaperProvider;
use crate::provider::{ImportCandidate, Provider};
use crate::store::descriptors::{self, CircleDescriptor};
use crate::store::{claims, records};

// Sysexits, so the whole binary speaks one dialect.

const EX_OK: i32 = 0;
const EX_FAILED: i32 = 1;
const EX_USAGE: i32 = 64;
const EX_DATA: i32 = 65;
const EX_UNAVAILABLE: i32 = 69;
const EX_INTERNAL: i32 = 70;
const EX_CONFIG: i32 = 78;

/// The envelope's own version, bumped only by a breaking change of shape.
const ENVELOPE_SCHEMA: u32 = 1;

/// v0.1's sole Collection, created with the Circle.
const COLLECTION: &str = "main";

/// The Provider compiled into this build.
const WALLPAPER: &str = "wallpaper";

// ── the honesty strings, verbatim (spec §7.2) ────────────────────────

/// Printed under any table with a Role in it; a Role shown without it is a bug.
const ROLE_CAVEAT_LONG: &str = "Roles are agreements, not enforcement. kith has no server: any Member's Device \
can add, change or delete Items in this Circle. What kith does guarantee is admission — only Devices you approve \
get in — and recovery: every other Device keeps the last 5 versions of every Item for 30 days.";

/// The one-line contexts: `status`, and anywhere a Role is a single cell.
const ROLE_CAVEAT_SHORT: &str = "Roles are agreements, not enforcement — admission is the only gate.";

/// Printed wherever attribution is shown — `kith list items` has an ADDED BY column.
const ATTRIBUTION_CAVEAT: &str = "Who added an Item is asserted by the Device that wrote the record, never proven \
— kith signs nothing. Attribution is believable because a human admitted that Device to the Circle, not because \
kith can verify what it says.";

/// The same honesty for the surface that lists People: a claim is not a proof.
const CLAIM_CAVEAT: &str = "A Membership claim is one Device saying which Person it speaks for. Nothing signs \
it — a claim is believable because a human admitted that Device to this Circle, not because kith can verify \
what it says.";

/// Presence with no engine behind it — never "not connected".
const PRESENCE_STALE: &str = "The Sync Engine is not answering, so this Device holds no connection state: every \
Presence reads unknown rather than not connected.";

// ── the verbs ────────────────────────────────────────────────────────

/// `kith list [items|circles|members]` — the Collection, the Circles, the People.
///
/// Never needs the Sync Engine: with it down the tree is still real, so every
/// subject exits 0. Only `status` treats unreachability as its own result.
pub async fn list(subject: Option<&str>, json: bool) -> i32 {
    let subject = match Subject::parse(subject) {
        Ok(s) => s,
        Err(f) => return Report::new("list", json).finish::<()>(None, Some(f), String::new),
    };

    let mut report = Report::new(subject.command(), json);

    let loaded = match configuration(&mut report) {
        Ok(loaded) => loaded,
        Err(f) => return report.finish::<()>(None, Some(f), String::new),
    };

    let me = identity(&mut report);
    let sync = connect(&loaded).await;
    if let Some(trouble) = &sync.trouble {
        report.note(WARN, "engine.unreachable", offline_line(trouble, sync.address.as_deref()));
    }
    let circles = sync.circles().await;

    match subject {
        Subject::Circles => list_circles(report, &sync, &circles, me.as_ref()).await,
        Subject::Items => match active(&circles) {
            Ok(circle) => list_items(report, &sync, circle),
            Err(f) => report.finish::<()>(None, Some(f), String::new),
        },
        Subject::Members => match active(&circles) {
            Ok(circle) => list_members(report, &sync, circle, me.as_ref()).await,
            Err(f) => report.finish::<()>(None, Some(f), String::new),
        },
    }
}

/// `kith status` — every Circle's sync state and per-peer completion.
///
/// Exits 69 when the Sync Engine is unreachable and prints the local facts
/// anyway, so it works as a health probe without becoming useless as a report.
pub async fn status(json: bool) -> i32 {
    let mut report = Report::new("status", json);

    let loaded = match configuration(&mut report) {
        Ok(loaded) => loaded,
        Err(f) => return report.finish::<()>(None, Some(f), String::new),
    };

    let me = identity(&mut report);
    let sync = connect(&loaded).await;
    let circles = sync.circles().await;

    let mut rows = Vec::new();
    let mut conflicts_seen = 0usize;
    for circle in &circles {
        let local = Local::read(&circle.root, sync.reserved());
        for trouble in &local.trouble {
            report.note(WARN, "circle.unreadable", trouble.clone());
        }

        let (peers, engine_status) = sync.look(&circle.id).await;
        let completion = completion_of(engine_status.as_ref());
        let (members, unclaimed) = member_rows(&local, peers.as_deref(), &completion, sync.local_device.as_deref());
        let conflicts = local.conflicts;
        conflicts_seen += conflicts.unwrap_or(0);

        rows.push(CircleStatusRow {
            id: circle.id.0.clone(),
            name: circle_name(circle, &local),
            root: circle.root.display().to_string(),
            state: engine_status.as_ref().map(|s| s.state.clone()),
            // The seam reports per-peer completion only; an aggregate here
            // would be invented.
            percent: None,
            bytes_needed: engine_status.as_ref().map(|s| s.bytes_needed),
            items: local.items.len(),
            conflicts,
            peers: members
                .iter()
                .filter(|m| !m.you)
                .map(|m| PeerRow {
                    person: m.person.clone(),
                    device: m.device.clone(),
                    presence: m.presence,
                    percent: m.percent,
                })
                .chain(unclaimed.iter().map(|u| PeerRow {
                    person: None,
                    device: u.device.clone(),
                    presence: u.presence,
                    percent: u.percent,
                }))
                .collect(),
            members: members.len(),
            connected: members.iter().filter(|m| m.presence == CONNECTED).count()
                + unclaimed.iter().filter(|u| u.presence == CONNECTED).count(),
            steward_device: local.steward_device().map(short_device),
            steward_person: local.steward_person(),
            steward_is_you: local.steward_is(sync.local_device.as_deref()),
            last_change: local.last_change.clone(),
        });
    }

    report.note(CAVEAT, "role.advisory", ROLE_CAVEAT_SHORT);
    if sync.trouble.is_some() {
        report.note(WARN, "presence.stale", PRESENCE_STALE);
    }
    if conflicts_seen > 0 {
        report.note(
            WARN,
            "circle.conflicts",
            format!(
                "{conflicts_seen} {} the Sync Engine made {} on disk next to their Items. v0.1 does not resolve these.",
                plural(conflicts_seen, "copy", "copies"),
                plural(conflicts_seen, "is", "are"),
            ),
        );
    }

    let data = StatusData {
        engine: EngineInfo {
            reachable: sync.trouble.is_none() && sync.address.is_some(),
            version: sync.version.clone(),
            address: sync.address.clone(),
            credentials: sync.credentials.as_ref().map(|p| p.display().to_string()),
        },
        person: me.as_ref().map(|id| PersonInfo {
            name: id.display_name.clone(),
            id: id.person.as_str().to_string(),
            device: sync.local_device.as_deref().map(short_device),
            device_full: sync.local_device.clone(),
        }),
        circles: rows,
    };

    // The one place unreachability is the answer rather than a degradation.
    let failure = sync
        .trouble
        .as_ref()
        .map(|t| engine_failure(t, sync.address.as_deref(), sync.credentials.as_deref()));

    let tty = std::io::stdout().is_terminal();
    let body = render_status(&data, tty);
    report.finish(Some(data), failure, || body)
}

// ── list: the three subjects ─────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Subject {
    Items,
    Circles,
    Members,
}

impl Subject {
    /// Items is the default, because the Collection is what a Person came for.
    fn parse(raw: Option<&str>) -> Result<Self, Failure> {
        match raw.map(str::trim).unwrap_or("items").to_ascii_lowercase().as_str() {
            "" | "items" | "item" => Ok(Subject::Items),
            "circles" | "circle" => Ok(Subject::Circles),
            "members" | "member" => Ok(Subject::Members),
            other => Err(Failure {
                code: "usage.unknown_subject",
                exit: EX_USAGE,
                message: format!("kith list has nothing called {other:?}"),
                detail: None,
                fix: Some("Say one of: kith list items, kith list circles, kith list members.".into()),
            }),
        }
    }

    fn command(self) -> &'static str {
        match self {
            Subject::Items => "list.items",
            Subject::Circles => "list.circles",
            Subject::Members => "list.members",
        }
    }
}

fn list_items(mut report: Report, sync: &Sync, circle: &CircleRef) -> i32 {
    let local = Local::read(&circle.root, sync.reserved());
    for trouble in &local.trouble {
        report.note(WARN, "circle.unreadable", trouble.clone());
    }
    if local.provider != WALLPAPER {
        report.note(
            WARN,
            "provider.unknown",
            format!(
                "This Collection names the {} Provider, which this build does not have — Items are listed without its facts.",
                local.provider
            ),
        );
    }

    let favourites = favourites_of(&circle.id.0);
    let rows = item_rows(&local, &favourites, local.provider == WALLPAPER);

    report.note(CAVEAT, "attribution.asserted", ATTRIBUTION_CAVEAT);

    let data = ItemsData {
        circle: CircleInfo {
            id: circle.id.0.clone(),
            name: circle_name(circle, &local),
            root: circle.root.display().to_string(),
        },
        collection: local.collection.clone(),
        items: rows,
    };
    let tty = std::io::stdout().is_terminal();
    let body = render_items(&data, tty);
    report.finish(Some(data), None, || body)
}

async fn list_circles(
    mut report: Report,
    sync: &Sync,
    circles: &[CircleRef],
    me: Option<&Identity>,
) -> i32 {
    let mut rows = Vec::new();
    for circle in circles {
        let local = Local::read(&circle.root, sync.reserved());
        for trouble in &local.trouble {
            report.note(WARN, "circle.unreadable", trouble.clone());
        }
        let state = sync.status_of(&circle.id).await.map(|s| s.state);
        rows.push(CircleRow {
            id: circle.id.0.clone(),
            name: circle_name(circle, &local),
            role: me.map(|id| local.role_of(&id.person)),
            members: local.people.len(),
            items: local.items.len(),
            state,
            root: circle.root.display().to_string(),
        });
    }

    if rows.is_empty() {
        report.note(
            INFO,
            "circle.none",
            "You are in no Circles yet. Run kith create <name>, or kith join <code> if someone invited you.",
        );
    }
    report.note(CAVEAT, "role.advisory", ROLE_CAVEAT_SHORT);

    let data = CirclesData { circles: rows };
    let tty = std::io::stdout().is_terminal();
    let body = render_circles(&data, tty);
    report.finish(Some(data), None, || body)
}

async fn list_members(
    mut report: Report,
    sync: &Sync,
    circle: &CircleRef,
    me: Option<&Identity>,
) -> i32 {
    let local = Local::read(&circle.root, sync.reserved());
    for trouble in &local.trouble {
        report.note(WARN, "circle.unreadable", trouble.clone());
    }

    let (peers, engine_status) = sync.look(&circle.id).await;
    let completion = completion_of(engine_status.as_ref());
    let (members, unclaimed) = member_rows(&local, peers.as_deref(), &completion, sync.local_device.as_deref());

    if peers.is_none() {
        report.note(WARN, "presence.stale", PRESENCE_STALE);
    }
    if let Some(id) = me
        && local.claims.iter().all(|c| c.person != id.person)
    {
        report.note(
            INFO,
            "member.claim_pending",
            "This Device has published no Membership claim in this Circle yet, so it names no row as you.",
        );
    }
    report.note(CAVEAT, "role.advisory", ROLE_CAVEAT_LONG);
    report.note(CAVEAT, "claim.asserted", CLAIM_CAVEAT);

    let data = MembersData {
        circle: CircleInfo {
            id: circle.id.0.clone(),
            name: circle_name(circle, &local),
            root: circle.root.display().to_string(),
        },
        members,
        unclaimed_devices: unclaimed,
    };
    let tty = std::io::stdout().is_terminal();
    let body = render_members(&data, tty);
    report.finish(Some(data), None, || body)
}

/// Which Circle a Circle-scoped verb acts on: the sole one, or a refusal.
///
/// `--circle` is not in this build's signature, and the CLI never guesses from
/// history.
fn active(circles: &[CircleRef]) -> Result<&CircleRef, Failure> {
    match circles {
        [] => Err(Failure {
            code: "circle.none",
            exit: EX_USAGE,
            message: "You are in no Circles yet.".into(),
            detail: None,
            fix: Some("Run kith create <name>, or kith join <code> if someone invited you.".into()),
        }),
        [only] => Ok(only),
        many => Err(Failure {
            code: "circle.ambiguous",
            exit: EX_USAGE,
            message: format!("you are in {} Circles; say which one with --circle <name>", many.len()),
            detail: Some(many.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join(", ")),
            fix: None,
        }),
    }
}

// ── local facts: one Circle's tree, read straight off disk ───────────

/// Everything one Circle says about itself on this Device. Asks the Sync Engine
/// nothing, which is what makes `kith list` work with the daemon down.
struct Local {
    descriptor: Option<CircleDescriptor>,
    collection: String,
    provider: String,
    items: Vec<Item>,
    claims: Vec<MembershipClaim>,
    people: Vec<Person>,
    /// PersonId → display name, from every claim including departed Members':
    /// a name has to keep resolving on the Items they added after they left.
    names: BTreeMap<String, String>,
    last_change: Option<String>,
    /// Copies the Sync Engine made. `None` when the seam could not be asked which
    /// names are the engine's own — a count nobody took is not a count of zero.
    conflicts: Option<usize>,
    /// What could not be read. Collected rather than raised: one hand-edited file
    /// must not blank a report.
    trouble: Vec<String>,
}

impl Local {
    fn read(root: &Path, reserved: Option<&[&'static str]>) -> Self {
        let mut trouble = Vec::new();

        let descriptor = match descriptors::read_circle(root) {
            Ok(d) => d,
            Err(e) => {
                trouble.push(format!("{}: {e}", root.display()));
                None
            }
        };

        // A Circle adopted before its descriptor arrived has no Collection
        // descriptor yet, which is a state and not a fault.
        let collection = COLLECTION.to_string();
        let provider = match descriptors::read_collection(root, &collection) {
            Ok(Some(d)) => d.provider,
            Ok(None) => WALLPAPER.to_string(),
            Err(e) => {
                trouble.push(format!("{}: {e}", root.display()));
                WALLPAPER.to_string()
            }
        };

        let records = match records::read_all(root, &collection) {
            Ok(r) => r,
            Err(e) => {
                trouble.push(format!("{}: {e}", root.display()));
                Vec::new()
            }
        };
        let last_change = records
            .iter()
            .map(|r| r.at().to_string())
            .max_by_key(|at| instant(at))
            .filter(|at| instant(at).is_some());
        let items = records::derive_items(&records, root);

        let claims = match claims::read_all(root) {
            Ok(c) => c,
            Err(e) => {
                trouble.push(format!("{}: {e}", root.display()));
                Vec::new()
            }
        };
        let people = claims::derive_people(&claims);
        let names = display_names(&claims);

        Self {
            descriptor,
            collection,
            provider,
            items,
            claims,
            people,
            names,
            last_change,
            conflicts: reserved.map(|globs| count_engine_copies(root, globs)),
            trouble,
        }
    }

    /// Admin iff this Person founded the Circle, member otherwise.
    fn role_of(&self, person: &PersonId) -> Role {
        match &self.descriptor {
            Some(d) if d.founder_person == person.as_str() => Role::Admin,
            _ => Role::Member,
        }
    }

    /// The Steward Device, read from the descriptor and never from the engine's
    /// peer flags: on the Steward's own Device no peer carries the flag.
    fn steward_device(&self) -> Option<&str> {
        self.descriptor.as_ref().map(|d| d.founder_device.as_str())
    }

    /// The Person that Device speaks for, if any claim names them. A Device that
    /// never ran kith has published none, and kith says so rather than invent one.
    fn steward_person(&self) -> Option<String> {
        let device = self.steward_device()?;
        let claim = self.claims.iter().find(|c| c.device == device)?;
        Some(claim.display_name.clone())
    }

    fn steward_is(&self, local_device: Option<&str>) -> bool {
        matches!((self.steward_device(), local_device), (Some(a), Some(b)) if a == b)
    }
}

/// Fold the claims into "who is this Person called".
///
/// Deliberately *not* [`claims::derive_people`], which drops a Person whose every
/// claim carries `left_at` — attribution has to outlive Membership. Newest
/// `asserted` wins, ties go to the smaller Device id, and an undatable claim never
/// wins a freshness tie-break against one kith can date.
fn display_names(claims: &[MembershipClaim]) -> BTreeMap<String, String> {
    let mut best: BTreeMap<String, (&MembershipClaim, Option<i128>)> = BTreeMap::new();
    for claim in claims {
        let asserted = instant(&claim.asserted);
        let key = claim.person.as_str().to_string();
        let fresher = match best.get(&key) {
            None => true,
            Some((held, held_at)) => match (asserted, held_at) {
                (Some(a), Some(h)) if a != *h => a > *h,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                _ => claim.device < held.device,
            },
        };
        if fresher {
            best.insert(key, (claim, asserted));
        }
    }
    best.into_iter()
        .map(|(person, (claim, _))| (person, claim.display_name.clone()))
        .collect()
}

/// How a Person is named where a claim can be found.
fn name_of(names: &BTreeMap<String, String>, person: &PersonId) -> Option<String> {
    names.get(person.as_str()).cloned()
}

/// An unresolvable attribution: the PersonId's short form, never a Device id.
fn unnamed(person_id: &str) -> String {
    let short: String = person_id.chars().take(8).collect();
    format!("unknown Person ({short})")
}

// ── rows ─────────────────────────────────────────────────────────────

/// One Item as `kith list items` renders it. No unseen dot: the CLI holds no
/// record of what this Person has looked at.
fn item_rows(local: &Local, favourites: &BTreeSet<String>, facts: bool) -> Vec<ItemRow> {
    let provider = facts.then(WallpaperProvider::default);
    local
        .items
        .iter()
        .map(|item| {
            let (width, height) = match (&provider, &item.path) {
                (Some(p), Some(path)) => {
                    // A record carries no Provider facts in this build, so the
                    // answer comes from the content itself.
                    let candidate = ImportCandidate { path, mime: None };
                    match p.extract_metadata(&candidate) {
                        Ok(f) => (f.width, f.height),
                        Err(_) => (None, None),
                    }
                }
                _ => (None, None),
            };
            ItemRow {
                id: item.id.as_str().to_string(),
                title: item.title.clone(),
                added_by: name_of(&local.names, &item.added_by),
                added_by_person: item.added_by.as_str().to_string(),
                added: normalise_time(&item.added_at),
                bytes: item.bytes,
                present: item.path.is_some(),
                path: item.path.as_ref().map(|p| p.display().to_string()),
                favourite: favourites.contains(item.id.as_str()),
                width,
                height,
            }
        })
        .collect()
}

/// The Members of a Circle, plus the Devices in it that no claim names. An
/// unclaimed Device is never hidden.
fn member_rows(
    local: &Local,
    peers: Option<&[PeerDevice]>,
    completion: &BTreeMap<String, f64>,
    local_device: Option<&str>,
) -> (Vec<MemberRow>, Vec<UnclaimedRow>) {
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    for claim in &local.claims {
        claimed.insert(claim.device.as_str());
    }

    let members = local
        .people
        .iter()
        .map(|person| {
            let you = person
                .devices
                .iter()
                .any(|d| Some(d.as_str()) == local_device);
            let (presence, in_circle) = presence_of(&person.devices, peers, you);
            let percent = person
                .devices
                .iter()
                .filter_map(|d| completion.get(d.as_str()).copied())
                .fold(None, |best: Option<f64>, p| Some(best.map_or(p, |b| b.max(p))));
            let asserted = local
                .claims
                .iter()
                .filter(|c| c.person == person.id)
                .max_by_key(|c| instant(&c.asserted))
                .map(|c| normalise_time(&c.asserted));

            MemberRow {
                person: Some(person.display_name.clone()),
                person_id: person.id.as_str().to_string(),
                role: local.role_of(&person.id),
                presence: presence.as_str(),
                device: person.devices.first().map(|d| short_device(d)).unwrap_or_default(),
                steward: person
                    .devices
                    .iter()
                    .any(|d| Some(d.as_str()) == local.steward_device()),
                you,
                in_circle,
                percent,
                asserted,
            }
        })
        .collect();

    let unclaimed = peers
        .unwrap_or(&[])
        .iter()
        .filter(|p| !claimed.contains(p.device.0.as_str()))
        .map(|p| UnclaimedRow {
            device: short_device(&p.device.0),
            name: p.name.clone(),
            presence: if p.connected { CONNECTED } else { NOT_CONNECTED },
            percent: completion.get(p.device.0.as_str()).copied(),
        })
        .collect();

    (members, unclaimed)
}

/// One Device's live view of one Person's Devices, and nothing more.
///
/// `Connected` if any of their Devices is connected to *this* one; `Unknown` when
/// the engine said nothing, and for this Person — kith holds no connection to
/// itself. The second value says whether the engine knows any of their Devices to
/// be in this Circle at all.
fn presence_of(devices: &[String], peers: Option<&[PeerDevice]>, you: bool) -> (Presence, Option<bool>) {
    if you {
        return (Presence::Unknown, None);
    }
    let Some(peers) = peers else {
        return (Presence::Unknown, None);
    };
    let mut seen = false;
    let mut connected = false;
    for device in devices {
        for peer in peers {
            if peer.device.0 == *device {
                seen = true;
                connected |= peer.connected;
            }
        }
    }
    match (seen, connected) {
        (true, true) => (Presence::Connected, Some(true)),
        (true, false) => (Presence::NotConnected, Some(true)),
        // A claim naming a Device the engine does not have in this Circle leaves
        // no connection state to report, and unknown is the honest value.
        (false, _) => (Presence::Unknown, Some(false)),
    }
}

// ── the Sync Engine, when it is there ────────────────────────────────

/// What this Device can currently say about the Sync Engine. `engine` is `Some`
/// only while it is answering, so nothing downstream asks a daemon that is gone.
struct Sync {
    engine: Option<SyncthingEngine>,
    address: Option<String>,
    credentials: Option<PathBuf>,
    version: Option<String>,
    trouble: Option<SyncError>,
    local_device: Option<String>,
    /// The globs the engine declares as its own, kept even when the daemon stops
    /// answering. `None` means kith never got as far as asking, and then it counts
    /// nothing rather than guess at names it must not know.
    reserved: Option<Vec<&'static str>>,
}

impl Sync {
    fn reserved(&self) -> Option<&[&'static str]> {
        self.reserved.as_deref()
    }

    /// The Circles this Device replicates. The engine's answer is the truth and
    /// is remembered; with it down the remembered copy keeps `kith list` working.
    async fn circles(&self) -> Vec<CircleRef> {
        if let Some(engine) = &self.engine
            && let Ok(circles) = engine.circles().await
        {
            remember_circles(&circles);
            return circles;
        }
        recall_circles()
    }

    async fn status_of(&self, circle: &CircleId) -> Option<CircleStatus> {
        self.engine.as_ref()?.status(circle).await.ok()
    }

    /// One Circle as the engine sees it: peers and sync state in one pass. `None`
    /// peers means it said nothing, which is what turns every Presence unknown.
    async fn look(&self, circle: &CircleId) -> (Option<Vec<PeerDevice>>, Option<CircleStatus>) {
        let Some(engine) = &self.engine else {
            return (None, None);
        };
        (engine.devices(circle).await.ok(), engine.status(circle).await.ok())
    }
}

/// Per-peer completion, keyed by Device. Bytes landing is all a percentage means.
fn completion_of(status: Option<&CircleStatus>) -> BTreeMap<String, f64> {
    status
        .map(|s| s.peers.iter().map(|p| (p.device.0.clone(), p.percent)).collect())
        .unwrap_or_default()
}

/// Discover the daemon and ask it whether it is there. Credentials are read,
/// never written: a rejected key is reported, never rotated or regenerated.
async fn connect(loaded: &config::Loaded) -> Sync {
    let Some(creds) = credentials(loaded) else {
        return Sync {
            engine: None,
            address: loaded.config.engine_address.clone(),
            credentials: None,
            version: None,
            trouble: Some(SyncError::Unreachable),
            local_device: None,
            reserved: None,
        };
    };

    let address = Some(creds.base_url.clone());
    let source = Some(creds.source.clone());
    let engine = SyncthingEngine::new(creds);
    // Asked once, up front, so a daemon that stops answering does not stop kith
    // from knowing which paths are the engine's own.
    let reserved = Some(engine.reserved_paths().to_vec());

    match engine.health().await {
        Ok(health) => {
            let local_device = engine.local_device().await.ok().map(|d| d.0);
            Sync {
                engine: Some(engine),
                address,
                credentials: source,
                version: Some(health.version),
                trouble: None,
                local_device,
                reserved,
            }
        }
        Err(trouble) => Sync {
            engine: None,
            address,
            credentials: source,
            version: None,
            trouble: Some(trouble),
            local_device: None,
            reserved,
        },
    }
}

fn credentials(loaded: &config::Loaded) -> Option<Credentials> {
    let cfg = &loaded.config;
    let discovered = SyncthingEngine::discover().ok();
    match (&cfg.engine_address, &cfg.engine_api_key) {
        (Some(address), Some(key)) => Some(Credentials {
            base_url: address.clone(),
            api_key: key.clone(),
            source: loaded.path.clone().unwrap_or_default(),
        }),
        _ => discovered.map(|mut creds| {
            if let Some(address) = &cfg.engine_address {
                creds.base_url = address.clone();
            }
            if let Some(key) = &cfg.engine_api_key {
                creds.api_key = key.clone();
            }
            creds
        }),
    }
}

/// The engine's own trouble, as a failure a Person can act on. Every fix line
/// points at `kith doctor`: the daemon's own name lives in exactly one module.
fn engine_failure(trouble: &SyncError, address: Option<&str>, credentials: Option<&Path>) -> Failure {
    let at = address.unwrap_or("the Sync Engine's address");
    match trouble {
        // No address means kith found no credentials to try, which is a different
        // sentence from a daemon that is not answering.
        SyncError::Unreachable if address.is_none() => Failure {
            code: "engine.unreachable",
            exit: EX_UNAVAILABLE,
            message: "kith found no Sync Engine credentials on this Device.".into(),
            detail: None,
            fix: Some("Run kith doctor — it names where kith looks and what has to be running.".into()),
        },
        SyncError::Unreachable => Failure {
            code: "engine.unreachable",
            exit: EX_UNAVAILABLE,
            message: format!("The Sync Engine is not answering at {at}."),
            detail: Some("Nothing is lost; changes sync when it returns.".into()),
            fix: Some("Start the Sync Engine daemon, then run: kith doctor".into()),
        },
        SyncError::Unauthorized => Failure {
            code: "engine.unauthorized",
            exit: EX_CONFIG,
            message: format!("The Sync Engine at {at} rejected the credentials kith found."),
            detail: credentials.map(|p| format!("read from {}", p.display())),
            fix: Some("Check that API key, or set [sync_engine] api_key in kith's config. kith never rewrites a key it did not issue.".into()),
        },
        SyncError::Incompatible(v) => Failure {
            code: "engine.incompatible",
            exit: EX_UNAVAILABLE,
            message: format!("The Sync Engine at {at} is below the version kith supports: {v}."),
            detail: None,
            fix: Some("Upgrade the Sync Engine daemon, then run: kith doctor".into()),
        },
        SyncError::NotFound => Failure {
            code: "engine.not_found",
            exit: EX_DATA,
            message: "The Sync Engine does not know this Circle.".into(),
            detail: None,
            fix: Some("Run kith doctor to see which Circles it does know.".into()),
        },
        SyncError::Engine(text) => Failure {
            code: "engine.failed",
            exit: EX_FAILED,
            message: format!("The Sync Engine at {at} answered with an error."),
            detail: Some(text.clone()),
            fix: None,
        },
    }
}

/// The `!` line a command that keeps working prints when the daemon is down.
fn offline_line(trouble: &SyncError, address: Option<&str>) -> String {
    match (trouble, address) {
        (SyncError::Unauthorized, Some(at)) => {
            format!("Sync Engine credentials rejected ({at}). Working from local content.")
        }
        (_, Some(at)) => format!("Sync Engine offline ({at}). Working from local content."),
        (_, None) => {
            "No Sync Engine credentials found — kith doctor says where it looks. Working from local content."
                .to_string()
        }
    }
}

// ── the Circles this Device knows about when the engine is down ──────
//
// The engine owns where a Circle's bytes live, so with the daemon down there is
// otherwise nothing to read a tree *from*. Written only from an answer the engine
// just gave, disposable, and the engine wins whenever both speak.

#[derive(Serialize, Deserialize, Default)]
struct KnownCircles {
    schema: u32,
    #[serde(default)]
    circle: Vec<KnownCircle>,
}

#[derive(Serialize, Deserialize, Clone)]
struct KnownCircle {
    id: String,
    name: String,
    root: String,
}

fn known_circles_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.cache_dir().join("kith/circles.toml"))
}

fn remember_circles(circles: &[CircleRef]) {
    let Some(path) = known_circles_path() else {
        return;
    };
    let known = KnownCircles {
        schema: 1,
        circle: circles
            .iter()
            .map(|c| KnownCircle {
                id: c.id.0.clone(),
                name: c.name.clone(),
                root: c.root.display().to_string(),
            })
            .collect(),
    };
    // Best effort throughout: a report must not fail because a cache is read-only.
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = toml::to_string_pretty(&known) {
        let _ = std::fs::write(&path, text);
    }
}

fn recall_circles() -> Vec<CircleRef> {
    let Some(path) = known_circles_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(known) = toml::from_str::<KnownCircles>(&text) else {
        return Vec::new();
    };
    known
        .circle
        .into_iter()
        .map(|c| CircleRef {
            id: CircleId(c.id),
            name: c.name,
            root: PathBuf::from(c.root),
        })
        .collect()
}

// ── Favourites: read, never written, never sent anywhere ─────────────

/// This Person's private marks, so `kith list items` can show the `★` column.
///
/// Read-only by construction: the toggle belongs to the Action, not to a report,
/// and Favourites live outside every synced tree.
fn favourites_of(circle: &str) -> BTreeSet<String> {
    let Some(path) = directories::BaseDirs::new().map(|b| b.data_dir().join("kith/favourites.jsonl")) else {
        return BTreeSet::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => fold_favourites(&text, circle),
        Err(_) => BTreeSet::new(),
    }
}

/// The effective set is the last operation per `(circle, item)`, and the log is
/// append-only, so file order *is* that order.
fn fold_favourites(text: &str, circle: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value.get("circle").and_then(serde_json::Value::as_str) != Some(circle) {
            continue;
        }
        let Some(item) = value.get("item").and_then(serde_json::Value::as_str) else {
            continue;
        };
        match value.get("k").and_then(serde_json::Value::as_str) {
            Some("fav") => {
                set.insert(item.to_string());
            }
            Some("unfav") => {
                set.remove(item);
            }
            _ => {}
        }
    }
    set
}

// ── copies the engine left behind ────────────────────────────────────

/// Count the copies the Sync Engine made inside a Circle.
///
/// The globs come from the seam. Plain directory names are what the walk *skips*;
/// filename patterns are what it counts. A count, never a resolution.
fn count_engine_copies(root: &Path, reserved: &[&str]) -> usize {
    let skip_dirs: Vec<&str> = reserved
        .iter()
        .filter_map(|glob| match glob.strip_suffix("/**") {
            Some(dir) => Some(dir),
            None if !glob.contains('*') => Some(glob),
            None => None,
        })
        .collect();
    let patterns: Vec<&str> = reserved
        .iter()
        .copied()
        .filter(|glob| glob.contains('*') && !glob.contains('/'))
        .collect();

    fn walk(dir: &Path, skip_dirs: &[&str], patterns: &[&str], depth: u32, found: &mut usize) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                // Neither kith's own tree nor the engine's is a copy sitting
                // next to an Item.
                if name == ".kith" || skip_dirs.contains(&name) {
                    continue;
                }
                walk(&entry.path(), skip_dirs, patterns, depth - 1, found);
            } else if patterns.iter().any(|p| glob_match(p, name)) {
                *found += 1;
            }
        }
    }

    let mut found = 0;
    walk(root, &skip_dirs, &patterns, 16, &mut found);
    found
}

/// `*` matches any run of characters; everything else is literal.
fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    if let Some(first) = parts.first() {
        let Some(stripped) = rest.strip_prefix(first) else {
            return false;
        };
        rest = stripped;
    }
    if let Some(last) = parts.last()
        && parts.len() > 1
    {
        if rest.len() < last.len() {
            return false;
        }
        let Some(stripped) = rest.strip_suffix(last) else {
            return false;
        };
        rest = stripped;
    }
    for middle in &parts[1..parts.len().saturating_sub(1)] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

// ── the JSON envelope (spec §3.2) ────────────────────────────────────

const INFO: &str = "info";
const WARN: &str = "warn";
const CAVEAT: &str = "caveat";

const CONNECTED: &str = "connected";
const NOT_CONNECTED: &str = "not_connected";

/// One invocation, one object — and nothing else on stdout while `--json` is on.
#[derive(Serialize)]
struct Envelope<T> {
    schema: u32,
    command: &'static str,
    ok: bool,
    exit: i32,
    data: Option<T>,
    error: Option<Failure>,
    notes: Vec<Note>,
}

/// The honesty channel: a caveat is a standing truth, not a problem.
#[derive(Serialize, Clone)]
struct Note {
    level: &'static str,
    code: &'static str,
    message: String,
}

#[derive(Serialize, Clone, Debug)]
struct Failure {
    code: &'static str,
    #[serde(skip_serializing)]
    exit: i32,
    message: String,
    detail: Option<String>,
    /// An imperative a Person can literally run, or nothing at all.
    fix: Option<String>,
}

/// Accumulates the notes a run produced and then prints exactly one report.
/// Nothing above this may print, on any path.
struct Report {
    command: &'static str,
    json: bool,
    notes: Vec<Note>,
}

impl Report {
    fn new(command: &'static str, json: bool) -> Self {
        Self { command, json, notes: Vec::new() }
    }

    fn note(&mut self, level: &'static str, code: &'static str, message: impl Into<String>) {
        self.notes.push(Note { level, code, message: message.into() });
    }

    /// Print the report and hand back the process's exit code. `data` is present
    /// even when a failure is attached — `kith status` with the engine down is a
    /// real answer plus a real failure.
    fn finish<T: Serialize>(
        self,
        data: Option<T>,
        error: Option<Failure>,
        human: impl FnOnce() -> String,
    ) -> i32 {
        let exit = error.as_ref().map(|e| e.exit).unwrap_or(EX_OK);

        if self.json {
            let envelope = Envelope {
                schema: ENVELOPE_SCHEMA,
                command: self.command,
                ok: exit == EX_OK,
                exit,
                data,
                error,
                notes: self.notes,
            };
            return match serde_json::to_string(&envelope) {
                Ok(line) => {
                    println!("{line}");
                    exit
                }
                // The envelope stays well-formed so the script parsing it still
                // gets one object.
                Err(e) => {
                    println!(
                        r#"{{"schema":{ENVELOPE_SCHEMA},"command":"{}","ok":false,"exit":{EX_INTERNAL},"data":null,"error":{{"code":"internal.unserialisable","message":{},"detail":null,"fix":null}},"notes":[]}}"#,
                        self.command,
                        serde_json::to_string(&e.to_string()).unwrap_or_else(|_| "\"\"".into()),
                    );
                    EX_INTERNAL
                }
            };
        }

        // Failure first, then what degraded, then the data on stdout, then the
        // standing caveats under it.
        if let Some(error) = &error {
            eprintln!("✗ {}", error.message);
            if let Some(fix) = &error.fix {
                eprintln!("  → {fix}");
            }
        }
        for note in self.notes.iter().filter(|n| n.level != CAVEAT) {
            let mark = if note.level == WARN { "!" } else { " " };
            eprintln!("{mark} {}", note.message);
        }

        let body = human();
        if !body.is_empty() {
            print!("{body}");
        }

        for note in self.notes.iter().filter(|n| n.level == CAVEAT) {
            eprintln!();
            eprintln!("{}", wrap(&note.message, 78));
        }
        exit
    }
}

// ── the data each verb emits ─────────────────────────────────────────

#[derive(Serialize)]
struct CircleInfo {
    id: String,
    name: String,
    root: String,
}

#[derive(Serialize)]
struct ItemsData {
    circle: CircleInfo,
    collection: String,
    items: Vec<ItemRow>,
}

#[derive(Serialize)]
struct ItemRow {
    /// The full Item id — a stable handle, addressed by prefix elsewhere.
    id: String,
    title: String,
    /// The Person's name, or `null` when no claim names them yet;
    /// `added_by_person` always carries the PersonId.
    added_by: Option<String>,
    added_by_person: String,
    added: String,
    bytes: Option<u64>,
    /// Whether this Device holds the bytes. Metadata outruns content by design,
    /// so `false` is an ordinary arrival state and not a fault.
    present: bool,
    path: Option<String>,
    /// Private to this Person, and never sent anywhere.
    favourite: bool,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Serialize)]
struct CirclesData {
    circles: Vec<CircleRow>,
}

#[derive(Serialize)]
struct CircleRow {
    id: String,
    name: String,
    role: Option<Role>,
    members: usize,
    items: usize,
    /// `null` when the Sync Engine is not answering.
    state: Option<String>,
    root: String,
}

#[derive(Serialize)]
struct MembersData {
    circle: CircleInfo,
    members: Vec<MemberRow>,
    /// Devices in this Circle that no Membership claim names. Never hidden.
    unclaimed_devices: Vec<UnclaimedRow>,
}

#[derive(Serialize)]
struct MemberRow {
    person: Option<String>,
    person_id: String,
    role: Role,
    /// Always `connected`, `not_connected` or `unknown` — never "online".
    presence: &'static str,
    device: String,
    /// Read from the Circle descriptor, never from the seam's peer flags.
    steward: bool,
    /// This Device's own Person, whose Presence is always `unknown`.
    you: bool,
    /// Whether the engine knows a Device of theirs to be in this Circle. `null`
    /// when the engine said nothing.
    in_circle: Option<bool>,
    /// Bytes landing is all it means: nothing here says anybody looked.
    percent: Option<f64>,
    /// When a Device of theirs last wrote its claim. Not a join date.
    asserted: Option<String>,
}

#[derive(Serialize)]
struct UnclaimedRow {
    device: String,
    /// The name the Device advertises to the transport. Not a Person.
    name: String,
    presence: &'static str,
    percent: Option<f64>,
}

#[derive(Serialize)]
struct StatusData {
    engine: EngineInfo,
    person: Option<PersonInfo>,
    circles: Vec<CircleStatusRow>,
}

#[derive(Serialize)]
struct EngineInfo {
    reachable: bool,
    version: Option<String>,
    address: Option<String>,
    /// Where the credentials were read from, so a rejected key can be fixed there.
    credentials: Option<String>,
}

#[derive(Serialize)]
struct PersonInfo {
    name: String,
    id: String,
    device: Option<String>,
    device_full: Option<String>,
}

#[derive(Serialize)]
struct CircleStatusRow {
    id: String,
    name: String,
    root: String,
    state: Option<String>,
    /// Always `null`: the seam reports per-peer completion only.
    percent: Option<f64>,
    bytes_needed: Option<u64>,
    items: usize,
    /// `null` when the seam could not be asked — a count nobody took is not zero.
    conflicts: Option<usize>,
    members: usize,
    connected: usize,
    peers: Vec<PeerRow>,
    steward_device: Option<String>,
    steward_person: Option<String>,
    steward_is_you: bool,
    last_change: Option<String>,
}

#[derive(Serialize)]
struct PeerRow {
    person: Option<String>,
    device: String,
    presence: &'static str,
    percent: Option<f64>,
}

// ── human rendering ──────────────────────────────────────────────────
//
// Piped output drops the header and the padding, separates cells with one tab so
// `cut -f2` works, and truncates nothing.

fn render_items(data: &ItemsData, tty: bool) -> String {
    if data.items.is_empty() {
        return if tty {
            format!(
                "Nothing in {} yet. kith add <paths…>, or wait — Items appear as they arrive.\n",
                data.circle.name
            )
        } else {
            String::new()
        };
    }

    let rows: Vec<Vec<String>> = data
        .items
        .iter()
        .map(|item| {
            vec![
                if item.favourite { "★".to_string() } else { String::new() },
                item.id.chars().take(8).collect(),
                item.title.clone(),
                item.added_by
                    .clone()
                    .unwrap_or_else(|| unnamed(&item.added_by_person)),
                relative(&item.added),
                item.bytes.map(bytes_human).unwrap_or_else(|| "—".into()),
                match (item.width, item.height) {
                    (Some(w), Some(h)) => format!("{w}×{h}"),
                    // A fact kith cannot verify prints as an em dash, never as a
                    // plausible number.
                    _ => "—".into(),
                },
            ]
        })
        .collect();

    table(
        &["", "ID", "TITLE", "ADDED BY", "ADDED", "SIZE", "DIMENSIONS"],
        &rows,
        &[1, 8, 28, 26, 16, 9, 11],
        tty,
    )
}

fn render_circles(data: &CirclesData, tty: bool) -> String {
    if data.circles.is_empty() {
        return String::new();
    }
    let rows: Vec<Vec<String>> = data
        .circles
        .iter()
        .map(|c| {
            vec![
                c.name.clone(),
                c.id.clone(),
                c.role.map(role_word).unwrap_or("—").to_string(),
                c.members.to_string(),
                c.items.to_string(),
                c.state.clone().unwrap_or_else(|| "unknown".into()),
                tilde(Path::new(&c.root)),
            ]
        })
        .collect();
    table(
        &["NAME", "ID", "ROLE", "MEMBERS", "ITEMS", "STATE", "ROOT"],
        &rows,
        &[20, 16, 6, 7, 6, 10, 40],
        tty,
    )
}

fn render_members(data: &MembersData, tty: bool) -> String {
    let mut rows: Vec<Vec<String>> = data
        .members
        .iter()
        .map(|m| {
            let person = match &m.person {
                Some(name) if m.you => format!("{name} (you)"),
                Some(name) => name.clone(),
                None => unnamed(&m.person_id),
            };
            vec![
                person,
                role_word(m.role).to_string(),
                presence_word(m.presence, m.in_circle, m.you).to_string(),
                m.device.clone(),
                if m.steward { "steward".to_string() } else { String::new() },
            ]
        })
        .collect();

    // An unclaimed Device gets a row of its own, never dressed up as a Person.
    for device in &data.unclaimed_devices {
        rows.push(vec![
            format!("· {} — no Membership claim yet", device.name),
            String::new(),
            presence_word(device.presence, None, false).to_string(),
            device.device.clone(),
            String::new(),
        ]);
    }

    if rows.is_empty() {
        return if tty {
            "No Membership claims in this Circle yet.\n".to_string()
        } else {
            String::new()
        };
    }

    // The PERSON column is wide enough for the whole of an unclaimed Device's
    // label: a truncated caveat is a caveat nobody reads.
    table(
        &["PERSON", "ROLE", "PRESENCE", "DEVICE", "STEWARD"],
        &rows,
        &[44, 6, 18, 8, 7],
        tty,
    )
}

fn render_status(data: &StatusData, _tty: bool) -> String {
    let mut out = String::new();

    let engine = match (&data.engine.reachable, &data.engine.address) {
        (true, Some(address)) => format!(
            "reachable · {} · {address}",
            data.engine.version.as_deref().unwrap_or("version unknown")
        ),
        (false, Some(address)) => format!("not answering · {address} · these are local facts, last known"),
        _ => "no credentials found — kith doctor says where it looks".to_string(),
    };
    out.push_str(&format!("Sync Engine   {engine}\n"));

    let you = match &data.person {
        Some(p) => match &p.device {
            Some(device) => format!("{} ({device})", p.name),
            None => p.name.clone(),
        },
        None => "no Identity yet — run kith init".to_string(),
    };
    out.push_str(&format!("You           {you}\n"));

    if data.circles.is_empty() {
        out.push_str("\nYou are in no Circles yet. Run kith create <name>, or kith join <code>.\n");
        return out;
    }

    for circle in &data.circles {
        out.push('\n');
        out.push_str(&format!(
            "{}  {}  {}\n",
            circle.name,
            circle.id,
            tilde(Path::new(&circle.root))
        ));

        let state = match (&circle.state, circle.bytes_needed) {
            (Some(state), Some(bytes)) if bytes > 0 => {
                format!("{state} · {} to receive", bytes_human(bytes))
            }
            (Some(state), _) => state.clone(),
            (None, _) => "unknown — the Sync Engine is not answering".to_string(),
        };
        out.push_str(&format!("  state     {state}\n"));
        out.push_str(&format!("  items     {}\n", circle.items));

        let mut members = format!(
            "{} {}",
            circle.members,
            plural(circle.members, "Member", "Members")
        );
        if data.engine.reachable {
            members.push_str(&format!(", {} connected", circle.connected));
        } else {
            members.push_str(", presence unknown");
        }
        let peers: Vec<String> = circle
            .peers
            .iter()
            .map(|p| {
                let who = p.person.clone().unwrap_or_else(|| p.device.clone());
                match p.percent {
                    Some(percent) => format!("{who} {percent:.0}%"),
                    None => format!("{who} {}", presence_word(p.presence, None, false)),
                }
            })
            .collect();
        if !peers.is_empty() {
            members.push_str(&format!(" — {}", peers.join(", ")));
        }
        out.push_str(&format!("  members   {members}\n"));

        let steward = match (&circle.steward_device, &circle.steward_person, circle.steward_is_you) {
            (Some(_), _, true) => "you — every join is approved on this Device".to_string(),
            (Some(device), Some(person), false) => format!("{person} ({device})"),
            (Some(device), None, false) => {
                format!("{device} — no Membership claim yet, so kith cannot name the Person")
            }
            (None, _, _) => "unknown — this Circle has no descriptor yet".to_string(),
        };
        out.push_str(&format!("  steward   {steward}\n"));

        if let Some(copies) = circle.conflicts.filter(|n| *n > 0) {
            out.push_str(&format!(
                "  copies    {copies} {} the Sync Engine made, next to their Items\n",
                plural(copies, "copy", "copies")
            ));
        }

        let changed = circle
            .last_change
            .as_ref()
            .map(|at| relative(at))
            .unwrap_or_else(|| "nothing recorded yet".to_string());
        out.push_str(&format!("  changed   {changed}\n"));
    }

    out
}

fn role_word(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

/// The one place a Presence is turned into words, so no surface can invent a
/// fourth one — least of all "online". `you` is this Device, which holds no
/// connection to itself and will not pretend to.
fn presence_word(presence: &str, in_circle: Option<bool>, you: bool) -> &'static str {
    match (you, presence, in_circle) {
        (true, _, _) => "this Device",
        (_, CONNECTED, _) => "connected",
        (_, NOT_CONNECTED, _) => "not connected",
        (_, _, Some(false)) => "not in this Circle",
        _ => "unknown",
    }
}

// ── formatting helpers ───────────────────────────────────────────────

fn table(header: &[&str], rows: &[Vec<String>], caps: &[usize], tty: bool) -> String {
    if !tty {
        let mut out = String::new();
        for row in rows {
            out.push_str(&row.join("\t"));
            out.push('\n');
        }
        return out;
    }

    let clipped: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| clip(cell, caps.get(i).copied().unwrap_or(0)))
                .collect()
        })
        .collect();

    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in &clipped {
        for (i, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            if i < widths.len() && width > widths[i] {
                widths[i] = width;
            }
        }
    }

    let mut out = String::new();
    out.push_str(&line(&header.iter().map(|h| (*h).to_string()).collect::<Vec<_>>(), &widths));
    for row in &clipped {
        out.push_str(&line(row, &widths));
    }
    out
}

fn line(cells: &[String], widths: &[usize]) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        let width = widths.get(i).copied().unwrap_or(0);
        out.push_str(cell);
        if i + 1 < cells.len() {
            let pad = width.saturating_sub(cell.chars().count()) + 2;
            out.push_str(&" ".repeat(pad));
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('\n');
    out
}

fn clip(text: &str, max: usize) -> String {
    if max == 0 || text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Wrap a caveat so it reads as a paragraph rather than one long line.
fn wrap(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut column = 0;
    for word in text.split_whitespace() {
        let len = word.chars().count();
        if column > 0 && column + 1 + len > width {
            out.push('\n');
            column = 0;
        } else if column > 0 {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += len;
    }
    out
}

/// SI units, because the labels say SI — `kith add` and `kith list` must never
/// print two different sizes for one Item.
fn bytes_human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

fn short_device(device: &str) -> String {
    device.chars().take(7).collect()
}

/// `~/kith/walls` rather than `/home/ana/kith/walls`.
fn tilde(path: &Path) -> String {
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    match home.and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A timestamp as a comparable instant, or `None` when it will not parse.
fn instant(at: &str) -> Option<i128> {
    at.parse::<jiff::Timestamp>().ok().map(|t| t.as_nanosecond())
}

/// RFC 3339 UTC on the way out; a timestamp kith cannot parse is passed through.
fn normalise_time(at: &str) -> String {
    match at.parse::<jiff::Timestamp>() {
        Ok(t) => t.to_string(),
        Err(_) => at.to_string(),
    }
}

/// Human time in this Device's zone, coarse on purpose: the record carries the
/// writer's wall clock, which is not a happens-before.
fn relative(at: &str) -> String {
    let Ok(then) = at.parse::<jiff::Timestamp>() else {
        return at.to_string();
    };
    let now = jiff::Timestamp::now();
    let seconds = (now.as_nanosecond() - then.as_nanosecond()) / 1_000_000_000;

    match seconds {
        s if s < 0 => "in the future".to_string(),
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => {
            let m = s / 60;
            format!("{m} {} ago", plural(m as usize, "minute", "minutes"))
        }
        s if s < 86_400 => {
            let h = s / 3600;
            format!("{h} {} ago", plural(h as usize, "hour", "hours"))
        }
        s if s < 172_800 => "yesterday".to_string(),
        _ => {
            let zoned = then.to_zoned(jiff::tz::TimeZone::system());
            let date = zoned.date();
            let today = now.to_zoned(jiff::tz::TimeZone::system()).date();
            if date.year() == today.year() {
                format!("{} {}", date.day(), month_name(date.month()))
            } else {
                format!("{} {} {}", date.day(), month_name(date.month()), date.year())
            }
        }
    }
}

fn month_name(month: i8) -> &'static str {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .get((month.max(1) as usize) - 1)
        .copied()
        .unwrap_or("???")
}

/// Prefers the descriptor every Member holds over the transport's own label.
fn circle_name(circle: &CircleRef, local: &Local) -> String {
    match &local.descriptor {
        Some(d) if !d.name.is_empty() => d.name.clone(),
        _ if !circle.name.is_empty() => circle.name.clone(),
        _ => circle.id.0.clone(),
    }
}

/// The config, plus its unknown-key warnings as notes, or the failure to print.
fn configuration(report: &mut Report) -> Result<config::Loaded, Failure> {
    match config::inspect() {
        Ok(loaded) => {
            for warning in loaded.warnings() {
                report.note(WARN, config::UNKNOWN_KEY_NOTE, warning);
            }
            Ok(loaded)
        }
        Err(e) => Err(Failure {
            code: e.code(),
            exit: EX_CONFIG,
            message: e.to_string(),
            detail: None,
            fix: e.fix(),
        }),
    }
}

/// This Person, if they have minted one. A missing Identity is said out loud and
/// the report continues.
fn identity(report: &mut Report) -> Option<Identity> {
    match crate::identity::load() {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            report.note(
                INFO,
                "identity.missing",
                "No Identity on this Device yet — run kith init. Listing still works.",
            );
            None
        }
        Err(e) => {
            report.note(WARN, "identity.unreadable", format!("{e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ItemId;

    const ANA_DEVICE: &str = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2";
    const BEN_DEVICE: &str = "K5J2FVL-B3QTXAO-7SWNDUE-HMR4YZI-6CPGA2N-XQTLB5V-JW3EOHY-RD6MSAK";

    /// Invented on purpose: this module cannot recognise a real one.
    const RESERVED_FIXTURE: &[&str] = &["bookkeeping", "archive/**", "*.engine-copy-*", "~engine~*.tmp"];

    fn claim(device: &str, person: &PersonId, name: &str, asserted: &str, left: Option<&str>) -> MembershipClaim {
        MembershipClaim {
            schema: 1,
            device: device.to_string(),
            person: person.clone(),
            display_name: name.to_string(),
            asserted: asserted.to_string(),
            left_at: left.map(str::to_string),
        }
    }

    fn local(descriptor: Option<CircleDescriptor>, claims: Vec<MembershipClaim>, items: Vec<Item>) -> Local {
        let people = claims::derive_people(&claims);
        let names = display_names(&claims);
        Local {
            descriptor,
            collection: COLLECTION.to_string(),
            provider: WALLPAPER.to_string(),
            items,
            claims,
            people,
            names,
            last_change: None,
            conflicts: Some(0),
            trouble: Vec::new(),
        }
    }

    fn descriptor(founder_person: &PersonId, founder_device: &str) -> CircleDescriptor {
        CircleDescriptor {
            schema: 1,
            id: "kith-4tj2q9xa".into(),
            name: "walls".into(),
            created: "2026-08-07T09:02:11Z".into(),
            founder_person: founder_person.as_str().to_string(),
            founder_device: founder_device.to_string(),
        }
    }

    fn item(id: &ItemId, by: &PersonId, at: &str, title: &str, present: bool) -> Item {
        Item {
            id: id.clone(),
            title: title.to_string(),
            added_by: by.clone(),
            added_at: at.to_string(),
            path: present.then(|| PathBuf::from("/tmp/kith-report-tests/sunset.png")),
            hash: Some("b3:aa".into()),
            bytes: Some(1_993_421),
        }
    }

    fn peer(device: &str, name: &str, connected: bool) -> PeerDevice {
        PeerDevice {
            device: crate::engine::DeviceId(device.to_string()),
            name: name.to_string(),
            connected,
            introducer: false,
        }
    }

    // ── the subject grammar ──────────────────────────────────────────

    #[test]
    fn items_is_the_default_subject_and_the_three_are_the_whole_list() {
        assert_eq!(Subject::parse(None).unwrap(), Subject::Items);
        assert_eq!(Subject::parse(Some("items")).unwrap(), Subject::Items);
        assert_eq!(Subject::parse(Some("Circles")).unwrap(), Subject::Circles);
        assert_eq!(Subject::parse(Some(" member ")).unwrap(), Subject::Members);

        let refused = Subject::parse(Some("everything")).unwrap_err();
        assert_eq!(refused.exit, EX_USAGE);
        assert!(refused.fix.unwrap().contains("kith list members"));
    }

    #[test]
    fn each_subject_names_itself_in_the_envelope() {
        assert_eq!(Subject::Items.command(), "list.items");
        assert_eq!(Subject::Circles.command(), "list.circles");
        assert_eq!(Subject::Members.command(), "list.members");
    }

    // ── which Circle a Circle-scoped verb acts on ────────────────────

    #[test]
    fn one_circle_needs_no_saying_and_three_are_refused_by_name() {
        let circle = |id: &str, name: &str| CircleRef {
            id: CircleId(id.into()),
            name: name.into(),
            root: PathBuf::from("/tmp/kith"),
        };

        let one = vec![circle("kith-a", "walls")];
        assert_eq!(active(&one).unwrap().name, "walls");

        let none: Vec<CircleRef> = Vec::new();
        assert_eq!(active(&none).unwrap_err().exit, EX_USAGE);

        let many = vec![circle("kith-a", "walls"), circle("kith-b", "photos")];
        let refused = active(&many).unwrap_err();
        assert_eq!(refused.exit, EX_USAGE);
        assert!(refused.message.contains("--circle"), "{}", refused.message);
        assert!(refused.detail.unwrap().contains("photos"));
    }

    // ── attribution ──────────────────────────────────────────────────

    #[test]
    fn attribution_outlives_membership() {
        let ana = PersonId::generate();
        let claims = vec![claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:00:00Z", Some("2026-09-01T18:20:00Z"))];

        assert!(claims::derive_people(&claims).is_empty(), "she left the Circle");
        assert_eq!(
            name_of(&display_names(&claims), &ana).as_deref(),
            Some("Ana"),
            "and her Items still say Ana"
        );
    }

    #[test]
    fn the_newest_assertion_names_a_person_and_an_undatable_claim_never_wins() {
        let ana = PersonId::generate();
        let names = display_names(&[
            claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:02:02Z", None),
            claim(BEN_DEVICE, &ana, "Ana Ruiz", "2026-08-07T09:02:02.117Z", None),
        ]);
        assert_eq!(names.get(ana.as_str()).map(String::as_str), Some("Ana Ruiz"));

        let names = display_names(&[
            claim(ANA_DEVICE, &ana, "dated", "2026-08-07T09:02:11Z", None),
            claim(BEN_DEVICE, &ana, "whenever", "last tuesday", None),
        ]);
        assert_eq!(names.get(ana.as_str()).map(String::as_str), Some("dated"));
    }

    #[test]
    fn a_person_no_claim_names_is_a_person_id_and_never_a_device_id() {
        let stranger = PersonId::generate();
        assert!(name_of(&BTreeMap::new(), &stranger).is_none());
        let printed = unnamed(stranger.as_str());
        assert!(printed.starts_with("unknown Person (p-"), "{printed}");
        assert!(!printed.contains(ANA_DEVICE));
    }

    // ── Roles and the Steward ────────────────────────────────────────

    #[test]
    fn the_founder_is_the_admin_and_everybody_else_is_a_member() {
        let ana = PersonId::generate();
        let ben = PersonId::generate();
        let l = local(Some(descriptor(&ana, ANA_DEVICE)), Vec::new(), Vec::new());

        assert_eq!(l.role_of(&ana), Role::Admin);
        assert_eq!(l.role_of(&ben), Role::Member);
    }

    #[test]
    fn a_circle_with_no_descriptor_yet_claims_no_admin() {
        let ana = PersonId::generate();
        let l = local(None, Vec::new(), Vec::new());
        assert_eq!(l.role_of(&ana), Role::Member, "no descriptor, no admin claim");
        assert!(l.steward_device().is_none());
    }

    /// The Steward's Device comes off disk, so it reads the same on every Device
    /// — including the Steward's own, where no peer carries the transport's flag.
    #[test]
    fn the_steward_is_read_from_the_descriptor_and_named_only_when_a_claim_names_them() {
        let ana = PersonId::generate();

        let unnamed = local(Some(descriptor(&ana, ANA_DEVICE)), Vec::new(), Vec::new());
        assert_eq!(unnamed.steward_device(), Some(ANA_DEVICE));
        assert_eq!(unnamed.steward_person(), None, "a Device that never ran kith names no Person");

        let named = local(
            Some(descriptor(&ana, ANA_DEVICE)),
            vec![claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:00:00Z", None)],
            Vec::new(),
        );
        assert_eq!(named.steward_person().as_deref(), Some("Ana"));
        assert!(named.steward_is(Some(ANA_DEVICE)));
        assert!(!named.steward_is(Some(BEN_DEVICE)));
        assert!(!named.steward_is(None));
    }

    // ── presence ─────────────────────────────────────────────────────

    #[test]
    fn presence_is_this_devices_own_view_and_unknown_is_a_real_answer() {
        let devices = vec![BEN_DEVICE.to_string()];

        // The engine said nothing at all.
        assert_eq!(presence_of(&devices, None, false), (Presence::Unknown, None));
        // It answered, and Ben is connected to this Device.
        let (presence, in_circle) = presence_of(&devices, Some(&[peer(BEN_DEVICE, "ben-thinkpad", true)]), false);
        assert_eq!(presence, Presence::Connected);
        assert_eq!(in_circle, Some(true));
        // It answered, and he is not.
        let (presence, _) = presence_of(&devices, Some(&[peer(BEN_DEVICE, "ben-thinkpad", false)]), false);
        assert_eq!(presence, Presence::NotConnected);
        // It answered, and his Device is not in this Circle at all.
        let (presence, in_circle) = presence_of(&devices, Some(&[peer(ANA_DEVICE, "ana-desk", true)]), false);
        assert_eq!(presence, Presence::Unknown, "no state about them is not 'not connected'");
        assert_eq!(in_circle, Some(false));
    }

    #[test]
    fn kith_never_claims_a_connection_to_itself() {
        let devices = vec![ANA_DEVICE.to_string()];
        assert_eq!(
            presence_of(&devices, Some(&[peer(ANA_DEVICE, "ana-desk", true)]), true),
            (Presence::Unknown, None)
        );
    }

    #[test]
    fn no_surface_word_for_presence_is_ever_online() {
        assert_eq!(Presence::Connected.as_str(), CONNECTED);
        assert_eq!(Presence::NotConnected.as_str(), NOT_CONNECTED);
        assert_eq!(Presence::Unknown.as_str(), "unknown");

        for presence in [CONNECTED, NOT_CONNECTED, "unknown"] {
            assert!(!presence_word(presence, None, false).contains("online"));
        }
        let ana = PersonId::generate();
        let row = MemberRow {
            person: Some("Ana".into()),
            person_id: ana.as_str().into(),
            role: Role::Admin,
            presence: "unknown",
            device: short_device(ANA_DEVICE),
            steward: true,
            you: true,
            in_circle: None,
            percent: None,
            asserted: None,
        };
        assert_eq!(presence_word(row.presence, row.in_circle, row.you), "this Device");
    }

    // ── the Members table ────────────────────────────────────────────

    #[test]
    fn members_carry_role_presence_steward_and_the_person_they_are() {
        let ana = PersonId::generate();
        let ben = PersonId::generate();
        let l = local(
            Some(descriptor(&ana, ANA_DEVICE)),
            vec![
                claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:00:00Z", None),
                claim(BEN_DEVICE, &ben, "Ben", "2026-08-07T10:00:00Z", None),
            ],
            Vec::new(),
        );
        let peers = vec![peer(BEN_DEVICE, "ben-thinkpad", true)];
        let completion = BTreeMap::from([(BEN_DEVICE.to_string(), 91.0)]);

        let (members, unclaimed) = member_rows(&l, Some(&peers), &completion, Some(ANA_DEVICE));
        assert_eq!(members.len(), 2);
        assert!(unclaimed.is_empty());

        let me = members.iter().find(|m| m.you).expect("this Device is a row");
        assert_eq!(me.person.as_deref(), Some("Ana"));
        assert_eq!(me.role, Role::Admin);
        assert!(me.steward, "the mark comes off disk, not off the transport");
        assert_eq!(me.presence, "unknown");
        assert_eq!(me.device, "P56IOI7");

        let them = members.iter().find(|m| !m.you).expect("Ben is a row");
        assert_eq!(them.role, Role::Member);
        assert_eq!(them.presence, CONNECTED);
        assert_eq!(them.percent, Some(91.0));
        assert!(!them.steward);
    }

    #[test]
    fn a_device_no_claim_names_still_gets_a_row() {
        let ana = PersonId::generate();
        let l = local(
            Some(descriptor(&ana, ANA_DEVICE)),
            vec![claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:00:00Z", None)],
            Vec::new(),
        );
        let peers = vec![peer(BEN_DEVICE, "ben-thinkpad", true)];

        let (members, unclaimed) = member_rows(&l, Some(&peers), &BTreeMap::new(), Some(ANA_DEVICE));
        assert_eq!(members.len(), 1);
        assert_eq!(unclaimed.len(), 1);
        assert_eq!(unclaimed[0].device, "K5J2FVL");
        assert_eq!(unclaimed[0].name, "ben-thinkpad");
        assert_eq!(unclaimed[0].presence, CONNECTED);

        let rendered = render_members(
            &MembersData {
                circle: CircleInfo { id: "kith-a".into(), name: "walls".into(), root: "/tmp".into() },
                members,
                unclaimed_devices: unclaimed,
            },
            true,
        );
        assert!(rendered.contains("no Membership claim yet"), "{rendered}");
    }

    #[test]
    fn with_the_engine_silent_every_presence_is_unknown() {
        let ana = PersonId::generate();
        let ben = PersonId::generate();
        let l = local(
            Some(descriptor(&ana, ANA_DEVICE)),
            vec![
                claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:00:00Z", None),
                claim(BEN_DEVICE, &ben, "Ben", "2026-08-07T10:00:00Z", None),
            ],
            Vec::new(),
        );

        let (members, unclaimed) = member_rows(&l, None, &BTreeMap::new(), None);
        assert!(unclaimed.is_empty(), "no engine, no Devices to report");
        assert!(members.iter().all(|m| m.presence == "unknown"));
        assert!(members.iter().all(|m| m.in_circle.is_none()));
    }

    // ── the Items table ──────────────────────────────────────────────

    #[test]
    fn item_rows_carry_attribution_favourites_and_a_stable_handle() {
        let ana = PersonId::generate();
        let id = ItemId::generate();
        let l = local(
            None,
            vec![claim(ANA_DEVICE, &ana, "Ana", "2026-08-07T09:00:00Z", None)],
            vec![item(&id, &ana, "2026-08-07T09:14:02.117Z", "sunset", false)],
        );
        let favourites = BTreeSet::from([id.as_str().to_string()]);

        let rows = item_rows(&l, &favourites, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id.as_str(), "the full id is the handle a script keeps");
        assert_eq!(rows[0].added_by.as_deref(), Some("Ana"));
        assert_eq!(rows[0].added_by_person, ana.as_str());
        assert!(rows[0].favourite);
        assert!(!rows[0].present, "a record whose bytes have not arrived is still an Item");
        assert_eq!(rows[0].bytes, Some(1_993_421));
    }

    #[test]
    fn an_item_nobody_can_attribute_still_lists() {
        let stranger = PersonId::generate();
        let id = ItemId::generate();
        let l = local(None, Vec::new(), vec![item(&id, &stranger, "2026-08-07T09:00:00Z", "sunset", false)]);

        let rows = item_rows(&l, &BTreeSet::new(), false);
        assert_eq!(rows[0].added_by, None);
        assert_eq!(rows[0].added_by_person, stranger.as_str());

        let rendered = render_items(
            &ItemsData {
                circle: CircleInfo { id: "kith-a".into(), name: "walls".into(), root: "/tmp".into() },
                collection: COLLECTION.into(),
                items: rows,
            },
            true,
        );
        assert!(rendered.contains("unknown Person"), "{rendered}");
    }

    // ── the envelope ─────────────────────────────────────────────────

    #[test]
    fn the_envelope_carries_every_field_the_contract_fixes() {
        let envelope = Envelope {
            schema: ENVELOPE_SCHEMA,
            command: "list.members",
            ok: true,
            exit: 0,
            data: Some(serde_json::json!({"members": []})),
            error: None,
            notes: vec![Note {
                level: CAVEAT,
                code: "role.advisory",
                message: ROLE_CAVEAT_LONG.to_string(),
            }],
        };
        let value: serde_json::Value = serde_json::from_str(&serde_json::to_string(&envelope).unwrap()).unwrap();

        for key in ["schema", "command", "ok", "exit", "data", "error", "notes"] {
            assert!(value.get(key).is_some(), "the envelope must always carry {key}");
        }
        assert_eq!(value["schema"], 1);
        assert_eq!(value["command"], "list.members");
        assert_eq!(value["ok"], true);
        assert!(value["error"].is_null());
        assert_eq!(value["notes"][0]["level"], "caveat");
        assert_eq!(value["notes"][0]["code"], "role.advisory");
    }

    #[test]
    fn the_honesty_strings_are_the_ones_the_spec_fixed() {
        assert!(ROLE_CAVEAT_LONG.starts_with("Roles are agreements, not enforcement."));
        assert!(ROLE_CAVEAT_LONG.contains("admission"));
        assert!(ROLE_CAVEAT_LONG.contains("last 5 versions of every Item for 30 days"));
        assert_eq!(
            ROLE_CAVEAT_SHORT,
            "Roles are agreements, not enforcement — admission is the only gate."
        );
        assert!(ATTRIBUTION_CAVEAT.contains("never proven"));
        assert!(PRESENCE_STALE.contains("unknown rather than not connected"));

        assert!(CLAIM_CAVEAT.contains("not because kith can verify"));

        for text in [ROLE_CAVEAT_LONG, ROLE_CAVEAT_SHORT, ATTRIBUTION_CAVEAT, CLAIM_CAVEAT, PRESENCE_STALE] {
            let lower = text.to_lowercase();
            for banned in ["online", "user", "account", "folder", "friend", "permission"] {
                assert!(!lower.contains(banned), "{banned} in {text}");
            }
            // "Server" is legal in exactly one shape: saying there is none.
            assert_eq!(
                lower.matches("server").count(),
                lower.matches("no server").count(),
                "a server may only ever be denied: {text}"
            );
        }
    }

    #[test]
    fn an_engine_that_is_not_there_is_a_failure_a_person_can_act_on() {
        let f = engine_failure(&SyncError::Unreachable, Some("http://127.0.0.1:8384"), None);
        assert_eq!(f.exit, EX_UNAVAILABLE);
        assert_eq!(f.code, "engine.unreachable");
        assert!(f.message.contains("http://127.0.0.1:8384"));
        assert!(f.fix.unwrap().contains("kith doctor"));

        let rejected = engine_failure(
            &SyncError::Unauthorized,
            Some("http://127.0.0.1:8384"),
            Some(Path::new("/home/ana/.local/state/engine/config.xml")),
        );
        assert_eq!(rejected.exit, EX_CONFIG, "a rejected key is configuration, not absence");
        assert!(rejected.detail.unwrap().contains("config.xml"), "say where the key came from");
        assert!(
            !rejected.fix.unwrap().to_lowercase().contains("regenerat"),
            "kith never rotates a key it did not issue"
        );
    }

    // ── rendering ────────────────────────────────────────────────────

    #[test]
    fn piped_output_drops_the_header_and_separates_cells_with_one_tab() {
        let rows = vec![vec!["walls".to_string(), "kith-a".to_string()]];
        let piped = table(&["NAME", "ID"], &rows, &[10, 10], false);
        assert_eq!(piped, "walls\tkith-a\n");

        let terminal = table(&["NAME", "ID"], &rows, &[10, 10], true);
        assert!(terminal.starts_with("NAME"), "{terminal}");
        assert!(terminal.contains("walls  kith-a"), "{terminal}");
    }

    #[test]
    fn a_terminal_table_truncates_with_an_ellipsis_and_a_pipe_does_not() {
        let long = "a-very-long-wallpaper-title-that-will-not-fit".to_string();
        let rows = vec![vec![long.clone()]];
        assert!(table(&["TITLE"], &rows, &[10], true).contains('…'));
        assert!(table(&["TITLE"], &rows, &[10], false).contains(&long));
    }

    #[test]
    fn a_count_kith_cannot_verify_prints_as_a_dash() {
        let ana = PersonId::generate();
        let id = ItemId::generate();
        let mut without = item(&id, &ana, "2026-08-07T09:00:00Z", "sunset", false);
        without.bytes = None;
        let l = local(None, Vec::new(), vec![without]);

        let rendered = render_items(
            &ItemsData {
                circle: CircleInfo { id: "kith-a".into(), name: "walls".into(), root: "/tmp".into() },
                collection: COLLECTION.into(),
                items: item_rows(&l, &BTreeSet::new(), false),
            },
            true,
        );
        assert!(rendered.contains('—'), "{rendered}");
    }

    #[test]
    fn status_prints_local_facts_and_says_the_engine_is_not_answering() {
        let data = StatusData {
            engine: EngineInfo {
                reachable: false,
                version: None,
                address: Some("http://127.0.0.1:8384".into()),
                credentials: None,
            },
            person: Some(PersonInfo {
                name: "Ana".into(),
                id: "p-01k1yfq2m7vj3w8t0pz4rxab6c".into(),
                device: None,
                device_full: None,
            }),
            circles: vec![CircleStatusRow {
                id: "kith-4npq7x2b".into(),
                name: "walls".into(),
                root: "/home/ana/kith/walls".into(),
                state: None,
                percent: None,
                bytes_needed: None,
                items: 42,
                conflicts: Some(2),
                members: 2,
                connected: 0,
                peers: Vec::new(),
                steward_device: Some("WXYZ123".into()),
                steward_person: None,
                steward_is_you: false,
                last_change: Some("2026-08-07T09:00:00Z".into()),
            }],
        };

        let rendered = render_status(&data, true);
        assert!(rendered.contains("not answering"), "{rendered}");
        assert!(rendered.contains("last known"), "{rendered}");
        assert!(rendered.contains("items     42"), "{rendered}");
        assert!(rendered.contains("presence unknown"), "{rendered}");
        assert!(
            rendered.contains("no Membership claim yet, so kith cannot name the Person"),
            "{rendered}"
        );
        assert!(rendered.contains("copies"), "conflicting copies are named: {rendered}");
        assert!(!rendered.to_lowercase().contains("online"));
    }

    #[test]
    fn status_names_this_device_as_the_steward_without_naming_the_transport() {
        let data = StatusData {
            engine: EngineInfo {
                reachable: true,
                version: Some("2.0.4".into()),
                address: Some("http://127.0.0.1:8384".into()),
                credentials: None,
            },
            person: Some(PersonInfo {
                name: "Ana".into(),
                id: "p-01k1yf".into(),
                device: Some("P56IOI7".into()),
                device_full: Some(ANA_DEVICE.into()),
            }),
            circles: vec![CircleStatusRow {
                id: "kith-4npq7x2b".into(),
                name: "walls".into(),
                root: "/home/ana/kith/walls".into(),
                state: Some("syncing".into()),
                percent: None,
                bytes_needed: Some(123_456_789),
                items: 42,
                conflicts: None,
                members: 2,
                connected: 1,
                peers: vec![PeerRow {
                    person: Some("Ben".into()),
                    device: "K5J2FVL".into(),
                    presence: CONNECTED,
                    percent: Some(91.0),
                }],
                steward_device: Some("P56IOI7".into()),
                steward_person: Some("Ana".into()),
                steward_is_you: true,
                last_change: Some("2026-08-07T09:00:00Z".into()),
            }],
        };

        let rendered = render_status(&data, true);
        assert!(rendered.contains("You           Ana (P56IOI7)"), "{rendered}");
        assert!(rendered.contains("syncing · 123 MB to receive"), "{rendered}");
        assert!(rendered.contains("2 Members, 1 connected — Ben 91%"), "{rendered}");
        assert!(rendered.contains("you — every join is approved on this Device"), "{rendered}");
        // Assembled rather than written down: the transport's name is spelled
        // inside one module.
        let transport = format!("{}{}", "sync", "thing");
        assert!(
            !rendered.to_lowercase().contains(&transport),
            "the transport is never named above the seam: {rendered}"
        );
    }

    // ── small honest helpers ─────────────────────────────────────────

    #[test]
    fn bytes_read_the_way_the_spec_writes_them() {
        assert_eq!(bytes_human(1_993_421), "2.0 MB");
        assert_eq!(bytes_human(123_456_789), "123 MB");
        assert_eq!(bytes_human(512), "512 B");
        assert_eq!(bytes_human(7_855), "7.9 kB");
    }

    #[test]
    fn relative_time_is_coarse_and_never_pretends_to_be_precise() {
        let now = jiff::Timestamp::now();
        let ago = |seconds: i64| {
            (now - jiff::SignedDuration::from_secs(seconds)).to_string()
        };
        assert_eq!(relative(&ago(10)), "just now");
        assert_eq!(relative(&ago(60)), "1 minute ago");
        assert_eq!(relative(&ago(7200)), "2 hours ago");
        assert_eq!(relative(&ago(100_000)), "yesterday");
        assert_eq!(relative("not a timestamp"), "not a timestamp");
    }

    #[test]
    fn timestamps_leave_as_rfc_3339_and_an_unparseable_one_is_passed_through() {
        assert_eq!(normalise_time("2026-08-07T09:00:00+02:00"), "2026-08-07T07:00:00Z");
        assert_eq!(normalise_time("last tuesday"), "last tuesday");
    }

    #[test]
    fn favourites_are_the_last_word_per_item_and_belong_to_one_circle() {
        let text = concat!(
            r#"{"v":1,"k":"fav","seq":1,"at":"2026-08-07T10:41:00Z","circle":"kith-a","item":"A"}"#,
            "\n",
            r#"{"v":1,"k":"fav","seq":2,"at":"2026-08-07T10:42:00Z","circle":"kith-a","item":"B"}"#,
            "\n",
            r#"{"v":1,"k":"unfav","seq":3,"at":"2026-08-07T10:52:00Z","circle":"kith-a","item":"A"}"#,
            "\n",
            r#"{"v":1,"k":"fav","seq":4,"at":"2026-08-07T10:53:00Z","circle":"kith-b","item":"C"}"#,
            "\n",
            "{ not a record at all\n",
        );

        let set = fold_favourites(text, "kith-a");
        assert_eq!(set, BTreeSet::from(["B".to_string()]));
        assert_eq!(fold_favourites(text, "kith-b"), BTreeSet::from(["C".to_string()]));
    }

    #[test]
    fn the_engines_own_globs_are_what_gets_matched_never_a_spelling_of_our_own() {
        assert!(glob_match("*.engine-copy-*", "sunset.engine-copy-20260807-091402.png"));
        assert!(glob_match("*.tmp", "a.tmp"));
        assert!(!glob_match("*.tmp", "a.png"));
        assert!(glob_match(".stfolder", ".stfolder"));
        assert!(!glob_match(".stfolder", "stfolder"));
        assert!(glob_match("~engine~*.tmp", "~engine~scratch.tmp"));
        assert!(!glob_match("~engine~*.tmp", "~engine~scratch.png"));
    }

    #[test]
    fn a_short_device_id_is_seven_characters_and_never_a_person() {
        assert_eq!(short_device(ANA_DEVICE), "P56IOI7");
        assert_eq!(short_device("AB"), "AB");
    }

    #[test]
    fn circles_survive_a_round_trip_through_the_remembered_copy() {
        let known = KnownCircles {
            schema: 1,
            circle: vec![KnownCircle {
                id: "kith-4tj2q9xa".into(),
                name: "walls".into(),
                root: "/home/ana/kith/walls".into(),
            }],
        };
        let text = toml::to_string_pretty(&known).unwrap();
        let back: KnownCircles = toml::from_str(&text).unwrap();
        assert_eq!(back.circle.len(), 1);
        assert_eq!(back.circle[0].name, "walls");
        assert_eq!(back.circle[0].root, "/home/ana/kith/walls");
    }

    // ── reading a real tree ──────────────────────────────────────────

    #[test]
    fn a_circle_on_disk_reads_as_items_members_and_a_steward() {
        let root = std::env::temp_dir().join(format!(
            "kith-report-{}-{}",
            std::process::id(),
            ulid::Ulid::generate()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let ana = crate::identity::Identity {
            schema: 1,
            person: PersonId::generate(),
            display_name: "Ana".into(),
            created: "2026-08-07T09:00:00Z".into(),
        };
        descriptors::write_circle(&root, &descriptor(&ana.person, ANA_DEVICE)).unwrap();
        descriptors::write_collection(
            &root,
            &descriptors::CollectionDescriptor {
                schema: 1,
                collection: COLLECTION.into(),
                provider: WALLPAPER.into(),
            },
        )
        .unwrap();
        claims::publish(&root, ANA_DEVICE, &ana, "2026-08-07T09:02:11Z").unwrap();

        std::fs::write(root.join("sunset.png"), b"not really a png").unwrap();
        // A copy the engine left behind, and the engine's own archive.
        std::fs::write(root.join("sunset.engine-copy-20260807-091402.png"), b"copy").unwrap();
        std::fs::create_dir_all(root.join("archive")).unwrap();
        std::fs::write(root.join("archive/sunset.engine-copy-20260101.png"), b"archived").unwrap();

        let id = ItemId::generate();
        records::append(
            &root,
            COLLECTION,
            ANA_DEVICE,
            &records::Record::Add {
                item: id.clone(),
                by: ana.person.clone(),
                at: "2026-08-07T09:14:02Z".into(),
                title: "sunset".into(),
                path: "sunset.png".into(),
                hash: "b3:aa".into(),
                size: 16,
            },
        )
        .unwrap();

        let l = Local::read(&root, Some(RESERVED_FIXTURE));
        assert!(l.trouble.is_empty(), "{:?}", l.trouble);
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].title, "sunset");
        assert_eq!(l.people.len(), 1);
        assert_eq!(l.role_of(&ana.person), Role::Admin);
        assert_eq!(l.steward_person().as_deref(), Some("Ana"));
        assert_eq!(l.last_change.as_deref(), Some("2026-08-07T09:14:02Z"));
        assert_eq!(l.conflicts, Some(1), "copies are counted, never hidden");

        let rows = item_rows(&l, &BTreeSet::new(), false);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].present, "the bytes are here");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
