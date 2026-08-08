//! The four social verbs — `wallsync invite`, `wallsync join`, `wallsync approve`,
//! `wallsync reject`.
//!
//! Admission is the one real gate, and it runs on the Steward's own Device. An
//! Invite is a pointer, not a credential: expiry bounds when knocks are
//! *expected*, never when they are possible, so v0.1 has no revoke. A Role is
//! derived from `.wallsync/circle.toml`, so approving writes nothing into the synced
//! tree. Rejection is a local ignore-list that never leaves this Device.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::engine::syncthing::{Credentials, SyncthingEngine};
use crate::engine::{CircleId, CircleOffer, DeviceId, InviteTicket, JoinRequest, SyncEngine, SyncError};
use crate::identity::{self, Identity};
use crate::invite::{self, InviteError};
use crate::store::claims;
use crate::store::descriptors::{self, CircleDescriptor};

// sysexits, so the whole binary speaks one dialect.
const EX_OK: i32 = 0;
const EX_FAIL: i32 = 1;
const EX_USAGE: i32 = 64;
const EX_UNAVAILABLE: i32 = 69;

/// v0.1 has no `--expires` flag, so 24h is the only Invite window it issues.
const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;

const INVITES: &str = "invites.json";
const KNOCKS: &str = "knocks.json";
const DISMISSED: &str = "dismissed.json";

// ── wallsync invite ──────────────────────────────────────────────────────

/// Mint or reprint this Circle's single open Invite window, and print its code.
///
/// The code is assembled from records this Device already holds, so it works
/// with the daemon down; the engine is contacted only to warn it is unreachable.
pub async fn invite(new: bool, address: Option<&str>) -> i32 {
    let Some(me) = me() else { return EX_FAIL };

    let engine = reach_engine().await;
    let circle = match pick(held_circles(engine.as_ref().ok()).await, "invite") {
        Ok(c) => c,
        Err(code) => return code,
    };
    let descriptor = match steward_check(&circle, &me, "invite") {
        Ok(d) => d,
        Err(code) => return code,
    };

    let now = jiff::Timestamp::now();
    let unix = now.as_second();
    let state = state_dir();
    let mut windows: Vec<Window> = state
        .as_deref()
        .map(|d| read_state(&d.join(INVITES)))
        .unwrap_or_default();

    let open = windows
        .iter()
        .find(|w| w.circle == descriptor.id && w.effective(unix) == WindowState::Open)
        .cloned();

    let (window, reprinted) = match open {
        Some(w) if !new => (w, true),
        superseded => {
            // At most one window per Circle: the new one replaces the row it
            // supersedes.
            let minted = Window {
                circle: descriptor.id.clone(),
                nonce: ulid::Ulid::generate().to_string(),
                issued_at: now.to_string(),
                expires_at: unix + DEFAULT_TTL_SECS,
                state: WindowState::Open,
                spent_by: None,
            };
            windows.retain(|w| w.circle != descriptor.id);
            windows.push(minted.clone());
            if let Some(dir) = &state {
                save(&dir.join(INVITES), &windows);
            }
            if superseded.is_some() {
                // stderr, so a piped `wallsync invite` carries only the code.
                eprintln!("The previous window for {} is closed.", circle.name);
            }
            (minted, false)
        }
    };

    let ticket = InviteTicket {
        circle: CircleId(descriptor.id.clone()),
        steward_device: DeviceId(descriptor.founder_device.clone()),
        expires_at: window.expires_at.max(0) as u64,
        address: address.map(str::to_string),
    };
    print_invite(&circle.name, &invite::encode(&ticket), &window, unix, reprinted);

    if let Err(e) = &engine {
        eprintln!();
        eprintln!("Your Device cannot be reached right now — they will not get in until it is.");
        eprintln!("  {}", engine_reason(e));
    }
    EX_OK
}

/// The whole of `wallsync invite`'s output.
///
/// Piped, the code goes to stdout bare and the prose to stderr. On a terminal
/// the code also goes to the clipboard by OSC 52, which works over ssh.
fn print_invite(name: &str, code: &str, window: &Window, unix: i64, reprinted: bool) {
    let remaining = human_duration(window.expires_at - unix);
    let piped = !io::stdout().is_terminal();
    let say = |line: &str| {
        if piped {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };

    if reprinted {
        say(&format!(
            "An invite for {name} is already open — expires in {remaining}. Same code:"
        ));
    } else {
        match local_time(window.expires_at) {
            Some(when) => say(&format!(
                "Invite for {name} — expires in {remaining} ({when})"
            )),
            None => say(&format!("Invite for {name} — expires in {remaining}")),
        }
    }
    say("");

    if piped {
        println!("{code}");
    } else {
        println!("  {code}");
        osc52(code);
    }
    say("");

    if reprinted {
        say("'wallsync invite --new' starts a fresh 24h window. The old code keeps pointing at your");
        say("Device either way; only the window changes.");
        return;
    }

    say("Send it to one person over a channel you already trust. wallsync has no messaging and");
    say("wants none.");
    say("");
    say(&format!(
        "Anyone who sees this code can ask to join {name}. Nobody gets in until you approve"
    ));
    say("them, and you will see who is asking. There is no way to un-send it — it stops");
    say(&format!("being expected in {remaining}."));
    say("");
    say("When they run 'wallsync join', their wallsync prints a fingerprint. Ask them to read it to");
    say("you, and check it before you approve.");
}

// ── wallsync join ────────────────────────────────────────────────────────

/// Consume an Invite code: knock at the Steward's Device and wait to be admitted.
///
/// Everything up to the knock is local. This build stops at the knock:
/// completing a join needs the change feed, which arrives only while wallsync runs.
pub async fn join(code: &str) -> i32 {
    let Some(me) = me() else { return EX_FAIL };

    let ticket = match invite::decode(code) {
        Ok(t) => t,
        Err(InviteError::Expired) => {
            // The error does not carry the expiry, so decode again with no clock.
            let stale = invite::decode_at(code, 0)
                .ok()
                .map(|t| human_duration(jiff::Timestamp::now().as_second() - t.expires_at as i64));
            match stale {
                Some(ago) => eprintln!("This invite expired {ago} ago."),
                None => eprintln!("This invite has expired."),
            }
            eprintln!("Ask for a new one — 'wallsync invite' takes them a second.");
            return EX_FAIL;
        }
        Err(InviteError::WrongVersion) => {
            eprintln!("This invite code was made by a newer wallsync than this one.");
            eprintln!("Update wallsync, or ask them for a code from a build that matches.");
            return EX_USAGE;
        }
        // A truncating chat client is the likely cause: re-paste, do not re-issue.
        Err(InviteError::Checksum) => {
            eprintln!("That invite code arrived damaged — check nothing was cut off, and paste");
            eprintln!("it again. Nothing has been contacted.");
            return EX_USAGE;
        }
        Err(InviteError::Malformed) => {
            eprintln!("That is not a wallsync invite code. One starts with WALLSYNC1- and is a single");
            eprintln!("line of letters and digits.");
            return EX_USAGE;
        }
    };

    // Nothing above has touched the Sync Engine; these two refusals keep it so.
    let held = local_circles();
    if let Some(already) = held.iter().find(|c| c.id.as_deref() == Some(ticket.circle.0.as_str())) {
        eprintln!("You are already in {} — this invite is for a Circle you hold.", already.name);
        eprintln!("  On disk  {}", already.root.display());
        return EX_FAIL;
    }

    let state = state_dir();
    let mut knocks: Vec<Knock> = state
        .as_deref()
        .map(|d| read_state(&d.join(KNOCKS)))
        .unwrap_or_default();
    let repeat = knocks
        .iter()
        .find(|k| k.circle == ticket.circle.0 && k.state == KnockState::Knocked)
        .cloned();

    let root = join_root(&ticket.circle);
    if repeat.is_none() {
        println!("Join a Circle?");
        println!("  Circle    {} — wallsync learns its name when its records arrive", ticket.circle.0);
        println!(
            "  Steward   Device {} — wallsync cannot name the Person yet",
            fingerprint(&ticket.steward_device.0)
        );
        println!(
            "  Expires   in {}",
            human_duration(ticket.expires_at as i64 - jiff::Timestamp::now().as_second())
        );
        println!("  As        {} ({})", me.display_name, me.person.short());
        match &root {
            Some(r) => println!("  On disk   {}", r.display()),
            None => println!("  On disk   wallsync has no data directory on this Device"),
        }
        println!();
        // No terminal means nobody to ask; refusing would only break scripts.
        if io::stdin().is_terminal() && !confirm("Join it?") {
            println!("Nothing was registered. The code stays good until it expires.");
            return EX_OK;
        }
    }

    let engine = match reach_engine().await {
        Ok(e) => e,
        Err(e) => return refuse_engine(&e),
    };
    let mine = match engine.local_device().await {
        Ok(d) => d,
        Err(e) => return refuse_engine(&e),
    };
    if mine.0 == ticket.steward_device.0 {
        eprintln!("That invite points at this Device — it is your own Circle's code.");
        eprintln!("Send it to the Person you want to invite instead.");
        return EX_FAIL;
    }

    // Written before the knock, so a crash leaves something resumable.
    if repeat.is_none() {
        knocks.push(Knock {
            circle: ticket.circle.0.clone(),
            steward_device: ticket.steward_device.0.clone(),
            root: root.clone(),
            state: KnockState::Knocked,
            first_knock_at: jiff::Timestamp::now().to_string(),
        });
        if let Some(dir) = &state {
            save(&dir.join(KNOCKS), &knocks);
        }
    }

    if let Err(e) = engine.begin_join(&ticket).await {
        return refuse_engine(&e);
    }

    if let Some(offer) = offered_back(&engine, &ticket).await
        && let Some(root) = &root
    {
        return place_circle(&engine, &offer, root, &me).await;
    }

    match &repeat {
        Some(k) => {
            let waited = since(&k.first_knock_at)
                .map(human_duration)
                .unwrap_or_else(|| "a while".to_string());
            println!("You asked to join this Circle {waited} ago, and are still waiting.");
            println!("wallsync has re-registered the request; nothing else changed.");
        }
        None => println!("Asked to join."),
    }
    println!();
    println!("  Your fingerprint   {}", fingerprint(&mine.0));
    println!("  Read it to them. They will see the same four-and-four before they approve.");
    println!();
    println!("They have to approve you on their own Device. wallsync cannot tell you whether they");
    println!("have looked — there is no server that would — and nothing arrives until they do.");
    println!();
    println!("Your request stays registered with the Sync Engine across restarts. Run");
    println!("'wallsync join <code>' again, or open wallsync, to pick it up.");
    EX_OK
}

async fn offered_back(engine: &SyncthingEngine, ticket: &InviteTicket) -> Option<CircleOffer> {
    engine
        .pending_circles()
        .await
        .ok()?
        .into_iter()
        .find(|o| o.circle.0 == ticket.circle.0)
}

async fn place_circle(
    engine: &SyncthingEngine,
    offer: &CircleOffer,
    root: &Path,
    me: &Identity,
) -> i32 {
    let circle = match engine.complete_join(offer, root).await {
        Ok(c) => c,
        Err(e) => return refuse_engine(&e),
    };

    let device = match engine.local_device().await {
        Ok(d) => d.0,
        Err(e) => return refuse_engine(&e),
    };
    let now = jiff::Timestamp::now().to_string();
    if let Err(e) = claims::publish(&circle.root, &device, me, &now) {
        eprintln!("Joined, but this Device could not publish its Membership claim: {e}");
        eprintln!("Other Members will see an unclaimed Device until it can.");
        return EX_FAIL;
    }

    mark_knock_joined(&offer.circle);

    let name = if circle.name.is_empty() { &offer.label } else { &circle.name };
    println!("Joined {} at {}.", display_name(name, &offer.circle), circle.root.display());
    println!();
    println!("Content arrives as it syncs. Nothing touches your screen until you press Apply.");
    println!("Open wallsync to watch it land.");
    EX_OK
}

fn display_name<'a>(name: &'a str, circle: &'a CircleId) -> &'a str {
    if name.is_empty() { &circle.0 } else { name }
}

fn mark_knock_joined(circle: &CircleId) {
    let Some(dir) = state_dir() else { return };
    let path = dir.join(KNOCKS);
    let mut knocks: Vec<Knock> = read_state(&path);
    for knock in knocks.iter_mut().filter(|k| k.circle == circle.0) {
        knock.state = KnockState::Joined;
    }
    save(&path, &knocks);
}

fn join_root(circle: &CircleId) -> Option<PathBuf> {
    circles_dir().map(|d| d.join(circle.0.to_lowercase()))
}

// ── wallsync approve ─────────────────────────────────────────────────────

/// Admit a knocking Device into the Circle this Person stewards.
///
/// All that is known about a knock is a Device Identity, a name that Device
/// announced about itself, and when it was first seen. There is no Person in it.
pub async fn approve(device: Option<&str>) -> i32 {
    let Some(me) = me() else { return EX_FAIL };
    let engine = match reach_engine().await {
        Ok(e) => e,
        Err(e) => return refuse_engine(&e),
    };

    let circle = match stewarded(&engine, &me, "approve").await {
        Ok(c) => c,
        Err(code) => return code,
    };
    let descriptor = match steward_check(&circle, &me, "approve") {
        Ok(d) => d,
        Err(code) => return code,
    };

    let pending = match engine.pending_joins().await {
        Ok(p) => p,
        Err(e) => return refuse_engine(&e),
    };
    let state = state_dir();
    let hidden: Vec<Dismissal> = state
        .as_deref()
        .map(|d| read_state(&d.join(DISMISSED)))
        .unwrap_or_default();
    let visible: Vec<JoinRequest> = pending
        .into_iter()
        .filter(|r| !hidden.iter().any(|h| h.circle == descriptor.id && h.device == r.device.0))
        .collect();
    let hidden_here = hidden.iter().filter(|h| h.circle == descriptor.id).count();

    let request = match choose(&visible, device, hidden_here, &circle.name) {
        Ok(r) => r.clone(),
        Err(code) => return code,
    };

    let mut windows: Vec<Window> = state
        .as_deref()
        .map(|d| read_state(&d.join(INVITES)))
        .unwrap_or_default();
    let unix = jiff::Timestamp::now().as_second();
    let solicited = solicited(&windows, &descriptor.id, unix);

    println!("A Device wants to join {}.", circle.name);
    println!();
    println!(
        "  Device name    {}     (announced by that Device — it can say anything)",
        request.name
    );
    println!("  Fingerprint    {}", fingerprint(&request.device.0));
    match since(&request.seen_at) {
        Some(secs) => println!("  First seen     {} ago", human_duration(secs)),
        None => println!("  First seen     {}", request.seen_at),
    }
    println!("  Invite         {}", solicited.line(unix, &circle.name));
    println!();
    println!("wallsync cannot tell you who this is. It sees a Device, not a Person. Ask your friend to");
    println!("read you the fingerprint their wallsync printed, and approve only if it matches.");
    println!();
    println!("Eight characters is enough to tell two Devices apart and to catch a mistake reading");
    println!("one back; it is not enough to resist someone deliberately grinding a matching");
    println!("prefix. The whole Identity, if it matters:");
    println!("  {}", request.device.0);
    println!();

    if !io::stdin().is_terminal() {
        eprintln!("Approving is deliberate, and v0.1 has no flag that skips the question.");
        eprintln!("Run 'wallsync approve' from a terminal.");
        return EX_USAGE;
    }
    let agreed = match &solicited {
        Solicited::ByOpenInvite { .. } => confirm("Approve?"),
        // Friction rather than a `--force` flag: the fingerprint is typed out.
        _ => {
            println!("This knock was not expected. Type its fingerprint to approve it anyway.");
            typed(&fingerprint(&request.device.0))
        }
    };
    if !agreed {
        println!("Nothing was admitted. The Device keeps knocking; 'wallsync reject' hides it.");
        return EX_OK;
    }

    if let Err(e) = engine.admit(&CircleId(descriptor.id.clone()), &request).await {
        return refuse_engine(&e);
    }

    // Approval is where an Invite is consumed: this is the Device that admits.
    if let Some(w) = windows.iter_mut().find(|w| w.circle == descriptor.id) {
        w.state = WindowState::Spent;
        w.spent_by = Some(request.device.0.clone());
        if let Some(dir) = &state {
            save(&dir.join(INVITES), &windows);
        }
    }

    println!();
    println!(
        "Admitted {} to {}. The invite is used up; 'wallsync invite' issues another.",
        fingerprint(&request.device.0),
        circle.name
    );
    println!("Nothing was written into the Circle: a Role is derived from who founded it, so");
    println!("there is no shared record of who is who for an approval to change.");
    println!("They publish their own Membership claim when their wallsync places the Circle — that");
    println!("is when wallsync can name the Person behind that Device, and not before.");
    println!("Every other Member learns their Device the next time their Device and yours are");
    println!("connected to each other.");
    EX_OK
}

// ── wallsync reject ──────────────────────────────────────────────────────

/// Hide a knocking Device. Local, and told to nobody.
///
/// Deliberately not `expel` (that removes an *admitted* Device) and deliberately
/// not a dismissal at the engine, which the Device would undo by dialling again.
pub async fn reject(device: Option<&str>) -> i32 {
    let Some(me) = me() else { return EX_FAIL };

    let engine = reach_engine().await;
    let circle = match &engine {
        Ok(e) => match stewarded(e, &me, "reject").await {
            Ok(c) => c,
            Err(code) => return code,
        },
        // Hiding a knock is local state, so it does not need the daemon.
        Err(_) => match stewarded_among(local_circles(), &me, "reject") {
            Ok(c) => c,
            Err(code) => return code,
        },
    };
    let descriptor = match steward_check(&circle, &me, "reject") {
        Ok(d) => d,
        Err(code) => return code,
    };

    let state = state_dir();
    let mut hidden: Vec<Dismissal> = state
        .as_deref()
        .map(|d| read_state(&d.join(DISMISSED)))
        .unwrap_or_default();

    // Keyed by the full Device Identity, which the pending list normally expands
    // a fingerprint into — but local state does not need the daemon.
    let target = match &engine {
        Ok(e) => match e.pending_joins().await {
            Ok(pending) => {
                let visible: Vec<JoinRequest> = pending
                    .into_iter()
                    .filter(|r| {
                        !hidden.iter().any(|h| h.circle == descriptor.id && h.device == r.device.0)
                    })
                    .collect();
                let hidden_here =
                    hidden.iter().filter(|h| h.circle == descriptor.id).count();
                match choose(&visible, device, hidden_here, &circle.name) {
                    Ok(r) => r.device.0.clone(),
                    Err(code) => return code,
                }
            }
            Err(e) => return refuse_engine(&e),
        },
        Err(e) => match device.filter(|d| looks_like_a_device_identity(d)) {
            Some(d) => d.to_string(),
            None => {
                eprintln!("wallsync cannot list who is knocking without the Sync Engine.");
                eprintln!("  {}", engine_reason(e));
                eprintln!("Hiding one still works if you name its whole Device Identity.");
                return EX_UNAVAILABLE;
            }
        },
    };

    if hidden.iter().any(|h| h.circle == descriptor.id && h.device == target) {
        println!("{} is already hidden.", fingerprint(&target));
        return EX_OK;
    }

    hidden.push(Dismissal {
        circle: descriptor.id.clone(),
        device: target.clone(),
        rejected_at: jiff::Timestamp::now().to_string(),
    });
    let where_ = state.as_ref().map(|d| d.join(DISMISSED));
    if let Some(path) = &where_ {
        save(path, &hidden);
    }

    println!(
        "Hidden. {} keeps trying to reach your Device — wallsync stops showing it.",
        fingerprint(&target)
    );
    println!("It is not told anything: there is no server to deliver a \"no\". If someone is");
    println!("waiting on you, tell them yourself.");
    match &where_ {
        // No `--forget` in v0.1, so the un-hide is the file itself.
        Some(path) => println!("Recorded in {} — remove its line to un-hide it.", path.display()),
        None => println!("wallsync has no state directory on this Device, so this will not survive a restart."),
    }
    EX_OK
}

// ── choosing between knocks ──────────────────────────────────────────

/// The knock a verb acts on: the named one, or the only one there is.
fn choose<'a>(
    visible: &'a [JoinRequest],
    query: Option<&str>,
    hidden_here: usize,
    circle: &str,
) -> Result<&'a JoinRequest, i32> {
    // A rejection must never become an invisible one, so hidden knocks are counted.
    let hidden_note = |code: i32| {
        if hidden_here > 0 {
            eprintln!(
                "{hidden_here} hidden knock{} — wallsync stops showing a Device you rejected.",
                if hidden_here == 1 { "" } else { "s" }
            );
        }
        code
    };

    match query {
        Some(q) => {
            let matched: Vec<&JoinRequest> =
                visible.iter().filter(|r| names(q, &r.device.0)).collect();
            match matched.len() {
                1 => Ok(matched[0]),
                0 => {
                    eprintln!("No Device knocking at {circle} matches {q:?}.");
                    list(visible);
                    Err(hidden_note(EX_FAIL))
                }
                _ => {
                    eprintln!("{q:?} matches more than one knocking Device. Name more of it:");
                    for r in matched {
                        eprintln!("  {}  {}", fingerprint(&r.device.0), r.device.0);
                    }
                    Err(EX_USAGE)
                }
            }
        }
        None => match visible.len() {
            1 => Ok(&visible[0]),
            0 => {
                println!("Nobody is knocking at {circle}.");
                Err(hidden_note(EX_OK))
            }
            _ => {
                eprintln!("{} Devices want to join {circle}. Name one by its fingerprint:", visible.len());
                list(visible);
                Err(EX_USAGE)
            }
        },
    }
}

fn list(visible: &[JoinRequest]) {
    for r in visible {
        eprintln!(
            "  {}  {}  (the name that Device announced about itself)",
            fingerprint(&r.device.0),
            r.name
        );
    }
}

/// Whether an argument names this Device: prefix match, grouping and case removed.
fn names(query: &str, device: &str) -> bool {
    let q = squash(query);
    !q.is_empty() && squash(device).starts_with(&q)
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Long enough to be a whole Device Identity rather than a fingerprint.
fn looks_like_a_device_identity(s: &str) -> bool {
    squash(s).len() >= 32
}

/// The first eight characters of a Device Identity, grouped 4-4: `UJZD-EGXD`.
///
/// The string two People read to each other out of band. 40 bits: enough to
/// catch a transcription error, not enough to resist a ground prefix.
pub fn fingerprint(device: &str) -> String {
    let squashed = squash(device);
    let head: String = squashed.chars().take(8).collect();
    match head.len() {
        0..=4 => head,
        _ => format!("{}-{}", &head[..4], &head[4..]),
    }
}

// ── the Circle a verb acts on ────────────────────────────────────────

/// A Circle whose bytes this Device holds.
#[derive(Clone, Debug)]
struct Held {
    /// `None` until the descriptor arrives — a real state, not a fault.
    id: Option<String>,
    /// Display only; the engine's label is never minted into the record.
    name: String,
    root: PathBuf,
    descriptor: Option<CircleDescriptor>,
}

/// Every Circle this Device holds. [`local_circles`] needs nothing running; the
/// engine's list is the only way to find one placed elsewhere with `--path`.
async fn held_circles<E: SyncEngine>(engine: Option<&E>) -> Vec<Held> {
    let mut found = local_circles();

    if let Some(engine) = engine
        && let Ok(refs) = engine.circles().await
    {
        for r in refs {
            let label = (!r.name.is_empty()).then(|| r.name.clone());
            push(
                read_held(&r.root, label.as_deref(), Some(r.id.0.as_str())),
                &mut found,
            );
        }
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// The Circles wallsync itself placed, read straight off this Device — no daemon.
fn local_circles() -> Vec<Held> {
    let mut found: Vec<Held> = Vec::new();
    let Some(dir) = circles_dir() else {
        return found;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return found;
    };

    let mut roots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    roots.sort();
    for root in roots {
        push(read_held(&root, None, None), &mut found);
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Add a Circle unless its root is already in the list.
fn push(held: Held, found: &mut Vec<Held>) {
    let key = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let root = key(&held.root);
    if !found.iter().any(|c| key(&c.root) == root) {
        found.push(held);
    }
}

fn read_held(root: &Path, label: Option<&str>, engine_id: Option<&str>) -> Held {
    let descriptor = match descriptors::read_circle(root) {
        Ok(d) => d,
        Err(e) => {
            // A descriptor that will not parse is no reason to hide the Circle.
            eprintln!("! {e}");
            None
        }
    };
    let name = descriptor
        .as_ref()
        .map(|d| d.name.clone())
        .or_else(|| label.map(str::to_string))
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| root.display().to_string());
    Held {
        id: descriptor
            .as_ref()
            .map(|d| d.id.clone())
            .or_else(|| engine_id.map(str::to_string)),
        name,
        root: root.to_path_buf(),
        descriptor,
    }
}

/// Choose the Circle a verb acts on.
///
/// v0.1's CLI has no `--circle`, and wallsync refuses to guess between several:
/// admitting a Device to the wrong Circle is real and irreversible.
fn pick(mut circles: Vec<Held>, verb: &str) -> Result<Held, i32> {
    match circles.len() {
        1 => Ok(circles.remove(0)),
        0 => {
            eprintln!("You are not in any Circle yet.");
            eprintln!("'wallsync create <name>' starts one, or 'wallsync join <code>' joins a friend's.");
            Err(EX_FAIL)
        }
        _ => {
            eprintln!("More than one Circle is on this Device, and 'wallsync {verb}' has no way to");
            eprintln!("choose between them in v0.1. wallsync will not guess which one you mean:");
            for c in &circles {
                eprintln!(
                    "  {}  {}  {}",
                    c.name,
                    c.id.as_deref().unwrap_or("(no record yet)"),
                    c.root.display()
                );
            }
            Err(EX_FAIL)
        }
    }
}

/// The Circle this Person stewards, for `approve` and `reject`.
async fn stewarded<E: SyncEngine>(engine: &E, me: &Identity, verb: &str) -> Result<Held, i32> {
    stewarded_among(held_circles(Some(engine)).await, me, verb)
}

fn stewarded_among(all: Vec<Held>, me: &Identity, verb: &str) -> Result<Held, i32> {
    let mine: Vec<Held> = all.iter().filter(|c| is_admin(c, me)).cloned().collect();
    if !mine.is_empty() || all.is_empty() {
        return pick(mine, verb);
    }

    // Circles are here, and every one of them is somebody else's to admit into.
    if all.len() == 1 {
        let only = all.into_iter().next().expect("length checked");
        return Err(steward_check(&only, me, verb).err().unwrap_or(EX_FAIL));
    }
    eprintln!("You are not the admin of any Circle on this Device, so there is nothing here");
    eprintln!("to {verb}. A knock only ever reaches the admin's own Device.");
    for c in &all {
        eprintln!("  {}  admin: {}", c.name, admin_of(c));
    }
    Err(EX_FAIL)
}

fn admin_of(c: &Held) -> String {
    match stewardship(c) {
        Stewardship::Held { person, .. } => name_person(c, &person),
        Stewardship::Vacant { was, .. } => format!("{} — left", name_person(c, &was)),
        Stewardship::Disputed { .. } => "this Circle disagrees about who its admin is".into(),
        Stewardship::Unknown => "not known yet — waiting for the Circle's records".into(),
    }
}

fn is_admin(c: &Held, me: &Identity) -> bool {
    matches!(stewardship(c), Stewardship::Held { ref person, .. } if person == me.person.as_str())
}

/// The Circle's descriptor, if this Person may act as its admin.
fn steward_check(c: &Held, me: &Identity, verb: &str) -> Result<CircleDescriptor, i32> {
    match stewardship(c) {
        Stewardship::Unknown => {
            eprintln!("{} · waiting for the Circle's records", c.name);
            eprintln!("wallsync has this Circle's bytes and not its `.wallsync/circle.toml`, so it cannot");
            eprintln!("say who its admin is. Content keeps syncing; membership waits.");
            Err(EX_FAIL)
        }
        Stewardship::Disputed { claimants } => {
            eprintln!("{} · this Circle disagrees about who its admin is", c.name);
            for who in &claimants {
                eprintln!("  {}", name_person(c, who));
            }
            eprintln!("Two Devices have each written the record that names the founder. wallsync will");
            eprintln!("not pick a winner; resolving it lands in v0.2.");
            Err(EX_FAIL)
        }
        Stewardship::Vacant { since, was } => {
            eprintln!(
                "{} · no admin — {} left on {}.",
                c.name,
                name_person(c, &was),
                short_date(&since)
            );
            eprintln!("v0.1 has no way to hand the admin role to someone else, so no new Members");
            eprintln!("until v0.2. Everyone already in keeps syncing with everyone else.");
            Err(EX_FAIL)
        }
        Stewardship::Held { person, .. } if person != me.person.as_str() => {
            let admin = claim_name(c, &person);
            match &admin {
                Some(named) => {
                    eprintln!(
                        "Only {}'s admin ({named}) can invite people or approve joins. Ask them to",
                        c.name
                    );
                    eprintln!("run 'wallsync invite'. wallsync refuses this on your Device — it cannot stop other");
                    eprintln!("software on your Device from doing something similar, and it will not");
                    eprintln!("pretend otherwise.");
                }
                // The short id, never a Device Identity: a Device is not a Person.
                None => {
                    eprintln!(
                        "Only {}'s admin (unknown Person {}) can invite people or approve joins.",
                        c.name,
                        short_person(&person)
                    );
                    eprintln!("wallsync refuses this on your Device — it cannot stop other software on your");
                    eprintln!("Device from doing something similar, and it will not pretend otherwise.");
                }
            }
            if verb != "invite" {
                eprintln!();
                eprintln!("A knock only ever reaches the admin's own Device. wallsync is not hiding");
                eprintln!("pending joins from you — they never arrive here.");
            }
            Err(EX_FAIL)
        }
        Stewardship::Held { .. } => Ok(c
            .descriptor
            .clone()
            .expect("a stewardship is Held because a descriptor named a founder")),
    }
}

/// Who a Circle's admin is, as its records say.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stewardship {
    /// `.wallsync/circle.toml` names a founder whose claims do not all say they left.
    Held { person: String, device: String },
    /// The founder stamped `left_at` into their own Device's Membership claim.
    Vacant { since: String, was: String },
    /// Copies of `.wallsync/circle.toml` disagree about `founder_person`.
    Disputed { claimants: Vec<String> },
    /// No readable `.wallsync/circle.toml` in the tree yet.
    Unknown,
}

/// Derive the Circle's stewardship from its records and nothing else.
///
/// Never from the engine's introducer flag: it is invisible from the Steward's
/// own Device, the one vantage point that matters most.
fn stewardship(c: &Held) -> Stewardship {
    let Some(d) = &c.descriptor else {
        return Stewardship::Unknown;
    };

    let mut claimants = vec![d.founder_person.clone()];
    for copy in descriptor_copies(&c.root) {
        if !claimants.contains(&copy.founder_person) {
            claimants.push(copy.founder_person);
        }
    }
    if claimants.len() > 1 {
        return Stewardship::Disputed { claimants };
    }

    // A Member has left when *every* claim carrying their Person says so. One
    // claim with `left_at` and one without means a Device stopped, not a Person.
    let claims = claims::read_all(&c.root).unwrap_or_default();
    let theirs: Vec<_> = claims
        .iter()
        .filter(|claim| claim.person.as_str() == d.founder_person)
        .collect();
    if !theirs.is_empty() && theirs.iter().all(|claim| claim.left_at.is_some()) {
        let since = theirs
            .iter()
            .filter_map(|claim| claim.left_at.clone())
            .max()
            .unwrap_or_default();
        return Stewardship::Vacant {
            since,
            was: d.founder_person.clone(),
        };
    }

    Stewardship::Held {
        person: d.founder_person.clone(),
        device: d.founder_device.clone(),
    }
}

/// Copies of `.wallsync/circle.toml` the transport left beside it.
///
/// Recognised by the format's own rule — the key is the segment before the first
/// dot — so the engine's spelling for a conflict copy stays below the seam.
fn descriptor_copies(root: &Path) -> Vec<CircleDescriptor> {
    let dir = descriptors::wallsync_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "circle.toml" || !name.ends_with(".toml") || name.split('.').next() != Some("circle")
        {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path())
            && let Ok(d) = toml::from_str::<CircleDescriptor>(&text)
        {
            out.push(d);
        }
    }
    out
}

/// The display name a Person's own claims give them, from the newest of them.
fn claim_name(c: &Held, person: &str) -> Option<String> {
    let claims = claims::read_all(&c.root).unwrap_or_default();
    claims
        .iter()
        .filter(|claim| claim.person.as_str() == person)
        .max_by_key(|claim| claim.asserted.parse::<jiff::Timestamp>().ok())
        .map(|claim| claim.display_name.clone())
}

/// A Person as a surface may print them: their name, or their id's short form.
/// Never the Device Identity in its place — a Device is not a Person.
fn name_person(c: &Held, person: &str) -> String {
    match claim_name(c, person) {
        Some(name) => name,
        None => format!("unknown Person ({})", short_person(person)),
    }
}

fn short_person(person: &str) -> String {
    person.chars().take(8).collect()
}

/// `$XDG_DATA_HOME/wallsync/circles` — where a Circle goes unless a path was named.
fn circles_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_dir().join("wallsync/circles"))
}

// ── the same three records, for the TUI ──────────────────────────────
//
// The Members screen decides what `wallsync approve` and `wallsync reject` decide, on
// the same three files. Quiet by construction: `eprintln!` corrupts the frame.

/// Whether this Device has an invite window open, as the Members screen renders it.
///
/// A window wallsync cannot read reads as `Unsolicited`, so the knock needs its
/// fingerprint typed out in full. Safe, and noisier.
pub fn open_window(circle: &str) -> crate::tui::members::Solicited {
    use crate::tui::members::{Solicited as Shown, WindowClose};

    let windows: Vec<Window> = state_dir()
        .map(|d| quiet_read(&d.join(INVITES)))
        .unwrap_or_default();
    let now = jiff::Timestamp::now().as_second();
    match solicited(&windows, circle, now) {
        Solicited::ByOpenInvite { issued_at, expires_at } => Shown::ByOpenInvite {
            issued_at,
            expires_at: jiff::Timestamp::from_second(expires_at)
                .map(|t| t.to_string())
                .unwrap_or_else(|_| expires_at.to_string()),
        },
        Solicited::ByClosedInvite(state) => Shown::ByClosedInvite {
            closed_at: String::new(),
            reason: match state {
                WindowState::Spent => WindowClose::Spent,
                _ => WindowClose::Expired,
            },
        },
        Solicited::Unsolicited => Shown::Unsolicited,
    }
}

/// The Devices this Person has already turned away in a Circle. An unreadable
/// record hides nobody, which shows a knock twice rather than dropping one.
pub fn dismissed(circle: &str) -> Vec<String> {
    let hidden: Vec<Dismissal> = state_dir()
        .map(|d| quiet_read(&d.join(DISMISSED)))
        .unwrap_or_default();
    hidden.into_iter().filter(|h| h.circle == circle).map(|h| h.device).collect()
}

/// Hide a knock from the TUI. Local, and told to nobody.
pub fn dismiss(circle: &str, device: &str, now: &str) {
    let Some(dir) = state_dir() else { return };
    let path = dir.join(DISMISSED);
    let mut hidden: Vec<Dismissal> = quiet_read(&path);
    if hidden.iter().any(|h| h.circle == circle && h.device == device) {
        return;
    }
    hidden.push(Dismissal {
        circle: circle.to_string(),
        device: device.to_string(),
        rejected_at: now.to_string(),
    });
    let _ = write_state(&path, &hidden);
}

/// Mark this Circle's window spent after an admission on the Members screen.
pub fn spend_window(circle: &str, device: &str) {
    let Some(dir) = state_dir() else { return };
    let path = dir.join(INVITES);
    let mut windows: Vec<Window> = quiet_read(&path);
    if let Some(w) = windows.iter_mut().find(|w| w.circle == circle) {
        w.state = WindowState::Spent;
        w.spent_by = Some(device.to_string());
        let _ = write_state(&path, &windows);
    }
}

fn quiet_read<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

// ── the local records (spec §2.3) ────────────────────────────────────

/// The invite window — the bound that matters, because it is checked on the
/// Steward's own hardware. Losing this file closes every window: safe, noisier.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Window {
    circle: String,
    /// The window's id. Not carried in the code this build prints.
    nonce: String,
    issued_at: String,
    expires_at: i64,
    state: WindowState,
    /// The Device the window was spent on, so "already used" can say by whom.
    spent_by: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WindowState {
    Open,
    Spent,
    Expired,
}

impl Window {
    /// The stored state with the clock applied: expiry needs no write.
    fn effective(&self, now: i64) -> WindowState {
        match self.state {
            WindowState::Open if self.expires_at <= now => WindowState::Expired,
            other => other,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Knock {
    circle: String,
    steward_device: String,
    /// Where the Circle lands when it is offered back; the joiner chooses it.
    root: Option<PathBuf>,
    state: KnockState,
    first_knock_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum KnockState {
    Knocked,
    Offered,
    Joined,
    Abandoned,
}

/// A Device this Person rejected. Local, and never told to anybody.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Dismissal {
    circle: String,
    device: String,
    rejected_at: String,
}

/// Why a knock was expected, or was not.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Solicited {
    ByOpenInvite { issued_at: String, expires_at: i64 },
    ByClosedInvite(WindowState),
    Unsolicited,
}

impl Solicited {
    fn line(&self, now: i64, circle: &str) -> String {
        match self {
            Solicited::ByOpenInvite {
                issued_at,
                expires_at,
            } => {
                let issued = since(issued_at)
                    .map(|s| format!("issued {} ago, ", human_duration(s)))
                    .unwrap_or_default();
                format!("open, {issued}expires in {}", human_duration(expires_at - now))
            }
            Solicited::ByClosedInvite(WindowState::Spent) => "already used".to_string(),
            Solicited::ByClosedInvite(_) => "expired".to_string(),
            Solicited::Unsolicited => {
                format!("none — you have not invited anyone to {circle}")
            }
        }
    }
}

fn solicited(windows: &[Window], circle: &str, now: i64) -> Solicited {
    match windows.iter().find(|w| w.circle == circle) {
        None => Solicited::Unsolicited,
        Some(w) => match w.effective(now) {
            WindowState::Open => Solicited::ByOpenInvite {
                issued_at: w.issued_at.clone(),
                expires_at: w.expires_at,
            },
            closed => Solicited::ByClosedInvite(closed),
        },
    }
}

/// `$XDG_STATE_HOME/wallsync` — machine-written state, not the Person's config.
fn state_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(
        base.state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("wallsync"),
    )
}

/// Read one of the three local records. Missing or unreadable is an empty list.
fn read_state<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            eprintln!("! {} is unreadable ({e}) — carrying on without it", path.display());
            Vec::new()
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            eprintln!("! {} could not be read ({e}) — carrying on without it", path.display());
            Vec::new()
        }
    }
}

/// Write one of the three local records whole, or not at all: write beside,
/// flush, rename. Nothing here replicates, so the temp name needs no agreement.
fn write_state<T: Serialize>(path: &Path, rows: &[T]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(rows)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

    let tmp = path.with_extension("json.tmp");
    let staged = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()
    });
    if let Err(e) = staged.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// [`write_state`], with a failure reported rather than propagated.
fn save<T: Serialize>(path: &Path, rows: &[T]) {
    if let Err(e) = write_state(path, rows) {
        eprintln!("! {} could not be written ({e})", path.display());
    }
}

// ── the Person, the engine, and the two of them failing ──────────────

fn me() -> Option<Identity> {
    match identity::load() {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            eprintln!("No Identity on this Device. Run 'wallsync init' and give wallsync a name to attach");
            eprintln!("to what you add.");
            None
        }
        Err(e) => {
            eprintln!("{e}");
            None
        }
    }
}

/// A Sync Engine handle that is known to answer. Credentials come from the
/// daemon's own configuration, overridden by `config.toml`; wallsync writes neither.
async fn reach_engine() -> Result<SyncthingEngine, SyncError> {
    let cfg = config::load();
    let creds = match SyncthingEngine::discover() {
        Ok(mut found) => {
            if let Some(address) = cfg.engine_address {
                found.base_url = address;
            }
            if let Some(key) = cfg.engine_api_key {
                found.api_key = key;
            }
            found
        }
        Err(e) => match (cfg.engine_address, cfg.engine_api_key) {
            (Some(base_url), Some(api_key)) => Credentials {
                base_url,
                api_key,
                source: config::path().unwrap_or_default(),
            },
            _ => return Err(e),
        },
    };

    let engine = SyncthingEngine::new(creds);
    engine.health().await?;
    Ok(engine)
}

fn engine_reason(e: &SyncError) -> String {
    match e {
        SyncError::Unreachable => {
            "The Sync Engine is not running, or wallsync cannot find its configuration.".to_string()
        }
        SyncError::Unauthorized => {
            "The Sync Engine rejected our credentials. wallsync never rewrites the daemon's config \
             — check its API key."
                .to_string()
        }
        SyncError::Incompatible(v) => {
            format!("The Sync Engine is version {v}, below the version wallsync needs.")
        }
        other => format!("The Sync Engine answered with a problem: {other}"),
    }
}

/// Every engine failure is exit 69: wallsync does not queue engine writes.
fn refuse_engine(e: &SyncError) -> i32 {
    eprintln!("{}", engine_reason(e));
    eprintln!("wallsync adapts a daemon you run; it never starts, embeds or configures one.");
    EX_UNAVAILABLE
}

// ── asking, and telling the time ─────────────────────────────────────

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
}

/// Confirmation by transcription, for decisions worth more than one letter.
fn typed(expected: &str) -> bool {
    print!("{expected} > ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    squash(&line) == squash(expected)
}

/// How long ago an RFC 3339 stamp was, in seconds.
fn since(stamp: &str) -> Option<i64> {
    let then: jiff::Timestamp = stamp.parse().ok()?;
    Some(jiff::Timestamp::now().as_second() - then.as_second())
}

fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => plural(s, "second"),
        60..=3599 => plural(s / 60, "minute"),
        // Up to two days reads in hours, so a fresh window says "24h".
        3600..=172_799 => match (s / 3600, (s % 3600) / 60) {
            (h, 0) => format!("{h}h"),
            (h, m) => format!("{h}h {m}m"),
        },
        _ => match (s / 86_400, (s % 86_400) / 3600) {
            (d, 0) => format!("{d}d"),
            (d, h) => format!("{d}d {h}h"),
        },
    }
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 {
        format!("{n} {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// A unix second in this Device's own timezone: `Fri 8 Aug, 14:13`.
fn local_time(unix: i64) -> Option<String> {
    let ts = jiff::Timestamp::from_second(unix).ok()?;
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    Some(zoned.strftime("%a %-d %b, %H:%M").to_string())
}

/// An RFC 3339 stamp as a bare date: `7 Aug`, else the stamp itself.
fn short_date(stamp: &str) -> String {
    stamp
        .parse::<jiff::Timestamp>()
        .ok()
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%-d %b")
                .to_string()
        })
        .unwrap_or_else(|| stamp.to_string())
}

/// Put the code on the system clipboard with an OSC 52 escape — no dependency,
/// it works over ssh, and a terminal without it ignores the sequence.
fn osc52(text: &str) {
    print!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = io::stdout().flush();
}

fn base64(bytes: &[u8]) -> String {
    const SET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(char::from(SET[(n >> 18) as usize & 0x3f]));
        out.push(char::from(SET[(n >> 12) as usize & 0x3f]));
        out.push(if chunk.len() > 1 {
            char::from(SET[(n >> 6) as usize & 0x3f])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(SET[n as usize & 0x3f])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MembershipClaim, PersonId};
    use crate::store::descriptors::write_circle;

    const ANA_DEVICE: &str = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2";
    const BEN_DEVICE: &str = "K5J2FVL-B3QTXAO-7SWNDUE-HMR4YZI-6CPGA2N-XQTLB5V-JW3EOHY-RD6MSAK";

    /// A scratch directory of our own, never the Person's home.
    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("wallsync-membership-tests")
            .join(format!("{label}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn descriptor(founder: &str) -> CircleDescriptor {
        CircleDescriptor {
            schema: 1,
            id: "wallsync-4tj2q9xa".into(),
            name: "walls".into(),
            created: "2026-08-07T09:02:11Z".into(),
            founder_person: founder.into(),
            founder_device: ANA_DEVICE.into(),
        }
    }

    fn claim(device: &str, person: &str, name: &str, left_at: Option<&str>) -> MembershipClaim {
        MembershipClaim {
            schema: 1,
            device: device.to_string(),
            person: person_id(person),
            display_name: name.to_string(),
            asserted: "2026-08-07T09:02:11Z".to_string(),
            left_at: left_at.map(str::to_string),
        }
    }

    fn identity(person: &str, display_name: &str) -> Identity {
        Identity {
            schema: 1,
            person: person_id(person),
            display_name: display_name.to_string(),
            created: "2026-08-07T09:00:00Z".to_string(),
        }
    }

    /// `PersonId`'s inner string is private, so one is built by deserialising.
    fn person_id(s: &str) -> PersonId {
        toml::from_str::<Wrapper>(&format!("person = {s:?}"))
            .expect("a PersonId is just a string on the wire")
            .person
    }

    #[derive(Deserialize)]
    struct Wrapper {
        person: PersonId,
    }

    fn put_claim(root: &Path, c: &MembershipClaim) {
        let dir = root.join(".wallsync/members");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.toml", c.device)),
            toml::to_string_pretty(c).unwrap(),
        )
        .unwrap();
    }

    fn held(root: &Path) -> Held {
        read_held(root, None, None)
    }

    // ── fingerprints ─────────────────────────────────────────────────

    #[test]
    fn a_fingerprint_is_eight_characters_grouped_four_and_four() {
        assert_eq!(fingerprint(ANA_DEVICE), "P56I-OI7M");
        assert_eq!(fingerprint(BEN_DEVICE), "K5J2-FVLB");
        // The grouping is cosmetic, so it survives being fed back in.
        assert_eq!(fingerprint(&fingerprint(ANA_DEVICE)), "P56I-OI7M");
    }

    #[test]
    fn a_short_or_empty_identity_never_panics_on_its_own_fingerprint() {
        assert_eq!(fingerprint(""), "");
        assert_eq!(fingerprint("AB"), "AB");
        assert_eq!(fingerprint("ABCDE"), "ABCD-E");
    }

    #[test]
    fn a_knock_is_named_however_the_fingerprint_was_read_back() {
        for query in ["P56I-OI7M", "p56i-oi7m", "P56IOI7M", " p56i oi7m ", ANA_DEVICE] {
            assert!(names(query, ANA_DEVICE), "{query:?} should name that Device");
        }
        assert!(!names("K5J2-FVLB", ANA_DEVICE));
        assert!(!names("", ANA_DEVICE), "an empty query names nothing");
    }

    #[test]
    fn only_a_whole_identity_can_be_hidden_without_the_pending_list() {
        assert!(looks_like_a_device_identity(ANA_DEVICE));
        assert!(!looks_like_a_device_identity("P56I-OI7M"));
    }

    // ── the invite window ────────────────────────────────────────────

    fn window(expires_at: i64, state: WindowState) -> Window {
        Window {
            circle: "wallsync-4tj2q9xa".into(),
            nonce: "01K1YFQ2M7VJ3W8T0PZ4RXAB6C".into(),
            issued_at: "2026-08-07T09:02:11Z".into(),
            expires_at,
            state,
            spent_by: None,
        }
    }

    #[test]
    fn a_window_closes_on_the_clock_and_not_on_a_write() {
        let w = window(1_000, WindowState::Open);
        assert_eq!(w.effective(999), WindowState::Open);
        assert_eq!(w.effective(1_000), WindowState::Expired);
        assert_eq!(
            window(9_999, WindowState::Spent).effective(0),
            WindowState::Spent,
            "a spent window does not re-open because time is left on it"
        );
    }

    #[test]
    fn a_knock_is_solicited_by_the_window_that_is_open_now() {
        let open = vec![window(1_000, WindowState::Open)];
        assert_eq!(
            solicited(&open, "wallsync-4tj2q9xa", 500),
            Solicited::ByOpenInvite {
                issued_at: "2026-08-07T09:02:11Z".into(),
                expires_at: 1_000
            }
        );
        assert_eq!(
            solicited(&open, "wallsync-4tj2q9xa", 2_000),
            Solicited::ByClosedInvite(WindowState::Expired)
        );
        assert_eq!(
            solicited(&[window(1_000, WindowState::Spent)], "wallsync-4tj2q9xa", 500),
            Solicited::ByClosedInvite(WindowState::Spent)
        );
        assert_eq!(solicited(&[], "wallsync-4tj2q9xa", 500), Solicited::Unsolicited);
        assert_eq!(
            solicited(&open, "wallsync-somewhere-else", 500),
            Solicited::Unsolicited
        );
    }

    #[test]
    fn the_invite_line_never_claims_more_than_the_window_knows() {
        let now = 500;
        let open = solicited(&[window(now + 3_600, WindowState::Open)], "wallsync-4tj2q9xa", now);
        assert!(open.line(now, "walls").starts_with("open, "));
        assert!(open.line(now, "walls").contains("expires in 1h"));
        assert_eq!(
            Solicited::ByClosedInvite(WindowState::Spent).line(now, "walls"),
            "already used"
        );
        assert_eq!(
            Solicited::Unsolicited.line(now, "walls"),
            "none — you have not invited anyone to walls"
        );
    }

    // ── the local records ────────────────────────────────────────────

    #[test]
    fn a_local_record_round_trips_and_a_missing_one_is_not_a_fault() {
        let dir = scratch("state-round-trip");
        let path = dir.join(INVITES);
        assert!(read_state::<Window>(&path).is_empty(), "nothing there yet");

        write_state(&path, &[window(1_000, WindowState::Open)]).unwrap();
        let back = read_state::<Window>(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].expires_at, 1_000);
        assert_eq!(back[0].state, WindowState::Open);

        // The staging file is never left behind.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![INVITES.to_string()], "{names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_window_file_closes_every_window_rather_than_failing() {
        let dir = scratch("state-damaged");
        let path = dir.join(INVITES);
        std::fs::write(&path, "{ not json").unwrap();

        let windows: Vec<Window> = read_state(&path);
        assert!(windows.is_empty());
        assert_eq!(
            solicited(&windows, "wallsync-4tj2q9xa", 0),
            Solicited::Unsolicited
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dismissal_hides_one_device_in_one_circle_and_nothing_else() {
        let hidden = [Dismissal {
            circle: "wallsync-4tj2q9xa".into(),
            device: ANA_DEVICE.into(),
            rejected_at: "2026-08-07T09:02:11Z".into(),
        }];
        let hides = |circle: &str, device: &str| {
            hidden.iter().any(|h| h.circle == circle && h.device == device)
        };
        assert!(hides("wallsync-4tj2q9xa", ANA_DEVICE));
        assert!(!hides("wallsync-4tj2q9xa", BEN_DEVICE), "one Device, not a class");
        assert!(!hides("wallsync-elsewhere", ANA_DEVICE), "one Circle, not all");
    }

    // ── stewardship ──────────────────────────────────────────────────

    #[test]
    fn the_admin_is_whoever_the_write_once_record_names() {
        let root = scratch("held");
        write_circle(&root, &descriptor("p-01k1yfq2m7vj3w8t0pz4rxab6c")).unwrap();
        assert_eq!(
            stewardship(&held(&root)),
            Stewardship::Held {
                person: "p-01k1yfq2m7vj3w8t0pz4rxab6c".into(),
                device: ANA_DEVICE.into()
            }
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_circle_with_no_record_has_no_steward_to_name() {
        let root = scratch("unknown");
        assert_eq!(stewardship(&held(&root)), Stewardship::Unknown);
        assert_eq!(held(&root).id, None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_founder_whose_only_claim_says_left_leaves_the_circle_without_an_admin() {
        let root = scratch("vacant");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&root, &descriptor(ana)).unwrap();
        put_claim(
            &root,
            &claim(ANA_DEVICE, ana, "Ana", Some("2026-09-01T18:20:00Z")),
        );

        assert_eq!(
            stewardship(&held(&root)),
            Stewardship::Vacant {
                since: "2026-09-01T18:20:00Z".into(),
                was: ana.into()
            }
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn one_device_of_the_founders_leaving_does_not_vacate_the_circle() {
        let root = scratch("still-held");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&root, &descriptor(ana)).unwrap();
        put_claim(&root, &claim(ANA_DEVICE, ana, "Ana", Some("2026-09-01T18:20:00Z")));
        put_claim(&root, &claim(BEN_DEVICE, ana, "Ana", None));

        assert!(matches!(stewardship(&held(&root)), Stewardship::Held { .. }));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn two_records_naming_two_founders_are_disputed_rather_than_resolved() {
        let root = scratch("disputed");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        let ben = "p-01k9r7wq3f8bx2m5nz4h7cvted";
        write_circle(&root, &descriptor(ana)).unwrap();
        let mut theirs = descriptor(ben);
        theirs.founder_device = BEN_DEVICE.into();
        std::fs::write(
            descriptors::wallsync_dir(&root).join("circle.sync-conflict-20260807-143122-K5J2FVL.toml"),
            toml::to_string_pretty(&theirs).unwrap(),
        )
        .unwrap();

        match stewardship(&held(&root)) {
            Stewardship::Disputed { claimants } => {
                assert_eq!(claimants.len(), 2);
                assert!(claimants.contains(&ana.to_string()));
                assert!(claimants.contains(&ben.to_string()));
            }
            other => panic!("expected a dispute, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_copy_that_agrees_about_the_founder_is_not_a_dispute() {
        let root = scratch("agreeing-copy");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&root, &descriptor(ana)).unwrap();
        std::fs::write(
            descriptors::wallsync_dir(&root).join("circle.sync-conflict-20260807-143122-K5J2FVL.toml"),
            toml::to_string_pretty(&descriptor(ana)).unwrap(),
        )
        .unwrap();

        assert!(matches!(stewardship(&held(&root)), Stewardship::Held { .. }));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ── who may act, and on which Circle ─────────────────────────────

    #[test]
    fn a_person_who_stewards_nothing_is_pointed_at_the_admin() {
        let root = scratch("not-mine");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&root, &descriptor(ana)).unwrap();
        put_claim(&root, &claim(ANA_DEVICE, ana, "Ana", None));
        let circle = held(&root);

        assert!(is_admin(&circle, &identity(ana, "Ana")));
        let ben = identity("p-01k9r7wq3f8bx2m5nz4h7cvted", "Ben");
        assert!(!is_admin(&circle, &ben));
        assert_eq!(admin_of(&circle), "Ana");

        match stewarded_among(vec![circle], &ben, "approve") {
            Err(code) => assert_eq!(code, EX_FAIL, "refused, not a usage error"),
            Ok(_) => panic!("Ben is not walls's admin and must not resolve it"),
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wallsync_refuses_to_guess_between_two_circles() {
        let (a, b) = (scratch("pick-a"), scratch("pick-b"));
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&a, &descriptor(ana)).unwrap();
        let mut other = descriptor(ana);
        other.id = "wallsync-9pq3zx71".into();
        other.name = "photos".into();
        write_circle(&b, &other).unwrap();

        assert_eq!(pick(vec![held(&a)], "invite").map(|c| c.name).ok(), Some("walls".into()));
        assert_eq!(pick(vec![held(&a), held(&b)], "invite").err(), Some(EX_FAIL));
        assert_eq!(pick(Vec::new(), "invite").err(), Some(EX_FAIL));
        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    // ── naming People, and refusing to name Devices ──────────────────

    #[test]
    fn an_admin_no_claim_names_is_a_person_id_and_never_a_device_identity() {
        let root = scratch("unnamed-admin");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&root, &descriptor(ana)).unwrap();

        let c = held(&root);
        let rendered = name_person(&c, ana);
        assert_eq!(rendered, "unknown Person (p-01k1yf)");
        assert!(!rendered.contains(ANA_DEVICE), "a Device is not a Person");

        put_claim(&root, &claim(ANA_DEVICE, ana, "Ana", None));
        assert_eq!(name_person(&held(&root), ana), "Ana");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ── telling the time ─────────────────────────────────────────────

    #[test]
    fn a_duration_reads_the_way_a_person_would_say_it() {
        assert_eq!(human_duration(1), "1 second");
        assert_eq!(human_duration(12), "12 seconds");
        assert_eq!(human_duration(60), "1 minute");
        assert_eq!(human_duration(4 * 60), "4 minutes");
        assert_eq!(human_duration(3_600), "1h");
        assert_eq!(human_duration(3_600 * 3 + 12 * 60), "3h 12m");
        assert_eq!(human_duration(DEFAULT_TTL_SECS), "24h");
        assert_eq!(human_duration(19 * 3_600 + 12 * 60), "19h 12m");
        assert_eq!(human_duration(2 * 86_400), "2d");
        assert_eq!(human_duration(7 * 86_400), "7d");
        assert_eq!(human_duration(-5), "0 seconds", "a lapsed bound is not negative");
    }

    #[test]
    fn a_stamp_wallsync_cannot_read_is_printed_rather_than_guessed_at() {
        assert_eq!(short_date("last tuesday"), "last tuesday");
        assert!(since("last tuesday").is_none());
        assert!(local_time(1_786_000_000).is_some());
    }

    // ── handing the code over ────────────────────────────────────────

    #[test]
    fn base64_matches_what_every_terminal_decodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"WALLSYNC1-AH6BTS5I"), "V0FMTFNZTkMxLUFINkJUUzVJ");
    }

    #[test]
    fn the_printed_code_carries_the_window_it_was_minted_under() {
        let w = window(1_786_000_000, WindowState::Open);
        let ticket = InviteTicket {
            address: None,
            circle: CircleId(w.circle.clone()),
            steward_device: DeviceId(ANA_DEVICE.into()),
            expires_at: w.expires_at as u64,
        };
        let code = invite::encode(&ticket);
        let back = invite::decode_at(&code, w.expires_at - 3_600).expect("a fresh code decodes");

        assert_eq!(back.circle.0, w.circle);
        assert_eq!(back.steward_device.0, ANA_DEVICE);
        assert_eq!(back.expires_at as i64, w.expires_at);
        assert_eq!(
            fingerprint(&back.steward_device.0),
            fingerprint(ANA_DEVICE),
            "both sides read the same four-and-four"
        );
    }
}
