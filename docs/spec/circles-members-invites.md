# SPEC: Circles, Members & Invites

- **Status:** Accepted
- **Date:** 2026-08-07
- **Resolves:** [#13 Spec: circles, members & invites](https://github.com/opx0/wp-sync/issues/13)
- **Informed by:** ADR-0002 §§2–4 (seam, steward discipline, Roles-as-policy), ADR-0001
  (authority rule), ADR-0003 §3 (unavailable-with-reason, exit codes), ROADMAP §§2–3
- **Depends on:** `docs/spec/identity-devices.md` (#12) mints the Person id and the local
  Identity; ADR-0004 (#11) fixes the on-disk encoding of the synced records named here.
  This spec owns their *paths, writers and fields*, not their bytes.

---

## 1. Purpose

The social core: how a Circle comes into existence, how a second Person gets into it, what
each Member can see about the others, and what kith refuses to claim. Everything the v0.1
walkthrough (ROADMAP §3) does between "Ana runs `kith create walls`" and "Ben's Gallery
fills up" is specified here.

Three rules from the ADRs bind every flow below and are not re-argued:

1. **One Steward per Circle.** A Steward is a **Member** — the Person holding `Admin` — and
   it is the *Steward's Device* that is the Circle's only route in or out (ADR-0002 §3).
   In v0.1 both are read from `.kith/circle.toml`: the Person from `founder_person`, the
   Device from `founder_device`. Membership changes stall while the Steward's Device is not
   connected; content does not.
2. **Gates that run on the gatekeeper's own Device are real; everything after admission is
   convention plus recovery** (ADR-0002 §4). Admission is the one real gate kith has. It
   is therefore the only thing this spec is allowed to describe as enforcement.
3. **kith never owns the daemon and never mutates engine config beyond its own Circles**
   (ADR-0002 §6). Every operation here is expressed as a `SyncEngine` call.

---

## 2. Domain objects involved

| Object | This spec's stake in it |
|---|---|
| **Person** | Identified by a stable `PersonId` minted at `kith init` (#12): the literal prefix `p-` plus a 26-character Crockford ULID — `p-01k9r7wq3f8bx2m5nz4h7cvted`, short form the first six characters after the prefix, `p-01k9r7`. The prefix is load-bearing: it makes a `PersonId` self-describing wherever one is printed and unmistakable for a Device Identity, which is the other opaque string this spec prints. This spec assumes only that it is stable and maps to one or more Device Identities. |
| **Device** | Identified by `DeviceId` — the seam's opaque handle on the Device's engine identity (ADR-0002 §1), printed as 52 base32 characters. v0.1: one Device per Person. |
| **Circle** | `CircleId` = `kith-` + 8 base32 characters, minted by the engine at creation, immutable, never derived from the name (ADR-0002 §2). Has a name, a root path on this Device, exactly one Collection, and exactly one admin. |
| **Member** | A Person's participation in one Circle: display name, Role, the moment their claim was last asserted, optional left-at, the Device set *derived* from that Person's Membership claims (§2.2), plus a *presence* value that is computed live and never stored. |
| **Role** | `Admin` or `Member`. Exactly one `Admin` per Circle in v0.1, *derived* — never stored as a Role anywhere (§2.2). Policy, not enforcement (§3.8). |
| **Steward** | The **Member** whose Device is the Circle's sole route in or out — `.kith/circle.toml`'s `founder_person`, whose Device is its `founder_device`. In v0.1 the Steward is the one `Admin`. A Steward is a Person, never a machine; where this spec means the machine it says *the Steward's Device*. |
| **Invite** | A time-bounded offer to join, materialised as a printed code (§3.2.1) plus an *invite window* recorded on the admin's Device (§3.2.3). |
| **Collection** | Created with the Circle, one per Circle, provider `wallpaper`. This spec creates it and stops; Items and import are `docs/spec/collections.md` (#14). |
| **Sync Engine** | Every engine interaction is a call on the `SyncEngine` trait (ADR-0002 §1). This spec adds no method to it. |

### 2.1 The core service

```rust
// crate::social — the only module above the seam that writes membership records.
pub struct Circles<E: SyncEngine> {
    engine: Arc<E>,
    me: Identity,        // PersonId + this Device's DeviceId, read from
                         // $XDG_DATA_HOME/kith/identity.toml (spec #12)
    state: StateDir,     // $XDG_STATE_HOME/kith — §2.3's three files
}

pub struct Circle {
    pub id: CircleId,
    pub name: String,
    pub root: PathBuf,
    pub created_at: DateTime<Utc>,
    pub steward: Stewardship,
}

pub enum Stewardship {
    /// `.kith/circle.toml` names a `founder_person` whose claims carry no `left_at`.
    Held { person: PersonId, device: DeviceId },   // device = `founder_device`
    /// The founder wrote `left_at` into their own Device's Membership claim (§3.9.3).
    Vacant { since: DateTime<Utc>, was: PersonId },
    /// Conflict copies of `.kith/circle.toml` disagree about `founder_person` (§4.9).
    Disputed { claimants: Vec<PersonId> },
    /// No `.kith/circle.toml` in the tree yet, so there is nobody to name (§4.10).
    Unknown,
}

pub struct Member {
    pub person: PersonId,
    /// From the claim with the newest `asserted`. Always present: a Member exists
    /// because a claim names them (§2.2). A `PersonId` with no claim anywhere is not a
    /// Member row — it renders as `unknown Person (p-01k9r7)` wherever it is named
    /// (§3.7), never as a device id.
    pub display_name: String,
    /// Derived, never stored: every Device whose Membership claim carries this `person`
    /// (§2.2). v0.1 length 1.
    pub devices: Vec<DeviceId>,
    /// Derived from `.kith/circle.toml`, never read from a stored Role (§2.2).
    pub role: Role,
    /// Newest `asserted` across their claims — when a Device of theirs last said
    /// "I am here". Not a join date; §3.7 renders it as what it is.
    pub asserted: DateTime<Utc>,
    pub left_at: Option<DateTime<Utc>>,
    pub presence: Presence,
    pub in_sync: Option<u8>,          // percent, from CircleStatus::peers
}

pub enum Role { Admin, Member }

/// Deliberately not called `Online`. See §3.7.1.
/// JSON spelling, everywhere: "connected" | "not_connected" | "unknown".
pub enum Presence {
    /// This Device holds an open connection to at least one of the Person's Devices.
    Connected,
    /// No open connection from this Device right now.
    NotConnected { last_connected: Option<DateTime<Utc>> },
    /// The Sync Engine could not be asked. kith knows nothing.
    Unknown,
}

/// A Device present in the Circle with no Membership claim of its own yet (§3.7).
pub struct UnclaimedDevice { pub device: DeviceId, pub announced_name: String, pub presence: Presence }

impl<E: SyncEngine> Circles<E> {
    /// `adopt` is `None` for a fresh Circle; `Some` takes §3.1's adoption branch.
    pub async fn create(&self, name: &str, path: Option<&Path>, adopt: Option<Adopt>)
        -> Result<Circle, SocialError>;
    pub async fn list(&self) -> Result<Vec<Circle>, SocialError>;
    pub async fn roster(&self, c: &CircleId)
        -> Result<(Vec<Member>, Vec<UnclaimedDevice>), SocialError>;

    pub async fn invite(&self, c: &CircleId, ttl: Duration, reissue: Reissue, hints: &[String])
        -> Result<Invite, SocialError>;
    pub fn open_invite(&self, c: &CircleId) -> Result<Option<Invite>, SocialError>;

    pub async fn join(&self, t: &InviteTicket, root: &Path) -> Result<JoinProgress<E>, SocialError>;
    pub async fn pending(&self) -> Result<Vec<PendingJoin>, SocialError>;
    pub async fn approve(&self, c: &CircleId, req: &JoinRequest) -> Result<(), SocialError>;
    pub fn reject(&self, c: &CircleId, device: &DeviceId, forget: bool) -> Result<(), SocialError>;

    pub async fn leave(&self, c: &CircleId) -> Result<LeaveReport, SocialError>;
}

pub enum Reissue { KeepOpen, Supersede }   // `kith invite` vs `kith invite --new`
pub enum Adopt { Detect, Dir(PathBuf) }    // `--adopt` vs `--adopt <dir>`

pub enum SocialError {
    Engine(SyncError),                     // pass-through; category only, never engine text
    NoIdentity,                            // `kith init` has not run
    /// `admin` is `founder_person`, always known when the descriptor is readable;
    /// `admin_name` is `None` until a claim names them (§3.7).
    NotAdmin { circle: CircleId, admin: PersonId, admin_name: Option<String> },
    Stewardless { circle: CircleId },
    AlreadyMember { circle: CircleId },
    NotAMember { circle: CircleId },
    Invite(InviteError),
    AmbiguousCircle(Vec<Circle>),
    /// Conflict copies of `.kith/circle.toml` only — a Membership claim cannot produce
    /// this (§4.9).
    DisputedStewardship { record: PathBuf, claimants: Vec<PersonId> },
    /// No `.kith/circle.toml` yet: the Circle has no readable identity (§4.10).
    NoCircleRecord { circle: CircleId },
    Io(std::io::Error),
}
```

### 2.2 The synced records this spec owns

Everything durable about membership lives in the Circle's synced tree, under `.kith/`
(reserved by ADR-0002 §2). SQLite holds none of it — under ADR-0001's authority rule the
tree is the source of truth and the cache is rebuildable.

| Record | Sole legitimate writer | Fields |
|---|---|---|
| `.kith/circle.toml` | the founding Device, **write-once** | `schema`, `circle`, `name`, `created`, `founder_person`, `founder_device` |
| `.kith/collections/main.toml` | the same Device, **write-once** | `schema`, `collection` (`main`), `name`, `provider` (`wallpaper`), `root` (`.`), `created` |
| `.kith/members/<device-id>.toml` — a **Membership claim** | the Device its filename names, and no other | `schema`, `device`, `person`, `display_name`, `asserted`, `left_at` (absent until §3.9); `[steward]` and `[grants]` reserved and unwritten in v0.1 |

Both descriptors are written with ADR-0004 §3's descriptor protocol — `<name>.toml.kith-tmp`
beside the target, `fsync`, `rename` — and so is every write to a Membership claim. There is
no other staging path in this spec; nothing here writes into `.kith/local/`.

**There is no roles record.** A Role is *derived*, never stored: the `Admin` is
`.kith/circle.toml`'s `founder_person`, and in v0.1 that Person is also the Steward whose
Device is `founder_device` (ADR-0004 §5). A Steward-written roles map would be a second
writer surface saying what the write-once descriptor already says, and it would be the only
record in the Circle two Devices could disagree about. v0.2's role editing does not bring
one back either: grants land in the `[grants]` table inside the Steward's own Device's
Membership claim — still one file, still one writer, still no migration.

**A Membership claim is keyed by Device, not by Person** (ADR-0004 §5). One Device, one
file, one writer: the Device named in the filename is the only Device that ever writes it,
and the claim is that Device saying *I am here, and I speak for this Person*.

There is therefore no `devices` list in the record. **A Person's Device set is derived** by
grouping every claim carrying the same `person`, and a Person is in the Circle because at
least one claim names them. Device-keying is what makes ADR-0004's W1 rule — exactly one
writer per file — literally true here, and it is what makes v0.3's second Device
migration-free: a second Device writes a second claim instead of contending with the first
over one file. Attribution is unaffected, because it never keyed on the filename: `person`
lives *inside* the claim, and nothing above the seam identifies a Member by Device.

Four derivations follow, stated once here and used throughout §3 (ADR-0004 §5 fixes the
same rules at the byte level):

- **A Person's Role** is `Admin` if their `PersonId` equals `founder_person`, `Member`
  otherwise. Nothing is written when a join is approved, because there is nothing to write.
- **A Person's display name** comes from their most recently written claim — newest
  `asserted`, ties broken by the smaller Device id. v0.1 has one claim per Person, so this
  rule never actually chooses.
- **A Member has left** when *every* claim carrying their `person` has `left_at`. One claim
  with it and one without means a Device stopped, not that the Person did.
- **An unclaimed Device** is a Device in the Circle with no `.kith/members/<device-id>.toml`
  at all — a filename lookup rather than a scan of every Member's Device list (§3.7).

`asserted` is the claim's only freshness field, and every tie-break above reads *newest
`asserted`*. A claim is **rewritable**: ADR-0004's append-only rule governs record logs, not
descriptors, which are read-modify-write under its §3 protocol. Writing `left_at` (§3.9.2)
is exactly that, and it refreshes `asserted` in the same write.

**The one property this spec requires of ADR-0004: every record above has exactly one
legitimate writer.** The social core therefore needs no merge rule, and a conflict copy on
any of these paths is a *symptom* rather than a decision to make — §4.9 says what kith does
with each, and for a Membership claim it is never "pick a winner between two People".
`.kith/local/` is per-Device scratch, excluded from replication by the ADR-0002 §2 recipe,
and nothing in this spec writes there.

### 2.3 The local records this spec owns

Three facts are authoritative, local, and **not derivable from the synced tree**, so by
ADR-0001's authority rule they may not live in SQLite. They live in
`$XDG_STATE_HOME/kith/` as JSON, written atomically (write to `<name>.tmp`, `fsync`,
`rename`) because they are machine-written state, not human config.

| Path | Holds | Loss behaviour |
|---|---|---|
| `invites.json` | per Circle: `{circle_id, nonce, issued_at, expires_at, state: open\|spent\|expired\|superseded, spent_by}` | Every window reads as closed → every knock is *unsolicited* (§3.5.1) → the human confirms. Safe, noisier. |
| `knocks.json` | joiner side: `{circle_id, circle_name, nonce, steward_device: DeviceId, root, state: knocked\|offered\|joined\|abandoned, first_knock_at}` | Re-run `kith join <code>`; a duplicate knock is idempotent at the engine. |
| `dismissed.json` | per Circle: `DeviceId`s the Person rejected, with `rejected_at` | Rejected Devices reappear in the pending list; reject them again. |

---

## 3. Behaviour

### 3.1 Creating a Circle

`kith create <name> [--path <path>] [--adopt [<dir>]]`

`--adopt` is not a second verb. Adoption is a *way of creating a Circle* — the wp-sync
migration path (ADR-0002 §7) — so it is a flag here rather than a `kith adopt`, and the
branch it takes is set out at the end of this section.

**Preconditions, checked in this order, each failing before anything is written:**

1. An Identity exists (`kith init` has run) — else `SocialError::NoIdentity`, exit 1:
   `No Identity on this Device. Run 'kith init' and give kith a name to attach to what you add.`
2. `engine.health()` returns reachable and at or above the version floor — else
   `SyncError::Unreachable` / `Incompatible`, exit 69. Creating a Circle writes engine
   config; kith will not queue that.
3. `<name>` is 1–64 characters, not only whitespace. Names are display strings and need not
   be unique; the `CircleId` is what is unique.

**Steps:**

1. **Resolve the root.** Default `$XDG_DATA_HOME/kith/circles/<slug>/`, where `<slug>` is
   the name lowercased with runs of non-`[a-z0-9]` collapsed to `-` and trimmed. On
   collision, append `-` plus the first four characters of the `CircleId` suffix.
   `--path` overrides; the path must be absent or already empty, must not sit inside
   another Circle's root, and is created with mode `0700`. With `--adopt` the root is the
   adopted directory, which is expected to be non-empty.
2. **`engine.create_circle(name, root)`** → `CircleRef { id, name, root }`. Below the seam
   this allocates the replicated space with the ADR-0002 §2 recipe (bidirectional,
   watcher on, 5 versions / 30 days, the `.kith/local` exclusion) and designates this Device
   as the Steward's Device for the Circle. kith does not touch any global engine setting.
   **Skipped when adopting a space the engine already replicates** — that Circle is kept as
   it is, same id, same root, same peers.
3. **Write `.kith/circle.toml`** with the fields in §2.2: this Person as `founder_person`,
   this Device as `founder_device`. That single write is what makes the founder the Circle's
   admin *and* its Steward — one Member seen twice, the Person in one field and their Device
   in the next — and §3.9 keeps the two in step.
4. **Write `.kith/collections/main.toml`**, `collection = "main"`. This *is* the
   Collection's creation. v0.1's one-Collection-per-Circle rule is one file in a directory
   that holds one file per Collection, so v0.3 adds a Collection by adding a descriptor
   beside it — no existing record is touched and there is no migration.
5. **Write this Device's Membership claim**, `.kith/members/<my-device-id>.toml`, carrying
   `schema`, `device`, `person`, `display_name` and `asserted`. One file, one writer, for
   the life of the Circle.
6. Return. Nothing is invited, nothing is imported, nothing is applied.

**The `--adopt` branch.** `--adopt [<dir>]` takes over a directory that already exists —
with no argument it auto-detects a wp-sync tree; zero or several candidates are `kith`'s
usual exits (cli-tui §4.2). Two cases, and only the second changes anything above:

- **A plain directory.** Steps 1–6 run unchanged; the bytes already there become Items
  through `docs/spec/collections.md` §4.4, which this spec does not own.
- **A space the Sync Engine already replicates** — a live wp-sync install with peers.
  Step 2 is skipped, and **steps 3 and 4 run only on the Device that the engine already
  treats as the Circle's only way in** (collections.md §4.3 owns that rule and its `--claim`
  escape hatch). Every other adopting Device writes step 5 and nothing else.

That last case has a consequence worth stating rather than papering over: **until the Device
that writes the descriptors has run kith, the Circle has no `.kith/circle.toml`, so it has
no `founder_person` and therefore no admin kith can name.** The Circle syncs perfectly and
its Stewardship is `Unknown` (§4.10). Even after the descriptor lands, that Device's Person
is unknown until it publishes a Membership claim of its own — kith says `no admin yet` and
names nobody. It does not substitute the Device's Identity for a Person, and it does not
invent a placeholder Member: a Device is not a Person, and the whole of §2.2 rests on
refusing to blur the two.

```
$ kith create walls
Created walls (kith-7QM4XKC2)
  Collection  walls · wallpaper
  On disk     ~/.local/share/kith/circles/walls
  You         Steward and admin — you invite, and you approve who joins

Add wallpapers with 'kith add <path>...', then 'kith invite' to bring someone in.
```

**Rollback.** If step 3, 4 or 5 fails (disk full, permissions), kith calls
`engine.leave(&id)` to undo step 2 and removes the root if it created it — neither, when
step 2 was skipped for an adopted Circle, because kith did not create that space and does
not dismantle it. A half-made Circle is never left behind; a Circle with no
`.kith/circle.toml` is treated as unnamed by every read path anyway (§4.10). Descriptor
writes are staged (`<name>.toml.kith-tmp`, then `rename`), so a failure leaves no partial
record and nothing partial ever replicates.

**No rename, no delete** in v0.1 (ROADMAP §2). `.kith/circle.toml` being write-once is what
makes that free: there is no second writer to reconcile.

### 3.2 Issuing an Invite: the code

`kith invite [--circle <circle>] [--expires <duration>] [--new] [--address <url>]`

Admin-only. A Member who is not the admin gets `SocialError::NotAdmin`, exit 1, with the
copy in §3.8.3.

#### 3.2.1 Exact shape

An invite code is one line of ASCII, designed to be **pasted**, and grouped so that it
*can* be read aloud if a Person is stuck with a phone call.

```
KITH1-AH6BTS5I-LIKW7QOT-P2L24QUN-ZEXMPMHI-4ZLNKXU5-UVRBZOUB-IQHMAW2F-3LKBG2TX-HGAJXVT4-OTCV5FCH-AV3WC3DM-OMAPFIN5-EI
```

`KITH1` is the literal prefix — `KITH` plus the format version, so a future format is
detectable before decoding. The body is RFC 4648 base32 (alphabet `A–Z2–7`, uppercase, no
padding) of the payload below, hyphenated every 8 characters. With a five-character Circle
name and no address hints the payload is 57 bytes, the body is 98 characters, and the whole
code is 111 characters — a paste, not a passphrase. The device Identity is 32 of those
bytes and is irreducible: it is the only thing that lets the invitee find the admin at all.

#### 3.2.2 What it encodes

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 1 | `version` = `0x01` | Mismatch → `InviteError::UnknownVersion`, refused before anything else is read |
| 1 | 5 | `circle` | The 8 base32 characters after `kith-`, decoded |
| 6 | 32 | `steward_device` | The Steward's Device Identity, raw |
| 38 | 4 | `expires_at` | Unix seconds, u32 big-endian |
| 42 | 8 | `nonce` | 8 bytes from the OS CSPRNG. The invite's id. |
| 50 | 1 | `name_len` (N) | ≤ 64 |
| 51 | N | `name` | Circle name, UTF-8 |
| 51+N | 1 | `hint_count` (H) | ≤ 3; **0 by default** |
| … | | H × (1-byte length + UTF-8, each ≤ 64 bytes) | address hints |

followed by a 4-byte **CRC-32 (IEEE, big-endian) of the payload**. The CRC is a checksum,
not a signature: it catches a truncated paste, a mangled hyphen, a mail client that ate a
line. It proves nothing about who made the code. Nothing in this format needs to, because
the gate is a human approving a knock on their own Device (§3.6) — and ROADMAP §5 forbids
home-grown cryptography, which is exactly what signing invites would be.

Address hints are empty unless `--address tcp://192.168.1.10:22000` is given; the engine's
own discovery resolves the Device Identity on any ordinary network. Hints exist for
LAN-only and discovery-disabled setups and are the only reason the payload is variable
length.

This maps exactly onto ADR-0002 §2's `InviteTicket` — the Circle, its name, the Steward's
Device, address hints, issue and expiry times, the nonce — with one deliberate omission:
**the issue time is not encoded.** It is display-only, the admin already knows it, and
leaving it out shortens the code without weakening anything. Above the seam this spec names
that Device field the Steward's Device; the ticket's own spelling is the seam's business
(ADR-0002 §1).

```rust
impl InviteTicket {
    pub fn encode(&self) -> String;
    pub fn parse(s: &str) -> Result<InviteTicket, InviteError>;
    /// First 8 characters of a Device Identity, grouped 4-4: "CVX4-DU36".
    pub fn fingerprint(d: &DeviceId) -> String;
}

pub enum InviteError {
    NotAnInviteCode,             // no KITH prefix after normalisation
    UnknownVersion(u8),          // KITH2… from a future kith
    Corrupt,                     // base32 or CRC failure
    Expired { by: Duration },
    Truncated,                   // decodes, but the payload is short
}
```

**Parsing, in order:** strip all whitespace and `-`; uppercase; require the `KITH1` prefix
and strip it; map `0`→`O`, `1`→`I`, `8`→`B` (the same confusable normalisation the engine
applies to Device Identities in the same alphabet); base32-decode; split the trailing 4
bytes and verify the CRC; decode the payload; reject any trailing bytes.

#### 3.2.3 How it is bounded

`--expires` accepts `30m`, `2h`, `24h`, `7d`. **Minimum 5m, maximum 7d, default 24h**; out
of range is an error, not a clamp. The bound is expressed twice, and the two are different
things:

- **In the code**, as an absolute `expires_at`. The invitee's kith checks it locally and
  refuses an expired code without contacting anyone — a courtesy that costs the admin
  nothing and saves the invitee a pointless wait.
- **On the admin's Device**, as the *invite window* in `invites.json`. This is the one that
  matters, because it runs on the gatekeeper's hardware (ADR-0002 §4).

**A Circle has at most one open invite window.** Consequences, all deliberate:

- `kith invite` with a window already open **reprints the same code** and the remaining
  time. Losing the code costs nothing and does not extend the window.
- `kith invite --new` supersedes: the old window is marked `superseded`, a new nonce and a
  new window are issued. The old *code* keeps working as a pointer — see §3.2.4.
- The window closes when it expires, or when a join is approved. Approval marks it `spent`,
  which is the glossary's "an Invite is consumed by joining", implemented at the only place
  kith can implement it.
- There is no `--close`. ROADMAP §2 says "no revoke (let it expire)", and §3.2.4 says why
  that is not a compromise.

```
$ kith invite
Invite for walls — expires in 24h (Fri 8 Aug, 14:13)

  KITH1-AH6BTS5I-LIKW7QOT-P2L24QUN-ZEXMPMHI-4ZLNKXU5-UVRBZOUB-IQHMAW2F-3LKBG2TX-HGAJXVT4-OTCV5FCH-AV3WC3DM-OMAPFIN5-EI

Send it to one person over a channel you already trust. kith has no messaging and
wants none.

Anyone who sees this code can ask to join walls. Nobody gets in until you approve
them, and you will see who is asking. There is no way to un-send it — it stops
being expected in 24h.

When they run 'kith join', their kith prints a fingerprint. Ask them to read it to
you, and check it before you approve.
```

Re-running it:

```
$ kith invite
An invite for walls is already open — expires in 19h 12m. Same code:

  KITH1-AH6BTS5I-…-EI

'kith invite --new' starts a fresh 24h window. The old code keeps pointing at your
Device either way; only the window changes.
```

#### 3.2.4 Why there is no revoke in v0.1

Not a cut for time — **there is nothing to revoke.**

The code's only secret is the admin's Device Identity, and a Device Identity cannot be
un-published. Revocation would have to mean one of three things:

| Candidate meaning | Why it fails |
|---|---|
| "The code stops working" | The code is a pointer, not a credential. Whoever has read it can knock for as long as that Device exists, revoked or not. |
| "Knocks from that code are refused" | kith cannot tell which code produced a knock. The knock carries a Device Identity and a name, nothing else — there is no field to smuggle a token through that is not the Device's *global* name, which ADR-0002 §6 forbids kith to touch. |
| "The Person is un-invited" | They were never *in*. Approval is a separate, deliberate human act on the admin's own Device. Declining to approve is the revocation. |

So expiry is not a weaker revoke; it is the honest form of the only thing a code can carry:
**a window in which knocks are expected.** Approval is the gate, and the gate has no
expiry. Invite revocation returns in v0.2 alongside Member removal, where it will mean
"close the window early" — a convenience, and it will be labelled as one.

What a leaked code discloses: the Circle's name, the admin's Device Identity, and an expiry
timestamp. Not content, not the Member list, not who else was invited. `kith invite` says
so in the copy above rather than in a manual.

### 3.3 Transmitting an Invite

kith has no transport for invites and never will (ROADMAP §5: not a social network).
`kith invite` writes the code to stdout and, when running attached to a terminal, copies it
to the system clipboard with an **OSC 52** escape — which needs no dependency and works
over ssh. When stdout is not a terminal the code is printed bare and nothing else, so
`kith invite | wl-copy` and `kith invite > code.txt` behave.

The TUI shows the same code in a modal with a live countdown and the same copy (§5.2).

**No QR, no `kith://` links** (ROADMAP §2). The base32 body is uppercase and in QR's
alphanumeric mode, so a future QR is a rendering decision and not a format change; a link
would mean registering a URI handler, which is a platform surface, and ROADMAP §6 rule 3
forbids new surfaces before v1.0.

### 3.4 Joining, from the invitee's side

`kith join <code> [--path <path>] [--no-wait]`

Step by step, from Ben's terminal:

1. **Identity check.** No Identity → exit 1, `Run 'kith init' first — kith needs a name to
   put on what you add.` Nothing is parsed until then.
2. **Parse the code locally.** Malformed / bad CRC / unknown version → exit 64,
   `That is not a kith invite code (or it arrived damaged — check nothing was cut off).`
   Nothing has touched the Sync Engine.
3. **Check expiry locally**, with a **5-minute grace** for clock skew. Expired → exit 1,
   `This invite expired 3h 12m ago. Ask for a new one — 'kith invite' takes them a second.`
   Still nothing touched.
4. **Check for a repeat.** Already a Member of that `CircleId` → `AlreadyMember`, exit 1.
   Nonce already in `knocks.json` in state `knocked` → print the current wait status (step
   8) and do not knock again.
5. **Show what is being joined, before doing it.**

   ```
   Join walls?
     Circle    walls (kith-7QM4XKC2)
     Steward   Device CVX4-DU36 — kith cannot name the Person yet
     Expires   in 23h 55m
     On disk   ~/.local/share/kith/circles/walls   (--path to change)
   ```

   Confirmed with `y`, or `--yes`. The invitee chooses the path *here*, before any engine
   call, because ADR-0002 §1 puts path choice on the joiner and forbids `autoAcceptFolders`.
6. **`engine.begin_join(&ticket)`.** The admin's Device is registered as this Circle's
   Steward's Device and the knock goes out. Requires the engine (`exit 69` otherwise). This writes
   `knocks.json` first, so a crash between the two leaves a resumable record, never a
   silent knock.
7. **Print the fingerprint** — the invitee's half of the out-of-band check:

   ```
   Asked to join walls.

     Your fingerprint   UJZD-EGXD
     Read it to them. They will see the same four-and-four before they approve.
   ```
8. **Wait**, unless `--no-wait`. kith consumes the change feed (`observe`) and distinguishes
   two states, because it genuinely can (§4.2):

   - No connection to the admin's Device →
     `Their Device isn't reachable yet. Nothing has reached them. kith keeps trying.`
   - Connected →
     `Their Device has your request. They have to approve it in kith — kith cannot tell
     you whether they have looked.`

   Ctrl-C is safe: `Your request stays registered. Run 'kith join <code>' again, or open
   kith, to pick it up.`
9. **On `Change::CircleOffered(offer)`** — `engine.complete_join(&offer, root)`, but only
   if **both** the offer's `CircleId` equals the ticket's *and* the offerer is the ticket's
   Steward's Device. Any other offer is never auto-accepted; it surfaces in the TUI as an
   unrequested Circle offer (§5.3) and is ignored by the CLI. This is the difference
   between accepting an invitation and accepting anything anyone sends you.
10. **Write this Device's Membership claim**, `.kith/members/<my-device-id>.toml`, as soon
    as the root is writable: `schema`, `device`, `person`, `display_name`, `asserted`. This
    is how the rest of the Circle learns that the Device it just admitted speaks for Ben,
    rather than being a Device with no Person attached to it (§3.7).
11. **Mark `knocks.json` `joined`**, print, exit 0.

    ```
    Joined walls. 128 Items on the way.
    Run 'kith' to watch them arrive.
    ```

Bytes and `.kith/` records now flow in. `.kith/circle.toml` arrives with the first sync, so
Ben sees who the Circle's admin is — `founder_person`, resolved to a name through the
Membership claims arriving alongside it — without asking anyone. `.kith/collections/main.toml`
arrives the same way and tells his kith the Collection's name and Provider.

**`--no-wait`** stops after step 7 and exits 0. The knock persists in engine config; the
join completes the next time kith runs — CLI or TUI. This is ADR-0002's accepted
consequence that "kith must be running to complete a join", made explicit instead of hidden
behind a spinner.

### 3.5 Approving or rejecting a pending join

`kith approve [<fingerprint>] [--circle <circle>] [--yes] [--force]`
`kith reject <fingerprint> [--circle <circle>] [--forget]`

#### 3.5.1 What the admin actually sees

`engine.pending_joins()` yields `JoinRequest { device, name, seen_at }`. That is the whole
truth available: a Device Identity, a name **that Device announced about itself**, and when
it was first seen. There is no Person in it. Every surface must be built on that.

```rust
pub struct PendingJoin {
    pub circle: CircleId,
    pub request: JoinRequest,
    pub fingerprint: String,     // "UJZD-EGXD"
    pub solicited: Solicited,
}

pub enum Solicited {
    /// An invite window for this Circle is open.
    ByOpenInvite { issued_at: DateTime<Utc>, expires_at: DateTime<Utc> },
    /// A window existed and has closed. Approvable, with a second confirmation.
    ByClosedInvite { closed_at: DateTime<Utc>, reason: WindowClose },
    /// No window has ever been opened for this Circle, or `invites.json` was lost.
    Unsolicited,
}

pub enum WindowClose { Expired, Spent, Superseded }
```

A pending Device is offered against **every** Circle in which this Person is the Steward,
because the engine's pending list is Circle-agnostic; `--circle` disambiguates and is
required when this Person stewards more than one.

#### 3.5.2 Approving

1. Resolve the Circle (§5.1) and require that this Person is `.kith/circle.toml`'s
   `founder_person` **and** that this Device is its `founder_device`. Otherwise `NotAdmin`,
   exit 1 (§3.9.3).
2. `Stewardship::Vacant`, `Disputed` or `Unknown` → `Stewardless`, `DisputedStewardship` or
   `NoCircleRecord`, exit 1. Nothing is admitted into a Circle whose admin has left, is
   contested, or has never been named.
3. Show the prompt (§5.3) and require confirmation. `--yes` skips it only for
   `ByOpenInvite`; `ByClosedInvite` and `Unsolicited` additionally require `--force`, and in
   the TUI a second, differently-worded confirmation. Friction here is the feature.
4. **`engine.admit(&circle, &request)`.** The Device joins the Circle's Device set and is
   never designated a second Steward's Device (ADR-0002 §3's never-mutual rule).
5. Mark the invite window `spent`, recording the admitted `DeviceId` in `spent_by`. Single
   use, per the glossary.
6. **Write nothing to the synced tree.** A Member's Role is derived and never stored
   (§2.2), so there is no shared record of who is who to append to; the joining Device
   writes its own Membership claim into a path no other Device writes. Approval therefore
   cannot conflict with anything — admitting someone changes the engine's Device set and
   nothing kith owns.

```
$ kith approve
A Device wants to join walls.

  Device name    ben-thinkpad     (announced by that Device — it can say anything)
  Fingerprint    UJZD-EGXD
  First seen     12 seconds ago
  Invite         open, issued 4 minutes ago, expires in 23h 55m

kith cannot tell you who this is. It sees a Device, not a Person. Ask your friend to
read you the fingerprint their kith printed, and approve only if it matches.

Approve? [y/N] y
Admitted UJZD-EGXD to walls. The invite is used up; 'kith invite' issues another.
```

**Fingerprint honesty.** Eight base32 characters are 40 bits. That is enough to tell two
Devices apart in a pending list and to catch a transcription error against a known-good
source; it is not enough to resist someone deliberately grinding a matching prefix. The
prompt's `[?]` expands to exactly that sentence plus
`Compare the full 52-character Identity with 'kith status --device' if it matters.`

#### 3.5.3 Rejecting

Rejection is **local and needs no seam method**. `Circles::reject` records the `DeviceId` in
`dismissed.json`, and kith filters it out of every pending surface. It is deliberately not
`engine.expel` (that removes an admitted Device) and deliberately not a dismissal at the
engine, which the Device would undo by dialling again — a local ignore-list is strictly
more durable, and it keeps the seam at ADR-0002's 17 methods instead of buying an
eighteenth for state that never leaves this Device.

```
$ kith reject UJZD-EGXD
Hidden. UJZD-EGXD keeps trying to reach your Device — kith stops showing it.
It is not told anything: there is no server to deliver a "no". If someone is waiting
on you, tell them yourself.
'kith reject --forget UJZD-EGXD' un-hides it.
```

`kith status` reports `1 hidden knock` so a rejection never becomes an invisible one.

### 3.6 Propagation: who learns what, and when

After the admin approves Ben, in order:

| # | What | When | If the Device is not connected |
|---|---|---|---|
| 1 | Ben's Device is in the Circle's Device set on the admin's Device | immediately, locally | — |
| 2 | Ben's kith receives the Circle offer | seconds, **while Ben's kith is running** | Queues; delivered next time Ben runs kith (§3.4 step 9) |
| 3 | `complete_join` places the Circle; content and `.kith/` records begin flowing both ways | seconds to hours, by size | — |
| 4 | Ben's Device's Membership claim, `.kith/members/<ben-device>.toml`, reaches everyone connected | one sync cycle | Arrives when each Device next connects to anyone who has it |
| 5 | Every **other** Member learns Ben's Device from the Steward's Device | the next time each of them connects to the Steward's Device | Until then Cara and Ben do not connect **directly** |

Step 4 carries the whole of "Ben is a Person": one file, written once by one Device, that
either has arrived or has not. There is no second writer to wait for and no existing record
to be merged into, which is why a join adds a Member without any Device rewriting anything
it already had.

Step 5 is the concrete cost of one Steward (ADR-0002 §3), and it is visible: until Cara has
learned Ben, everything Cara adds reaches Ben only while the admin's Device is connected to
relay it. `kith status` states this rather than leaving it as mysterious slowness:

```
walls · 3 Members, 2 connected
  Cara has not learned Ben's Device yet — that happens next time her Device
  and the admin's are connected to each other.
```

Between steps 1 and 4 the admin's Members screen shows Ben as an **unclaimed Device**
(§3.7), which is also exactly how an adopted wp-sync peer (ADR-0002 §7) appears — a peer
that syncs perfectly and has no Membership claim, possibly forever. One state, one
rendering, no special case.

### 3.7 Listing Members, and the honesty of presence

`kith list members [--circle <circle>]` and the TUI Members screen read the same
`Circles::roster`, which is assembled from three sources and nothing else:

| Source | Contributes |
|---|---|
| `.kith/members/*.toml` — every Membership claim in the synced tree | one claim per Device: the Device Identity in its filename, and `person`, `display_name`, `asserted`, `left_at` inside it. Grouping claims by `person` is what turns a pile of Devices into Members (§2.2) |
| `.kith/circle.toml` | `founder_person` — the one `admin`; every other Person is a `member`. `founder_device` names the Steward's Device |
| `engine.devices(&circle)` + `engine.status(&circle)` | which Devices are actually in the Circle, `connected`, per-peer completion |

Cross-referencing produces exactly four rows, all of which must render:

1. **Member with Devices in the Circle** — the normal row: one or more claims carrying their
   `person`, each naming a Device the engine confirms is in the Circle.
2. **Unclaimed Device** — in the Circle, with no `.kith/members/<its-id>.toml`. Rendered by
   fingerprint, labelled `no Membership claim yet`. Never hidden: a Device receiving the
   Circle's bytes must appear in the Members screen even when kith cannot name it.
3. **Member with no Device in the Circle** — claims present, every Device they name absent
   from the engine's Device set. Either they left (`left_at` on all of their claims →
   `left · 3 Aug`) or they were never admitted / were removed (no `left_at` →
   `not in this Circle`).
4. **Self** — marked `(you)`; presence is `—`, not `connected`. kith does not have a
   connection to itself and will not pretend to.

**Naming the admin when no claim names them.** `founder_person` is a `PersonId`, and a
`PersonId` alone is not a Member — no claim, no row (§2.2). It happens: an adopted Circle
whose descriptor arrived before the founder's claim did (§3.1), or a founder's claim that
has simply not synced yet. The admin badge then reads **`unknown Person (p-01k9r7)`** — the
id's short form, prefix included, exactly as it would render on an Item nobody can attribute
— and never the `founder_device` Identity in its place. A Device is not a Person, and
printing one where the other belongs is the single confusion this spec exists to prevent.

#### 3.7.1 What kith can know about presence, and what it must not claim

`PeerDevice { device, name, connected }` is the entire input. `connected` means one thing:
**this Device holds an open connection to that Device, right now.**

| kith may say | kith must never say |
|---|---|
| `connected` — an open connection exists from this Device | "online", "available", "active" |
| `not connected` — no open connection from this Device | "offline" as a fact about the Person or their Device |
| `last connected 2h ago` — the last `PeerOffline` this Device *observed* | "last seen 2h ago" — kith did not see them, it saw a socket |
| `100%` — that Device holds every byte this Device knows about | "they have seen it", "they got your wallpaper" |
| `—` when the Sync Engine is unreachable | anything at all. Unknown is a value. |

Three specific prohibitions, each with a real failure behind it:

- **Presence is pairwise, not global.** Ben may be connected to Cara and not to this Device.
  The column is this Device's own view and the footer says so. There is no Circle-wide
  presence to report and kith will not synthesise one.
- **Connected ≠ at the computer.** A Device syncs while its Person is asleep. The word
  "online" implies a human; `connected` implies a socket, which is what kith has.
- **Completion ≠ attention.** 100% means bytes landed. Whether anyone looked is unknowable
  and would be surveillance if it were not — Favourites are private for the same reason
  (CONTEXT), and kith has no read receipts by construction.

`last_connected` is derived from `PeerOffline` events cached in SQLite. It is a
convenience, it is lost when the cache is rebuilt, and it renders as `unknown` rather than
`never` when missing — an empty cache must not be reported as a cold friendship.

Presence is computed at read time and never stored in a synced record. v0.1 has one Device
per Person, so Member presence and Device presence coincide; the rollup rule is already
`Connected` if *any* of the Person's Devices is connected, so v0.3's second Device needs no
change here.

```
$ kith list members --circle walls
MEMBER           ROLE    PRESENCE        IN SYNC  ASSERTED
Ana (you)        admin   —               —        6 Aug
Ben              member  connected       100%     7 Aug
Cara             member  not connected   62%      6 Aug   (last connected 2h ago)
·                        UJZD-EGXD — a Device with no Membership claim yet

"Presence" is this Device's view of right now: an open connection between this Device
and theirs. It does not mean they are at their computer, and someone shown as not
connected may be connected to another Member.

"Asserted" is when a Device of theirs last wrote its Membership claim.
```

**Why the column is `ASSERTED` and not `JOINED`.** The claim carries one timestamp,
`asserted`, and it means *when this Device last said it was here* (§2.2). In v0.1 a claim is
written at join and rewritten only on leave, so for a present Member the two dates coincide
— but they are not the same fact, and a column headed `JOINED` would be kith asserting a
join date it does not record. The same rule that forbids "last seen" forbids this.

### 3.8 Roles in v0.1

Two Roles, one Circle-wide invariant: **exactly one `Admin` — that Member is the Circle's
Steward, and their Device is its only route in or out.** The invariant holds by
construction rather than by check: the `Admin` is whoever `.kith/circle.toml` names as
`founder_person`, that record is written once, and nothing in v0.1 can name a second
(§2.2).

| | `Admin` | `Member` |
|---|---|---|
| Issue an Invite | yes | no — kith refuses |
| Approve / reject a pending join | yes | no — kith refuses |
| Add, delete, favourite, apply Items | yes | yes |
| Leave the Circle | yes (§3.9.3) | yes |
| Rename / delete the Circle, edit Roles, remove a Member | **nobody, in v0.1** | |

That is the complete list. Role editing, removal and succession are all out (ROADMAP §2:
"No kick, no role editing"; `kith adopt-steward`, ADR-0002 §3's succession verb, is v0.2).

#### 3.8.1 The line, stated where the Person is

kith is allowed to describe as enforcement only the one gate that runs on the gatekeeper's
own Device. Concretely, in this spec's surface:

| Promise | Real? | Because |
|---|---|---|
| Only People the admin approved are in this Circle | **yes** | Admission runs on the admin's Device and is the only route in |
| An invite stops being expected after it expires | **yes** | The window is checked on the admin's Device at approval time |
| Only the admin can invite | **no** | kith refuses on a non-admin Device; another program on that Device can hand out its own pointer. See §3.8.2 |
| Members cannot delete or overwrite each other's Items | **no** | Any Device can write to content it holds. Recoverable, not preventable |
| The admin can take someone's copies back | **no, and never** | §3.10 |

#### 3.8.2 The caveat nobody likes: the Circle is as tight as its least careful Member

A Member who is not the admin can, from their own Device, share the Circle with an outsider.
kith refuses to do it — but nothing outside kith is stopped, and because only the Steward's
Device propagates its Device list, **that outsider would sync with that one Member and might
never appear on the admin's Members screen at all.**

This is stated in the Roles help text, not buried here. It is the honest reading of
ADR-0002 §4's "everything after admission is convention" applied to the Circle boundary
itself, and it is why kith's answer to trust is "invite people you trust" rather than a
permission model.

#### 3.8.3 Exact UI copy

Named strings, used verbatim in both the TUI and the CLI. These are the copy this spec
requires; wording changes are spec changes.

`roles.footer` — always visible on the Members screen, two lines, never scrolled away:

```
Roles are an agreement, not a lock. Any Member can add or delete anything here;
every other Device keeps 30 days of previous versions if they do.        [?]
```

`roles.long` — behind `[?]`, and printed by `kith list members --explain`:

```
kith has no server, so it has nothing that can enforce a rule on someone else's
computer. A Role says what a well-behaved kith will do and what this Circle has
agreed to.

What is real:
  · Nobody joins without the admin approving them, on the admin's own Device.
  · Every Device keeps the last 5 versions of anything another Member changed or
    deleted, for 30 days.

What is not:
  · Nothing stops a Member from deleting or replacing Items. It can be put back
    from another Member's copy; it cannot be prevented.
  · Nothing stops a Member from sharing this Circle onward from their own Device.
    kith refuses to do it. A different program would not.
  · Nothing takes back bytes that already arrived somewhere. Nothing ever will.

Invite people you trust. That is the whole security model, stated plainly.
```

`roles.not_admin` — when a Member runs `kith invite` or `kith approve`:

```
Only walls's admin (Ana) can invite people or approve joins. Ask her to run
'kith invite'. kith refuses this on your Device — it cannot stop other software
on your Device from doing something similar, and it will not pretend otherwise.
```

When `NotAdmin.admin_name` is `None` — the admin's `PersonId` is known from
`founder_person`, but no Membership claim has named them yet (§3.7) — the first clause
becomes `Only walls's admin (unknown Person p-01k9r7) can invite people or approve joins`
and the second sentence drops, because kith cannot tell you who to ask. It never fills the
gap with the Device Identity from `founder_device`.

`roles.badge` — next to the admin on the Members screen: `admin — invites and approves`.
Nothing more expansive, because there is nothing more.

**Note on the 30-day claim.** The versions are configured in v0.1 by the ADR-0002 §2 recipe,
so the copy is true. The *restore* command is v0.3 (ROADMAP §2, History). The copy therefore
says "keeps versions", never "you can restore" — and `kith doctor` prints the archive's
path so the answer to "so where are they" is one command, not a v0.3 promise.

### 3.9 Leaving a Circle

**Leave is a TUI action, not a CLI verb, in v0.1.** ROADMAP §2 puts "Leave a Circle" in the
Members module and does *not* list `leave` in the v0.1 CLI surface; the ceiling is the
ceiling. Members screen → `L`. `kith leave` arrives in v0.2 next to `kith remove`.

#### 3.9.1 The confirmation

```
Leave walls?

Your kith stops syncing this Circle. The 128 Items already here stay exactly where
they are (~/.local/share/kith/circles/walls) — kith deletes nothing.

The wallpapers you added stay with the Circle. Leaving does not take back what you
contributed, and nothing can.

The others will see you as "left" once their Devices notice. You cannot come back
without a new invite.

  Type the Circle's name to confirm:  ▁
```

Typing the name — not `y` — because this is the one irreversible social act v0.1 has.

#### 3.9.2 What happens

1. **Write `left_at` into this Device's own Membership claim**,
   `.kith/members/<my-device-id>.toml` — a read-modify-write of the existing file under
   ADR-0004 §3's descriptor protocol, refreshing `asserted` in the same write. Descriptors
   are rewritable; it is *record logs* that are append-only, and a claim is a descriptor.
   A tombstone in a file only this Device ever writes, never a deletion: a delete-then-
   recreate is exactly the pattern that produces conflict copies, and the claim must survive
   to say "left" rather than vanish into "was never here" — it is also what keeps this
   Person's name on the Items they added (ADR-0004 §5). By §2.2 a Member has left when *all*
   of their claims carry `left_at`; v0.1 has one claim per Person, so leaving is one write
   on one Device and nothing to reconcile.
2. **Give the tombstone a chance to travel.** kith waits up to 10 seconds for the change
   feed to report the Circle idle with at least one connected peer, then proceeds either
   way. Honest, and stated: if nobody is connected, the tombstone leaves with the Person and
   the others see a Member whose Device simply never connects again — indistinguishable from
   a Device that is merely not connected (§4.6).
3. **`engine.leave(&circle)`.** Replication stops. Local bytes are kept; ADR-0002 §1 is
   explicit that nothing is deleted here, and the confirmation already promised it.
4. **Clear this Device's designation of the admin's Device — conditionally.** The
   designation is Device-scoped, not Circle-scoped (ADR-0002 §3's documented leak), so
   `engine.set_introducer(&admin_device, false)` runs **only if no other Circle this Device
   holds names that same Person as its Steward**. Otherwise it is left alone and
   `LeaveReport` says which Circle kept it.
5. Drop this Circle's rows from `invites.json`, `knocks.json`, `dismissed.json`. Keep the
   root on disk untouched.

```rust
pub struct LeaveReport {
    pub circle: CircleId,
    pub items_kept: u64,
    pub root: PathBuf,
    pub tombstone_propagated: bool,        // best effort; false is normal and reported
    pub steward_flag_kept_for: Option<CircleId>,
}
```

#### 3.9.3 The admin leaves

Allowed, with a different confirmation, because refusing would be theatre — a Person can
uninstall kith and the Circle is in the same state, minus the warning.

```
You are walls's admin. If you leave:

  · Nobody can be invited or approved into walls again. v0.1 has no way to hand
    the admin role to someone else — that lands in v0.2.
  · Everyone already in keeps syncing with everyone else, indefinitely. Content
    is not affected in any way.

  Type the Circle's name to confirm:  ▁
```

Afterwards the Circle is `Stewardship::Vacant` on every Member's Device once the tombstone
arrives, and every surface says so:

- Members screen header: `walls · no admin — Ana left on 7 Aug. No new Members until v0.2.`
- `kith invite` on that Circle: `SocialError::Stewardless`, exit 1, same sentence.
- `kith status`: `walls  no admin (Ana left 7 Aug) · syncing normally`.

**kith cannot distinguish a departed admin from a dead one.** An admin whose Device is
destroyed leaves no tombstone; their Circle shows an admin who is permanently `not
connected`, and `kith status` says exactly that and nothing more. Both states are terminal
for membership changes in v0.1 and harmless for content. This is the honest cost of one
Steward and no succession verb, accepted in ADR-0002 §3 and paid here.

### 3.10 Member removal — v0.2, and what it will not do

**Not in v0.1.** ROADMAP §2: "No kick." It is specified here because the shape of removal
constrains what §3.9's copy is allowed to promise today.

When it lands (v0.2, `kith remove <member>` and Members screen `x`, admin only), it will be
`engine.expel(&circle, &device)` per Device: the Device is dropped from the Circle's Device
set on the admin's Device, and the removal cascades to every Member that learned of that
Device from the Steward's Device, **as each of them next connects to it** (ADR-0002 §3). The
admin also records the removal **in their own Device's Membership claim** — the same
single-writer surface v0.2's `[grants]` table uses (ADR-0004 §5) — so Members screens can
say "removed" rather than "not in this Circle". Nobody writes into the removed Device's
claim: that file has exactly one writer and it is not the admin. There is no shared record
of who is who to append a tombstone to, and v0.2 does not introduce one.

What removal will **not** do — the list that has to be on screen before anyone presses the
key:

- **It cannot claw back bytes.** Every Item already on that Device stays there, readable,
  forever. There is no remote delete, no kill switch, no expiring copy. kith will never
  offer one, and any product that offers one over a peer-to-peer transport is lying.
- **It is not instant.** It lands per Member, on their next connection to the admin's
  Device. Until then the removed Device may still sync with Members that have not caught up.
- **The cascade only reaches what the Steward's Device taught.** A Device another Member added by
  hand is not reached by it — §3.8.2's least-careful-Member caveat, in its removal form.
- **It does not remove their contributions.** Items they added stay in the Collection.
  Removing a Person and deleting their content are different acts, and conflating them
  would delete content the rest of the Circle now depends on.
- **It is not a punishment kith can deliver.** The removed Person is told nothing, for the
  same reason a rejected knock is told nothing: there is no server to deliver the message.

What protects the rest is not removal but **versioning**. If a Member goes destructive
before or after being removed, every other Device holds the last 5 versions, 30 days, of
everything they changed or deleted (ADR-0002 §4); restore (v0.3) puts it back and
re-propagates to the whole Circle. The honest sentence, and the one that belongs in the
removal dialog when it ships:

```
Removing someone is forward-looking. The only thing you can take away is what
happens next.
```

---

## 4. Edge cases & failure honesty

**4.1 An invite is redeemed after it expires.** Three distinct sub-cases, three behaviours.
(a) The invitee runs `kith join` late: refused locally, nothing touched, exit 1 (§3.4 step
3). (b) The invitee's clock is behind, so their kith accepts a code the admin considers
dead: the knock arrives, and the admin's prompt shows `Invite — expired 2h ago` with
`Solicited::ByClosedInvite`, needing `--force`. (c) The invite expires *while the invitee
waits for approval*: the knock is already registered and is not retracted; the admin sees
`expired 10 minutes ago` and decides. **Expiry bounds when knocks are expected, never when
they are possible** — stated in `kith invite`'s own copy so it is not a surprise later.

**4.2 The admin's Device is not connected during a join.** `begin_join` succeeds locally: the
knock is engine config on the invitee's Device, and it persists across restarts. The pending
entry only materialises on the admin's Device when the two actually connect. kith
distinguishes the two waits because `engine.devices()` tells it whether *this* Device holds a
connection to the Steward's Device (§3.4 step 8) — and stops there: a connection proves the
request arrived, never that a human looked at it. If the invitee gives up, nothing is lost;
re-running `kith join <code>` resumes from `knocks.json`, and an already-registered knock is
idempotent.

**4.3 Two People redeem the same code.** Expected, not an error. A code carries no invitee
binding — it cannot, since a knock carries only a Device Identity. Both knocks appear as
separate pending joins with **different fingerprints**, and that is the whole resolution:
the admin approves the one whose fingerprint matches what their friend read aloud, and
rejects the other. The first approval marks the window `spent`, so the second knock is
`ByClosedInvite` and needs `--force` — deliberate friction against approving the leftover
out of habit. If both are genuinely wanted, `kith invite --new` and approve the second under
a fresh window.

**4.4 The admin leaves.** §3.9.3. Circle becomes `Vacant`: content sync is untouched
forever, membership is frozen until v0.2's succession. Every surface names it rather than
failing mysteriously.

**4.5 The admin leaves while a join is pending.** The knock is stranded: `Vacant` blocks
approval (§3.5.2 step 2) and there is no other Device that can admit. The invitee sees the
connected-but-waiting message indefinitely, because from their side the two situations are
identical. kith says so: after 24 hours of `knocked`, `kith join`'s wait line becomes
`Still waiting. kith cannot tell whether they have not looked, said no, or stopped using
kith. Ask them.`

**4.6 A Member leaves while disconnected from everyone.** The `left_at` never leaves their
own Device's Membership claim, because that claim is the only place it is ever written. On
every other Device that Person stays a Member whose Device never connects — indistinguishable
from a Device that is simply never connected. There is no fix without a server; `kith status`
reports `not connected` and a `last connected` date, and the humans work it out. Documented,
not papered over.

**4.7 The Sync Engine is unreachable.** `create`, `join`, `approve` all refuse with exit 69
and change nothing — each writes engine config, and kith does not queue engine writes.
`kith invite` **works**: it needs this Device's own Identity, which is cached, and issuing a
code is a local act. It appends
`Your Device cannot be reached right now — they will not get in until it is.`
`kith list members` renders every presence as `—` — `Presence::Unknown`, the value that
exists precisely for this — under a banner: `Sync Engine unreachable — kith cannot see
anyone right now.` Never `not connected`, which would be a claim.

**4.8 Two People redeem invites to two different Circles from the same admin.** Both knocks
land in one Circle-agnostic pending list. `--circle` is mandatory for `approve`/`reject`
whenever this Person is the Steward of more than one Circle, and the TUI prompt names the
Circle in its title. Admitting a Device to the wrong Circle is a real and irreversible
mistake, so kith never guesses.

**4.9 A single-writer record has conflict copies.** By §2.2 this cannot happen legitimately,
so kith never merges one — but what it *means* now differs sharply by record.

**`.kith/circle.toml` in conflict** is the only way a Circle can disagree about who its admin
is, and having no separate record of Roles is what keeps it rare: exactly one record names
the admin, it is written once, and no Steward rewrites it afterwards. So a conflict copy here
is never two People contesting a shared list of Members — it is one write-once record written
twice, from a restored backup, a cloned home directory, or two Devices both claiming an
adopted Circle (collections.md §4.3). If the copies agree on `founder_person`, kith keeps the
copy with the earliest `created` per ADR-0004 §8 and `doctor` reports it. If they disagree →
`Stewardship::Disputed { claimants }`; `invite` and `approve` refuse with
`SocialError::DisputedStewardship`; the Members screen shows `walls · this Circle disagrees
about who its admin is` and lists the claimants by name, or by `unknown Person (p-…)` where
no claim names one. Resolution is v0.2's job. Refusing to guess is v0.1's.

**A missing `founder_device`, or one the engine contradicts, is a warning and not a
dispute.** `founder_device` is the source of truth for which Device stewards the Circle; the
engine's own flag on a peer is a *cross-check only* and cannot stand in for it, because the
Circle's Steward Device flags no peer at all — so on that very Device nothing carries the
flag, `devices()` never returns self, and the flag is invisible from exactly the vantage
point that matters most (ADR-0002 §1, §3). A mismatch between the two is therefore a `kith doctor`
warning naming both Devices, never a reason to refuse an operation and never a reason to
believe the flag over the record.

**A conflict copy of a Membership claim is near-impossible, and is never a dispute.** The
filename names a Device and only that Device writes it (§2.2), so two writers of one claim
are not two People disagreeing about a shared record — they are two machines asserting the
same Device identity. What can still produce one: an install restored from backup, a cloned
home directory or a copied VM, and a Person hand-editing `.kith/`. kith reads the claim with
the newest `asserted`, renders the Member row normally, and reports the fault as what it is —
`a Device identity is in use on more than one machine` — rather than as a `disputed` Member.
The owning Device re-asserts its claim and deletes the copy (ADR-0004 §8); no other Device
touches it. Nothing is blocked meanwhile: a claim is not a gate, admission runs on the
Steward's Device (§3.8.1), and presence and sync percentages come from the engine either
way. Two claims naming *different* Devices with the same `person` are not a conflict at
all — that is a Person with two Devices, which is v0.3 arriving early and handled by §2.2's
derivations.

What single-writer does **not** buy, and no surface may imply it does: any admitted Device
can write a claim for a Device that is not it, carrying any `person` it likes. That
produces no conflict copy and kith cannot detect it. Device-keying removes the accident, not
the forger — claims are convention, not cryptography (ADR-0004 §5) — and §3.8.2 is the
honest reading either way.

**4.10 A Circle root with no `.kith/circle.toml`.** Three causes: a join whose first sync has
not delivered it, a root a Person emptied, or an adopted wp-sync Circle whose descriptor-
writing Device has not run kith yet (§3.1). `Stewardship::Unknown`. It is listed as `walls ·
waiting for the Circle's records`, membership operations refuse with
`SocialError::NoCircleRecord`, and content syncs throughout — the Gallery does not wait on a
descriptor. kith never invents the record from the engine's label, which would silently mint
a second source of truth for the Circle's name, and it never promotes a Device Identity into
the empty `founder_person` slot.

**4.11 Two Circles with the same name.** Legal — names are display strings. `--circle`
resolves by exact name, then by unique `CircleId` prefix; ambiguity is
`AmbiguousCircle`, exit 1, listing both with ids and roots. The TUI switcher shows the id
next to any name it has to print twice.

**4.12 An invite code pasted with mangled case, wrapped lines, or confusable characters.**
Normalisation handles whitespace, hyphens, case, and `0`/`1`/`8` (§3.2.2). Anything else
fails on the CRC and reports `arrived damaged — check nothing was cut off` rather than
`invalid`, because a truncating chat client is the overwhelmingly likely cause and the fix
is to re-paste, not to re-issue.

**4.13 Clock skew, generally.** kith trusts its own clock and no other. Expiry is absolute
Unix seconds; the invitee applies a 5-minute grace; the admin renders remaining time from
their own clock. A wildly wrong clock on either side degrades to §4.1(b) — a confirmable
prompt with an honest label. There is no time authority in a serverless product and kith
does not simulate one.

---

## 5. CLI & TUI surface touchpoints

`docs/spec/cli-tui.md` (#16) owns the global grammar, argument conventions and keymap. This
section owns the semantics of these verbs and screens, and the copy in §3 is normative.

### 5.1 CLI

| Invocation | Does |
|---|---|
| `kith create <name> [--path <path>] [--adopt [<dir>]]` | §3.1, including the adoption branch |
| `kith invite [--circle <c>] [--expires <dur>] [--new] [--address <url>]` | §3.2; prints the code, OSC 52 to clipboard when attached to a terminal |
| `kith join <code> [--path <path>] [--yes] [--no-wait]` | §3.4 |
| `kith approve [<fingerprint>] [--circle <c>] [--yes] [--force]` | §3.5.2; no fingerprint + exactly one pending join → prompt for that one; more than one → error listing fingerprints |
| `kith reject <fingerprint> [--circle <c>] [--forget]` | §3.5.3 |
| `kith list circles` | id, name, Members, connected count, Items, Role, root |
| `kith list members [--circle <c>] [--explain]` | §3.7; `--explain` prints `roles.long` |
| `kith status [--circle <c>]` | Per Circle: Members, connected count, invite window, pending joins, hidden knocks, stewardship state |

**Presence in output.** Human-facing counts read `3 Members, 2 connected` — never "2
online", and never a count of Members the word "online" would imply kith can see. JSON emits
`"presence": "connected" | "not_connected" | "unknown"` per Member, and `"steward": true` on
the one Member the Circle stewards. No surface above the seam prints the transport's word
for that Device; the word here is Steward.

**Circle resolution**, everywhere `--circle` appears: exact name → unique `CircleId` prefix
→ if the Person has exactly one Circle, omitting it is allowed → otherwise
`AmbiguousCircle`. `approve` and `reject` additionally require `--circle` whenever this
Person is the Steward of more than one Circle (§4.8), one Circle or not.

**Exit codes**, aligned with ADR-0003 §2 so the whole binary speaks one dialect:

| Code | Meaning here |
|---|---|
| 0 | Done |
| 1 | Refused: expired invite, not admin, already a Member, stewardless, disputed stewardship, no Circle record, ambiguous |
| 64 | The invite code is not a kith invite code (prefix, CRC, version, truncation) |
| 69 | Sync Engine unreachable, unauthorised, or below the version floor |

```
$ kith status
walls · kith-7QM4XKC2 · 128 Items · Steward: you (admin)
  Sync Engine   connected · idle
  Members       3 Members, 1 connected
                Ben   connected      100%
                Cara  not connected   62%   (last connected 2h ago)
  Invite        open, expires in 19h 12m
  Waiting       1 Device wants to join — 'kith approve'
                1 hidden knock — 'kith reject --forget UJZD-EGXD' to un-hide
```

### 5.2 TUI — Members screen

Reached with `m`; one of the five surfaces ROADMAP §2 allows.

```
┌ walls ────────────────────────────────── 3 Members, 1 connected ─┐
│ MEMBER          ROLE    PRESENCE      IN SYNC  ASSERTED          │
│ Ana (you)       admin   —              —        6 Aug            │
│ Ben             member  connected      100%     7 Aug            │
│ Cara            member  not connected   62%     6 Aug            │
│ ·               UJZD-EGXD — a Device with no Membership claim    │
├──────────────────────────────────────────────────────────────────┤
│ Roles are an agreement, not a lock. Any Member can add or delete │
│ anything here; every other Device keeps 30 days of previous      │
│ versions if they do.                                       [?]   │
│ Presence is this Device's own view, right now.                   │
├──────────────────────────────────────────────────────────────────┤
│ [i] invite  [L] leave  [enter] details  [?] what Roles mean      │
└──────────────────────────────────────────────────────────────────┘
```

- The admin row carries the Steward badge; nothing on this screen prints the transport's
  name for that Device (§5.1).
- `roles.footer` and the presence sentence are **always rendered**, never collapsed behind a
  toggle and never scrolled out of view. Honesty that can be dismissed is decoration.
- `[i]` on a non-admin Device is rendered **greyed with its reason** — `invite · only Ana
  can` — following ADR-0003 §3's unavailable-with-reason rule rather than hiding the key.
- `[enter]` opens a detail pane: full 52-character Device Identity, fingerprint, when the
  claim was last asserted, last-connected, per-Device sync percentage. No new screen; a pane
  on this one.
- A header banner appears for `Vacant` (§3.9.3), `Disputed` and `Unknown` (§4.9, §4.10), and
  an unreachable Sync Engine (§4.7).
- A `1 Device wants to join` header line appears when `pending()` is non-empty and opens the
  prompt below.

### 5.3 TUI — the pending-join prompt

One modal component, two modes, matching the two sides of a join. ROADMAP §2 budgets "a
pending-join prompt"; the joiner's completion prompt is the same component and the same
decision ("does this belong to the invite I am expecting?"), so it costs no new surface.

**Mode A — inbound knock (admin side).** Triggered by `Change::JoinRequested` from anywhere
in the TUI, and from the Members header. Renders §3.5.2's block, with the `Invite` line
reflecting `Solicited`:

| `Solicited` | Line | Keys |
|---|---|---|
| `ByOpenInvite` | `open, issued 4 minutes ago, expires in 23h 55m` | `[a]` approve · `[r]` reject · `[esc]` decide later |
| `ByClosedInvite` | `expired 2 days ago` / `already used` / `replaced by a newer one` | `[a]` opens a second confirmation typed in full |
| `Unsolicited` | `none — you have not invited anyone to walls` | as above, and the modal leads with the warning, not the Device |

The modal never auto-dismisses and never times out. `[esc]` is free and the request stays in
the header — a prompt that punishes hesitation trains people to press yes.

**Mode B — Circle offered (joiner side).** Triggered by `Change::CircleOffered`. If the
offer matches a `knocked` row in `knocks.json` — same `CircleId`, same Steward's Device — it
shows `walls is ready. Place it at ~/.local/share/kith/circles/walls?` with a path editor
and completes on confirm. If it matches nothing, it shows
`A Device you have not asked to join anything is offering you a Circle.` with the offerer's
fingerprint, and the default is to dismiss. kith never accepts an unrequested offer, in
either surface (ADR-0002 §1's no-auto-accept rule, made visible).

### 5.4 TUI — Circle switcher

Appears only when the Person has more than one Circle (ROADMAP §2: "a plain Circle
switcher"). `c` opens a plain list; Enter switches the Gallery; Esc closes. One line per
Circle:

```
walls    2 Members, 1 connected · 3 unseen   admin
photos   4 Members, 0 connected                    ⚠ no admin
```

The id is appended only where two Circles print the same name (§4.11). No creation, no
leaving, no settings here — those live where they are specified.

---

## 6. Out of scope for v0.1

Everything below is named so it can be refused by pointing at a line, per ROADMAP §2.

| Not in v0.1 | Where it goes |
|---|---|
| Member removal (`kith remove`, Members `x`), and its removal record | v0.2 — shape fixed in §3.10; it lands in the admin's own Membership claim, because there is no shared record of who is who to write to and v0.2 does not add one |
| Invite revocation as "close the window early" | v0.2 — a convenience, and §3.2.4 is why it is only that |
| Role editing, a third Role, per-Collection Roles | v0.2 — as a `[grants]` table inside the Steward's Device's own Membership claim (§2.2), reserved and unwritten today |
| Steward succession (`kith adopt-steward`, ADR-0002 §3), the `[steward]` table that records it, and any recovery from a dead admin | v0.2 — the table is reserved and unwritten in v0.1; v0.1's Steward is `circle.toml`'s `founder_device` and nothing else |
| `kith leave` as a CLI verb | v0.2, with `kith remove` |
| Circle rename, Circle delete, Circle settings | not scheduled; `.kith/circle.toml` is write-once until one exists |
| Restore of versioned content (`kith restore`) — the copy in §3.8.3 promises versions are *kept*, never that v0.1 restores them | v0.3, with History |
| More than one Collection per Circle; per-Collection membership | v0.3 — `.kith/collections/` holds one descriptor per Collection, so a second Collection is a second file beside `main.toml` and no existing record is touched |
| A second Device per Person; Device management, Device naming, per-Device presence rollup UI | v0.3 — a second Device is a second Membership claim, `Member.devices` is already derived by grouping them (§2.2), and `Presence` already rolls up |
| QR codes, `kith://` links, any invite transport | not scheduled (ROADMAP §5); the base32 body is QR-alphanumeric-safe if that changes |
| Signed invites, invite-to-a-named-Person binding, any home-grown cryptography | not scheduled (ROADMAP §5) |
| Telling a rejected or removed Person anything | never — there is no server to deliver it |
| Read receipts, "seen by", activity-derived presence | never — §3.7.1, and Favourites are private for the same reason |
| Blocking a Device from knocking at the engine level | never — the local ignore-list (§3.5.3) is more durable and adds no eighteenth method to ADR-0002's 17-method seam |
