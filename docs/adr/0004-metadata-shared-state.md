# ADR-0004: Metadata & shared state — one writer per file, append-only records

- **Status:** Accepted
- **Date:** 2026-08-07
- **Resolves:** [#11 ADR: metadata & shared-state model](https://github.com/opx0/wp-sync/issues/11)
- **Informed by:** `research/prior-art` (implication 2: this layer is "simultaneously the moat and the hardest design problem"), `research/syncthing-api` §§7–8, 11, ADR-0001, ADR-0002, ADR-0003

## Context

The Sync Engine moves bytes and knows nothing about meaning (ADR-0002). Every fact that is
*about* content rather than *in* content — who added an Item, when, its title, whether the
Collection still contains it, who a Person is — has to survive on top of a transport with
three hostile properties:

1. **No coordinator.** Every Member Device writes concurrently; nothing serialises them.
2. **Last-writer-wins per file, with conflict copies.** Concurrent edits to one file produce
   `<name>.sync-conflict-<date>-<time>-<deviceid>.<ext>` on the losing side, and that copy is
   then an ordinary file that replicates to everyone. Since v2.0 a *delete* can win, turning
   the surviving content into a conflict copy.
3. **No authorship guarantee.** Any admitted Device can write any path in the Circle tree.
   ADR-0002 §4 already committed the product to honesty about this.

The design space collapses once one thing is admitted: **a conflict is not something to
resolve, it is something to make structurally impossible.** Two Members must never write the
same file. Everything below follows from that, plus ADR-0001's authority rule (the synced
tree is the sole source of truth; SQLite is a rebuildable cache).

Scope discipline: this ADR fixes the *format and semantics of the bytes on disk*. It adds no
capability beyond ROADMAP §2. Fields and record kinds that later milestones need are
**reserved** here — written by nobody in v0.1 — because reserving them costs nothing and
buying a migration later costs everything.

## Decision

### 1. The spine: one writer per file, append-only, union-merged

Three rules, in force everywhere in `.kith/`:

| # | Rule | What it buys |
|---|---|---|
| **W1** | **Every synced file names its writing Device in its path, and only that Device ever writes it.** | Conflict copies become an anomaly, not a workflow. |
| **W2** | **Record files are append-only and never rewritten in v0.1.** | The only remaining conflict generator (rewrite-in-flight) is removed. Compaction is deferred to v0.3 and lands as a *new generation file*, never an in-place rewrite (§11). |
| **W3** | **The derived view is a pure function of the union of all records.** | Read order, arrival order and *which* copy of a conflicted file won are all irrelevant: absorbing a conflict copy is just reading one more log (§8). |

W1 keys on **Device**, not Person, because a Person gets a second Device in v0.3 and the
invariant must not need re-earning. Attribution keys on **Person**, carried *inside* the
records. The two are bridged by the per-Device Membership claim (§5).

The consequence worth stating plainly: **this design is a CRDT** — a grow-only set of records
with a deterministic reducer. It is just one whose on-wire format is `tail -f`-able text and
whose repair tool is a text editor, rather than an opaque blob (§ Alternatives).

### 2. On-disk layout

The Circle's synced tree is rooted at `CircleRef.root` (ADR-0002 §1). v0.1's sole Collection
is the tree root, adopting the existing wp-sync layout rather than recreating it (ADR-0001).

```
<circle root>/                                  # = the Collection root, v0.1
├── sunset.png                                  # Item bytes, the Person's own filenames
├── forest-4k.jpg
├── .kith/                                      # all kith shared state; hidden from the Gallery
│   ├── circle.toml                             # written once, by the founding Device
│   ├── collections/
│   │   └── main.toml                           # one Collection descriptor per Collection
│   ├── members/                                # one Membership claim per Device
│   │   ├── P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2.toml
│   │   └── K5J2FVL-B3QTXAO-7SWNDUE-HMR4YZI-6CPGA2N-XQTLB5V-JW3EOHY-RD6MSAK.toml
│   ├── items/
│   │   └── main/                               # per Collection
│   │       ├── P56IOI7-…-XZWICQ2.jsonl         # ← Ana's Device's record log
│   │       └── K5J2FVL-…-RD6MSAK.jsonl         # ← Ben's Device's record log
│   └── local/                                  # never syncs (.stignore); staging only
│       └── incoming/
├── .stversions/                                # engine's version archive — the recovery net
└── .stignore
```

**Hidden from the Gallery** (never a tile, never adopted as an Item, never hashed):
`.kith/**`, every path the engine declares as its own, and `*.sync-conflict-*` — the last
surfaced as a resolve affordance instead, per ADR-0002 §2. This ADR asks ADR-0002's trait for
exactly one addition, so those names stay behind the seam — ADR-0002 §1 carries it as of its
2026-08-07 amendment:

```rust
/// Globs the engine owns inside a Circle root. The core hides them from the
/// Gallery and never adopts them as Items. The only ADR-0004 addition to the seam.
fn reserved_paths(&self) -> &[&'static str];
```

`engine::syncthing` returns `[".stfolder", ".stversions/**", ".stignore", ".stglobalignore",
".syncthing.*.tmp", "~syncthing~*.tmp", "*.sync-conflict-*"]`.

It also extends ADR-0002's `.stignore` seed by one line, for the descriptor write protocol
(§3):

```
(?d).kith/local
(?d)*.kith-tmp
```

Everything else under `.kith/` **does** replicate — including into `.stversions`. That is
deliberate: metadata rides the same recovery net as content, so a Member who wipes `.kith/`
is undone by any other Member's `kith restore`, exactly like a Member who wipes the images.

**Local, unsynced state** lives outside the tree entirely:

| Path | Authority | Rebuildable |
|---|---|---|
| `$XDG_CONFIG_HOME/kith/config.toml` | human-authored | n/a |
| `$XDG_DATA_HOME/kith/identity.toml` | **authoritative** — Person id, display name | **no** |
| `$XDG_DATA_HOME/kith/favourites.jsonl` | **authoritative** — Favourites (§7) | **no** |
| `$XDG_CACHE_HOME/kith/cache.sqlite3` | cache | yes (§9) |
| `$XDG_CACHE_HOME/kith/thumbs/` | cache (ADR-0003 §5) | yes |

**kith writes exactly two files it cannot rebuild**, and `kith doctor` prints both paths with
their record counts. Everything else is either the synced tree or disposable.

### 3. Formats: JSON Lines for logs, TOML for descriptors

| Kind | Format | Why |
|---|---|---|
| Record logs | **JSON Lines** (`.jsonl`), one record per line, LF-terminated | TOML has no append-safe line grammar — a record spans lines, so a torn write or a half-absorbed conflict copy poisons the *whole document*. In JSONL a damaged line costs exactly one record, and appending is one `write(2)`. |
| Descriptors | **TOML** | Small, singleton, read-modify-write documents a human may open. Matches ADR-0001's config choice. |

**Append protocol** (logs). `open(O_WRONLY|O_APPEND|O_CREAT)` → `flock(LOCK_EX)` (guards two
local kith processes, the only same-Device race) → one `write(2)` of the complete line
including its `\n` → `fdatasync` → unlock. A reader that finds a trailing line without `\n`
discards it: the only way to produce one is a local crash mid-append, because the engine
stages incoming files and renames them into place, so a *remote* reader never sees a torn
file.

**Descriptor protocol.** Write `<name>.toml.kith-tmp` in the same directory → `fsync` →
`rename(2)` over the target. The `.kith-tmp` suffix is ignored from sync (§2), so a partial
descriptor never replicates.

**Byte import protocol.** `kith add` copies into `.kith/local/incoming/` (ignored, same
filesystem) → `rename(2)` into the Collection root → *then* appends the `add` record. Ordering
is deliberate and asymmetric:

> **Bytes before record on add; record before bytes on remove.**

So the tree never advertises an Item whose bytes were never staged, and never shows an Item it
has already declared gone. A crash in either window is self-healing: an unrecorded file is
adopted as an orphan (§4.5), a tombstoned file with bytes is reported by `kith doctor`.

### 4. The Sidecar: record grammar and merge

A **Sidecar** is not a file per Item. It is the derived per-Item view produced by reducing the
Circle's record logs — the domain object CONTEXT.md names, made concrete.

#### 4.1 Identifiers

| Thing | Form | Notes |
|---|---|---|
| Item id | ULID (26-char Crockford base32) | Minted once by the adding Device. **Never changes** — this is what lets an Item survive being moved, renamed or re-encoded (CONTEXT.md). |
| Person id | ULID | Minted at `kith init`, stored in `identity.toml`, identical across every Circle. No new correlation risk: the Device id is already shared across Circles by the engine. |
| Device id | the engine's device identity, canonical form | Opaque above the seam; used only as a filename and a merge tie-break. |
| Content hash | `b3:` + 64 hex chars (BLAKE3) | The *binding* between an Item and the bytes currently representing it — never the Item's identity. |
| Timestamp | RFC 3339 UTC, milliseconds, `Z` | The writer's wall clock. Honest limits in §4.4. |

#### 4.2 Record

```rust
#[derive(Serialize, Deserialize)]
pub struct Record {
    pub v: u32,            // schema version of THIS record (§11)
    pub seq: u64,          // per-log, monotonic, gapless, from 1
    pub at: Timestamp,     // writer's wall clock
    pub by: PersonId,      // asserted, not proven (§5)
    #[serde(flatten)]
    pub body: Body,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,   // unknown fields: captured, never dropped
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "lowercase")]
pub enum Body {
    /// This Item entered this Collection.
    Add { item: ItemId, path: RelPath, hash: Hash, size: u64,
          title: String, facts: ProviderFacts,        // ADR-0003 `extract_metadata`
          #[serde(default)] adopted: bool },          // found on disk, not added through kith
    /// This Item's bytes are now at this path with this hash (move, rename, re-encode).
    Bind { item: ItemId, path: RelPath, hash: Hash, size: u64 },
    /// Metadata assertion. RESERVED — v0.1 writes none (§11).
    Meta { item: ItemId, title: Option<String>, tags: Option<TagDelta> },
    /// Tombstone: this Item is no longer in this Collection.
    Remove { item: ItemId },
    #[serde(other)]
    Unknown,               // counted, reported, never applied, never rewritten
}
```

The Collection is encoded by the log's directory and the Device by its filename, so neither is
a field. One line, as actually written:

```json
{"v":1,"k":"add","seq":7,"at":"2026-08-07T09:14:02.117Z","by":"01K1YFQ2M7VJ3W8T0PZ4RXAB6C","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVD","path":"sunset.png","hash":"b3:5f2c9d…a10","size":1987654,"title":"sunset","facts":{"provider":"wallpaper","width":3840,"height":2160,"format":"png"}}
```

~250 bytes per record. A 1000-Item Collection with five Members costs roughly 1.5 MB of
metadata — small enough that compaction is a v0.3 nicety, not a v0.1 requirement.

**v0.1 writes three kinds: `add`, `bind`, `remove`.** `meta` is reserved: v0.1 has no title
editing and no tags (ROADMAP §2), and titles ship inside the `add` record, defaulted from the
filename stem.

#### 4.3 Log file naming

`.kith/items/<collection-id>/<device-id>.jsonl`. Readers glob `*.jsonl` and take the **segment
before the first `.`** as the writing Device id. Everything after it is meaningful only to the
owner, which makes two future things free and one present thing safe:

| Filename | Read as |
|---|---|
| `P56IOI7-…-XZWICQ2.jsonl` | Ana's Device's log |
| `P56IOI7-…-XZWICQ2.sync-conflict-20260807-091402-K5J2FVL.jsonl` | the same log — merged, not resolved (§8) |
| `P56IOI7-…-XZWICQ2.2.jsonl` | the same log, generation 2 (compaction, v0.3) |

#### 4.4 Merge — from records to a `CollectionView`

```rust
/// Everything kith knows about one Collection. A pure function of the record set.
pub struct CollectionView { pub items: BTreeMap<ItemId, ItemView>, pub health: MetadataHealth }

pub struct ItemView {
    pub id: ItemId,
    pub added_by: PersonId,
    pub added_at: Timestamp,
    pub adopted: bool,
    pub title: String,
    pub tags: BTreeSet<String>,          // v0.3
    pub facts: ProviderFacts,
    pub bytes: Option<ByteBinding>,      // None = record here, bytes not here yet
    pub removed: Option<Removal>,        // tombstone
}
```

The reducer, in order:

1. **Parse** every `*.jsonl` under the Collection's record directory, conflict copies included.
   Drop and *count*: unparseable lines, records with `v` above the supported version, `Unknown`
   kinds. Never rewrite the source.
2. **Total order** all records by `(at, device_id, seq)`. Deterministic on every Device.
3. **Alias.** Group `add` records by `hash`; the canonical Item is the one from the record with
   the smallest `(at, device_id)`; every other Item id in the group aliases to it, and records
   naming an alias apply to the canonical Item. This is what makes independent adoption of a
   pre-existing tree converge (§4.5).
4. **Apply.** `add`: first wins for `added_by`, `added_at`, `adopted`, `facts`, `title`; later
   duplicates contribute only their path/hash binding. `bind`: last wins. `meta`: per-field
   last-writer-wins; tags are a per-tag add/remove set with per-tag LWW. `remove`: sets the
   tombstone; a later `add` or `bind` with `at` greater than the tombstone's revives the Item.
5. **Reconcile with disk.** For every Provider-claimed, non-reserved file in the Collection
   root: match by hash → bind it; else match by path → hash differs, so the bytes were
   re-encoded in place: append a `bind` to *our own* log; else it is an **orphan** → §4.5.
6. **Bytes absent** → `bytes: None`. The Gallery renders a placeholder tile with the title and
   attribution it already has, because a 250-byte record beats a 4 MB wallpaper across the
   wire. Metadata arriving first is what makes walkthrough step 9 ("thumbnails appear as bytes
   arrive") a designed behaviour rather than a race.

**Clock honesty.** `(at, device_id, seq)` is a total order, not a happens-before. A Device with
a wrong clock misplaces its own Items in the Gallery's date sort; it corrupts nothing. One
guard, because the date sort is the Gallery's spine: a record whose `at` is more than 24 h
ahead of the reading Device's clock is sorted at its arrival position, rendered with a `?`
marker, and named by `kith doctor`.

#### 4.5 Orphans and adoption

An orphan is a Provider-claimed file with no `add` record. Every Device that sees one appends
an `add` for it, with `adopted: true` and **`at` set to the file's modification time, not
now**. Because the engine preserves mtimes, two Devices adopting the same pre-existing tree
produce records with *identical* `at`, so §4.4 step 3's tie-break reduces to device id and both
Devices reach the same answer without talking.

This is not a corner case — it is the wp-sync migration path (ADR-0002 §7), where Ana and Ben
already hold the same 200 wallpapers and both run `kith create --adopt`. The cost is one
redundant record per Item per Device (~50 KB per 200 Items), one-off and bounded. The visible
artefact is honest and brief: until both logs have crossed, a Device may show a duplicate tile
that merges when the peer's log arrives.

Adoption also covers coexistence: a peer still running plain wp-sync writes bytes and no
records. Their additions are adopted and attributed to whichever kith Person adopted them —
marked `adopted`, so Preview can say *found by Ana* rather than claim she added it.

### 5. Identity, attribution and Roles

Each Device asserts itself once per Circle in a **Membership claim**: one file at
`.kith/members/<device-id>.toml`, keyed by the **Device** and written only by the Device it
names. It is W1 applied to identity — the path carries its writer, so no two Devices ever
touch one claim, and a Person's second Device in v0.3 adds a second claim instead of
contending for one file.

```toml
# .kith/members/<device-id>.toml — the Membership claim; sole writer = this Device
schema       = 1
device       = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2"
person       = "01K1YFQ2M7VJ3W8T0PZ4RXAB6C"
display_name = "Ana"
asserted     = "2026-08-07T09:02:11.004Z"

# RESERVED, unwritten in v0.1 — succession (ADR-0002 §3), lands as a command in v0.2:
# [steward]
# asserted    = "…"
# predecessor = "<device-id>"
```

Written by the founder at `create_circle` and by the joiner immediately after
`complete_join`. **Never deleted** — which is precisely how attribution survives Devices coming
and going: an expelled Device's claim stays in the tree, so its records keep resolving to a
named Person forever.

**Keyed by Device, attributed to Person.** The filename and the `device` field exist only to
satisfy W1 and to bridge to the engine's device list (`SyncEngine::devices()`). Every fact the
product states — who added an Item, who is a Member, whose Role is what — keys on the
**PersonId** carried *inside* the claim, and is never expressed as a DeviceId. That split is
the whole reason v0.3's second Device is one more file rather than a migration: the set of
claims sharing a `person` *is* the Person, and the reducer never learns a new shape.

Derivation:

| Question | Answer |
|---|---|
| Who is Person P? | Devices = every Membership claim with `person = P`; display name = the claim with the newest `asserted` (ties → smallest device id, and `doctor` reports the disagreement). |
| Who added this Item? | The `by` of the winning `add`, resolved through the Membership claims. No claim → *Unknown Person (`P56IOI7…`)*, never a blank. |
| What is P's Role? | v0.1: `admin` iff P is `circle.toml`'s `founder_person` or the current Steward; otherwise `member`. Role *editing* (v0.2) lands as a `[grants]` table in the Membership claim of the Steward's Device — still single-writer, still no migration. |

**Circle and Collection descriptors** are the two singletons, and v0.1's ROADMAP row ("no
rename, no delete, no Circle settings") makes them write-once:

```toml
# .kith/circle.toml — written once by the founding Device, never rewritten.
schema         = 1
circle         = "kith-4tj2q9xa"
name           = "walls"
created        = "2026-08-07T09:02:11.004Z"
founder_person = "01K1YFQ2M7VJ3W8T0PZ4RXAB6C"
founder_device = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2"
```

```toml
# .kith/collections/main.toml — v0.1's id is literally "main"; the field is opaque,
# so v0.3's additional Collections get random ids without a format change.
schema     = 1
collection = "main"
name       = "walls"
provider   = "wallpaper"
root       = "."                 # relative to the Circle root
created    = "2026-08-07T09:02:11.004Z"
```

#### Trust: convention only, and we say so

**Records are not signed.** A file named for Ana's Device is Ana's *by convention*; any
admitted Device can create it and write anything in it, including a false `by`.

This is a decision, not an omission. Signing is unavailable and unwanted:

- ADR-0002 §2 forbids kith from touching the engine's key material ("kith never reads, copies,
  or backs it up"), so the only identity the transport provides cannot sign our records.
- ROADMAP §5 rules out home-grown cryptography and a second identity system.
- The threat model does not justify it. Every Member can already delete every Item (ADR-0002
  §4). Forging attribution is strictly *less* damage than the damage the design already
  accepts; signing would harden the second-weakest link.

What is available and does ship: kith cross-checks the Devices that own logs and claims against
`SyncEngine::devices()`, and `kith doctor` flags **records written by a Device that is not a
Member of this Circle**. That catches accident, stale state and lazy tampering — not a
determined forger, and the surface must never imply otherwise, exactly as with Roles.

If signing ever ships (v1.0, alongside the formal consent framework), it lands as an additive
`sig` field on the record. The field name is reserved now, which costs nothing.

### 6. Removal and tombstones

Removal is two independent propagations, and kith does them in this order:

1. **Domain removal** — append a `remove` record. Coordinator-free, converging, and reversible
   in data: the `add` record still exists.
2. **Byte removal** — delete the file. The engine propagates the delete; every *other* Device's
   `simple` versioning (keep 5 / 30 days, ADR-0002 §4) archives the previous content into
   `.stversions`.

**The tombstone is authoritative for the Gallery, regardless of bytes.** Bytes can outlive a
tombstone — an offline Device modifies a file that was deleted meanwhile, and a modify/delete
conflict resurrects it as a conflict copy — and the Gallery stays clean anyway. `kith doctor`
reports "N removed Items still have bytes on disk"; deleting again is the ordinary Delete
Action.

The reverse window is equally survivable: a delete that propagates before its record leaves
peers with an Item whose `bytes` is `None`, which renders as a placeholder and settles into a
clean tombstone when the record lands. No state is unrepresentable and nothing silently
vanishes.

**Roles.** ADR-0002 §4 already fixed the line: nothing after admission is enforceable. Applied
here, and this is the one place the honest answer is also the *convergent* one:

- Any Member may remove any Item, and every removal is attributed and recoverable.
- A well-behaved kith **warns and confirms** when the Item was added by someone else — *added
  by Ana; she loses it too; recoverable from versions for 30 days* — rather than refusing, and
  it never pretends the refusal would have stopped anyone.
- Readers **always honour a tombstone**, including one written by a Member whose Role would not
  have permitted it. The alternative — honouring removals conditionally on a derived Role —
  makes two Devices disagree about the Gallery depending on when the Membership claims reached
  them. A policy check that costs convergence buys nothing, because it enforces nothing.

Restore (v0.3 History) needs no new record kind: restore the bytes through
`SyncEngine::restore`, append a `bind`, and §4.4's revival rule does the rest.

### 7. Favourites: local, authoritative, never synced

CONTEXT.md makes Favourites private per-Person, and ADR-0002's mapping already states that a
Favourite never crosses the seam. This ADR settles *where*: **outside the synced tree**, at
`$XDG_DATA_HOME/kith/favourites.jsonl`, in the same append-only grammar:

```json
{"v":1,"k":"fav","seq":12,"at":"2026-08-07T10:41:00.220Z","circle":"kith-4tj2q9xa","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVD"}
{"v":1,"k":"unfav","seq":13,"at":"2026-08-07T10:52:19.881Z","circle":"kith-4tj2q9xa","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVD"}
```

The effective set is the last op per `(circle, item)`. Rejected alternative: a synced-but-
ignored path such as `.kith/local/favourites.jsonl`. Three reasons it loses:

1. **Privacy by construction beats privacy by configuration.** Nothing about Favourites is ever
   written into a directory the engine watches, so no mis-edited ignore pattern, no engine
   upgrade, and no `defaults/ignores` change can leak them. Walkthrough step 10 — *Ben
   favourites an Item; Ana learns nothing* — becomes structurally true.
2. `.kith/local` is ignored with `(?d)`, which explicitly authorises the engine to delete it
   when it blocks a directory removal. That is fine for a staging directory and disqualifying
   for authoritative state.
3. Favourites are ADR-0003 §6's rotation pool. Losing them silently changes what appears on a
   Person's screen, so they must not share a lifetime with the Circle's tree — leaving a Circle
   must not consume them.

The price, accepted: Favourites do not follow a Person to their second Device (v0.3), and a
lost home directory loses them. Both are stated in the docs; neither is worth putting a
Person's private marks into a file every Member holds.

### 8. When a conflict copy appears anyway

W1 and W2 make conflicts on `.kith/` an anomaly, but not an impossibility: a restored-from-
backup or cloned install produces two Devices writing under one device id, and a Person can
always edit `.kith/` by hand. The response is **absorb, never resolve**:

| File | Reader behaviour | Owner behaviour | `doctor` |
|---|---|---|---|
| Record log | Merge the conflict copy as one more log (W3 makes this free — the union is unchanged by which copy won). | On startup: append records absent from its own log, then delete the conflict copy. Only the owning Device ever deletes one. | Reports each absorption. |
| `circle.toml` | Keep the copy with the earliest `created` — the original. | n/a (write-once). | Reports it loudly: someone rewrote a write-once record. |
| Membership claim | Keep the newest `asserted`. | Owner re-asserts and deletes the copy. | Reports it. |
| Collection descriptor | Keep the newest `created`; ties → smallest content hash. | Owner re-writes and deletes the copy. | Reports it. |

Two anomalies get named rather than papered over, because both mean something is genuinely
wrong with the Circle:

- **Forked log** — two records sharing `(device, seq)` with different content. Dedupe is by
  record hash; both records are kept and applied, and `doctor` says a Device id is in use twice.
- **Sequence gap** — `seq` jumping 3 → 7. Records 4–6 were lost (hand-editing, truncation, a
  partially restored file). `doctor` reports the gap and the versioned copies that may contain
  them; kith does not invent them.

### 9. The cache, and the rebuild path

ADR-0001's authority rule, restated in this ADR's terms: **anything in
`$XDG_CACHE_HOME/kith/cache.sqlite3` is derivable from the synced tree plus the local
Favourites file. If they disagree, they win and the cache is rebuilt.** The cache holds the
derived Item views, a hash index keyed by `(inode, size, mtime)`, per-log read positions
`(device, collection, byte_offset, last_seq)`, the mirrored Favourite set, the engine event
cursor (ADR-0002 §5), the thumbnail index and the rotation cursor (ADR-0003 §7). Its schema is
deliberately **not normative**: it is an implementation detail behind the authority rule, free
to change in any release without touching a Circle.

**Rebuild** — triggered by cache loss or corruption, `Change::Desynced`, a `kith doctor
--rebuild`, or a kith upgrade whose cache schema moved:

1. Drop the database file; recreate it empty.
2. For each `CircleRef` from `SyncEngine::circles()`: read `circle.toml`, every
   `collections/*.toml`, every `members/*.toml` Membership claim.
3. Stream every `items/<collection>/*.jsonl` (conflict copies included) through §4.4's reducer,
   recording each log's end offset and `seq`.
4. Walk each Collection root, hashing Provider-claimed files, and reconcile per §4.4 step 5 —
   appending `bind`/`add` records to *this Device's own* log for re-encodes and orphans.
5. Overlay `favourites.jsonl`.
6. Resubscribe with `resume: None`.

Cost is O(bytes in the Collection), because binding is by content hash: BLAKE3 runs at
GB/s, so a 5 GB Collection rehashes in seconds on an SSD, and rebuild is a rare event.
Steady state never pays it — `ItemsChanged` names the paths that moved, so only those are
rehashed, and each log is read forward from its stored offset.

**Losing the cache loses nothing but time.** That sentence is the whole point of the authority
rule, and it is what makes every other decision in this ADR safe to get wrong once.

### 10. Activity is derived, not logged

Activity ships in v0.2 (ROADMAP §2). This ADR's only obligation is that it be *derivable then*
from what v0.1 writes *now*, with no migration. It is, from three sources — and their limits
are stated because a timeline that lies is worse than no timeline:

| Half | Derived from | Consistency |
|---|---|---|
| **Durable, Circle-wide** | `add` (Item added by P at T), `remove` (removed by P at T), Membership claims (`asserted` → Person joined) | Survives cache rebuild, replays identically on every Device, and is complete for everything the Device has received. |
| **Ephemeral, Device-local** | the engine change feed: peers connecting, sync errors, arrival times | Not replicated, not replayable, starts when kith started, and gone after `Change::Desynced`. |

Honest limits, to be printed near the feature rather than discovered:

- **No happens-before.** Ordering is the writer's wall clock (§4.4). Two Members acting in the
  same second may be shown in either order.
- **Record time, not arrival time.** A Member offline for a week sees a week of correctly dated
  history appear at once. That is the right answer, and it means "new since you last looked" is
  a *local* notion (the unseen-Item dot), never a shared one.
- **Nothing is recorded about reading.** No views, no "Ana looked at this", and never a
  Favourite — §7 makes that structurally impossible, not merely omitted.
- **Only what reached this Device.** A Member expelled for a month has a month-shaped hole; an
  Item added and removed while a Device was offline may leave only the tombstone.
- **No record is written for Activity's benefit.** Every record above exists because a v0.1
  behaviour needs it. If Activity ever needs a fact nothing else records, that is a new record
  kind — additive, and free by §11.

### 11. Schema versioning and evolution

- **Every record carries `v`**; every descriptor carries `schema`. Per-record rather than
  per-file, so a log whose tail was written by a newer kith is still fully readable up to the
  point where it stops being understood.
- **Forward-compat rule: unknown record kinds and unknown fields are preserved, counted and
  ignored — never dropped, never rewritten.** With W1 and W2 this is *structural*, not
  disciplinary: an older client physically cannot damage a newer client's records, because it
  never writes their file and never rewrites its own.
- A reader meeting `v` above what it supports applies nothing from those records and reports
  through `kith doctor`: *N records written by a newer kith (schema 2) — upgrade to see them.*
  Degraded, never broken; the same posture as the preview ladder.
- **A breaking change is a new path, not a rewrite.** `v2` records land in
  `.kith/v2/items/<collection>/<device>.jsonl` with the v1 logs left in place and still read.
  Nothing an upgrade does ever deletes from the tree.
- **Compaction (v0.3)** is owner-only and writes a *new generation* — `<device>.2.jsonl` —
  then deletes generation 1. No in-place rewrite, so W2 survives compaction, and §4.3's naming
  rule already reads it.
- **Reserved and unwritten in v0.1**, each with its milestone: the `meta` record (titles and
  tags, v0.3), `[steward]` and `[grants]` in the Membership claim of the Steward's Device
  (succession command and role editing, v0.2), `sig` on a record (v1.0). Moving an Item between
  Collections (v0.3) needs no reservation at all: Item ids are Collection-independent, so it is
  a `remove` in one log and an `add` with the same id in another.

## Consequences

**Accepted:**

- **Attribution is convention, not cryptography.** An admitted Device can forge a `by`. The
  product says so wherever attribution is shown, in the same voice it uses for Roles.
- **Metadata only grows in v0.1.** No compaction until v0.3; ~250 bytes per event is the budget
  that makes this a non-problem for the wedge and a real one at 100k records.
- **Adoption writes duplicate records** — one per Item per Device when an existing tree is
  adopted — and can show a transient duplicate tile before the peer's log arrives.
- **Per-field LWW silently drops one side of a genuinely simultaneous title or tag edit.** With
  no `meta` writes in v0.1 this cannot bite until v0.3, when it wants a visible resolution
  affordance rather than a better algorithm.
- **kith does not own the Collection root's filename namespace.** People will `mv`, `rm` and
  `cp` in it, and reconciliation by hash-then-path is the price of adopting an existing tree
  instead of hiding one.
- **Rebuild is O(bytes)**, because content hashing is what makes an Item survive a rename.
- **Favourites are Device-local**, so they do not follow a Person to a second Device in v0.3.

**Gained:**

- **Conflicts are structurally rare rather than routinely handled**, and when one appears it is
  absorbed by reading it — the union is what the reducer wanted anyway.
- **Forward compatibility is a property of the layout**, not a promise about future code.
- **Metadata outruns bytes**, so the Gallery is populated and attributed before the first
  thumbnail decodes.
- **Everything is greppable text under version control by the engine.** A broken Circle is
  diagnosable with `tail` and repairable with `$EDITOR` — and `.kith/` rides `.stversions` like
  every Item, so `kith restore` covers metadata too.
- **Every honesty commitment in ADR-0002 has a concrete data shape here** — attribution,
  succession, tombstones, removal recovery — instead of a promise deferred to code review.

**Deliberately deferred:** compaction and generations (v0.3); signed records (v1.0); `meta`
writes, tags and title editing (v0.3); Steward succession and Role grants (v0.2); Item lineage
across re-encode beyond the `bind` record; any cross-Device sync of Favourites.

## Alternatives considered

**One shared metadata store in the tree — a single SQLite file, or one big TOML/JSON document.**
The obvious design, and the one that fails fastest: every Member writes it, so every add
produces a conflict copy, and merging two divergent copies of a whole database is a manual
operation with no correct answer. Byte-sync transports also corrupt live SQLite files outright
— both Syncthing and Dropbox document this. Rejected before it was designed.

**A CRDT library (automerge, yjs) holding the metadata document.** The theoretically right
answer, and what any-sync does with node infrastructure to back it. Rejected: its artefact is
one opaque binary blob with many writers — the same conflict problem — unless it is sharded per
Device, at which point it *is* this design, with a dependency, an unreadable file format, and
no `tail`. Our per-Device append-only log is already a CRDT: a grow-only set with a
deterministic reducer, in text.

**A Sidecar file per Item beside its bytes (`sunset.png.kith`, XMP-style).** The convention most
photo tools use. Rejected on three counts: it is multi-writer on the hot path (a title from any
Member conflicts), it doubles the file count in the Collection root — every one an index entry
and a sync unit — and it is destroyed by the rename it exists to survive.

**A file per (Item, Person): `.kith/items/main/<item>/<person>.toml`.** Genuinely conflict-free,
and the shape the ticket floated. Rejected on file count: 1000 Items × 5 Members is 5000 tiny
files, each a separate index entry and transfer, against five append-only logs where a new
record moves only the trailing block. The Item-major layout also forces a full directory walk
to answer "what changed", which the log answers with a byte offset.

**Metadata inside the image (EXIF/XMP).** Rejected: writing a 20-byte title mutates a 4 MB file,
changes its hash, and wakes every Member to re-transfer it; it is format-specific, so it
contradicts the Provider seam's generality; and it is destructive to content kith was trusted
to store, not edit.

**Signing every record with a kith-issued key.** Rejected: ADR-0002 puts the engine's key
material off limits, ROADMAP §5 forbids a second identity system, and the threat it addresses
is smaller than the one already accepted — anyone who can forge attribution can simply delete
everything instead. Reserved as an additive field so the decision is revisitable, not a
rewrite.

**Favourites synced but namespaced per Person (`.kith/private/<device>.jsonl`).** Rejected:
"private" inside a directory every Member holds a byte-identical copy of is a lie, and the
glossary's rule is that marking an Item announces nothing. Local-only makes that true rather
than polite.

**Vector or Lamport clocks for ordering.** They would give real causality, and every consumer —
Gallery date sort, Activity, Preview's "added today" — wants *human* time anyway, which a
logical clock cannot supply. Skew produces a wrong human timestamp either way. `seq` provides
the only ordering actually required (within a log, for gap detection and resume), and the
24-hour clamp bounds the damage on the one screen that sorts by time.
