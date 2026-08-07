# ADR-0002: Sync Engine seam & Syncthing mapping

- **Status:** Accepted
- **Date:** 2026-08-06
- **Resolves:** [#7 ADR: sync-engine seam & Syncthing mapping](https://github.com/opx0/wp-sync/issues/7)
- **Informed by:** `research/syncthing-api`, ADR-0001
- **Amended:** 2026-08-07 — §1 gains `SyncEngine::reserved_paths()` (asked for by ADR-0004
  §2) and an `introducer` flag on `PeerDevice` (asked for by `docs/spec/collections.md`).
  The seam is **17 methods**. Both amendments are marked in place below.

## Context

kith's transport is a separately-running Syncthing daemon driven over REST plus a
long-poll event stream (ADR-0001). Three facts from the research force the shape of this
ADR:

1. **Every domain operation maps to REST, but the REST surface churns.** The config API
   was rebuilt in v1.12, pending endpoints arrived in v1.13, v2.0 changed conflict
   semantics. The seam must be narrow enough that Syncthing version churn — or a wholesale
   engine replacement — is absorbed in one module.
2. **Enforcement is local-only.** Folder types, versioning and ignores are self-imposed by
   each device; the cluster cannot compel compliance. Anything kith says about Roles must
   be built on that honesty.
3. **The introducer mechanism is the only native group primitive.** Device-list
   propagation is automatic, transitive, and cascades removals — a workable circle-admin
   story with documented sharp edges (mutual introducers resurrect removed devices).

This ADR locks the seam interface, the domain→Syncthing mapping, the introducer
discipline, the honest Role story, change observation, daemon lifecycle, and wp-sync
migration. Syncthing vocabulary is legal *below* this line and nowhere above it.

## Decision

### 1. The seam: trait `SyncEngine`

One Rust trait is the churn firewall. Everything above it speaks kith vocabulary;
everything below it may speak Syncthing. Exactly one production implementor in v0.1
(`engine::syncthing::SyncthingEngine`, tokio + reqwest), plus a test double. The word
"Syncthing" appears in that module and this document only.

```rust
/// The transport seam. Makes a Circle's bytes present on every Member Device.
/// No opinions about Collections, Items, Providers.
pub trait SyncEngine: Send + Sync + 'static {
    /// Live change feed. Ends only on unrecoverable engine loss.
    type Changes: Stream<Item = Envelope> + Send + Unpin;

    // ── engine & self ────────────────────────────────────────────────
    /// Reachability + version floor check. Cheap; drives the status bar.
    async fn health(&self) -> Result<EngineHealth, SyncError>;
    /// This Device's engine identity (Syncthing: /rest/system/status .myID).
    async fn local_device(&self) -> Result<DeviceId, SyncError>;
    /// Globs the engine owns inside a Circle root — its own bookkeeping, its temp
    /// files and its conflict copies. Not async: a constant of the implementation.
    /// (Amendment 2026-08-07, ADR-0004 §2.)
    fn reserved_paths(&self) -> &[&'static str];

    // ── circle lifecycle ─────────────────────────────────────────────
    /// Create a Circle: allocate its replicated space, become its introducer.
    async fn create_circle(&self, name: &str, root: &Path) -> Result<CircleRef, SyncError>;
    /// Circles this engine replicates (kith-created or adopted).
    async fn circles(&self) -> Result<Vec<CircleRef>, SyncError>;
    /// Joiner, phase 1: consume an Invite — register the introducer and knock.
    async fn begin_join(&self, invite: &InviteTicket) -> Result<(), SyncError>;
    /// Joiner, phase 2: the Circle was offered back; place it at `root`.
    async fn complete_join(&self, offer: &CircleOffer, root: &Path) -> Result<CircleRef, SyncError>;
    /// Introducer: Devices currently knocking (Syncthing: /rest/cluster/pending/devices).
    async fn pending_joins(&self) -> Result<Vec<JoinRequest>, SyncError>;
    /// Introducer: admit a knocking Device into a Circle. Deliberate, never automatic.
    async fn admit(&self, circle: &CircleId, request: &JoinRequest) -> Result<(), SyncError>;
    /// Introducer: remove a Device; de-introduction propagates the removal.
    async fn expel(&self, circle: &CircleId, device: &DeviceId) -> Result<(), SyncError>;
    /// Leave a Circle: stop replicating. Local bytes are kept, never deleted here.
    async fn leave(&self, circle: &CircleId) -> Result<(), SyncError>;
    /// Flag/unflag a peer Device as this Device's introducer. Succession: v0.2 (§3).
    async fn set_introducer(&self, device: &DeviceId, flag: bool) -> Result<(), SyncError>;

    // ── inspection ───────────────────────────────────────────────────
    /// Peer Devices sharing a Circle, with connection state.
    async fn devices(&self, circle: &CircleId) -> Result<Vec<PeerDevice>, SyncError>;
    /// Local sync state + per-peer completion for one Circle.
    async fn status(&self, circle: &CircleId) -> Result<CircleStatus, SyncError>;

    // ── change feed ──────────────────────────────────────────────────
    /// Subscribe from `resume` (None = now). Gaps surface as Change::Desynced.
    async fn observe(&self, resume: Option<Cursor>) -> Result<Self::Changes, SyncError>;

    // ── damage control ───────────────────────────────────────────────
    /// Archived versions the engine holds for one path (Syncthing: /rest/folder/versions).
    async fn versions(&self, circle: &CircleId, path: &RelPath) -> Result<Vec<Version>, SyncError>;
    /// Restore one archived version — the "a Member deleted everything" recovery path.
    async fn restore(&self, circle: &CircleId, path: &RelPath, version: &Version) -> Result<(), SyncError>;
}
```

`reserved_paths` is the only method that returns names instead of doing work, and it
exists so that engine artefact *names* never climb above the seam. `.stfolder`,
`.stversions/**`, `.stignore`, the daemon's temp-file patterns and the conflict-copy
pattern are Syncthing spellings; the layers that must skip those paths — the Gallery, the
Item scanner, the hasher — must never contain one. The implementation answers with its own
globs and the core only ever asks. `engine::syncthing`'s answer is fixed in ADR-0004 §2.

Supporting types, all defined beside the trait, none Syncthing-shaped: `CircleId` and
`DeviceId` (opaque strings), `CircleRef { id, name, root }` (root: where the Circle's
bytes live, so the core reads Items and their record logs straight off disk), `PeerDevice {
device, name, connected, introducer }` (`introducer`: this peer is flagged as *this*
Device's introducer in local config — a local fact, and **a cross-check only**. It cannot be
the source of truth for who stewards the Circle: §3's own rule is that the introducer flags
nobody, so on the Steward's own Device no peer carries the flag, and `devices()` never
returns self — the flag therefore fails to name that Device from precisely the vantage point
that matters most. The Circle's Steward Device is read from `circle.toml`'s `founder_device`
(ADR-0004 §5); a peer flagged here that disagrees with it, or a Circle whose
`founder_device` is flagged nowhere, is a `kith doctor` warning and never a silent
correction. Amendment 2026-08-07, `docs/spec/collections.md`),
`CircleStatus { state, items, bytes_needed, peers: Vec<PeerCompletion>
}`, `JoinRequest { device, name, seen_at }`, `CircleOffer { circle, from, label }`,
`InviteTicket` (§2), `Cursor` (opaque replay token; Syncthing: last event id), `Envelope {
cursor, change }`, `Version { archived_at }`, `EngineHealth { reachable, version }`.

Errors are one closed enum — the upper layers match on category, never on engine text:

```rust
pub enum SyncError {
    Unreachable,            // daemon absent or refusing connections
    Unauthorized,           // credentials rejected — never auto-repaired (§6)
    Incompatible(String),   // engine below the supported floor
    NotFound,               // circle/device unknown to the engine
    Engine(String),         // everything else, opaque upward
}
```

Every method exists because a domain operation demands it; nothing else made the cut:

| Domain operation | Method(s) |
|---|---|
| Create a Circle | `create_circle` |
| Consume an Invite / join | `begin_join` → `complete_join` |
| Issue-side admission | `pending_joins`, `admit` |
| Remove a Member's Device | `expel` |
| Leave a Circle | `leave` |
| List Members' Devices, presence | `devices` |
| Report sync status | `status`, `health` |
| Observe changes (Activity, gallery refresh) | `observe` |
| Recover from destructive edits | `versions`, `restore` |
| Point this Device at its introducer; succession (v0.2) | `set_introducer` |
| Attribute this Device | `local_device` |
| Keep engine artefacts out of the Gallery | `reserved_paths` |

Joining is genuinely two-phase and two-sided; the trait exposes that rather than
pretending it is synchronous. `begin_join` knocks; the offer comes back through `observe`
as `Change::CircleOffered`; `complete_join` places the Circle at a root the Person chose.
kith never uses Syncthing's `autoAcceptFolders` — acceptance is explicit so the joiner
controls the path and no global default is touched (§6).

### 2. The mapping

| Domain concept | Syncthing counterpart | Notes |
|---|---|---|
| Person | **none — deliberately** | Asserted in synced Membership claims above the seam (format: ADR-0004 §5). The engine knows only devices. |
| Device | device (certificate-derived 64-char device ID) | |
| Identity | the device's TLS certificate + key | Owned by the daemon. kith never reads, copies, or backs it up. |
| Circle | folder | ID `kith-` + 8 random base32 chars, immutable, never derived from the name. Label = Circle name, mutable. Adopted installs keep their existing ID (§7). |
| Member | the set of a Person's device IDs present in the folder's `devices` array | The Person↔Device grouping lives in the Membership claims — one per Device, each naming its Person — not in Syncthing. |
| Role | **none** — a policy record in the synced tree | See §4. |
| Invite | **none** — a kith `InviteTicket` + the pending-device handshake | Ticket: `{circle, circle_name, introducer: DeviceId, address_hints, issued, expires, nonce}`, serialized to a compact paste/QR string, carried out-of-band. Consumption = `begin_join` → introducer sees `JoinRequested` → `admit`. Expiry and revocation are checked by the admitting Device at `admit` time — enforceable, because admission is the one gate that runs on the gatekeeper's own hardware. |
| Collection | a directory within the folder | v0.1: the sole Collection is the folder root (wp-sync compat). `.kith/` is reserved for the Circle's record logs, its descriptors and its Membership claims; `.kith/local/` is per-Device scratch, ignored from sync. |
| Item | a file | `*.sync-conflict-*` copies are filtered from the gallery and surfaced as a resolve affordance — they replicate to everyone, so they must be handled, not hidden. |
| Sidecar | **none — derived** | A Sidecar is the per-Item view reduced from the Circle's record logs (ADR-0004 §4), not a file the engine carries. The logs themselves are files, synced like any other bytes; their conflict-tolerant format is ADR-0004's problem. |
| Favourite | **never crosses the seam** | Per-Person, private, local. |
| Activity | derived from the change feed + the synced tree | Never an authoritative log. |

**Folder recipe** — what `create_circle` writes, and the config kith converges adopted
folders toward:

| Field | Value | Why |
|---|---|---|
| `type` | `sendreceive` | Every Member contributes; that is the wedge. Curator (send-only) topologies deferred. |
| `fsWatcherEnabled` | `true` | Imports and Provider Actions show up without manual rescans. |
| `versioning` | `{"type": "simple", "params": {"keep": "5", "cleanoutDays": "30"}}` | The real Role mitigation — §4. |
| `maxConflicts` | 10 (default) | Conflict copies bounded per file. |
| `.stignore` | `(?d).kith/local` | Device-local files (thumbnails, scratch) never replicate; `(?d)` so they cannot block a directory delete. |

### 3. Circle admin: exactly one introducer

Every Circle has exactly one introducer Device — the Device that created the Circle,
until succession. All other Members flag that Device `introducer: true` in their local
config; the introducer flags nobody.

**What it buys.** A joiner configures exactly one Device and receives the entire Circle:
Syncthing copies the introducer's per-folder device list (IDs, names, addresses) for every
mutually shared folder, transitively. Removal cascades: a Device dropped by the introducer
is auto-removed from every Member that learned of it by introduction (tracked via
`introducedBy`) on their next connection to the introducer. `admit` and `expel` on one
Device therefore administer the whole Circle.

**What it costs.** The introducer is a single point of failure for *membership changes
only*. Data flows mesh — every Member syncs with every Member directly — so an offline
introducer stalls nothing but joins, expulsions, and membership propagation to newcomers.
Those queue until it returns.

**Sharp edges, and the rules that blunt them:**

- **Never two introducers, never mutual.** Two Devices introducing each other re-add
  every removed Device forever. kith refuses `set_introducer(_, true)` for any Device other
  than the one the Circle already names as its Steward's Device — `circle.toml`'s
  `founder_device` in v0.1, the `[steward]` claim from v0.2 on — and succession always
  clears the old flag before setting the new one.
- **The flag is device-scoped, not folder-scoped.** If the same two People share two
  Circles, the introducer of one propagates device lists for both mutually shared
  folders. Harmless (propagation is additive and only covers folders both sides already
  share) but documented, because it means "introducer of Circle A" is a kith-level
  designation that Syncthing cannot scope.
- **Introduction is one-shot.** Address/name changes after introduction do not propagate;
  discovery handles addressing anyway (`dynamic`).

**Who the Steward's Device is, in v0.1.** It is the Device recorded at creation:
`circle.toml`'s `founder_device` (ADR-0004 §5), written once and never moved. The
`[steward]` table in the Membership claim is reserved and unwritten until v0.2 (ADR-0004
§11), so in v0.1 there is exactly one record of the Circle's Steward Device, in the one file
that is written once. Every surface that names it reads it from there — not from the
introducer flag on `PeerDevice`, which §1 restricts to a cross-check for the reason this
section creates: the introducer flags nobody, so on the Steward's own Device the flag is on
no peer at all.

**Succession — v0.2.** v0.1 ships without it: a Circle whose Steward's Device is retired or
lost keeps syncing and admits nobody, exactly as CONTEXT.md warns, and no command changes
that. In v0.2 a surviving Member runs `kith adopt-steward` — flat, like every other kith
verb — and their kith writes the `[steward]` table into its *own* Membership claim
(single-writer, synced to everyone); each Member's kith applies the local config change —
clear the old flag, set the new — as the claim arrives, and from then on that claim, not
`founder_device`, names the Circle's Steward Device. `founder_device` is left as written: it
records who founded the Circle, never who stewards it now. Devices learned via the old
introducer are *not* lost: introduced entries persist in each Member's config; only future
propagation moves to the successor. Honesty requirement: any Member can seize succession —
the claim is advisory like every Role (§4). That is acceptable; a Circle that cannot trust
its Members to not hijack stewardship has a people problem no protocol fixes.

### 4. Roles are policy; versioning is the enforcement

CONTEXT.md already commits us: a Role is a policy, not an enforcement. Concretely:

| Role promise | Enforceable? | Mechanism |
|---|---|---|
| Only invited People join | **Yes** | Admission runs on the introducer's Device and is the only way in. |
| Invites expire / can be revoked | **Yes** | Checked by the admitting kith at `admit` time. |
| A removed Member stops receiving new content | **Mostly** | `expel` + de-introduction cascade. Forward-looking only: bytes already synced stay on their Device, and the cascade lands as each Member next connects to the introducer. |
| Only some Members may add Items | **No** | Any Device can write to the folder it holds. A well-behaved kith refuses the action; a hostile client cannot be stopped. |
| Members cannot delete or overwrite others' Items | **No** | Cannot be prevented. Can be *recovered*: see below. |
| A Member can be made read-only | **No (v0.1)** | Receive-only is self-imposed by a device on itself; it cannot be imposed remotely. |

The line: **gates that run on the gatekeeper's own Device are real; everything after
admission is convention plus recovery.** Every kith surface that shows a Role must be
written against that line.

**The recovery net.** Every Circle folder on every Device runs Syncthing **simple file
versioning** with `keep: "5"`, `cleanoutDays: "30"`. Versioning archives *remote-originated*
changes and deletes into `.stversions` — local edits are never versioned — which is
exactly the threat model: a Member (malicious, confused, or running `rm -rf`) deleted or
overwrote everything, and every *other* Device holds the previous five versions for
thirty days. Recovery is `kith restore` → `versions`/`restore` on the seam →
`GET`/`POST /rest/folder/versions`. Restored files propagate back to the whole Circle as
ordinary new versions, including onto the Device that did the damage. This is the honest
answer to "what if someone deletes everything": not prevention — restoration, on any
surviving Member's say-so.

`ignoreDelete` is rejected as a mitigation: it is a documented consistency footgun that
leaves the cluster permanently disagreeing about what exists.

### 5. Change observation

One long-poll loop per engine: `GET /rest/events?since=<cursor>&events=<filter>&timeout=60`.
The subscription filter is exactly:

`ItemFinished, StateChanged, FolderSummary, FolderCompletion, DeviceConnected,
DeviceDisconnected, PendingDevicesChanged, PendingFoldersChanged, ConfigSaved,
FolderErrors`

mapped to the seam's `Change` enum:

| Engine event | `Change` variant | Feeds |
|---|---|---|
| `ItemFinished`, `FolderSummary` | `ItemsChanged { circle, paths }` | Gallery refresh, cache update, Activity |
| `StateChanged` | `CircleState { circle, state }` | Status bar (idle/scanning/syncing) |
| `FolderCompletion` | `PeerProgress { circle, device, percent }` | Per-Member progress |
| `DeviceConnected` / `DeviceDisconnected` | `PeerOnline` / `PeerOffline` | Presence badges |
| `PendingDevicesChanged` | `JoinRequested(JoinRequest)` | The invite inbox (introducer side) |
| `PendingFoldersChanged` | `CircleOffered(CircleOffer)` | Join completion (joiner side) |
| `ConfigSaved` | `EngineReconfigured` | Re-read `circles()`; someone changed config under us |
| `FolderErrors` | `CircleTrouble { circle, error }` | Failure surfacing, never swallowed |

**Replay and disconnects.** Every `Envelope` carries a `Cursor` (the event id); kith
persists the last handled cursor in the SQLite cache. On reconnect it resumes with
`since=<cursor>`. Two conditions force a resync: a gap in event ids (the daemon's event
buffer overflowed) and a cursor from a previous daemon run (ids reset on restart). Both
surface as `Change::Desynced`, and the response is always the same, cheap by the ADR-0001
authority rule: rescan the synced tree, rebuild the cache, resubscribe with
`resume: None`. Losing the cache loses only the cursor — a full rescan, never data. While
the engine is unreachable the loop probes `/rest/noauth/health` with jittered exponential
backoff (1s → 60s) and the TUI shows the Sync Engine offline.

### 6. Daemon lifecycle & credential discovery

**kith never owns the process.** It does not start, stop, restart, or supervise the
daemon, and it never calls `/rest/system/restart` or `/rest/system/shutdown` — config
writes it makes apply live (v1.12+ semantics). If the engine ever reports a pending
restart requirement, kith tells the Person and does nothing.

**Credential discovery**, first hit wins:

1. `[sync_engine] address / api_key` in kith's TOML config — explicit override.
2. `$XDG_STATE_HOME/syncthing/config.xml`, then `~/.local/state/syncthing/config.xml`
   (v2 layout), then `~/.config/syncthing/config.xml` (v1) — parse `<gui>` for
   `<address>` and `<apikey>`.
3. Legacy `~/.config/wp-sync/identity` (`API_KEY=`) — migration courtesy, read-only.

Non-loopback addresses are refused unless explicitly configured in step 1.

**Failure behaviour.** `Unreachable`: kith stays useful — browsing, previews, Favourites
and Apply all work off the synced tree and cache; join/invite/status surfaces say plainly
that the Sync Engine is offline. `Unauthorized`: report where the key was found and what
to fix; never regenerate, rotate, or guess a key. `Incompatible` (below v1.13, the
pending-endpoints floor; spec'd against 2.x): refuse membership operations, explain,
leave everything untouched.

**What kith never mutates in an existing config:** the `<gui>` block (address, API key,
TLS), global options (listen addresses, relays, discovery, NAT), `defaults/*`, any folder
it did not create or adopt, and any device entry beyond (a) adding entries for admitted
Circle Devices and (b) the introducer flag on Circle peers. kith's config writes are
scoped to its own folders and their `devices` arrays, nothing else. This is the surviving
wrappers' pattern (ADR-0001); the one wrapper that rewrote daemon config aged worst.

### 7. Migration: adopting wp-sync installs

`kith create --adopt` finds the legacy folder (ID `wallpapers`, or `$WP_FOLDER_ID`) and
adopts it in place: the existing folder ID, path, and device entries are kept — nothing is
recreated, no bytes move, peers on old wp-sync keep syncing unmodified. Adoption writes
this Device's `.kith/` Membership claim and, if the tree has no `circle.toml` yet, that
descriptor too — recording the existing introducer entry in `founder_device` as the Circle's
Steward Device (ADR-0004 §5). The `[steward]` table is *not* written; it is reserved until
v0.2 succession (§3). The honest consequence, and kith says it in exactly these terms:
`founder_device` names a **Device**, and a Steward is a **Person** — so if that peer has
never run kith it has published no Membership claim, and the Circle can name the Steward's
Device but not the Steward until it does. No placeholder Person is invented to fill the gap.
Adoption then converges the folder toward the §2 recipe (adds versioning and the
`.stignore` seed — additive only), and retires the wp-sync systemd path unit, because
auto-apply without consent contradicts the consent rule; the Person re-applies
deliberately from the gallery. One prompted, opt-in cleanup: wp-sync set the global
`defaults/device.autoAcceptFolders: true`, which silently auto-accepts folders from any
future peer; kith offers to clear it, explains why, and touches it only on explicit yes —
the sole exception to the `defaults/*` rule, because wp-sync itself put it there.

## Consequences

**Accepted:**
- Joins and removals stall while the introducer is offline; v0.1 has no succession at all,
  and the succession that arrives in v0.2 is advisory — any Member can seize stewardship.
  Both are the honest price of serverless membership.
- Post-admission Roles are convention. The product's promise downgrades from "cannot" to
  "will be restored", and every Role surface must say so.
- A device-scoped introducer flag means cross-Circle introduction bleed between People
  who share multiple Circles. Additive-only, but it is a place the mapping leaks.
- The associated `Changes` stream makes the trait generic-first; dynamic dispatch needs
  boxing. With one implementor and a test double, monomorphization is fine.
- Explicit folder acceptance (no `autoAcceptFolders`) means kith must be running to
  complete a join. Acceptable: joining is an interactive act.

**Gained:**
- A 17-method firewall, each method pinned to a domain operation. Engine churn — or an
  engine swap — lands in one module; `rg -i syncthing` outside `engine::syncthing` and
  `docs/` returning hits is a review failure.
- Fully event-driven UI: one long-poll loop, no REST polling, gap recovery that degrades
  to a rescan instead of corruption.
- An enforceable admission gate and a concrete recovery story (`simple` versioning,
  keep 5 / 30 days, restore that re-propagates), instead of pretend ACLs.
- Existing wp-sync installs adopt, not migrate: same folder, same peers, day one.

## Alternatives considered

**No trait — call REST from wherever.** Rejected: the research shows the REST surface
churning across v1.12/v1.13/v2.0; without the firewall that churn lands everywhere, and
the engine can never be replaced.

**Owning the daemon.** Re-rejected here for the seam specifically: lifecycle ownership is
what forced SyncTrayzor to rewrite GUI addresses and API keys — exactly the mutations §6
forbids. ADR-0001 already carries the survivorship evidence.

**Multiple or mutual introducers for redundancy.** Rejected: mutual introduction breaks
removal permanently (resurrection loop), and "two admins" buys little when succession is
a one-command Membership-claim change (`kith adopt-steward`, v0.2). One introducer,
explicit succession.

**`ignoreDelete` as delete protection.** Rejected: per-device, invisible to peers, and
documented as leaving the cluster permanently inconsistent. Versioning restores state;
`ignoreDelete` forks it.

**Receive-only Members as a read-only Role.** Rejected for v0.1: the folder type is
self-imposed, so it models self-discipline, not permission; and the wedge is "everyone
contributes". The send-only curator topology stays on the shelf for a future moderated
Collection.

**Untrusted-device encryption as permissions.** Rejected: officially beta, and within a
Circle it grants nothing granular — every Member holding the password reads everything.
Its future is an always-on ciphertext relay, not a Role.

**Global `autoAcceptFolders` (wp-sync's own approach).** Rejected: it mutates behaviour
for non-kith usage of the same daemon and auto-trusts every future peer; kith accepts
offers explicitly, and offers to undo wp-sync's global default at adoption.
