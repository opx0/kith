//! The four social verbs — `kith invite`, `kith join`, `kith approve`,
//! `kith reject`.
//!
//! ROADMAP §3 steps 6, 7 and 8: everything that happens between "Ana has a
//! Circle" and "Ben's Gallery fills up". Four rules from the ADRs shape every
//! line of it, and none of them is re-argued here.
//!
//! * **Admission is the one real gate kith has.** It runs on the Steward's own
//!   Device, which is why it is the only thing in this module described as
//!   enforcement (ADR-0002 §4). Everything after admission is convention plus
//!   recovery, and the copy says so where the Person is standing.
//! * **An Invite is a pointer, not a credential.** The code carries a Circle id,
//!   a Device Identity and an expiry; whoever reads one can knock for as long as
//!   that Device exists. Expiry bounds when knocks are *expected*, never when
//!   they are possible — so v0.1 has no revoke and is honest about why.
//! * **A Role is derived, never stored.** The admin is `.kith/circle.toml`'s
//!   `founder_person` and nothing else. Approving a join therefore writes
//!   *nothing* into the synced tree: there is no shared record of who is who,
//!   and admitting someone changes the engine's Device set alone.
//! * **Rejection never leaves this Device.** It is a local ignore-list, not an
//!   engine call and not a message: there is no server to deliver a "no".
//!
//! The three local records this module owns (circles spec §2.3) live in
//! `$XDG_STATE_HOME/kith/` as JSON, because none of them is derivable from the
//! synced tree and ADR-0001's authority rule keeps non-derivable facts out of the
//! rebuildable cache. Each one degrades safely when it is lost, and the way it
//! degrades is documented beside it.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::engine::syncthing::{Credentials, SyncthingEngine};
use crate::engine::{CircleId, DeviceId, InviteTicket, JoinRequest, SyncEngine, SyncError};
use crate::identity::{self, Identity};
use crate::invite::{self, InviteError};
use crate::store::claims;
use crate::store::descriptors::{self, CircleDescriptor};

// Exit codes are sysexits, so the whole binary speaks one dialect (spec §5.1).
const EX_OK: i32 = 0;
/// Refused: not the admin, already a Member, expired, stewardless, ambiguous.
const EX_FAIL: i32 = 1;
const EX_USAGE: i32 = 64;
/// The Sync Engine is unreachable, unauthorised, or below the version floor.
const EX_UNAVAILABLE: i32 = 69;

/// Default Invite window. Spec §3.2.3 allows 5m–7d through `--expires`; v0.1's
/// CLI carries no such flag, so 24h is the only bound this build issues.
const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;

const INVITES: &str = "invites.json";
const KNOCKS: &str = "knocks.json";
const DISMISSED: &str = "dismissed.json";

// ── kith invite ──────────────────────────────────────────────────────

/// Mint or reprint this Circle's single open Invite window, and print its code.
///
/// Admin-only, and **local**: the code is assembled from records this Device
/// already holds — the Circle descriptor names both the Circle and the Steward's
/// Device — so a Person whose daemon is down can still hand a friend a code
/// (spec §4.7). The engine is contacted anyway, but only so that kith can say
/// plainly that the Device the code points at cannot be reached yet.
///
/// `new` supersedes an open window instead of reprinting it. Reprinting is the
/// default because losing the code costs nothing and must not cost the Person
/// their remaining window either.
pub async fn invite(new: bool) -> i32 {
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
            // At most one window per Circle, so the new one replaces the row it
            // supersedes rather than sitting beside it. Nothing is lost by that:
            // a knock arriving now is solicited by the window that is open now.
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
                // A note, not the code: stderr, so a piped `kith invite` still
                // carries nothing but the one line a script asked for.
                eprintln!("The previous window for {} is closed.", circle.name);
            }
            (minted, false)
        }
    };

    let ticket = InviteTicket {
        circle: CircleId(descriptor.id.clone()),
        steward_device: DeviceId(descriptor.founder_device.clone()),
        expires_at: window.expires_at.max(0) as u64,
    };
    print_invite(&circle.name, &invite::encode(&ticket), &window, unix, reprinted);

    if let Err(e) = &engine {
        eprintln!();
        eprintln!("Your Device cannot be reached right now — they will not get in until it is.");
        eprintln!("  {}", engine_reason(e));
    }
    EX_OK
}

/// The whole of `kith invite`'s output, and the honesty that has to travel with
/// a code (spec §3.2.3).
///
/// When stdout is not a terminal the code goes to stdout **bare** so that
/// `kith invite | wl-copy` and `kith invite > code.txt` behave, and the prose
/// goes to stderr, where the Person still reads it. When stdout *is* a terminal
/// the code is also pushed to the system clipboard with an OSC 52 escape — no
/// dependency, and it works over ssh.
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
        say("'kith invite --new' starts a fresh 24h window. The old code keeps pointing at your");
        say("Device either way; only the window changes.");
        return;
    }

    say("Send it to one person over a channel you already trust. kith has no messaging and");
    say("wants none.");
    say("");
    say(&format!(
        "Anyone who sees this code can ask to join {name}. Nobody gets in until you approve"
    ));
    say("them, and you will see who is asking. There is no way to un-send it — it stops");
    say(&format!("being expected in {remaining}."));
    say("");
    say("When they run 'kith join', their kith prints a fingerprint. Ask them to read it to");
    say("you, and check it before you approve.");
}

// ── kith join ────────────────────────────────────────────────────────

/// Consume an Invite code: knock at the Steward's Device and wait to be admitted.
///
/// Everything up to the knock is local, so a bad paste, a stale code or a Circle
/// this Device is already in costs nobody a round trip (spec §3.4 steps 2–4).
///
/// This build stops at the knock. Completing a join needs the change feed —
/// `Change::CircleOffered` arrives only while kith is running — so `join` says
/// what it has done and what has not happened yet rather than hiding the wait
/// behind a spinner. That is ADR-0002's accepted consequence, made explicit.
pub async fn join(code: &str) -> i32 {
    let Some(me) = me() else { return EX_FAIL };

    let ticket = match invite::decode(code) {
        Ok(t) => t,
        Err(InviteError::Expired) => {
            // The expiry itself is not carried by the error, and the Person
            // deserves to know how stale the code is rather than just that it is.
            let stale = invite::decode_at(code, 0)
                .ok()
                .map(|t| human_duration(jiff::Timestamp::now().as_second() - t.expires_at as i64));
            match stale {
                Some(ago) => eprintln!("This invite expired {ago} ago."),
                None => eprintln!("This invite has expired."),
            }
            eprintln!("Ask for a new one — 'kith invite' takes them a second.");
            return EX_FAIL;
        }
        Err(InviteError::WrongVersion) => {
            eprintln!("This invite code was made by a newer kith than this one.");
            eprintln!("Update kith, or ask them for a code from a build that matches.");
            return EX_USAGE;
        }
        // A truncating chat client is the overwhelmingly likely cause of a code
        // that decodes to the wrong checksum, so kith says "damaged" rather than
        // "invalid": the fix is to re-paste it, not to ask for a new one.
        Err(InviteError::Checksum) => {
            eprintln!("That invite code arrived damaged — check nothing was cut off, and paste");
            eprintln!("it again. Nothing has been contacted.");
            return EX_USAGE;
        }
        Err(InviteError::Malformed) => {
            eprintln!("That is not a kith invite code. One starts with KITH1- and is a single");
            eprintln!("line of letters and digits.");
            return EX_USAGE;
        }
    };

    // Nothing below has touched the Sync Engine, and the two refusals here keep
    // it that way.
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
        println!("  Circle    {} — kith learns its name when its records arrive", ticket.circle.0);
        println!(
            "  Steward   Device {} — kith cannot name the Person yet",
            fingerprint(&ticket.steward_device.0)
        );
        println!(
            "  Expires   in {}",
            human_duration(ticket.expires_at as i64 - jiff::Timestamp::now().as_second())
        );
        println!("  As        {} ({})", me.display_name, me.person.short());
        match &root {
            Some(r) => println!("  On disk   {}", r.display()),
            None => println!("  On disk   kith has no data directory on this Device"),
        }
        println!();
        // A join is already a deliberate act — the Person pasted a code — so the
        // confirmation exists to show *what* is being joined before anything is
        // registered. With no terminal there is nobody to ask, and refusing
        // would make `kith join` unusable from a script for no gain in safety.
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

    // The record is written before the knock, so a crash between the two leaves
    // something resumable rather than a knock nobody remembers making.
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

    match &repeat {
        Some(k) => {
            let waited = since(&k.first_knock_at)
                .map(human_duration)
                .unwrap_or_else(|| "a while".to_string());
            println!("You asked to join this Circle {waited} ago, and are still waiting.");
            println!("kith has re-registered the request; nothing else changed.");
        }
        None => println!("Asked to join."),
    }
    println!();
    println!("  Your fingerprint   {}", fingerprint(&mine.0));
    println!("  Read it to them. They will see the same four-and-four before they approve.");
    println!();
    println!("They have to approve you on their own Device. kith cannot tell you whether they");
    println!("have looked — there is no server that would — and nothing arrives until they do.");
    println!();
    println!("Your request stays registered with the Sync Engine across restarts. Run");
    println!("'kith join <code>' again, or open kith, to pick it up.");
    EX_OK
}

/// Where a joined Circle's bytes will land.
///
/// The Circle's *name* is not in the ticket — it arrives with `.kith/circle.toml`
/// on the first sync — so the id names the directory. It is unique by
/// construction, which the name is not (spec §4.11).
fn join_root(circle: &CircleId) -> Option<PathBuf> {
    circles_dir().map(|d| d.join(circle.0.to_lowercase()))
}

// ── kith approve ─────────────────────────────────────────────────────

/// Admit a knocking Device into the Circle this Person stewards.
///
/// The whole truth available about a knock is a Device Identity, a name **that
/// Device announced about itself**, and when it was first seen (spec §3.5.1).
/// There is no Person in it, so every line printed here is built on a Device and
/// the prompt says so before it asks anything.
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
    println!("kith cannot tell you who this is. It sees a Device, not a Person. Ask your friend to");
    println!("read you the fingerprint their kith printed, and approve only if it matches.");
    println!();
    println!("Eight characters is enough to tell two Devices apart and to catch a mistake reading");
    println!("one back; it is not enough to resist someone deliberately grinding a matching");
    println!("prefix. The whole Identity, if it matters:");
    println!("  {}", request.device.0);
    println!();

    if !io::stdin().is_terminal() {
        eprintln!("Approving is deliberate, and v0.1 has no flag that skips the question.");
        eprintln!("Run 'kith approve' from a terminal.");
        return EX_USAGE;
    }
    let agreed = match &solicited {
        Solicited::ByOpenInvite { .. } => confirm("Approve?"),
        // An unsolicited knock, or one arriving against a window that has closed,
        // gets more friction rather than a different flag: spec §3.5.2 spends
        // `--force` here and this build has none, so the Person types the
        // fingerprint out instead. Friction here is the feature.
        _ => {
            println!("This knock was not expected. Type its fingerprint to approve it anyway.");
            typed(&fingerprint(&request.device.0))
        }
    };
    if !agreed {
        println!("Nothing was admitted. The Device keeps knocking; 'kith reject' hides it.");
        return EX_OK;
    }

    if let Err(e) = engine.admit(&CircleId(descriptor.id.clone()), &request).await {
        return refuse_engine(&e);
    }

    // Single use, per the glossary: approval is where "an Invite is consumed by
    // joining" can actually be implemented, because this is the Device where
    // admission happens.
    if let Some(w) = windows.iter_mut().find(|w| w.circle == descriptor.id) {
        w.state = WindowState::Spent;
        w.spent_by = Some(request.device.0.clone());
        if let Some(dir) = &state {
            save(&dir.join(INVITES), &windows);
        }
    }

    println!();
    println!(
        "Admitted {} to {}. The invite is used up; 'kith invite' issues another.",
        fingerprint(&request.device.0),
        circle.name
    );
    println!("Nothing was written into the Circle: a Role is derived from who founded it, so");
    println!("there is no shared record of who is who for an approval to change.");
    println!("They publish their own Membership claim when their kith places the Circle — that");
    println!("is when kith can name the Person behind that Device, and not before.");
    println!("Every other Member learns their Device the next time their Device and yours are");
    println!("connected to each other.");
    EX_OK
}

// ── kith reject ──────────────────────────────────────────────────────

/// Hide a knocking Device. Local, and told to nobody.
///
/// Rejection needs no seam method and gets none: it records the Device in
/// `dismissed.json` and kith filters it out of every pending surface. It is
/// deliberately not `expel` (that removes an *admitted* Device) and deliberately
/// not a dismissal at the engine, which the Device would undo by dialling again.
pub async fn reject(device: Option<&str>) -> i32 {
    let Some(me) = me() else { return EX_FAIL };

    let engine = reach_engine().await;
    let circle = match &engine {
        Ok(e) => match stewarded(e, &me, "reject").await {
            Ok(c) => c,
            Err(code) => return code,
        },
        // Hiding a knock is local state, so it does not need the daemon to
        // decide which Circle it belongs to either.
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

    // A dismissal is keyed by the full Device Identity, so the pending list is
    // normally what expands a fingerprint into one. When the engine is not
    // answering, a Person who has the whole Identity in front of them can still
    // hide it — rejection is local state, and local state does not need a daemon.
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
                eprintln!("kith cannot list who is knocking without the Sync Engine.");
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
        "Hidden. {} keeps trying to reach your Device — kith stops showing it.",
        fingerprint(&target)
    );
    println!("It is not told anything: there is no server to deliver a \"no\". If someone is");
    println!("waiting on you, tell them yourself.");
    match &where_ {
        // v0.1's CLI carries no `--forget`, so the un-hide is the file itself.
        // Naming it is what keeps a rejection from becoming an invisible one.
        Some(path) => println!("Recorded in {} — remove its line to un-hide it.", path.display()),
        None => println!("kith has no state directory on this Device, so this will not survive a restart."),
    }
    EX_OK
}

// ── choosing between knocks ──────────────────────────────────────────

/// The knock a verb acts on: the named one, or the only one there is.
///
/// With several knocking Devices and no argument kith lists them and stops. A
/// fingerprint is how one knock is told from another out of band, so it is also
/// how one is named here.
fn choose<'a>(
    visible: &'a [JoinRequest],
    query: Option<&str>,
    hidden_here: usize,
    circle: &str,
) -> Result<&'a JoinRequest, i32> {
    // A rejection must never become an invisible one, so a hidden knock is
    // always counted out loud wherever the pending list looks empty.
    let hidden_note = |code: i32| {
        if hidden_here > 0 {
            eprintln!(
                "{hidden_here} hidden knock{} — kith stops showing a Device you rejected.",
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

/// Whether a Person's argument names this Device.
///
/// Matched against the Identity with its grouping removed, so `UJZD-EGXD`,
/// `ujzdegxd` and the whole 52-character Identity all name the same Device.
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
///
/// Only used where there is no pending list to expand a fingerprint against, and
/// deliberately generous: the seam's handle is opaque above it (ADR-0002 §1), so
/// kith checks that a Person typed something Identity-shaped and no more.
fn looks_like_a_device_identity(s: &str) -> bool {
    squash(s).len() >= 32
}

/// The first eight characters of a Device Identity, grouped 4-4: `UJZD-EGXD`.
///
/// This is the string two People read to each other out of band — the joiner's
/// kith prints theirs, the Steward's kith prints the one that knocked, and a
/// match is the only evidence either of them has about who is on the other end.
/// Eight base32 characters is 40 bits: enough to tell two Devices apart and to
/// catch a transcription error, not enough to resist someone grinding a prefix.
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
    /// The Circle's id, once its descriptor has arrived. `None` is spec §4.10 —
    /// a real state, not a fault.
    id: Option<String>,
    /// What to call it on screen: the descriptor's name, else the transport's
    /// label, else the directory it lives in. kith never mints the Circle's name
    /// from the engine's label into the record; this is display only.
    name: String,
    root: PathBuf,
    descriptor: Option<CircleDescriptor>,
}

/// Every Circle this Device holds, from the two sources that between them see
/// all of them.
///
/// [`local_circles`] needs nothing running, which is what lets `kith invite`
/// work with the daemon down. The Sync Engine's list is the only way to find a
/// Circle a Person placed somewhere else with `--path`, so it is folded in
/// whenever the engine is answering.
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

/// The Circles kith itself placed, read straight off this Device.
///
/// No engine, no daemon, no network: a Circle root is a directory with a
/// `.kith/` in it, and that is enough to know the Circle exists and who founded
/// it. Everything `kith invite` needs lives here.
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

/// Add a Circle unless its root is already in the list. Two sources see the
/// same Circle whenever kith placed it where kith puts them.
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
            // A descriptor that exists and does not parse is somebody's problem,
            // but not a reason to pretend the Circle is not here.
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
/// v0.1's CLI has no `--circle` (ROADMAP §2 fixes the flag list), so exactly one
/// resolvable Circle is the only case kith can act on. It refuses to guess
/// between several rather than picking: admitting a Device to the wrong Circle
/// is real and irreversible (spec §4.8).
fn pick(mut circles: Vec<Held>, verb: &str) -> Result<Held, i32> {
    match circles.len() {
        1 => Ok(circles.remove(0)),
        0 => {
            eprintln!("You are not in any Circle yet.");
            eprintln!("'kith create <name>' starts one, or 'kith join <code>' joins a friend's.");
            Err(EX_FAIL)
        }
        _ => {
            eprintln!("More than one Circle is on this Device, and 'kith {verb}' has no way to");
            eprintln!("choose between them in v0.1. kith will not guess which one you mean:");
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
///
/// Pending joins are Circle-agnostic at the seam, so the Circle is resolved
/// first and the knock is admitted into that one. A Person who holds a Circle
/// they do not steward is told who its admin is rather than told they have none.
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

/// A Circle's admin, as one line for a list.
fn admin_of(c: &Held) -> String {
    match stewardship(c) {
        Stewardship::Held { person, .. } => name_person(c, &person),
        Stewardship::Vacant { was, .. } => format!("{} — left", name_person(c, &was)),
        Stewardship::Disputed { .. } => "this Circle disagrees about who its admin is".into(),
        Stewardship::Unknown => "not known yet — waiting for the Circle's records".into(),
    }
}

/// Whether this Person is the Circle's admin, by the one record that says so.
fn is_admin(c: &Held, me: &Identity) -> bool {
    matches!(stewardship(c), Stewardship::Held { ref person, .. } if person == me.person.as_str())
}

/// The Circle's descriptor, if this Person may act as its admin.
///
/// Every refusal here is exit 1 and names what is actually wrong: a Circle whose
/// records have not arrived, one whose admin has left, one whose Devices
/// disagree about who founded it, and one whose admin is somebody else.
fn steward_check(c: &Held, me: &Identity, verb: &str) -> Result<CircleDescriptor, i32> {
    match stewardship(c) {
        Stewardship::Unknown => {
            eprintln!("{} · waiting for the Circle's records", c.name);
            eprintln!("kith has this Circle's bytes and not its `.kith/circle.toml`, so it cannot");
            eprintln!("say who its admin is. Content keeps syncing; membership waits.");
            Err(EX_FAIL)
        }
        Stewardship::Disputed { claimants } => {
            eprintln!("{} · this Circle disagrees about who its admin is", c.name);
            for who in &claimants {
                eprintln!("  {}", name_person(c, who));
            }
            eprintln!("Two Devices have each written the record that names the founder. kith will");
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
                    eprintln!("run 'kith invite'. kith refuses this on your Device — it cannot stop other");
                    eprintln!("software on your Device from doing something similar, and it will not");
                    eprintln!("pretend otherwise.");
                }
                // The admin's PersonId is known and no claim has named them yet.
                // kith prints the id's short form and never the founder's Device
                // Identity in its place: a Device is not a Person.
                None => {
                    eprintln!(
                        "Only {}'s admin (unknown Person {}) can invite people or approve joins.",
                        c.name,
                        short_person(&person)
                    );
                    eprintln!("kith refuses this on your Device — it cannot stop other software on your");
                    eprintln!("Device from doing something similar, and it will not pretend otherwise.");
                }
            }
            if verb != "invite" {
                eprintln!();
                eprintln!("A knock only ever reaches the admin's own Device. kith is not hiding");
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
    /// `.kith/circle.toml` names a founder whose claims do not all say they left.
    Held { person: String, device: String },
    /// The founder stamped `left_at` into their own Device's Membership claim.
    Vacant { since: String, was: String },
    /// Copies of `.kith/circle.toml` disagree about `founder_person` (spec §4.9).
    Disputed { claimants: Vec<String> },
    /// No readable `.kith/circle.toml` in the tree yet (spec §4.10).
    Unknown,
}

/// Derive the Circle's stewardship from its records and nothing else.
///
/// The Steward is read from `.kith/circle.toml` rather than from the transport,
/// because that record reads the same from every Device including the Steward's
/// own — the engine's introducer flag is invisible from exactly the vantage
/// point that matters most (ADR-0002 §3), so it is a cross-check and never a
/// source.
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

/// Copies of `.kith/circle.toml` that the transport left beside it.
///
/// The name of a copy belongs to the engine, so this recognises them by the rule
/// the format itself gives (ADR-0004 §4.3): the key is the segment before the
/// first dot, and anything else named `circle.*.toml` is a second copy of the
/// one write-once record. That keeps the engine's spelling below the seam.
fn descriptor_copies(root: &Path) -> Vec<CircleDescriptor> {
    let dir = descriptors::kith_dir(root);
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
///
/// Never the Device Identity from `founder_device` in its place. A Device is not
/// a Person, and printing one where the other belongs is the single confusion
/// the whole membership design exists to prevent.
fn name_person(c: &Held, person: &str) -> String {
    match claim_name(c, person) {
        Some(name) => name,
        None => format!("unknown Person ({})", short_person(person)),
    }
}

fn short_person(person: &str) -> String {
    person.chars().take(8).collect()
}

/// `$XDG_DATA_HOME/kith/circles` — where a Circle goes unless a path was named.
fn circles_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_dir().join("kith/circles"))
}

// ── the same three records, for the TUI ──────────────────────────────
//
// The Members screen decides exactly what `kith approve` and `kith reject`
// decide, so it has to read and write the same three files — a Device hidden
// from one surface and visible on the other is two answers to one question.
//
// Every function here is quiet by construction: a TUI is inside the alternate
// screen, where a stray `eprintln!` corrupts the frame a Person is looking at.
// The private CLI readers narrate their failures on stderr, which is right for a
// command and wrong here, so these read the files themselves and degrade in the
// documented direction instead — silently, and towards the safer answer.

/// Whether this Device has an invite window open for a Circle, in the shape the
/// Members screen renders.
///
/// A window kith cannot read reads as `Unsolicited`: the knock then needs its
/// fingerprint typed out in full before anybody is admitted. Safe, and noisier —
/// which is the right way for this particular record to fail (spec §2.3).
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
/// record hides nobody, which shows a knock twice rather than silently dropping
/// one — the same direction as everything else here.
pub fn dismissed(circle: &str) -> Vec<String> {
    let hidden: Vec<Dismissal> = state_dir()
        .map(|d| quiet_read(&d.join(DISMISSED)))
        .unwrap_or_default();
    hidden.into_iter().filter(|h| h.circle == circle).map(|h| h.device).collect()
}

/// Hide a knock from the TUI. Local, and told to nobody: there is no server to
/// deliver a "no", and that Device may keep knocking.
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

/// Mark this Circle's window spent after an admission made on the Members
/// screen. Approval is where "an Invite is consumed by joining" is actually
/// implemented, because this is the Device where admission happens — and that
/// is as true of the TUI's prompt as it is of `kith approve`.
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

/// [`read_state`] with nothing written to stderr.
fn quiet_read<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

// ── the local records (spec §2.3) ────────────────────────────────────

/// The invite *window*: the bound that actually matters, because this is the one
/// checked on the Steward's own hardware at approval time.
///
/// Losing this file closes every window, so every knock reads as unsolicited and
/// the human confirms it by hand. Safe, and noisier — which is the right way for
/// this particular record to fail.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Window {
    circle: String,
    /// The window's id. Not carried in the code this build prints — v0.1's
    /// ticket has no room for it — and kept because it is what a later format
    /// binds a knock to a window with.
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
    /// The stored state, with the clock applied. An open window whose moment has
    /// passed is expired whether or not anything has written that down yet.
    fn effective(&self, now: i64) -> WindowState {
        match self.state {
            WindowState::Open if self.expires_at <= now => WindowState::Expired,
            other => other,
        }
    }
}

/// The joiner's half: a knock this Device has made and is waiting on.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Knock {
    circle: String,
    steward_device: String,
    /// Where the Circle will be placed when it is offered back. Chosen by the
    /// joiner before anything is registered, because the joiner chooses the path
    /// and the engine never auto-accepts (ADR-0002 §1).
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

/// Why a knock was expected, or was not (spec §3.5.1).
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

/// `$XDG_STATE_HOME/kith` — machine-written state, not the Person's config.
fn state_dir() -> Option<PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(
        base.state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("kith"),
    )
}

/// Read one of the three local records. A missing file is an empty list, and so
/// is an unreadable one: every record here documents how it degrades when it is
/// lost, and none of them degrades into a failed command.
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

/// Write one of the three local records whole, or not at all.
///
/// The same staging the synced descriptors use — write beside, flush, rename —
/// for the same reason: a half-written record must never be read as a whole one.
/// Nothing here replicates, so the temp name needs no engine agreement.
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
///
/// None of these records is authoritative about anything a Circle shares, so a
/// Device that cannot write one still invites, joins and admits — it simply
/// remembers less, in the documented way.
fn save<T: Serialize>(path: &Path, rows: &[T]) {
    if let Err(e) = write_state(path, rows) {
        eprintln!("! {} could not be written ({e})", path.display());
    }
}

// ── the Person, the engine, and the two of them failing ──────────────

/// This Person, or the one sentence that says why kith cannot act for them.
fn me() -> Option<Identity> {
    match identity::load() {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            eprintln!("No Identity on this Device. Run 'kith init' and give kith a name to attach");
            eprintln!("to what you add.");
            None
        }
        Err(e) => {
            eprintln!("{e}");
            None
        }
    }
}

/// A Sync Engine handle that is known to answer.
///
/// Credentials come from the daemon's own configuration, overridden by whatever
/// the Person wrote in `config.toml`. kith reads both and writes neither.
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

/// One sentence per engine failure, in kith's vocabulary rather than the
/// transport's.
fn engine_reason(e: &SyncError) -> String {
    match e {
        SyncError::Unreachable => {
            "The Sync Engine is not running, or kith cannot find its configuration.".to_string()
        }
        SyncError::Unauthorized => {
            "The Sync Engine rejected our credentials. kith never rewrites the daemon's config \
             — check its API key."
                .to_string()
        }
        SyncError::Incompatible(v) => {
            format!("The Sync Engine is version {v}, below the version kith needs.")
        }
        other => format!("The Sync Engine answered with a problem: {other}"),
    }
}

/// Every engine failure is exit 69 here: `join` and `approve` both write engine
/// configuration, and kith does not queue engine writes (spec §4.7).
fn refuse_engine(e: &SyncError) -> i32 {
    eprintln!("{}", engine_reason(e));
    eprintln!("kith adapts a daemon you run; it never starts, embeds or configures one.");
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

/// Confirmation by transcription rather than by keystroke, for the decisions
/// that should cost more than one letter.
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

/// A duration as a Person would say it out loud.
fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => plural(s, "second"),
        60..=3599 => plural(s / 60, "minute"),
        // Up to two days is read in hours, so a fresh 24h window says "24h"
        // rather than "1d" and the copy matches the Invite it describes.
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
///
/// kith trusts its own clock and no other; there is no time authority in a
/// serverless product and kith does not simulate one.
fn local_time(unix: i64) -> Option<String> {
    let ts = jiff::Timestamp::from_second(unix).ok()?;
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    Some(zoned.strftime("%a %-d %b, %H:%M").to_string())
}

/// An RFC 3339 stamp as a bare date: `7 Aug`. Falls back to the stamp itself,
/// which is ugly and true.
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

/// Put the code on the system clipboard with an OSC 52 escape.
///
/// kith has no transport for an Invite and never will (ROADMAP §5), so the most
/// it does is make the code easy to hand over. OSC 52 needs no dependency and
/// works over ssh; a terminal that does not implement it ignores the sequence.
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
            .join("kith-membership-tests")
            .join(format!("{label}-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn descriptor(founder: &str) -> CircleDescriptor {
        CircleDescriptor {
            schema: 1,
            id: "kith-4tj2q9xa".into(),
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

    /// `PersonId`'s inner string is private, so a Person is built here the way
    /// every other reader builds one: by deserialising it.
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
        let dir = root.join(".kith/members");
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

    /// The one string two People read to each other. Both sides compute it the
    /// same way or the out-of-band check is worthless.
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

    /// A Person types back what they were read: any case, any spacing, or the
    /// whole Identity pasted.
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
            circle: "kith-4tj2q9xa".into(),
            nonce: "01K1YFQ2M7VJ3W8T0PZ4RXAB6C".into(),
            issued_at: "2026-08-07T09:02:11Z".into(),
            expires_at,
            state,
            spent_by: None,
        }
    }

    /// Expiry is a clock fact, not a stored one: nothing has to run for a window
    /// to close, which is what makes losing the file safe.
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
            solicited(&open, "kith-4tj2q9xa", 500),
            Solicited::ByOpenInvite {
                issued_at: "2026-08-07T09:02:11Z".into(),
                expires_at: 1_000
            }
        );
        assert_eq!(
            solicited(&open, "kith-4tj2q9xa", 2_000),
            Solicited::ByClosedInvite(WindowState::Expired)
        );
        assert_eq!(
            solicited(&[window(1_000, WindowState::Spent)], "kith-4tj2q9xa", 500),
            Solicited::ByClosedInvite(WindowState::Spent)
        );
        // No window at all, and a window belonging to another Circle, both mean
        // nobody was invited here.
        assert_eq!(solicited(&[], "kith-4tj2q9xa", 500), Solicited::Unsolicited);
        assert_eq!(
            solicited(&open, "kith-somewhere-else", 500),
            Solicited::Unsolicited
        );
    }

    #[test]
    fn the_invite_line_never_claims_more_than_the_window_knows() {
        let now = 500;
        let open = solicited(&[window(now + 3_600, WindowState::Open)], "kith-4tj2q9xa", now);
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

        // The staging file is never left behind: a Circle a Person cannot read
        // with `ls` is one they cannot check.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![INVITES.to_string()], "{names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Spec §2.3's loss behaviour, in code: a lost `invites.json` closes every
    /// window, so every knock reads as unsolicited and the human confirms it.
    /// Safe, and noisier — never a failed command.
    #[test]
    fn an_unreadable_window_file_closes_every_window_rather_than_failing() {
        let dir = scratch("state-damaged");
        let path = dir.join(INVITES);
        std::fs::write(&path, "{ not json").unwrap();

        let windows: Vec<Window> = read_state(&path);
        assert!(windows.is_empty());
        assert_eq!(
            solicited(&windows, "kith-4tj2q9xa", 0),
            Solicited::Unsolicited
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_dismissal_hides_one_device_in_one_circle_and_nothing_else() {
        let hidden = [Dismissal {
            circle: "kith-4tj2q9xa".into(),
            device: ANA_DEVICE.into(),
            rejected_at: "2026-08-07T09:02:11Z".into(),
        }];
        let hides = |circle: &str, device: &str| {
            hidden.iter().any(|h| h.circle == circle && h.device == device)
        };
        assert!(hides("kith-4tj2q9xa", ANA_DEVICE));
        assert!(!hides("kith-4tj2q9xa", BEN_DEVICE), "one Device, not a class");
        assert!(!hides("kith-elsewhere", ANA_DEVICE), "one Circle, not all");
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

    /// §4.10: a Circle whose records have not arrived has no admin kith can
    /// name, and kith never promotes a Device Identity into the empty slot.
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

    /// One claim with `left_at` and one without means a Device stopped, not that
    /// the Person did — so the Circle still has its admin.
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

    /// §4.9: two copies of the one write-once record naming two founders is the
    /// only way a Circle can disagree about its admin. kith refuses to pick.
    #[test]
    fn two_records_naming_two_founders_are_disputed_rather_than_resolved() {
        let root = scratch("disputed");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        let ben = "p-01k9r7wq3f8bx2m5nz4h7cvted";
        write_circle(&root, &descriptor(ana)).unwrap();
        let mut theirs = descriptor(ben);
        theirs.founder_device = BEN_DEVICE.into();
        std::fs::write(
            descriptors::kith_dir(&root).join("circle.sync-conflict-20260807-143122-K5J2FVL.toml"),
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

    /// A copy that agrees with the record is not a dispute — there is one
    /// founder and two files saying so.
    #[test]
    fn a_copy_that_agrees_about_the_founder_is_not_a_dispute() {
        let root = scratch("agreeing-copy");
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&root, &descriptor(ana)).unwrap();
        std::fs::write(
            descriptors::kith_dir(&root).join("circle.sync-conflict-20260807-143122-K5J2FVL.toml"),
            toml::to_string_pretty(&descriptor(ana)).unwrap(),
        )
        .unwrap();

        assert!(matches!(stewardship(&held(&root)), Stewardship::Held { .. }));
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ── who may act, and on which Circle ─────────────────────────────

    /// A Member who is not the admin is pointed at the Person who is, rather
    /// than told there is nothing here. Only the admin's own Device ever sees a
    /// knock, and saying so is the honest half of refusing.
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

    /// v0.1's CLI has no `--circle`, so kith acts on one Circle or on none.
    /// Admitting a Device to the wrong Circle cannot be undone.
    #[test]
    fn kith_refuses_to_guess_between_two_circles() {
        let (a, b) = (scratch("pick-a"), scratch("pick-b"));
        let ana = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
        write_circle(&a, &descriptor(ana)).unwrap();
        let mut other = descriptor(ana);
        other.id = "kith-9pq3zx71".into();
        other.name = "photos".into();
        write_circle(&b, &other).unwrap();

        assert_eq!(pick(vec![held(&a)], "invite").map(|c| c.name).ok(), Some("walls".into()));
        assert_eq!(pick(vec![held(&a), held(&b)], "invite").err(), Some(EX_FAIL));
        assert_eq!(pick(Vec::new(), "invite").err(), Some(EX_FAIL));
        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    // ── naming People, and refusing to name Devices ──────────────────

    /// The single confusion this design exists to prevent: an admin nobody has
    /// claimed renders as a Person's short id, never as the founder's Device.
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
        // A fresh window says what its own copy says.
        assert_eq!(human_duration(DEFAULT_TTL_SECS), "24h");
        assert_eq!(human_duration(19 * 3_600 + 12 * 60), "19h 12m");
        assert_eq!(human_duration(2 * 86_400), "2d");
        assert_eq!(human_duration(7 * 86_400), "7d");
        assert_eq!(human_duration(-5), "0 seconds", "a lapsed bound is not negative");
    }

    #[test]
    fn a_stamp_kith_cannot_read_is_printed_rather_than_guessed_at() {
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
        assert_eq!(base64(b"KITH1-AH6BTS5I"), "S0lUSDEtQUg2QlRTNUk=");
    }

    /// The code and the ticket that made it are the same fact twice, and the
    /// window is what bounds both.
    #[test]
    fn the_printed_code_carries_the_window_it_was_minted_under() {
        let w = window(1_786_000_000, WindowState::Open);
        let ticket = InviteTicket {
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
