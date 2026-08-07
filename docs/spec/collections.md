# SPEC — Collections

- **Status:** Accepted
- **Date:** 2026-08-07
- **Resolves:** [#14 Spec: collections](https://github.com/opx0/wp-sync/issues/14)
- **Informed by:** ADR-0001 (authority rule), ADR-0002 §§1–3, 7 (seam, mapping, Steward, adoption),
  ADR-0003 §1 (`claims`, `extract_metadata`), ADR-0004 (record grammar, merge, tombstones),
  ROADMAP §2 (Collections row), `docs/spec/cli-tui.md` §§1.4, 4.6–4.7

---

## Purpose

The Collections module owns the answer to one question: **what is in this Circle, and how did
it get there.** It turns bytes on disk into Items, and Items back into bytes, and it is the
only module allowed to write an `add`, `bind` or `remove` record.

Everything above it — Gallery, Preview, Actions, `kith list` — reads a `CollectionView` and
never touches the tree. Everything below it — the Sync Engine — moves bytes and knows nothing
about Items. Collections is the layer where those two facts are reconciled, continuously and
without a coordinator.

Three properties are load-bearing, and each is a consequence of a decision already locked:

1. **The tree is the truth, the view is derived** (ADR-0001). Losing the cache loses time,
   never Items.
2. **Membership of a Collection is declared by records, not by directory contents**
   (ADR-0004). A file is not an Item until a record says so; an Item is not gone because its
   file is.
3. **kith does not own the Collection root's filename namespace** (ADR-0004 Consequences).
   People will `mv`, `rm` and `cp` in it, and a peer may still be running plain wp-sync. The
   module is written to survive that rather than to forbid it.

---

## Domain objects involved

| Object | Role here |
|---|---|
| **Collection** | The unit this module manages. Exactly one per Circle in v0.1, created with it, bound to the wallpaper Provider. |
| **Item** | The domain object minted on import and retired by a tombstone. Identified by a ULID; *bound* to bytes by content hash. |
| **Sidecar** | The derived per-Item metadata view. **Not a file** (ADR-0004 §4) — a Sidecar is read, never written: `kith add` appends an `add` record to this Device's record log, and the Sidecar is what the reduction makes of it. |
| **Provider** | Decides what may become an Item (`claims`) and supplies the facts recorded at import (`extract_metadata`). The wallpaper Provider in v0.1. |
| **Person / Device** | Attribution (`by` = Person) versus write ownership (one log file per Device). ADR-0004 §5. |
| **Circle** | The container. Its root is the Collection's root in v0.1. Lifecycle is `docs/spec/circles-members-invites.md` (#13); this spec starts once a `CircleRef` exists. |
| **Sync Engine** | Supplies `CircleRef.root`, `ItemsChanged` paths, per-peer completion, and version recovery. Never consulted about Items. |
| **Favourite** | Local, never in the tree (ADR-0004 §7). Collections exposes only the Item ids the Favourite set keys on. |

---

## Behaviour

### 1. What a Collection is

#### 1.1 In the domain

A Collection is a named set of Items bound to one Provider, living inside one Circle. It is a
*logical* space: the glossary's "Collections over folders" is not a slogan here but the
implementation. Two Items whose bytes sit in `nature/` and in the root are peers in the same
flat Collection; the Gallery has no tree view and never will (ROADMAP §5, "not a file
explorer").

#### 1.2 On disk

Per ADR-0004 §2, and unchanged by this spec:

```
<circle root>/                          # v0.1: the Collection root, descriptor root = "."
├── sunset.png                          # Item bytes, the Person's own filenames
├── nature/forest-4k.jpg                # nested bytes are ordinary Items (§1.4)
├── .kith/
│   ├── circle.toml
│   ├── collections/main.toml           # the Collection descriptor
│   ├── members/<device-id>.toml        # Membership claims, one per Device (ADR-0004 §5)
│   ├── items/main/<device-id>.jsonl    # ← the Collection's record logs, one per Device
│   └── local/incoming/                 # import staging; never syncs
└── .stversions/
```

The descriptor is written once, by whichever Device creates or first adopts the Circle:

```toml
# .kith/collections/main.toml
schema     = 1
collection = "main"
name       = "walls"
provider   = "wallpaper"
root       = "."
created    = "2026-08-07T09:02:11.004Z"
```

**`main` is a literal in v0.1 and an opaque string in the format.** Nothing reads it as a
constant except the code path that creates it (§8).

#### 1.3 The module's face

```rust
pub struct CollectionId(String);           // opaque; v0.1 always "main"

/// The descriptor, resolved against a Circle.
pub struct Collection {
    pub id: CollectionId,
    pub circle: CircleId,
    pub name: String,
    pub provider: ProviderId,
    pub root: PathBuf,                     // absolute: circle.root.join(descriptor.root)
    pub log: PathBuf,                      // absolute: .kith/items/<id>/<this-device>.jsonl
    pub created: Timestamp,
}

pub trait Collections: Send + Sync {
    /// The Circle's Collections, descriptor order. v0.1 returns exactly one (§8).
    fn list(&self, circle: &CircleId) -> Result<Vec<Collection>, CollectionError>;

    /// The reduced view (ADR-0004 §4.4), served from cache, rebuilt on miss.
    fn view(&self, c: &CollectionId) -> Result<CollectionView, CollectionError>;

    /// What an import would do. Writes nothing, reads every source byte exactly once (§3.2).
    fn plan_import(&self, c: &CollectionId, sources: &[PathBuf], opts: &ImportOptions)
        -> Result<ImportPlan, CollectionError>;

    /// Execute a plan. Stops at the first fatal I/O error; everything already imported stands.
    fn import(&self, plan: &ImportPlan, progress: &mut dyn FnMut(ImportProgress))
        -> Result<ImportReport, CollectionError>;

    /// Tombstone Items and unlink their local bytes (§5).
    fn remove(&self, c: &CollectionId, items: &[ItemId], opts: &RemoveOptions)
        -> Result<RemoveReport, CollectionError>;

    /// Bring the view and the tree back into agreement. `changed = None` walks the whole
    /// root (cold start, Desynced); `Some(paths)` handles an ItemsChanged batch (§6).
    fn reconcile(&self, c: &CollectionId, changed: Option<&[RelPath]>)
        -> Result<ReconcileReport, CollectionError>;

    /// Local knowledge only. Every field's provenance is §7's table.
    fn stats(&self, c: &CollectionId) -> Result<CollectionStats, CollectionError>;
}

pub enum CollectionError {
    NoSuchCircle,
    NoSuchCollection,
    UnknownProvider(ProviderId),   // descriptor names a Provider this build lacks (§9)
    RootUnusable(PathBuf, io::Error),
    LogUnwritable(PathBuf, io::Error),
    Io(io::Error),
}
```

`import`, `remove` and `reconcile` are the **only** writers of `.kith/items/**` in the whole
binary. `rg 'items/.*jsonl' --type rust` outside `collections::` returning a write is a review
failure, the same discipline ADR-0002 applies to `rg -i syncthing`.

#### 1.4 What counts as a candidate inside the root

The Collection root is scanned **recursively**. Nested images are ordinary Items with their
relative path preserved in the record — because wp-sync trees in the wild have subdirectories,
and an Item that syncs but is invisible is the worst outcome available.

Never scanned, never hashed, never a tile:

| Excluded | Source |
|---|---|
| `.kith/**` | ADR-0004 §2 |
| everything `SyncEngine::reserved_paths()` returns — `.stfolder`, `.stversions/**`, `.stignore`, `.stglobalignore`, `.syncthing.*.tmp`, `~syncthing~*.tmp`, `*.sync-conflict-*` | ADR-0002 §1 (the method), ADR-0004 §2 (the set) |
| any other dot-entry at any depth | this spec — matches `kith add`'s skip rule (cli-tui §4.6) |
| symlinks | this spec (§9) |
| anything the Provider does not `claim` | ADR-0003 §1 |

A directory that becomes empty is of no interest to kith; the engine's own directory handling
applies.

---

### 2. Item identity, content hash and dedup

#### 2.1 Two identifiers, two jobs

| | Item id | Content hash |
|---|---|---|
| Form | ULID, 26-char Crockford base32 | `b3:` + 64 lowercase hex |
| Minted by | the adding Device, once | the bytes |
| Changes when | never | the bytes are re-encoded, edited or replaced |
| Answers | "is this the same Item?" | "are these the same bytes?" |

**The hash is a binding, never an identity** (ADR-0004 §4.1). That split is what lets an Item
survive `mv`, and what lets two Members who added the same wallpaper converge on one tile.

#### 2.2 The hash, exactly

**BLAKE3, default configuration**: unkeyed, no derivation context, 32-byte (256-bit) output,
rendered lowercase hex with a `b3:` prefix. Computed over the file's bytes and nothing else —
no filename, no mtime, no mode. Streamed with a 1 MiB buffer; `blake3`'s `update_mmap_rayon`
is used for files above 16 MiB.

Why: it runs at GB/s, which is the entire reason ADR-0004 §9 can afford an O(bytes) rebuild;
256 bits makes accidental collision unreachable; and it is not a security boundary — attribution
is convention, not cryptography (ADR-0004 §5), so a collision-resistant hash is chosen for
correctness under accident, not under attack.

The same digest is reused three ways, and is computed **once per file per import**: the record's
`hash` field, the dedup key, and the thumbnail cache key `<content-hash>-<class>.png`
(ADR-0003 §5).

#### 2.3 Dedup: the same image added twice

Three cases, three answers:

| Case | What happens |
|---|---|
| **Same Device, same Collection, live Item** — `kith add sunset.png` when a non-tombstoned Item already has that hash | Skipped, verdict `Duplicate { item }`, `info` note, no bytes copied, no record written. Exit code unaffected (cli-tui §4.6). |
| **Same Device, same hash as a *tombstoned* Item** | **Imported.** An `add` is written; ADR-0004 §4.4's revival rule (a record later than the tombstone) brings the original Item back with its original attribution. The CLI says so: `sunset was removed from walls earlier — adding it back.` |
| **Two Members, concurrently, on two Devices** | Two `add` records, two Item ids, one hash. ADR-0004 §4.4 step 3 aliases them: the canonical Item is the one from the record with the smallest `(at, device_id)`, every other id aliases to it, and the Gallery shows **one tile**. Both Devices reach the same answer without talking. |

The third case leaves **two files** in the tree — `sunset.png` and `sunset-2.png`, or two
different names entirely — because both Devices staged bytes before either saw the other's
record. kith does not delete one. Deleting bytes a peer just added is a delete that propagates
to everyone, and doing it automatically on a heuristic is exactly the class of action this
product refuses. Instead:

```rust
pub struct ItemView {
    // … ADR-0004 §4.4 …
    pub bytes: Option<ByteBinding>,     // the effective binding
    pub extra_paths: Vec<RelPath>,      // other local files with the same hash
}
```

- **Effective binding** = the winning `bind`/`add` path if that file exists locally and hashes
  to the Item's hash; otherwise the lexicographically smallest local path that does. Ties are
  impossible; the answer is the same on every Device that holds the same files.
- `extra_paths` is surfaced once, in `kith doctor`: `2 Items have duplicate copies on disk
  (7.4 MB) — the same wallpaper under two names. Deleting either deletes it for everyone.`
- Preview and Apply always use the effective binding, so which duplicate the Person sees is
  stable across restarts.

**No perceptual hashing, ever in v0.1.** The same photo at 4K and at 1080p, or as PNG and as
WebP, is two Items. kith compares bytes; it does not compare pictures. Saying otherwise would
require a similarity threshold, and a threshold that silently merges two Members' content is a
decision no product should make on their behalf.

---

### 3. Adding Items — the import flow

#### 3.1 Copy, not move

**Decision: `kith add` copies. `--move` is opt-in and never the default.**

| Reason | |
|---|---|
| The source is the Person's library | `~/Pictures/walls` is theirs and predates kith. Rearranging it is not an import's job. |
| Copy is reversible | A bad `kith add` is undone by deleting Items. A bad `kith add --move` is undone by nothing. |
| Cross-filesystem "move" is a lie | It is copy-then-unlink, with a window where a crash has consumed the source and not yet recorded the Item. Making that the default would make the dangerous path the quiet one. |
| Disk is cheap; a re-download is not | The wedge's Collections are tens of wallpapers. Duplicating 50 MB to keep a Person's library intact is the right trade every time. |

Three modes, decided by where the source is:

| Source | Mode | Bytes |
|---|---|---|
| Outside the Circle root | **copy** (default) | staged, then renamed into the Collection root |
| Outside the Circle root, `--move` | **move** | same filesystem: one `rename(2)`. Across filesystems: copy, record, `fdatasync`, *then* unlink the source. |
| Already inside the Circle root | **register in place** | none. No copy, no move, no rename — this is how an adopted tree gets its records (cli-tui §4.6, §4 here). |

#### 3.2 The plan phase

`plan_import` walks the sources and produces a verdict per candidate, writing nothing:

```rust
pub struct ImportOptions { pub mode: ImportMode, pub dry_run: bool, pub assume_yes: bool }
pub enum   ImportMode   { Copy, Move }

pub struct ImportPlan {
    pub collection: CollectionId,
    pub entries: Vec<PlanEntry>,
    pub total_bytes: u64,          // sum over Import entries only
    pub free_bytes: u64,           // statvfs on the Collection root's filesystem
    pub needs_confirmation: bool,  // §9 thresholds
}

pub struct PlanEntry { pub source: PathBuf, pub dest: RelPath, pub size: u64,
                       pub hash: Hash, pub verdict: Verdict }

pub enum Verdict {
    Import,                              // copy or move into `dest`
    Register,                            // already inside the root; record only
    Duplicate { item: ItemId },          // §2.3
    Revive    { item: ItemId },          // §2.3, tombstoned
    Unclaimed { reason: String },        // Provider said no
    Unreadable{ error: String },         // permissions, I/O, vanished mid-walk
}
```

Walk order and rules:

1. **Expand.** Directory arguments recurse depth-first, entries sorted byte-wise so two runs
   over the same tree produce the same order and the same collision suffixes. Symlinks are not
   followed and not imported (§9). Dot-entries are skipped silently.
2. **Never eat your own tail.** The Collection root — and every other Circle root known to this
   Device — is excluded from recursion. `kith add ~/Pictures` when the Circle lives at
   `~/Pictures/Wallpapers` recurses `~/Pictures`, skips the Circle root entirely, and imports
   the rest. Without this rule that single command doubles a Collection.
3. **Sniff.** Read a bounded 8 KiB prefix, derive a MIME guess, hand
   `ImportCandidate { path, mime }` to `Provider::claims` (ADR-0003 §1). 8 KiB rather than 512 B
   because an SVG's XML prologue and comments can push the root element past a small window.
4. **Hash.** Every claimed candidate is hashed in the same pass (§2.2). This is the only
   full read of the source in the whole import: the copy phase reuses nothing but re-reads
   sequentially, and verifies (§3.3).
5. **Resolve the destination.**

   | Argument | Source | `dest` |
   |---|---|---|
   | `kith add ~/Downloads/sunset.png` | a file | `sunset.png` |
   | `kith add ~/Pictures/walls` | `walls/nature/forest.jpg` | `nature/forest.jpg` |
   | `kith add ~/Pictures/walls/nature` | `nature/forest.jpg` | `forest.jpg` |

   A directory argument **preserves its internal structure** below the root; file arguments land
   at the root. Structure is storage, not domain: the Gallery is flat either way (§1.1). Missing
   destination directories are created with `mkdir -p`.
6. **Sanitise the destination name**, in this order: reject path separators and NUL; strip ASCII
   control characters; normalise to NFC; strip leading dots (`.hidden.jpg` never gets here —
   dot-entries are skipped — but a sanitised name must not *become* hidden); truncate to 255
   bytes preserving the extension. A name that sanitises to empty gets the Item id as its stem.
7. **Collide.** Comparison is **case-insensitive** (NFC, Unicode-lowercased), because a Circle
   may include a Device on a case-insensitive filesystem and `Sunset.png` beside `sunset.png` is
   a permanent conflict there. Same hash → `Duplicate`. Different hash → `sunset-2.png`,
   `sunset-3.png`, … with an `info` note naming the new filename.
8. **Total.** Sum `Import` sizes, `statvfs` the root's filesystem, set `needs_confirmation` per
   §9.

`--dry-run` prints the plan and stops here. It is exactly this structure rendered, so what a
Person previews is what runs.

#### 3.3 The write phase

Per entry, in this order — ADR-0004 §3's **bytes before record on add**:

1. **Stage.** Copy into `.kith/local/incoming/<item-id>.<ext>` — same filesystem as the root by
   construction, so step 3 is an atomic rename, and ignored from sync, so a half-copied file
   never replicates. Mode `0644`: kith clears the executable bit, because a wallpaper is not a
   program and the engine replicates that bit.
2. **Verify.** The copy is hashed as it is written. A digest differing from the plan's means the
   source changed underneath us: unlink the staged file, record the entry as
   `Unreadable { error: "source changed during import" }`, continue.
3. **Publish.** `fsync` the staged file, `rename(2)` into `<root>/<dest>`, `fsync` the
   destination directory.
4. **Record.** Append one `add` line to `.kith/items/<collection>/<this-device>.jsonl` under
   `flock` (ADR-0004 §3):

   ```json
   {"v":1,"k":"add","seq":7,"at":"2026-08-07T09:14:02.117Z","by":"01K1YFQ2M7VJ3W8T0PZ4RXAB6C","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVD","path":"nature/forest-4k.jpg","hash":"b3:5f2c9d…a10","size":4187361,"title":"forest-4k","facts":{"provider":"wallpaper","width":3840,"height":2160,"format":"jpeg"}}
   ```

   - `title` is the destination filename's **stem, verbatim** — `IMG_2031.JPG` titles as
     `IMG_2031`. No de-underscoring, no title casing: prettifying is a guess, and the stem is
     what the Person chose. Title *editing* is v0.3 (`meta`, ADR-0004 §11).
   - `facts` is `Provider::extract_metadata`'s output, called on
     `tokio::task::spawn_blocking` (ADR-0003 §1). Extraction failure is **not** an import
     failure: the record is written with `facts: {"provider":"wallpaper"}` and a `warn` note.
     A wallpaper whose dimensions could not be read still belongs in the Collection.
   - `adopted` is absent (false). It is set only by §4's adoption pass.
5. **`--move` only:** unlink the source, after the record is durable. Never before.

**Durability and batches.** ADR-0004 §3's append protocol is per record. A 5 000-file import
issuing 5 000 `fdatasync` calls is minutes of nothing, so `import` holds the `flock` for the run
and issues **one `fdatasync` per 64 records or 250 ms, whichever comes first, and always before
returning**. This is safe under the ADR's own crash rule: a crash in the window leaves files
whose records were not flushed, and those files are adopted as orphans on the next run (ADR-0004
§4.5) with `at` = their mtime — the same self-healing path adoption already uses. The invariant
that matters, *bytes before record*, is untouched.

**`seq` allocation.** On acquiring the lock, kith seeks to the end of its own log, reads back the
last complete line, and continues from its `seq`. A trailing line without `\n` is discarded first
(ADR-0004 §3). The cache's `last_seq` is a hint, checked against the file and never trusted over
it. This makes two concurrent `kith add` runs on one Device correct rather than merely unlikely.

**Progress.** `ImportProgress { done, total, bytes_done, bytes_total, current: PathBuf }`, emitted
at most every 250 ms. The CLI renders it on stderr (cli-tui §1.5) and suppresses it under
`--json`; the TUI does not import in v0.1 (§10).

#### 3.4 What the Person is told

```
Added 12 Items to walls (48.3 MB). Skipped 2.
  info     forest.jpg imported as forest-2.jpg — a different image already had that name
  skipped  ~/Pictures/walls/notes.txt — not claimed by the wallpaper Provider
  skipped  ~/Pictures/walls/ridge.png — already in walls (duplicate)
```

The JSON shape is cli-tui §4.6's, unchanged.

---

### 4. Importing an existing directory, and the wp-sync migration

ROADMAP's Collections row asks for one capability with two faces: *import an existing directory
of wallpapers as Items, adopting the current wp-sync tree rather than recreating it.* Both are
`kith create <name> --adopt [<DIR>]` (cli-tui §4.2), and they differ only in whether the Sync
Engine already knows the directory.

#### 4.1 Two branches, one content pass

| | **Branch A — the engine already replicates DIR** (a wp-sync install) | **Branch B — DIR is a plain directory** |
|---|---|---|
| Detection | `SyncEngine::circles()` contains a `CircleRef` whose `root` is DIR — for wp-sync, id `wallpapers` or `$WP_FOLDER_ID`, at `~/Pictures/Wallpapers` | no match |
| Circle | **kept as it is**: same id, same root, same peers. Nothing is created, no bytes move, peers still running wp-sync keep syncing unmodified (ADR-0002 §7). | `create_circle(name, DIR)` — a fresh `kith-` id, this Device is founder and the Circle's Steward Device |
| Then | § 4.2 config convergence, § 4.3 descriptors, § 4.4 content adoption | § 4.3 descriptors, § 4.4 content adoption |

Auto-detection with no argument, zero candidates and several candidates behave as cli-tui §4.2
specifies.

#### 4.2 Converging an adopted Circle's configuration (Branch A)

Additive only, and each line is ADR-0002 §2's recipe:

1. Add `versioning = {type: "simple", keep: 5, cleanoutDays: 30}` if absent. This is the Role
   honesty net (ADR-0002 §4) and the single most valuable thing adoption switches on.
2. Add `fsWatcherEnabled = true` if absent.
3. Append the ignore seed if the lines are absent — `(?d).kith/local` and `(?d)*.kith-tmp` —
   preserving every existing line.
4. Leave everything else exactly as found: folder id, label, path, type, device list, and the
   whole `<gui>` block (ADR-0002 §6).
5. **Retire the auto-apply**, because content arriving from a Circle must never change a screen
   (ADR-0003 §6):

   ```
   systemctl --user disable --now wp-sync-apply.path
   ```

   `~/.local/bin/wp-sync-apply.sh` and `~/.config/systemd/user/wp-sync-apply.{path,service}`
   are **left on disk** — kith deletes nothing it did not create. The exact undo command is
   printed:

   ```
   Disabled wp-sync's automatic wallpaper apply (wp-sync-apply.path).
   New wallpapers now wait in your Gallery until you press a.
   To put it back: systemctl --user enable --now wp-sync-apply.path
   ```
6. **Offer** the `defaults/device.autoAcceptFolders` cleanup — prompt on stderr, default **no**,
   skipped with a `warn` note when stderr is not a terminal (ADR-0002 §7, cli-tui §4.2). This is
   the sole `defaults/*` write kith will ever make, and only on an explicit yes.

#### 4.3 Who writes the descriptors

`circle.toml` and `collections/main.toml` are write-once singletons (ADR-0004 §5). In a fresh
Circle exactly one Device exists, so there is no question. In an adopted wp-sync Circle, Ana and
Ben may both run `kith create --adopt` on Tuesday and there is no coordinator to arbitrate.

**Decision: `circle.toml`'s `founder_device` names the Circle's Steward Device, and that is what
every surface above the seam reads. The descriptor is written once, by the Device the transport
already admits peers through; every other Device adopts into the descriptor that arrives.**

Two questions hide inside that, and keeping them apart is the whole of this section:

- **Who is the Steward's Device, from now on?** `circle.toml`. It is a fact in the tree, it
  reads the same from every vantage point, and it survives the engine being reconfigured. The
  Steward's *Person* is the `founder_person` beside it, resolved through that Device's
  Membership claim like every other attribution (ADR-0004 §5).
- **Who writes `circle.toml` in the one moment before it exists?** The transport, because it is
  the only thing that knows anything yet. A wp-sync install has exactly one Device that admits
  peers, by construction, and the engine's device list is the only signal of which one that is
  before any kith metadata does.

That bootstrap signal is a fact about a **Device**, never about a Person. A peer that has never
run kith has published no Membership claim, so kith may know which Device stewards the Circle
and still be unable to name the Person behind it — which is exactly what the output below says
rather than inventing a placeholder.

> *Seam reference:* the bootstrap signal is `PeerDevice.introducer: bool`, which ADR-0002 §1's
> `PeerDevice` carries — one field, no new method, no new endpoint (the folder's device list
> already carries the flag). The seam budget (ROADMAP rule 2) is charged one field. The word
> stays behind the seam: above it — in prose, in `kith list members`, in `kith status` and in
> the Members screen — this is the **Steward**, and the JSON field is `"steward": true`
> (cli-tui §3.2).
>
> *And the flag is a cross-check, never the source of truth.* ADR-0002 §3's own rule is that the
> Device the engine treats as introducer flags nobody, so `devices()` never returns self and on
> that Device no peer carries the flag. The flag therefore cannot name the Steward's Device from
> every vantage point; only `circle.toml` can. Once both exist and they disagree, `doctor` warns
> under `circle.descriptor` — no new check — and `circle.toml` wins.

| This Device, running `kith create --adopt` | Adoption writes |
|---|---|
| `circle.toml` is already in the tree | no descriptors — read `founder_device` for the Steward's Device, write this Device's own Membership claim if it has none, then §4.4 |
| No `circle.toml`, and a peer holds the engine's introducer entry → **that peer is the Steward's Device** | its own Membership claim, then §4.4. No descriptors: the ones that peer writes will arrive. |
| No `circle.toml`, and no peer does → **this Device is the one that admits peers** | `circle.toml` (`founder_*` = this Person/Device, `adopted = true`), `collections/main.toml`, its own Membership claim, then §4.4 |

The third row is an inference, and it is the honest one available before any kith metadata
exists: by ADR-0002 §3 the admitting Device sees no flagged peer, and a wp-sync Circle has
exactly one such Device. Where that assumption fails — the admitting Device was removed from the
engine's list, or two installs were stitched together by hand — two Devices may each write a
descriptor. That is not a new failure mode: ADR-0004 §8 keeps one copy and `doctor` reports the
collision loudly (below).

A Member that adopts before the Steward's Device has upgraded gets a working Collection
immediately — the Gallery fills from §4.4 — and an honest gap:

```
Adopted Wallpapers (folder "wallpapers") at ~/Pictures/Wallpapers — 214 Items.
This Circle has no kith record yet; it appears under its Sync Engine label, and kith cannot
name its Steward until ben-thinkpad runs kith. Everything else works now.
```

`kith doctor` carries this as `circle.descriptor` = warn until the descriptor arrives. The
escape hatch for a Circle whose Steward's Device will never upgrade is `--claim`, which writes
the descriptors anyway; its help text states the consequence plainly: if two Devices claim,
ADR-0004 §8 keeps the copy with the earliest `created` (ties → smallest `founder_device`) and
`doctor` reports the collision loudly.

> *Tie-break recorded here:* ADR-0004 §8 gives no tie-break for two `circle.toml` copies with an
> identical `created`. This spec adds one — smallest `founder_device` — so that convergence never
> depends on which copy the engine happened to keep.

#### 4.4 The content adoption pass

Identical for both branches, and identical to what `reconcile` does forever after (§6) — adoption
is not a special mode, it is the first reconcile:

1. Walk the Collection root under §1.4's rules.
2. Hash every claimed file.
3. For each: no `add` record anywhere with that hash → **orphan**. Append an `add` with
   `adopted: true` and **`at` = the file's mtime, not now** (ADR-0004 §4.5). Because the engine
   preserves mtimes, Ana's and Ben's records for the same file carry the same `at`, so the alias
   tie-break reduces to device id and both Devices converge without talking.
4. Bytes are not touched. Not copied, not renamed, not moved into subdirectories, not
   normalised. A 200-wallpaper tree is 200 records and zero byte movement.

Cost, stated because it is visible: one redundant record per Item per Device — roughly 50 KB per
200 Items — and a transient duplicate tile on each Device until the peer's log arrives
(ADR-0004 §4.5).

#### 4.5 Coexistence with peers still on wp-sync

A peer that never upgrades keeps working, and **adoption cannot break them**:

- kith adds one directory, `.kith/`, and one ignore file line. wp-sync's apply script selects
  images with `find "$DIR" -maxdepth 1 -type f -iname '*.jpg' …`; `.kith` is a directory, at
  depth 1, containing no images. It is invisible to that command.
- Their additions arrive as files with no records. kith adopts them (§4.4) and marks them
  `adopted`, so Preview says **found by Ana** rather than claiming she added them — the
  attribution is true at the level kith can actually observe.
- Their deletions arrive as file deletions with no tombstone. kith reports them (§6.3) rather
  than inventing a `remove` record on their behalf.

#### 4.6 The walkthrough, for an adopting pair

```
$ kith init                      # Ana, on the Steward's Device
$ kith create walls --adopt
Adopted the existing wallpaper tree at ~/Pictures/Wallpapers.
  circle      walls (kith kept the synced space it was already in — nothing moved)
  recovery    versioning enabled (keep 5, 30 days)
  auto-apply  disabled — wallpapers now wait in your Gallery
  items       214 adopted (0 bytes copied)
You are this Circle's Steward: invites and joins run on this Device.
```

Ben, on the machine that joined that tree, runs the same command and sees §4.3's Member variant.
Neither Person moved a file, re-shared a folder, or re-invited anybody. ADR-0002's promise —
*existing wp-sync installs adopt, not migrate* — is this section.

---

### 5. Removing Items

#### 5.1 Order and mechanism

ADR-0004 §3's other half: **record before bytes on remove.**

1. Append one `remove` line per Item to this Device's log, `fdatasync`.
2. Unlink the effective binding **and every `extra_path`** (§2.3) — an Item is not half-removed
   because it had two copies.
3. Let the engine propagate the deletes. Every *other* Device's `simple` versioning archives the
   previous content into `.stversions` (ADR-0002 §4). The deleting Device archives nothing: local
   edits are not versioned.

```rust
pub struct RemoveOptions { pub confirmed_foreign: bool }   // §5.2
pub struct RemoveReport {
    pub removed: Vec<ItemId>,
    pub bytes_freed: u64,
    pub records_only: Vec<ItemId>,   // no local bytes — the tombstone is the whole operation
    pub failed: Vec<(ItemId, String)>,
}
```

Removing an Item whose bytes never arrived is legal and useful: it writes the tombstone and
nothing else. It also works with the Sync Engine down — the record is in the tree and propagates
when the daemon returns.

#### 5.2 Roles, honestly

ADR-0004 §6 fixed the line, and this module implements exactly it:

- **Any Member may remove any Item.** There is no check to make; a Role check that costs
  convergence buys nothing because it enforces nothing.
- **kith warns and confirms** when the Item was added by someone else, and never pretends the
  refusal it is not making would have stopped anyone:

  ```
  Delete sunset from walls? Ana added it — she loses it too.
  This deletes it for every Member. Other Devices keep the last 5 versions for 30 days;
  v0.1 has no restore. [y/N]
  ```
- **Readers always honour a tombstone**, including one written by a Member whose Role would not
  have permitted it.

#### 5.3 Removal is not the only way bytes leave

A Person running `rm` in the Collection root is a first-class case, not a violation (§1, property
3). The file's absence propagates; the record does not change; the Item becomes
`bytes: None` on every Device. §6.3 says what kith does about it — which is to report it, never
to guess a tombstone. The two states are genuinely different: *removed from the Collection* and
*bytes missing*, and collapsing them would let one Person's `rm` silently retire another
Person's Item everywhere.

---

### 6. Reconciliation — keeping the view and the tree honest

#### 6.1 When it runs

| Trigger | Call | Cost |
|---|---|---|
| `Change::ItemsChanged { paths }` (ADR-0002 §5) | `reconcile(c, Some(paths))` | hashes only the named paths |
| `kith add`, or the Delete Action, completing | `reconcile(c, Some(touched))` | trivial |
| kith startup | `reconcile(c, None)` for each Circle, after reading logs forward from their cached offsets | one `stat` per file; hashes only where `(inode, size, mtime)` misses the cache index |
| `Change::Desynced`, cache loss, cache schema bump, `kith doctor --rebuild` | full rebuild per ADR-0004 §9, ending in `reconcile(c, None)` | O(bytes) — seconds per 5 GB |

#### 6.2 The five outcomes

For each claimed, non-reserved file, after the record reduction (ADR-0004 §4.4 step 5):

| Disk says | Records say | Action |
|---|---|---|
| file at `p`, hash `h` | an Item has hash `h` and a binding to `p` | nothing |
| file at `p`, hash `h` | an Item has hash `h`, bound elsewhere | append `bind {item, path: p, hash: h, size}` — **only if the winning binding does not already name `p`** (§6.4) |
| file at `p`, hash `h′` | an Item is bound to `p` with hash `h` | the bytes were re-encoded in place: append `bind` with `h′` |
| file at `p`, hash `h` | nothing matches | **orphan** → adopt per §4.4, subject to the settle window (§6.5) |
| no file | an Item is bound to `p` | `bytes: None`, placeholder tile, counted by §7 |

#### 6.3 Two anomalies that get named, not fixed

- **Tombstoned Item with bytes on disk.** A modify/delete conflict, or a Device that, while it
  was not connected, edited a file deleted meanwhile. The tombstone wins for the Gallery
  regardless (ADR-0004 §6); `doctor` says `3 removed Items still have bytes on disk (11 MB) —
  deleting them again is the ordinary Delete.`
- **Live Item with no bytes anywhere this Device can see.** Either still transferring, or
  someone `rm`'d it. kith cannot distinguish those from here, and says so: `4 Items have no
  bytes on this Device. They may still be arriving, or a Member may have deleted the files
  without removing the Items.` No tombstone is invented.

#### 6.4 Not everyone writes a `bind`

A rename propagates to every Device, and every Device's reconcile would reach the same
hash-match-at-a-new-path conclusion — N Devices, N redundant `bind` records. The rule above
("only if the winning binding does not already name `p`") removes the steady-state case: by the
time most Devices scan, the originator's `bind` has usually landed and there is nothing to
write. The race — two Devices reconciling in the same second — costs one redundant record whose
content differs only in `by` and `at`, and last-writer-wins over identical paths and hashes is
the same answer either way. Accepted, for the same reason ADR-0004 accepts adoption's duplicate
records: bounded, convergent, and cheaper than coordination that does not exist.

#### 6.5 The settle window

A Provider-claimed file with no record is **not adopted immediately**. Adoption waits until the
file's mtime is older than **60 seconds**, or until an explicit `kith add` / `kith create
--adopt` asks for it now.

The reason is arrival ordering: a 250-byte record beats a 4 MB wallpaper across the wire
(ADR-0004 §4.4 step 6), so a byteless record is the *normal* arrival state and a recordless file
is usually a peer's log still in flight. Adopting instantly would mint a second Item id for
content whose real record lands two seconds later — correct after aliasing, but a duplicate
record and a visible flicker for nothing. Sixty seconds is long enough for a log line to follow
its bytes on a working link and short enough that a wp-sync peer's drop feels immediate.

Within the window the file is not invisible: it renders as an **arriving** tile (§9).

---

### 7. Size and statistics — exactly what kith knows

```rust
pub struct CollectionStats {
    pub items: u64,                  // live Items with a record on this Device
    pub items_with_bytes: u64,
    pub items_awaiting_bytes: u64,
    pub bytes_here: u64,             // sum of effective bindings present locally
    pub bytes_declared: u64,         // sum of record sizes for live Items
    pub removed_with_bytes: u64,     // tombstoned, file still present (§6.3)
    pub duplicate_byte_copies: u64,  // extra_paths across all Items (§2.3)
    pub unclaimed_files: u64,        // in the root, not claimed by the Provider (§9)
    pub unclaimed_bytes: u64,
    pub conflict_copies: u64,        // *.sync-conflict-*, per ADR-0002 §2
    pub as_of: Timestamp,            // when the last reconcile finished
}
```

Every field is derived from this Device's tree and cache. Nothing here is a network call.

**What kith can say, and what it must never say:**

| Question | Answer |
|---|---|
| How many Items are in this Collection? | *How many this Device has received records for.* A Member whose Device has not connected for a week has a smaller number, and it is not wrong — it is what they hold. |
| How many bytes does this Collection use here? | `bytes_here`, exactly, from local `stat`. A fact. |
| How big is the Collection in total? | `bytes_declared` — the sum of what the *records* claim, including Items whose bytes have not arrived. Label it as such; never present it as disk usage. |
| Does Ben have this Item? | **kith cannot say.** The engine reports per-peer *completion of the whole synced space* as a percentage and a byte count, as of the last connection (ADR-0002 §1, `CircleStatus.peers`). That is folder-wide and byte-shaped; it cannot answer a per-Item question, and v0.1 does not ask one. `kith status` shows `Ben connected (91%)` and stops there. |
| How much disk does Ben use? | **kith cannot say**, and there is nowhere to learn it from. |
| Did a Member add and then remove something while I was away? | **Yes, and reliably** — both records live in the same append-only log, so a Device that has not connected for a year receives the `add` and the `remove` together and reduces to the correct tombstone. This is a genuine property of the log design, not a hope. |
| Was something added and removed by a Member who has since left? | Yes; Membership claims are never deleted (ADR-0004 §5), so the attribution still resolves to a named Person. |
| Why does `bytes_declared − bytes_here` disagree with `kith status`'s "MB to receive"? | Because they measure different things: the first is Item bytes this Device lacks, the second is everything the engine still owes for the whole synced space, `.kith/` and conflict copies included. Both are printed; neither is adjusted to match the other. |

The rule behind the table: **a count kith prints is a count of what this Device holds, and it
says so when that differs from what a Person would assume.** `kith status` prefixes per-peer
figures with their staleness (cli-tui §7.5); `list items` prints what reduced locally. There is
no aggregate "Circle total" anywhere in v0.1, because there is no honest way to compute one
without a server, and inventing one is precisely the kind of number that makes two friends
distrust the tool.

---

### 8. One Circle, many Collections — modelled now, opened in v0.3

ROADMAP: *modelled one-to-many from the start; opened up in v0.3.* What that costs today is
close to nothing, and what it buys is no migration.

| Already one-to-many | Where |
|---|---|
| `CollectionId` is an opaque string | §1.3; `main` is a literal in exactly one function, `create_collection` |
| Descriptors are a directory, not a file | `.kith/collections/*.toml` (ADR-0004 §2) |
| Record logs are namespaced per Collection | `.kith/items/<collection-id>/<device>.jsonl` |
| The root is explicit and relative | `root = "."` today; `root = "photos"` in v0.3, with no format change |
| Every module API takes a `CollectionId` | §1.3 — nothing resolves "the Collection" implicitly except one CLI helper |
| Moving an Item between Collections needs no new record kind | `remove` in one log, `add` with the same Item id in another (ADR-0004 §11) |

**What v0.1 deliberately does not have:** a `--collection` flag, a `kith collection` verb, a
Collection switcher, or a second descriptor writer. Adding any of them would breach ROADMAP §2's
ceiling. The single CLI helper that resolves a Circle to its Collection returns the sole element
and errors as `NoSuchCollection` if there is none.

**Forward compatibility.** A v0.1 kith opening a Circle written by a later kith with three
Collections uses the one whose id is `main`, else the lexicographically smallest, and reports:
`walls has 3 Collections; this version shows 1 (main) — upgrade to see the others.` Degraded,
never broken; the same posture as the preview ladder and as ADR-0004 §11's newer-`v` rule. It
writes no descriptor, so nothing it does damages the others.

---

## Edge cases & failure honesty

| Situation | What happens |
|---|---|
| **Huge directory** — `kith add ~/Pictures` with 20 000 files | The plan phase streams; nothing is held in memory but the current entry and a bounded channel. Above **500 Items or 2 GiB** the plan requires confirmation, showing counts, total size and free space. `--yes` skips it; a non-terminal stderr without `--yes` exits 64 with the numbers rather than importing 20 000 files unasked. *Call recorded here:* `--yes` is one flag beyond cli-tui §4.6's signature; without it `kith add` is unscriptable at scale, which the ROADMAP capability requires. |
| **Import would not fit** | Refused before the first copy: `statvfs` is compared against the plan total plus a 64 MiB margin. `Importing 214 Items needs 5.1 GB; ~/Pictures/Wallpapers has 2.3 GB free. Nothing was copied.` Exit 1, `io.no_space`. |
| **Disk fills mid-import anyway** (a peer syncing in parallel) | The staged file is unlinked, so nothing partial lands in the tree. The run **stops immediately** — every subsequent copy would fail, and 400 identical error lines help nobody. Items already imported keep their records and their bytes. Exit 1, naming the filesystem, bytes free and bytes remaining. Re-running the same command resumes: everything already imported is a `Duplicate` skip. |
| **Non-image content dropped into the tree by a Member** | Not claimed → not an Item → not a tile → not in `list items` → not in `items`/`bytes_here`. It still syncs to everyone, and kith says so rather than pretending it is not there: `doctor` reports `3 files in walls are not claimed by the wallpaper Provider (12 MB) — kith ignores them; they still sync to every Member.` kith never deletes them and never writes an ignore rule for them: it does not own the namespace, and an ignore would silently stop replicating a file a Member deliberately shared. |
| **Partially-synced Item in the Gallery** | Impossible to see as a torn file: the engine writes `.syncthing.<name>.tmp` and renames into place, and that name is in `reserved_paths()`. The two real states are rendered distinctly — *record, no bytes* → placeholder tile carrying title, attribution and dimensions from the record; *bytes, no record* → arriving tile (below). |
| **Bytes present, the `add` record not arrived** | Inside the 60 s settle window (§6.5) the file shows as an **arriving** tile: filename as its label, attribution `unknown`, never a fabricated Person. After the window it is adopted with `adopted: true`, and if the peer's record lands later the alias rule merges the two into one tile with the true adder's name. |
| **Record arrived, bytes not** | The normal case, by design — 250 bytes beat 4 MB. Placeholder tile, full fact line, and Preview's text tier renders from the record alone (ADR-0003 §5). Apply is offered but fails honestly (`ActionError::Unavailable`, "the bytes for this Item have not arrived yet") rather than being hidden. |
| **A duplicate tile appears after adoption, then merges** | Expected and bounded (ADR-0004 §4.5): two Devices adopting the same pre-existing tree each write a record, and the tiles merge when the second log arrives. Documented in the adoption output, so it is a known behaviour rather than a bug report. |
| **Re-adding content that was removed** | Revives the original Item with the original adder's name (§2.3). Stated in the CLI output at the moment it happens, because it is surprising: `sunset was removed from walls earlier — adding it back (originally added by Ana).` |
| **Symlink in the Collection root, or as an import source** | Never adopted, never followed, never recreated. `doctor` names them: `2 symlinks in walls are not Items — kith does not sync links.` A symlink passed to `kith add` imports its **target's bytes** as an ordinary copy and leaves the link alone, with an `info` note. |
| **Hardlinked duplicates** | Two names, one inode, one hash → §2.3's `extra_paths`. Reported, never deleted. |
| **A file changes while its Item is on screen** | The next `ItemsChanged` reconciles: new hash → `bind` → the thumbnail cache key changes, so the tile re-renders. The Item id, its attribution and its date are untouched. |
| **`kith add` with the Sync Engine down** | Bytes land, records land, exit 0 with a `warn` note. They sync when the daemon returns (ADR-0002 §6, cli-tui §4.6). Import never talks to the engine. |
| **Two `kith add` runs at once on one Device** | `flock` on the log serialises the record writes, and `seq` is read from the file under the lock (§3.3). Staged filenames are Item ids, so the staging directory cannot collide. |
| **Crash between staging and recording** | The file is in the tree with no record → adopted as an orphan on the next reconcile with `at` = its mtime. The Item exists, attributed to this Person, marked `adopted`. Nothing is lost and nothing is duplicated. |
| **Crash between recording and `--move`'s unlink** | The source file survives. Worst case is a copy the Person expected to be a move; re-running `kith add --move` reports `Duplicate` and leaves it. Never the reverse. |
| **Stale staging files** | `.kith/local/incoming/` is swept at every `kith add` and at startup; entries older than 24 h are unlinked. It is ignored from sync and `(?d)`-marked, so nothing there is authoritative (ADR-0004 §7). |
| **Descriptor names a Provider this build lacks** | `CollectionError::UnknownProvider`. No file is adopted, the Gallery is empty rather than wrong, and `doctor` says `walls's Collection uses the "comics" Provider, which this version does not have.` |
| **Descriptor missing entirely** (adopted Circle, the Steward's Device not upgraded) | §4.3: the Collection works, named by the engine's folder label, and kith cannot name its Steward until that Device runs kith. `circle.descriptor` warns until the descriptor arrives. |
| **Conflict copy of a record log** | Absorbed, not resolved (ADR-0004 §8): it is read as one more log, the union is unchanged, and only the owning Device deletes it. Collections does nothing special. |
| **A record's `at` is far in the future** | ADR-0004 §4.4's 24 h clamp applies: sorted at arrival position, rendered with `?`, named by `doctor`. Import never trusts a source file's mtime for a *new* `add` — only adoption uses mtime, and only because convergence requires it. |
| **100 000 Items** | Works, and costs ~25 MB of records per Device (ADR-0004 §4.2). kith does not paginate the reduce in v0.1 and does not pretend it has been tested there; the wedge is tens of wallpapers, compaction is v0.3, and the honest statement is in the docs rather than in a benchmark nobody ran. |

---

## CLI / TUI surface touchpoints

The full CLI contract is `docs/spec/cli-tui.md`. What Collections owns:

| Surface | Owned here |
|---|---|
| `kith add [--circle C] [--move] [--dry-run] [--yes] <PATH>…` | The whole of §3. `data.imported[]` gains `title` and `adopted`; `data.skipped[].reason` is `Verdict`'s rendering. |
| `kith list items` | Rows are `CollectionView.items`, tombstones excluded, newest first by `added_at`. `SIZE` is the record's `size`; a byteless Item shows its size in parentheses and a `—` where the thumbnail state would be. |
| `kith create <name> --adopt [DIR] [--claim]` | §4 in full: branch detection, config convergence, descriptor policy, content adoption, the auto-apply retirement, and the printed undo command. `--claim` is added here (§4.3). |
| `kith status` | `items` comes from `CollectionStats.items`; the per-peer line stays byte-shaped and stale-labelled (§7). |
| `kith doctor` | Contributes to `circle.<id>.sync` and adds the counts §7 names: unclaimed files, duplicate byte copies, tombstoned-with-bytes, byteless Items, symlinks, and `circle.descriptor` — which also carries §4.3's Steward cross-check. Each is a `warn` at most — none of them is a broken Circle. |
| Gallery (#15) | Consumes `CollectionView` only. Three tile states originate here: **normal**, **placeholder** (record, no bytes), **arriving** (bytes, no record, inside the settle window). Empty-state copy is cli-tui §6.5's. |
| Preview (#15) | Fact line fields come from the record: title, `by` resolved to a Person through the Membership claims, `added_at`, `facts.width×height`, `size`. An `adopted` Item reads *found by Ana*, never *added by Ana*. |
| Delete Action (#15) | Calls `remove` with §5.2's confirmation text. |
| Live refresh | `Change::ItemsChanged` → `reconcile(Some(paths))` → the Gallery repaints. `kith add` running in another terminal therefore appears in an open TUI without either process knowing about the other; the tree is the channel. |

**No import UI in v0.1.** The TUI has no `add` screen, no file picker and no drag target
(cli-tui's coverage table puts TUI import at v0.2). `kith add` is the only way in, and the
Gallery is the only way to look.

---

## Out of scope for v0.1

Named, so that adding any of them is a decision someone makes rather than a drift:

| Not in v0.1 | Why, and when |
|---|---|
| Rename or delete a Collection; Collection settings | ROADMAP §2 makes the descriptor write-once. v0.3, with multiple Collections. |
| More than one Collection per Circle; a `--collection` flag; moving Items between Collections | Modelled (§8), opened in v0.3. The data format already supports it. |
| Tags, categories, and editing an Item's title | The `meta` record is reserved and unwritten (ADR-0004 §11). v0.3. |
| Export, copy-out, or transforming an Item on the way out | ADR-0003 defers the `export` verb; copy path and reveal cover v0.1. |
| Restore / undo a removal | Needs `SyncEngine::versions`/`restore` plus a History surface; ROADMAP puts it in v0.3, designed together with Role honesty. The bytes are kept meanwhile (30 days, 5 versions). |
| Resolving `*.sync-conflict-*` copies | Counted and named (ADR-0002 §2, cli-tui §9); resolution is the v0.2 Health screen. |
| Compaction of record logs | v0.3, as a new generation file (ADR-0004 §11). v0.1 logs only grow. |
| Perceptual or near-duplicate detection | §2.3. Not planned; byte identity is the only identity kith will assert. |
| Watching a directory and importing automatically | Automation II, v0.3. Nothing in the wedge watches anything, and an importer that runs unattended is a consent surface. |
| Per-Item sync state ("does Ben have this?") | §7. Not derivable from the transport, and not faked. |
| Quotas, retention policies, automatic cleanup of unclaimed files | kith does not own the namespace and does not delete what it did not create. |
| A second Provider, or Collections of anything but wallpapers | ROADMAP §2. The seam exists (ADR-0003); nothing external plugs into it before v1.0. |
