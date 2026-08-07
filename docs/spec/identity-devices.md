# Spec: Identity & Devices

- **Status:** Accepted
- **Date:** 2026-08-07
- **Resolves:** [#12 Spec: identity & devices](https://github.com/opx0/wp-sync/issues/12)
- **Informed by:** `CONTEXT.md`, `ROADMAP.md` §2 (Identity row), ADR-0001, ADR-0002 §§2–4 & 6,
  ADR-0003 §6, `research/syncthing-api` §§4–5, §11

---

## 1. Purpose

This module answers one question for every other module: **who added this, and may this
Device be here?**

It owns four things and nothing else:

1. **Minting a Person** on first run — a display name and a stable `PersonId`, held on
   this Device, issued by nobody.
2. **Binding this Device** to that Person, where the Device's only real identity is the
   Sync Engine daemon's certificate-derived device identity, which kith reads and never
   owns.
3. **Publishing and resolving the roster** — the synced records that say which Device
   speaks for which Person, so a Circle can show People instead of 63-character device
   IDs.
4. **Telling the truth about trust and loss** — what admission does and does not
   guarantee, when a human is asked, and what "no recovery authority" means in
   sentences a Person can act on.

Everything above the module addresses People. Everything below the Sync Engine seam
addresses Devices. This module is the only place the two meet, and it meets them in
files, not in the seam: **linking Devices into one Person requires no seam method, in
v0.1 or ever.**

---

## 2. Domain objects involved

| Object | Role here |
|---|---|
| **Person** | Minted by `kith init`. Carries `PersonId` + display name. The attribution key for every Item, every Member row, every Sidecar. |
| **Device** | One kith installation bound to one Sync Engine daemon. Identified *by* that daemon's device identity — kith mints no second ID space (§4.2). |
| **Identity** | The daemon's TLS certificate and key. kith never reads, copies, or backs it up (ADR-0002 §2). This module records *that it exists* and which Person claims it. |
| **Member** | A Person's participation in a Circle. Resolved by folding roster records; the Role attached to it is the circles spec's problem ([#13]). |
| **Circle** | The scope of trust. Admission is per-Circle; there is no global trust store (§6.1). |
| **Invite** | The only way a Device becomes trusted. Ticket format and expiry are ADR-0002 §2 and [#13]; this spec covers only the human moment of approval. |
| **Sidecar** | Records `added_by: PersonId` — never a `DeviceId`. This is the single constraint that makes a second Device in v0.3 a no-migration change (§5.3). |
| **Sync Engine** | Supplies `local_device()`, `devices()`, `pending_joins()`, `admit()`, and the `JoinRequested` change. Supplies no notion of Person, deliberately. |

---

## 3. On-disk layout

| Path | Contents | Authority |
|---|---|---|
| `$XDG_DATA_HOME/kith/identity.toml`<br>(`~/.local/share/kith/identity.toml`) | This Device's Identity claim: `PersonId`, display name, bound `DeviceId`, device name | **Local source of truth for who you are.** Not synced, not a secret, not a key. |
| `<circle root>/.kith/roster/<DEVICE-ID>.toml` | One published roster record per Device per Circle | **Synced source of truth for who the Circle's Members are.** Single-writer: only the named Device ever writes its own file. |
| `$XDG_CACHE_HOME/kith/cache.sqlite` | Resolved roster, for startup without re-reading every record | Rebuildable cache (ADR-0001 authority rule). Deleting it costs a re-read, never data. |
| `<circle root>/.kith/local/` | Atomic-write staging (§4.4). Ignored from sync by ADR-0002's `.stignore` seed | Per-Device scratch. |
| daemon config dir (`~/.local/state/syncthing/`, v1: `~/.config/syncthing/`) | The certificate and key that *are* the Identity | Owned by the daemon. kith reads `config.xml` for credentials only (ADR-0002 §6) and never touches `cert.pem`/`key.pem`. |

Identity **never** lives in `$XDG_CONFIG_HOME/kith/config.toml`. Config is hand-edited
and copied between machines; an Identity copied between machines is two Devices claiming
one Device ID, which is the failure in §7.2.

> **Naming trap.** The legacy `~/.config/wp-sync/identity` file holds `API_KEY=` — daemon
> *credentials*, not an Identity. ADR-0002 §6 reads it as a credential source of last
> resort. kith never derives a Person from it, and `kith doctor` names it as a credential
> path so nobody mistakes the two.

### 3.1 `identity.toml`

```toml
# kith Identity. This file names you; it does not authenticate you.
# The only thing that authenticates this Device is the Sync Engine key it is bound to,
# which lives in the daemon's config directory and which kith never reads.
schema = 1

[person]
id           = "p-7f3k9x2m4qb8ycv0jhr5tdn6ew"
display_name = "Ana"
created      = "2026-08-07T14:02:11Z"

[device]
id       = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2"
name     = "ana-thinkpad"
bound_at = "2026-08-07T14:02:11Z"
```

Written with mode `0600` — not because it is secret (it is not), but because a stray
world-readable copy is the one thing that lets another local account publish records in
your name (§7.2). Timestamps are RFC 3339, UTC, second precision, everywhere in this
spec.

### 3.2 The roster record

`<circle root>/.kith/roster/P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2.toml`

```toml
schema       = 1
device       = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2"
device_name  = "ana-thinkpad"
person       = "p-7f3k9x2m4qb8ycv0jhr5tdn6ew"
display_name = "Ana"
joined       = "2026-08-07T14:03:02Z"
updated      = "2026-08-07T14:03:02Z"
```

Four rules make this format conflict-tolerant without a coordinator, which is the
property ADR-0004 must preserve when it fixes the encoding for Sidecars:

| Rule | Why |
|---|---|
| **The filename is the key.** A record whose `device` field ≠ its filename is ignored and reported. | Removes the only ambiguity a merge would have to resolve. |
| **One writer per file** — a Device writes `<its own id>.toml` and no other. | Two Devices never race on one path, so `*.sync-conflict-*` copies do not occur in normal operation. When one appears anyway (§7.4) it is evidence, not a merge problem. |
| **Whole-file replace, last write wins.** No field-level merging, ever. | The only writer is the Device itself; its latest statement about itself is definitionally current. |
| **Unknown keys are ignored on read; a `schema` above the known major is read best-effort and never rewritten.** | An older kith beside a newer one degrades to "I can see Ben, I can't see his new field" instead of corrupting the record. |

A Device in three Circles publishes three records — the roster lives in the Circle's
tree, not in a global place, because there is no global place. v0.1 publishes the same
`display_name` to every Circle from `identity.toml`; per-Circle names are representable
and unimplemented.

---

## 4. Behaviour

### 4.1 First run — `kith init`

`kith init` is the only verb that mints a Person. It requires a reachable Sync Engine,
because a Device that does not yet know its own device identity cannot be bound, and
kith refuses to invent a placeholder it would later have to reconcile.

**Order of operations, all-or-nothing:**

1. Resolve daemon credentials (ADR-0002 §6). On `Unreachable` → exit **69** with the
   probed address and config paths; nothing is written. On `Unauthorized` → exit 69 with
   where the key was found; never regenerate one.
2. `engine.local_device()` → `DeviceId`. Also `engine.health()` for the version floor;
   `Incompatible` → exit 69, nothing written.
3. If `identity.toml` exists → §4.6 (idempotent path / rebind), never a second mint.
4. Prompt for the display name (or take `--name`). Validate per §4.3.
5. Propose a device name (`--device-name` overrides): the system hostname, lowercased
   and slugified. If the hostname is empty or one of `localhost`, `archlinux`, `fedora`,
   `debian`, `ubuntu`, `nixos`, `pc` — indistinguishable between two friends' machines —
   propose `<person-slug>-<first device-ID group, lowercased>` instead, e.g.
   `ana-p56ioi7`.
6. Mint `PersonId`: 128 bits from `getrandom`, Crockford base32, prefixed `p-` →
   `p-` + 26 chars. There is no registry, so there is no uniqueness check; a collision
   would merge two People in a Circle's display and is accepted at 2⁻¹²⁸.
7. Write `identity.toml` atomically (temp file in the same directory, `fsync`,
   `rename(2)`, `0600`).
8. Print the identity and the loss warning (§4.2 transcript), then the next verb.

`kith init` writes **nothing** into any Circle and touches **nothing** in the daemon's
config. It is purely local. Roster publication happens when a Circle exists (§4.4).

**Transcript — interactive:**

```
$ kith init
kith — local-first collections shared with the people you trust.

Sync Engine  reachable · Syncthing 2.1.3 · http://127.0.0.1:8384
             credentials from ~/.local/state/syncthing/config.xml

Your name, as the People you share with will see it: Ana
Name for this Device [ana-thinkpad]:

Identity created.
  Person   Ana            p-7f3k9x…
  Device   ana-thinkpad   P56IOI7…XZWICQ2
  Stored   ~/.local/share/kith/identity.toml

This Device is your Identity. kith issues no accounts and holds no key it could
give back to you: if you lose this Device, your Circles keep everything you added
and your name stays on it, but nothing — not kith, not your friends — can restore
your ability to add anything more as Ana.

Next:  kith create <name>        start a Circle
       kith join <invite code>   join someone else's
```

**Non-interactive:** `kith init --name "Ana" [--device-name ana-thinkpad]` prompts for
nothing. If stdin is not a TTY and `--name` is absent → exit **78** with
`kith init --name <name>` in the message. Automation is a supported path; guessing a
human's name is not.

### 4.2 One ID space: the Device *is* the daemon

kith mints no device identifier of its own. The Device's ID is the Sync Engine's
certificate-derived device ID, verbatim.

The alternative — a kith-level device UUID that survives an engine identity change — was
rejected because it manufactures a continuity nothing else in the system honours. Peers
authenticate a certificate, folders list device IDs, admission adds a device ID. A kith
UUID would let the TUI say "same Device" about a machine every Circle correctly treats
as a stranger. One ID space keeps the honest answer the only representable one.

Consequences, all deliberate:

- The Device's identity changes when the daemon's key changes. That is not a bug to
  paper over; it is §7.3.
- **A Device is a daemon, not a login.** Two OS accounts sharing one system-wide daemon
  are one Device to kith, and §7.2 is what happens then.
- kith never writes the daemon's own `name` field. The Device name in this spec is a
  kith-level fact carried in the roster record, honouring ADR-0002 §6's list of things
  kith does not mutate.

**Short forms**, used in every surface where the full ID would not fit: Device
`P56IOI7…XZWICQ2` (first group, ellipsis, last group); Person `p-7f3k9x` (prefix plus
six). Full forms appear in `kith doctor`, in the approval prompt's detail view, and
nowhere else.

### 4.3 Names

Names are asserted, never verified. The validation below exists to keep a hostile name
from lying about the *UI*, not about the Person.

| Field | Rule |
|---|---|
| `display_name` | 1–32 characters after trimming; at least one non-whitespace; no C0/C1 controls, no newlines. Unicode welcome. **Bidi overrides and isolates (U+202A–U+202E, U+2066–U+2069) and zero-width joiners at the boundaries are stripped on input and on render** — a name is shown inside an approval prompt, and a right-to-left override there can make a device ID read backwards. No uniqueness requirement: there is no registry to enforce one against. |
| `device_name` | Slug: lowercase, `[a-z0-9]` first, then `[a-z0-9._-]`, 1–32 chars. Non-conforming input is slugified with a printed notice, not rejected. |

**Collision display.** Two People named "Ben" in one Circle are both shown as
`Ben (p-3k9x2m)` — the disambiguating suffix appears only while ambiguity exists, so the
common case stays clean. Two *Devices* of one Person disambiguate by `device_name`.

**Resolution order** for the name shown against a peer Device:

1. Its roster record's `display_name` — the Person's own claim.
2. The engine's device name for that peer (`PeerDevice.name`, learned by introduction),
   rendered in quotes and dimmed: `"bens-laptop" · no kith record yet`.
3. The short device ID.

Rung 2 exists for the window between admission and the roster record arriving, and it is
visibly distinguished because it is a name the *engine* carries, not one the Person
published.

### 4.4 The reconciler — publishing this Device into a Circle

One function runs at every kith start, CLI or TUI, before any command does its work. It
is cheap: one REST call already needed for `status`, plus one `stat` per Circle.

```rust
// src/identity/mod.rs
pub enum IdentityState {
    /// No identity.toml. Only `init`, `doctor`, `status`, `version` may proceed.
    Absent,
    /// Stored DeviceId == engine's local_device(). Everything works.
    Bound(LocalIdentity),
    /// Engine reachable, and it is not the Device we were bound to. §7.3.
    Mismatched { stored: LocalIdentity, live: DeviceId },
    /// Engine unreachable: the binding could not be checked. Assume it holds, say so.
    Unverified(LocalIdentity),
}

pub struct LocalIdentity { pub person: Person, pub device: DeviceRecord }
pub struct Person       { pub id: PersonId, pub display_name: String, pub created: OffsetDateTime }
pub struct DeviceRecord { pub id: DeviceId, pub name: String, pub bound_at: OffsetDateTime }

/// Loads identity.toml, checks the binding, publishes roster records where needed.
/// Never blocks on the network beyond one `local_device()` with the engine's normal timeout.
pub async fn reconcile(
    engine: &impl SyncEngine,
    circles: &[CircleRef],
    store: &IdentityStore,
) -> Result<(IdentityState, Vec<RosterProblem>), IdentityError>;
```

Publication, per Circle root, is a three-branch decision — and the third branch is the
whole reason the reconciler is a named thing rather than an inline write:

| Existing `.kith/roster/<me>.toml` | Action |
|---|---|
| absent | Write it. This is the first write a joining Device makes into a Circle, immediately after `complete_join`. |
| present, `person` == ours, fields equal | Do nothing. The common path costs one read. |
| present, `person` == ours, fields differ | Rewrite with `updated` bumped. |
| present, `person` != ours | **Stop. Never write.** Emit `RosterProblem::PersonContradiction` and report it. |

That last branch is not defensive coding, it is the fix for §7.2: two Persons behind one
Device ID would otherwise rewrite each other's record on every start, forever, and
replicate the flapping to the whole Circle. kith would rather show a Circle a problem
than generate churn in it.

**Atomic write across a synced tree.** Writing `foo.tmp` then renaming inside
`.kith/roster/` would replicate the temp file to every Member. Instead the record is
written to `<circle root>/.kith/local/roster-<device>.tmp`, `fsync`ed, and `rename(2)`d
into `.kith/roster/` — same filesystem, and `.kith/local` is already ignored from sync by
ADR-0002's `.stignore` seed. Members see one atomic appearance, never a partial file.

### 4.5 Reading the roster — People from Devices

```rust
pub struct RosterRecord {
    pub device: DeviceId, pub device_name: String,
    pub person: PersonId, pub display_name: String,
    pub joined: OffsetDateTime, pub updated: OffsetDateTime,
}

pub enum RosterProblem {
    FilenameMismatch    { path: PathBuf, claimed: DeviceId },
    ConflictCopy        { path: PathBuf },                     // §7.4
    PersonContradiction { device: DeviceId, ours: PersonId, theirs: PersonId },
    Unreadable          { path: PathBuf, detail: String },
}

pub struct PersonView {
    pub id: PersonId,
    pub display_name: String,
    pub devices: Vec<DeviceView>,   // ≥ 1; v0.1 shows 1, the type never assumed it
    pub is_me: bool,
    pub online: bool,               // any Device connected
}

impl Roster {
    pub fn load(circle_root: &Path) -> (Roster, Vec<RosterProblem>);
    pub fn person_of(&self, device: &DeviceId) -> Option<&PersonId>;
    /// Fold records by PersonId, join against engine presence, sort by display name.
    pub fn people(&self, peers: &[PeerDevice], me: &LocalIdentity) -> Vec<PersonView>;
}
```

`people()` is the Person/Device split in one function: it groups by `PersonId` and folds
presence with OR across a Person's Devices. It is written for N Devices in v0.1 and
exercised with 1. `load` never fails — an unreadable record becomes a `RosterProblem` and
the rest of the Circle still resolves, because one bad file must not blank a Members
screen.

**Devices with no record.** A peer the engine reports but the roster does not describe is
shown as an *unidentified Device* — rung 2 or 3 of §4.3 — never hidden. It is either a
Member whose record has not arrived yet or a Device someone admitted outside kith; both
are things a Person should be able to see.

### 4.6 Re-running `kith init`

| State | Behaviour |
|---|---|
| `Bound`, no flags | Print the current identity, change nothing, exit **0**. Idempotent by design: `kith init` is what a setup script runs unconditionally. |
| `Bound`, `--name`/`--device-name` given | Refuse, exit **64**. v0.1 has no rename (ROADMAP §2). |
| `Mismatched` | Offer to rebind: keep the `PersonId` and `display_name`, replace `device.id`/`bound_at`. Prints exactly what re-admission this costs (§7.3) and requires `y` or `--yes`. |
| `Absent` | Mint (§4.1). |
| engine unreachable | Exit **69**. Rebinding requires knowing what to rebind *to*. |

> **Rename, honestly.** The reconciler publishes whatever `identity.toml` currently says,
> so a Person who edits the file by hand gets the new `display_name` published on the next
> run. That is a consequence of an idempotent publisher, not a feature: there is no verb,
> no `--help` entry, and no TUI path, and this spec records it so that the behaviour is
> known rather than discovered. A real rename — with the Circle-wide "Ana is now Anna"
> question it drags along — belongs to whichever milestone claims it.

---

## 5. Person ↔ Device over the seam

### 5.1 The seam knows nothing, and needs to know nothing

ADR-0002 §2 maps Person to **none — deliberately**. Linking Devices into one Person is
therefore entirely a file-format question above the seam:

| Concern | Where it lives |
|---|---|
| Does this key belong to this machine? | The daemon. Cryptographic, real. |
| May this Device be in this Circle? | The introducer's `admit()` call. Enforceable, because it runs on the gatekeeper's own hardware (ADR-0002 §4). |
| Which Person does this Device speak for? | `person` in the Device's own roster record. **A claim.** |
| Which Devices are one Person? | A fold over roster records (`Roster::people`). |

The trust chain has exactly one cryptographic link and it is the first one. Everything
after is a human having said yes to a device ID, plus that Device's self-description.
Every surface that shows a name is showing rung 4, and §8.3 makes the UI say so.

### 5.2 What v0.1 implements and what it does not

| | v0.1 |
|---|---|
| Person/Device as distinct types, distinct IDs | **Yes** |
| `Sidecar.added_by` is a `PersonId` | **Yes** |
| Roster keyed by Device, folded to People | **Yes** |
| Reading N Devices per Person — grouping, presence-OR, per-Device rows | **Yes.** A Circle that already contains two Devices claiming one `PersonId` — a Member running a later kith — renders correctly as one Person with two Devices, on a v0.1 client, with no update. |
| Writing a second Device for an existing Person | **No.** There is no verb, no prompt, no file path that produces one. `kith init` on a machine with no `identity.toml` always mints a *new* Person. |
| Enrolment: getting an existing `PersonId` onto a new Device | **No.** This is the entire missing piece (§5.3). |

The split in one sentence: **the read path is already plural; the write path is
deliberately singular.**

### 5.3 Why the second Device is not a migration

Nothing changes on disk when v0.3 arrives:

- `identity.toml` already stores a `PersonId` independent of the `DeviceId`, so a second
  Device holds the same Person without any field being repurposed.
- Roster records are already per-Device with a `person` field; two records naming one
  Person is a case the format already expresses and `people()` already folds.
- Sidecars already attribute to `PersonId`, so every Item written in v0.1 attributes
  correctly forever. **This is the load-bearing constraint** — a Sidecar that attributed
  to a `DeviceId` would make the second Device a data migration across every Member's
  disk, which is exactly the migration ROADMAP forbids.
- The seam is untouched: `admit()` already admits a device ID; a Person's second Device
  is simply a second admission. No new `SyncEngine` method, so ADR-0002's 16-method
  budget survives v0.3.

What v0.3 must build is only the enrolment moment, and the honest sketch is: Device A
prints its `PersonId` plus a short nonce; Device B takes it at `init`; B then knocks at
each Circle and is admitted like any Device, publishing a record naming the same Person.
The nonce guards against typos, not forgery — ROADMAP forbids home-grown cryptography,
and kith cannot sign with a key the daemon owns. A second Device is believed for the same
reason a first one is: a human approved it.

---

## 6. Trust

### 6.1 What trusting a Device means

Exactly one thing: **admission** — a Device ID appearing in a Circle's device list on the
admitting Device, via `SyncEngine::admit`. That is the entire trust primitive.

| Trusting a Device does | Trusting a Device does **not** |
|---|---|
| Let it connect and exchange the Circle's bytes | Grant a Role — Roles are policy (ADR-0002 §4) |
| Let it write anything into the Circle, including other People's Items | Verify that its display name is that human's name |
| Let it publish a roster record claiming any Person | Bind it to a Person in any enforceable way |
| Propagate, when done by the introducer, to every Member (ADR-0002 §3) | Extend to any other Circle |

**Trust is per-Circle. There is no global trust store, no per-Person allow-list, no
"contacts".** A Device admitted to `walls` is a stranger to `photos` until admitted
there. The one leak is ADR-0002 §3's device-scoped introducer flag: when the same two
People share two Circles, the introducer propagates device lists for both mutually shared
folders. Additive only, and named here because it is the one place where "trust is
per-Circle" is not the whole truth.

### 6.2 When a Person is asked

Four moments, and — importantly — one non-moment.

| # | Trigger | Who is asked | What they see | Outcome |
|---|---|---|---|---|
| 1 | `kith create` | nobody | — | You founded it; you are its steward and sole introducer. |
| 2 | `Change::JoinRequested` — a Device consumed your Invite and is knocking | the **steward**, on the Device that issued the Invite | claimed device name, short + full device ID, first-seen time, which Invite it matched and when that Invite expires | `admit()` or dismiss |
| 3 | `Change::CircleOffered` — the Circle is offered back after your join | the **joiner** | Circle name, offering Device, proposed local path | `complete_join(offer, root)` or dismiss |
| 4 | Startup finds `IdentityState::Mismatched` | this Device's Person | old and new device IDs | rebind or quit (§7.3) |
| — | **The steward admits somebody new to a Circle you are in** | **nobody** | Members gains a row | No prompt, ever. |

That last row is a decision, not an omission. Being in a Circle *is* delegating admission
to its steward — that is what an introducer is, and pretending otherwise by prompting
would offer a veto that does not exist: the Device is already in your config by the time
you could answer, propagated automatically (ADR-0002 §3). kith shows the arrival in
Members and tells the truth about the recourse, which is leaving the Circle.

### 6.3 The approval prompt

The moment where a human converts an unverifiable claim into transport-level trust, so it
is specified to the character:

```
  Join request — walls

  Device      "bens-laptop"                        ← the Device's own name for itself
              P56IOI7…XZWICQ2                      ← the only fact kith can verify
  Seen        2026-08-07 14:31 (2 minutes ago)
  Invite      issued 14:12 today · expires in 47 hours

  kith cannot tell you who is holding this Device. Approve it only if you asked
  someone for this join and can confirm the ID above with them out of band.

  [y] approve   [n] reject   [d] full device ID   [esc] decide later
```

- `y` → `admit()`. `n` → dismiss the pending device. `esc` → leave it pending; the engine
  keeps it, and the prompt returns next time.
- The quoted device name is attacker-controlled text; it is quoted, dimmed, and never
  rendered as the primary identifier — the device ID is. §4.3's bidi stripping applies
  here first and foremost.
- No auto-approval exists: `admit` is never called without a human keystroke or an
  explicit `kith approve --yes` in a script the Person wrote. ADR-0002 §6's refusal to use
  `autoAcceptFolders` is the same rule seen from the joiner's side.

### 6.4 Distrust

v0.1 has no distrust verb. `SyncEngine::expel` exists on the seam and ROADMAP puts Member
removal in v0.2; until then the honest statements are: leaving a Circle stops your
Device replicating it (`leave`, bytes kept), and nothing retracts bytes already on a
Device. Every removal story in this product is forward-looking; §9's copy says so where a
Person will read it.

---

## 7. Edge cases & failure honesty

### 7.1 A Device is re-installed

The daemon's key is gone, so the Device ID is gone. What survives depends on what was
kept:

| Kept | Result |
|---|---|
| `identity.toml` only | Same Person, new Device. `kith init` rebinds (§4.6). Past Items stay attributed to you, your name persists. **Every Circle must admit the new Device by a fresh Invite**, exactly like a stranger — because it is one, cryptographically. |
| daemon config only | Same Device ID, no Person. `kith init` mints a **new** Person. Old Items keep the old `PersonId`, which now resolves to nobody and renders as *unknown Person (p-7f3k9x)*. Worse: the Circle already holds a roster record for this Device ID naming the old Person, so the reconciler hits the contradiction branch (§4.4) and refuses to publish. `kith doctor` names the cause and the fix — delete the stale record, which this Device is the rightful writer of, or restore `identity.toml`. |
| both | Nothing happened. |
| neither | §7.5. |

**If the re-installed Device was the Circle's steward**, v0.1's answer is blunt: the
Circle keeps syncing — data flows mesh, every Member holds every byte — but no new Member
can join and no removal propagates, because the introducer is gone and v0.1 ships no
succession verb (ADR-0002 §3 designs one; the v0.1 CLI surface does not include it).
`kith doctor` says exactly that, in those terms, rather than reporting a healthy Circle.

### 7.2 Two People share a machine

| Arrangement | Works? |
|---|---|
| Two OS accounts, each running its own user-session daemon (the distro default: `syncthing.service` as a user unit) | **Yes, fully.** Separate XDG dirs, separate daemons, separate device IDs, separate Identities. Nothing special is needed and nothing is shared. |
| Two OS accounts sharing one system-wide daemon | **No.** One daemon means one Device ID means one Device. Both Identities bind to the same ID; whichever runs first publishes its roster record, and the second hits §4.4's contradiction branch and **refuses to write** — no flapping record, no churn replicated to the Circle. `kith doctor` reports it as *shared Sync Engine daemon* and prescribes a per-account daemon, or `[sync_engine] address` pointing kith at a different one. |
| One OS account, two humans | **Not supported.** One Identity per account per daemon. The workaround is separate OS accounts, and the docs say so plainly rather than inventing a profile switcher. |

The rule underneath all three: **a Device is a daemon, not a login, not a machine.**

### 7.3 The daemon's device identity changes underneath kith

Causes: re-install (§7.1), a wiped state directory, a restored-from-backup daemon,
`[sync_engine] address` newly pointing at a different daemon, or a Person running kith
over an SSH tunnel to the wrong host.

Detection is one comparison at every start — `identity.toml`'s `device.id` against
`engine.local_device()` — and it is free, because that call is already made for `status`.

| Engine | State | kith does |
|---|---|---|
| reachable, IDs match | `Bound` | everything |
| reachable, IDs differ | `Mismatched` | **Refuse to write anything anywhere.** Banner in the TUI, error on the CLI. |
| unreachable | `Unverified` | Proceed on the stored identity, and say "Sync Engine offline — Device binding unverified" in `status`. Never guess. |

What `Mismatched` blocks, and why blocking is the right call rather than a warning:

| Blocked | Why |
|---|---|
| `add` | New Items would get Sidecars attributing them to a Person whose Device the Circle no longer holds — and the bytes would sync to nobody, because this device ID is in no folder's device list. Silent non-delivery is the worst outcome available. |
| `create`, `invite`, `approve`, `reject`, `join` | Every one of them puts a device ID into somebody's config. Publishing a device ID that is about to be rebound is a mess in other People's daemons, not just this one. |

What still works, because kith is a gallery before it is a sync client: browsing,
preview, Apply, Favourites, `list`, `status`, `doctor`. Exit code for a blocked verb is
**78**, and the message is one line plus the fix: `kith init` to rebind.

### 7.4 A roster record has a conflict copy

`.kith/roster/<DEVICE>.sync-conflict-20260807-143122-P56IOI7.toml` cannot happen
under the single-writer rule, so its existence is information: two Devices wrote one
record (§7.2's shared daemon), or a Member restored an old tree over a live one, or
something is impersonating. kith **never merges** it. It is filtered out of the Members
screen, reported by `kith doctor` with both records' `person` and `updated` values side
by side, and left on disk for a human. This is the same posture ADR-0002 §2 takes toward
conflict copies in the gallery: handled, never hidden.

### 7.5 Losing every Device — the honesty section

There is no recovery authority because there is no authority. Concretely:

| Question | Answer |
|---|---|
| Can kith restore my Identity? | No. The key is the daemon's; kith never copied it and has nowhere to copy it to. |
| Can my friends restore it? | No. They hold your *content* and a record saying "device X is Ana". Neither reconstructs the key. |
| Is my content gone? | **No.** Every Item you added lives on every Member's disk. That is the whole point of peer-to-peer. |
| Does my name stay on it? | Yes. Sidecars attribute to `PersonId`, which is a fact in the synced tree, not a lookup against you. |
| Can I come back? | As a new Person, always — `kith init`, fresh Invite. As the *same* Person, only if you kept `identity.toml`, and even then every Circle must admit your new Device. |
| Is `identity.toml` a backup of my Identity? | **No, and this matters.** It backs up your *name and attribution*, not your ability to prove anything. Anyone who copies it can claim the same name. It is a convenience; the Device is the Identity. |
| What if I was the steward? | The Circle survives and keeps syncing; it can admit nobody new (§7.1). |

### 7.6 Smaller sharp edges

| Case | Behaviour |
|---|---|
| `identity.toml` unparseable or truncated | Treated as `Absent` for safety but **never overwritten**. Exit 78, print the path and the parse error. A Person's only copy of their `PersonId` is not something kith gets to clobber on a bad read. |
| `schema` newer than this kith | Read best-effort, refuse to write, `doctor` says upgrade. |
| Clock skew across Devices | `joined`/`updated` are display facts only. Nothing orders, resolves, or expires on them. Invite expiry is checked by the admitting Device against its own clock (ADR-0002 §2) — never against a peer's timestamp. |
| Two People pick the same display name | Both shown with `(p-xxxxxx)` suffixes (§4.3). Not an error; there is no registry. |
| A Device publishes a record naming a `PersonId` that also belongs to a different Device | In v0.1 this is exactly the v0.3 shape, so it renders as one Person with two Devices. `doctor` notes it as unexpected for v0.1 rather than treating it as corruption. |
| `$XDG_DATA_HOME` on a network filesystem that loses the file | Same as §7.5's "identity.toml lost". Nothing here is precious except the daemon's key, which is not ours. |

---

## 8. CLI & TUI surface

Grammar and argument syntax across the whole binary are the CLI/TUI spec's ([#16]). This
section fixes only what identity contributes, within ROADMAP's v0.1 verb list:
`init`, `create`, `join`, `invite`, `approve`, `reject`, `add`, `list`, `status`,
`doctor`, `version`.

### 8.1 Identity requirements per verb

| Verb | Needs Identity | Needs engine | Identity behaviour |
|---|---|---|---|
| `version` | no | no | Prints the version. Touches nothing. |
| `doctor` | no | no | Reports identity fully in every state, including `Absent`. The verb you can always run. |
| `status` | no | no | Prints an Identity block; `Identity: none — run kith init` when absent, exit 0. Diagnostics do not fail on the thing they diagnose. |
| `init` | creates it | **yes** | §4.1, §4.6 |
| `create` | yes | yes | Publishes this Device's roster record into the new Circle as its first write, right after `create_circle`. |
| `join` | yes | yes | On `complete_join`, publishes the roster record before anything else — a Member should be nameable the instant they appear. |
| `invite` | yes | yes | Ticket carries this Device's ID as introducer (ADR-0002 §2). |
| `approve` / `reject` | yes | yes | §6.3. |
| `add` | yes | no | Stamps `added_by = <PersonId>` into each Sidecar. Blocked when `Mismatched` (§7.3). |
| `list` | yes | no (degraded) | Member rows resolve through `Roster::people`; without the engine, presence renders as unknown rather than offline. |

Missing Identity on a verb that needs one:

```
$ kith create walls
error: no Identity on this Device.
       run `kith init` to create one — it takes one question.
$ echo $?
78
```

Exit codes follow ADR-0003's sysexits convention: **0** ok, **1** refused (rejected
prompt, `doctor` found problems), **64** usage (`--name` on an existing Identity), **69**
Sync Engine unavailable where required, **78** no Identity / Identity mismatch.

### 8.2 `kith approve` / `kith reject`

Pending joins are shown only on the Circle's steward Device. A non-steward Member whose
daemon sees a knock gets a `doctor` line explaining that the steward approves — v0.1 does
not offer the pairwise admission the transport technically permits, because a
partially-connected Circle is harder to explain than a frozen one.

```
$ kith approve
walls — 1 join request

  1  "bens-laptop"  P56IOI7…XZWICQ2   seen 2m ago   invite expires in 47h

approve #1? [y/N]
```

With several pending, each is listed and `kith approve <n>` or
`kith approve <device-id-prefix>` selects one. `--yes` approves without prompting;
`--all` is not offered, because "approve everything currently knocking" is precisely the
mistake the prompt exists to prevent. `kith reject <n>` dismisses. Both exit 78 without
an Identity, 69 when the engine is unreachable, 1 when the Person declines.

### 8.3 `kith status` and `kith doctor`

`status` — identity block first, because "who am I on this machine" is the question a
status command is usually being asked:

```
Identity     Ana · p-7f3k9x
Device       ana-thinkpad · P56IOI7…XZWICQ2
Sync Engine  reachable · Syncthing 2.1.3
Circles      walls (3 Members, 2 online) · synced
```

`doctor` — the identity checks, each with a one-line remedy on failure:

| Check | Failure text names |
|---|---|
| `identity.toml` present, parses, `schema` known | the path and the parse error |
| Sync Engine reachable, credentials found | the address and every config path probed (ADR-0002 §6) |
| stored `DeviceId` == `local_device()` | both IDs, what is blocked, and `kith init` |
| shared-daemon collision (§7.2) | the other `PersonId` and the per-account-daemon fix |
| roster record published, current, in every Circle | which Circles are missing it |
| roster problems: filename mismatch, conflict copy, contradiction | file paths, unmerged |
| stewardship: is this Circle's steward reachable | that joins are frozen while it is not (§7.1) |
| legacy `~/.config/wp-sync/identity` found | that it is a credentials file, not an Identity |

`doctor` exits 1 if any check fails, 0 otherwise, and prints every check either way —
including the passing ones, because a Person running `doctor` is usually trying to find
out what kith can see, not only what is broken.

### 8.4 TUI touchpoints

Within ROADMAP's v0.1 TUI surface — Gallery, Preview, Members, pending-join prompt,
Circle switcher — identity appears in exactly five places:

| Surface | Identity contribution |
|---|---|
| **Status bar** | `Ana · walls · 3 Members · synced`. The Person's name is the leftmost element on every screen. |
| **Members** | One row per **Person**, not per Device: presence dot, display name, Role, `(you)`, and the device name dimmed at the end. Unidentified Devices get their own rows via §4.3's rungs. Footer honesty line, always present: *Names are what each Person calls themselves. kith verifies Devices, not names.* (The Role honesty line is [#13]'s and sits beside it.) |
| **Pending-join prompt** | §6.3, verbatim. Raised by `Change::JoinRequested`, dismissible with `esc` without deciding. |
| **Preview** | *added by Ana* resolves `Sidecar.added_by` through the roster; an unresolvable `PersonId` renders as *unknown Person (p-7f3k9x)* and never as blank. |
| **Identity-mismatch banner** | A persistent bar in `Mismatched` state naming what is disabled and `kith init` as the fix. It cannot be dismissed — a Device that cannot sync should not look like one that can. |

**No onboarding wizard.** Bare `kith` with no Identity does not open a TUI into an empty
shell; it prints the `kith init` line and exits 78. ROADMAP's TUI surface lists five
things and a first-run wizard is not among them, and a one-question CLI verb does not
need a screen.

---

## 9. What the docs tell People

This copy is normative — README, `kith init` output, and `doctor`'s loss-related remedies
carry it in this form or shorter, never softer:

> **kith cannot recover your Identity, because kith never issued it.**
>
> Your Identity is a key your Sync Engine daemon holds on this Device. kith does not copy
> it, does not escrow it, and has nowhere to escrow it to — there is no server, no
> account, no reset link, and no support address.
>
> - **If you lose this Device:** everything you added stays on your friends' machines,
>   with your name on it. What you lose is the ability to add more *as you*.
> - **`identity.toml` is your name, not your key.** Keeping a copy preserves your name and
>   your past attributions on a new Device. It proves nothing — anyone who copies it can
>   claim the same name — and even with it, every Circle must admit your new Device by
>   Invite, exactly like a stranger's.
> - **Nobody is ever locked out of the content.** A Circle's Items live on every Member's
>   disk. Losing a Person loses a voice, never a Collection.
> - **Trust runs one way and only forward.** Approving a Device lets it into a Circle
>   from that moment; nothing you approve can be un-given for bytes already sent.

---

## 10. Out of scope for v0.1

Named cuts, each with where it returns if it returns:

| Cut | Note |
|---|---|
| **Enrolling a second Device for one Person** | The read path exists (§5.2); the enrolment moment does not. v0.3, no migration (§5.3). |
| **Renaming a Person or Device** | ROADMAP §2 says no rename. The hand-edit consequence is documented in §4.6 and advertised nowhere. |
| **Avatars** | ROADMAP §2. Nothing in the record format precludes a later `avatar` key; unknown keys are already ignored on read (§3.2). |
| **Device grouping as a UI** | The grouping *function* ships (`Roster::people`); a screen for managing it does not. |
| **Removing a Member / revoking a Device** | `expel` exists on the seam; the verb is v0.2 (ROADMAP §4). |
| **Introducer succession** | Designed in ADR-0002 §3; no verb in v0.1's CLI surface. §7.1 states the consequence honestly instead of hiding it. |
| **Any global contacts list, blocking, or muting** | Trust is per-Circle (§6.1). A global trust store is a registry, and kith has no registries. |
| **Signing, verifying, or proving a Person claim** | Requires either home-grown cryptography (ROADMAP §5 forbids it) or reading the daemon's private key (ADR-0002 §2 forbids it). The claim stays a claim, and every surface says so. |
| **Identity export/import verbs** | `cp identity.toml` is the whole feature, and dressing it as a verb would imply it is a backup of something it is not (§7.5). |
| **Multiple Identities per OS account; profile switching** | §7.2. Separate OS accounts. |
| **Per-Circle display names** | Representable in the record format, unimplemented on purpose. |

[#13]: https://github.com/opx0/wp-sync/issues/13
[#16]: https://github.com/opx0/wp-sync/issues/16
