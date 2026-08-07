# SPEC — Gallery, Preview & Actions

- **Status:** Accepted
- **Date:** 2026-08-07
- **Resolves:** [#15 Spec: gallery, preview & actions](https://github.com/opx0/wp-sync/issues/15)
- **Informed by:** `ROADMAP.md` §§2–3, `CONTEXT.md`, ADR-0001, ADR-0003, ADR-0004, `research/tui-landscape` §§2–4, `docs/spec/cli-tui.md`

## Purpose

This is the wedge's whole surface. Everything a Person does with content in v0.1 happens
here: a grid of the Collection's Items, a fullscreen Preview of one of them, and five
Actions. If this screen is good, kith is good; if it is slow, blank, or dishonest, no
other module can save it.

This spec fixes: the Gallery's layout and sort, the thumbnail pipeline and its cache
rules, the unseen-Item dot and where its state lives, selection and navigation semantics,
the Preview screen and its Sidecar facts, the v0.1 Action set with Apply's monitor picker,
what a Person sees when bytes have not arrived, what they see at each rung of the preview
ladder, and how failure is surfaced.

**Scope is ROADMAP §2's Gallery, Preview and Actions rows and nothing else.** No grouping,
no filtering beyond the favourites toggle, no infinite scroll, no zoom, no hash panel, no
share/duplicate/move/restore. `docs/spec/cli-tui.md` (#16) owns the frame, the screen
stack, the keymap and the wording rules; this document owns what happens inside the
content area and is not permitted to add a key, a screen or a verb. Where the two touch,
cli-tui.md is cited rather than restated.

## Domain objects involved

| Object | How this surface touches it |
|---|---|
| **Collection** | The Gallery *is* a Collection rendered. v0.1: one per Circle, so the Gallery's scope is the active Circle. |
| **Item** | One tile, one Preview. Identified by its ULID (ADR-0004 §4.1) — never by path, so a rename does not move a tile. |
| **Sidecar** | Every fact on a tile caption and in the Preview fact block: title, who added it, when, resolution, byte size. Derived per ADR-0004 §4.4, never read from a per-Item file. |
| **Favourite** | The `★` marker, the `F` filter, the `f` key. Local, authoritative, never synced (ADR-0004 §7). |
| **Person** | Attribution — *added by Ana*, *added by you*, *found by Ana*, *unknown Person*. Resolved through the Membership claims at `.kith/members/<device-id>.toml`, which key on Device and carry the PersonId (ADR-0004 §5). |
| **Provider** | Produces pixels and declares Actions (ADR-0003 §1). The Gallery calls it; it never calls the Gallery. |
| **Action** | Five in v0.1: Apply, Favourite, Reveal, Delete, Copy path. |
| **Apply** | The one Action that changes the world outside kith. §6.6 specifies it; §7 is its consent argument. |
| **Sync Engine** | Only as an event source: `ItemsChanged` repaints, `CircleState` fills the status row. It has no reference to the Provider registry and therefore no path to a screen (ADR-0003 §6). |
| **Circle / Member / Role** | Context, not content. Switching Circles resets this screen (cli-tui.md §6.5). |

---

## Behaviour

### 1. The Gallery

The root screen. A scrolling grid of every Item in the active Circle's Collection, newest
first, with an image thumbnail, a title, a favourite marker and an unseen dot.

#### 1.1 What is in it, and what is not

The Gallery renders exactly the Items of `CollectionView` (ADR-0004 §4.4), which means:

| Included | Excluded, and why |
|---|---|
| Items with bytes present | `.kith/**` — kith's own shared state, never an Item (ADR-0004 §2) |
| Items whose record arrived but whose bytes have not (§4) | Every path in `SyncEngine::reserved_paths()` — `.stfolder`, `.stversions/**`, `.stignore`, engine temp files |
| Items added by any Member, including this one | `*.sync-conflict-*` copies — counted by `kith status`, never a tile (ADR-0002 §2) |
| Adopted Items found on disk rather than added through kith | Tombstoned Items — the tombstone is authoritative for the Gallery even when bytes survive it (ADR-0004 §6) |
| | Content the wallpaper Provider does not `claims()` — it is not an Item at all |

There is no "show hidden", no "show removed" and no "show conflicts" toggle in v0.1. A
Person who needs to see those runs `kith doctor`, which counts every one of them.

#### 1.2 Grid geometry

Tiles are laid out from the terminal's real cell size, queried once at startup and
re-queried on `Resize`. Where the terminal will not report it, kith assumes **8×16 px**
and says so in `kith doctor` (`preview.cell_size`, cli-tui.md §5).

```rust
pub struct CellSize { pub w_px: u16, pub h_px: u16 }

pub struct Geometry {
    pub cols: u16,      // tiles per row
    pub tile_w: u16,    // cells
    pub img_h: u16,     // cells of image
    pub tile_h: u16,    // img_h + 1 caption row
    pub pad_left: u16,  // grid is centred in the content area
}

const GUTTER_W: u16 = 2;     // cells between tiles
const GUTTER_H: u16 = 1;     // cells between tile rows
const MIN_TILE_W: u16 = 14;
const MAX_TILE_W: u16 = 30;
const MAX_COLS: u16 = 10;

/// Target tile width. Wider on halfblocks, where each cell is worth only
/// two pixels and a small tile stops being recognisable (§8).
fn target_tile_w(rung: Rung) -> u16 { if rung == Rung::Halfblocks { 26 } else { 20 } }

fn geometry(area: Rect, cell: CellSize, rung: Rung) -> Geometry {
    let inner_w = area.width.saturating_sub(2);                    // 1 cell padding each side
    let t = target_tile_w(rung);
    let cols = (((inner_w + GUTTER_W) as f32 / (t + GUTTER_W) as f32).round() as u16)
        .clamp(1, MAX_COLS);
    let tile_w = ((inner_w - (cols - 1) * GUTTER_W) / cols).clamp(MIN_TILE_W, MAX_TILE_W);
    // 16:9 is the wallpaper norm; the tile frames it, letterboxing anything else.
    let img_h = (((tile_w * cell.w_px) as f32 * 9.0 / (16.0 * cell.h_px as f32)).round() as u16)
        .clamp(3, 10);
    let grid_w = cols * tile_w + (cols - 1) * GUTTER_W;
    Geometry { cols, tile_w, img_h, tile_h: img_h + 1, pad_left: 1 + (inner_w - grid_w) / 2 }
}
```

Worked, at the 8×17 px cells `kith doctor`'s example terminal reports:

| Terminal | cols | tile_w | img_h | tile_h | Tiles on screen |
|---|---|---|---|---|---|
| 60×18 (the floor) | 3 | 18 | 5 | 6 | 3 × 2 = 6 |
| 80×24 | 4 | 18 | 5 | 6 | 4 × 3 = 12 |
| 120×34 | 5 | 22 | 6 | 7 | 5 × 4 = 20 |
| 200×50 | 9 | 20 | 5 | 6 | 9 × 6 = 54 |
| 80×24, halfblocks | 3 | 24 | 6 | 7 | 3 × 2 = 6 |

The content area is the terminal minus cli-tui.md §6.1's three fixed rows. Visible tile
rows are `floor((content_h + GUTTER_H) / (tile_h + GUTTER_H))`; a partial row is never
drawn, because half an image is worse than white space. **On the sixel rung one content
row is reserved and left blank at the bottom**: `ratatui-image` documents that a sixel
image on the terminal's last line can scroll the screen, and the cheapest fix is to never
put one there.

If a resize takes the terminal below 60×18 while kith is running, the grid stops drawing
and the content area carries cli-tui.md §4.11's sentence with the measured size. The
frame rows stay, so nothing is lost and resizing back restores the grid with the same
selection.

#### 1.3 Sort order — date added, newest first

One order, no flags, no menu (ROADMAP: no sort, no grouping). It is the same order
`kith list items` prints, so the two surfaces never disagree.

```rust
/// Descending. Ties broken by Item id descending — ULIDs are time-ordered,
/// so this is stable, deterministic, and identical on every Device.
fn sort_key(v: &ItemView, now: Timestamp) -> (Timestamp, ItemId) {
    let at = if v.added_at > now + Duration::hours(24) { v.discovered_at } else { v.added_at };
    (at, v.id)
}
```

The clamp is ADR-0004 §4.4's clock-honesty guard made visible: a record claiming a time
more than 24 hours ahead of this Device's clock is sorted at its **arrival** position
instead of its claimed one, and its tile carries a `?` marker. The Gallery's spine is a
date sort; one Device with a wrong clock must not be able to pin itself to the top of
everyone's screen forever. Preview shows the claimed date, marked (§4.3).

`discovered_at` is when this Device first reduced a record naming the Item. It lives in
the cache and is therefore rebuildable; after a rebuild it is the rebuild time, which
affects nothing except the position of records that were already lying about their date.

#### 1.4 Tile anatomy

```
   ┌──────────────┐  ← no border is drawn; the image occupies these cells outright
   │              │
   │   image      │    img_h rows, aspect-preserved, letterboxed, never cropped
   │              │
   ★●  sunset-4k    ← caption row: markers, then title, truncated with …
```

| Element | Rule |
|---|---|
| Image | `Preview::Image` scaled to fit the tile's pixel budget, centred, aspect preserved. Never cropped — a wallpaper's composition is the thing being browsed. |
| `★` | Favourite. Private to this Person; drawn from the local Favourite set, never from anything synced. |
| `●` | Unseen (§3). Drawn to the left of the title, after `★` if both apply. |
| `?` | Clock-skew marker (§1.3). Rare; takes the position after `●`. |
| `↓` | Bytes have not arrived (§4). Replaces the image with a placeholder field. |
| Title | The Sidecar title — the filename stem by default (ADR-0004 §4.2). Truncated to `tile_w - markers` with `…`. |

**Selection is drawn outside the image.** On the kitty and iTerm2 rungs the image widget
owns its cells and the TUI may not overwrite them, so a border or a highlight painted over
the picture is not available. The selected tile is marked by (a) its caption row rendered
in reverse video and (b) a `▌` bar in the gutter column immediately left of the tile, on
every row of the tile. Both survive every rung, and neither depends on colour
(cli-tui.md §7.6).

#### 1.5 Navigation and selection

Movement keys are cli-tui.md §6.4's; the grid semantics are this spec's.

| Key | In the grid |
|---|---|
| `h` `←` / `l` `→` | Previous / next Item in **sort order**, wrapping across row ends. The grid is a linear list wrapped for display; linear movement never dead-ends in a corner. |
| `j` `↓` / `k` `↑` | One tile row down / up, keeping the column. From the last (possibly short) row, `j` clamps to the final Item. At the ends, movement simply stops — bounds are silent, unlike an unbound key (cli-tui.md §6.3). |
| `gg` / `G` | Newest / oldest Item. |
| `Ctrl-d` / `Ctrl-u` | Half a screen of tile rows. |
| `PgDn` / `PgUp` | A full screen of tile rows. |
| `Home` / `End` | Same as `gg` / `G`. |
| `Enter` | Preview (§5). |

Scrolling moves by whole tile rows; the viewport follows the selection with no scroll
margin, because tiles are tall enough that a margin would waste a third of the screen.
Rendering is on `Key`, `Resize`, `Sync` and `Tick` only (cli-tui.md §6.2) — the Gallery has
no animation and no idle redraw.

**Two invariants, both about not moving the ground under a Person's feet:**

1. **Arrival never changes the selection.** New Items sort to the top; the selected Item
   stays selected and stays where it is on screen — the viewport index shifts, the
   selection does not. Pressing `a` must apply what was under the cursor when the Person
   decided to press it, not whatever Ben added half a second earlier. This is a small
   scheduling detail with a consent-shaped consequence (§7).
2. **Reflow keeps the selection.** A resize recomputes `Geometry` and re-lays the grid
   around the same Item, re-centring the viewport on it.

After a Delete the selection moves to the next Item in sort order, or the previous one if
the deleted Item was last. After a Circle switch the Gallery opens at the newest Item.

#### 1.6 The favourites toggle

`F` toggles the Gallery between all Items and Favourites only. This is v0.1's **only**
filter, per ROADMAP's Gallery row.

- The title row changes from `42 Items · 3 unseen` to `7 favourites of 42`, so the filtered
  state is never invisible.
- The selected Item is preserved if it is in the filtered set; otherwise the selection
  moves to the nearest Item in sort order that is.
- **The filter is not persisted.** Opening the Gallery always shows everything. A remembered
  invisible filter is how People conclude their content has vanished, and kith's whole job
  is showing that it has not.
- Unfavouriting the selected Item while filtered does **not** make it disappear under the
  cursor. It keeps its place, loses its `★`, and the status row says
  `unfavourited — still shown until you leave the favourites view`. Pressing `f` again
  restores it. Re-entering the filter drops it.
- Empty: `No favourites yet. Press f on an Item to mark it — favourites are private to you.`

#### 1.7 Live arrival

`Change::ItemsChanged { circle, paths }` (ADR-0002 §5) triggers an incremental reduce of
the affected records, not a rescan. The Gallery then:

1. Re-sorts (a cheap insert; `order` is a `Vec<ItemId>` and arrivals almost always land at
   the front).
2. Marks genuinely new Items unseen per §3.
3. Requests thumbnails for any new Item that is inside or near the viewport (§2.3).
4. Redraws — keeping the selection put (§1.5).

Because metadata is ~250 bytes and a wallpaper is megabytes, records usually arrive first
(ADR-0004 §4.4 step 6): a tile appears with its title, attribution and unseen dot, and its
image fills in when the bytes land. That is walkthrough step 9 as a designed sequence
rather than a race.

`Change::Desynced` shows `resynchronising…` on the status row and rebuilds; the grid keeps
rendering the last good view meanwhile and re-lays out when the rebuild completes.

#### 1.8 Empty states

| Situation | Content area |
|---|---|
| No Circles | `No Circles yet. Run kith create <name>, or kith join <code> if someone invited you.` (cli-tui.md §4.11) |
| Circle with no Items | `Nothing here yet. kith add <paths…>, or wait — Ben's Items appear as they arrive.` — the Member name is the other Member's when there is exactly one, otherwise "the other Members'". |
| Joined, nothing arrived yet | `Waiting for the first Items. Ana's Device has to be online too.` plus the Circle's sync state on the status row. Never a bare empty grid: a Person who just joined and sees nothing needs to know whether that is a bug. |
| Favourites filter, nothing marked | §1.6. |

---

### 2. The thumbnail pipeline

The known performance trap for this product is re-encoding a 4–8 MB wallpaper on every
redraw (`research/tui-landscape` implication 4). The pipeline exists to make that
impossible.

#### 2.1 Two classes, two budgets

ADR-0003 §5 fixes two budget classes. This spec fixes their sizes:

| Class | Fits within | Used by |
|---|---|---|
| `Thumb` | 512 × 512 px, aspect preserved, never upscaled | Gallery tiles |
| `Full` | 2048 × 2048 px, aspect preserved, never upscaled | the Preview pane |

Both are **canonical, geometry-independent sizes**, not the current tile's pixel budget.
That is the point: a cached artefact keyed on tile geometry would be invalidated by every
resize, whereas a 512 px PNG re-scaled to a 144 × 85 px tile costs microseconds and
survives every reflow. The Provider is still asked for a budgeted preview — kith passes
`PixelBudget { 512, 512 }` or `{ 2048, 2048 }` — so a Provider never decodes at full
resolution for a thumbnail.

#### 2.2 Cache rules

```
$XDG_CACHE_HOME/kith/thumbs/<content-hash-hex>-thumb.png
$XDG_CACHE_HOME/kith/thumbs/<content-hash-hex>-full.png
```

`<content-hash-hex>` is ADR-0004 §4.1's BLAKE3 hash with the `b3:` prefix stripped —
64 hex characters. The shape is ADR-0003 §5's `<content-hash>-<class>.png` exactly.

| Rule | |
|---|---|
| **Keyed by content, not by path or Item id** | A rename, a move, or a Sync Engine relocation never invalidates a thumbnail. Two Items with identical bytes share one. |
| **Written by the core, not the Provider** | Providers stay stateless (ADR-0003 §5). The core receives `Preview::Image`, encodes the PNG, writes it. |
| **Written atomically** | `<hash>-<class>.png.tmp` in the same directory, `fsync`, `rename(2)`. Two kith processes never see half a PNG. |
| **Rebuildable, deletable, authoritative over nothing** | ADR-0001's authority rule. `rm -rf ~/.cache/kith/thumbs` costs re-decodes and nothing else. A truncated or unreadable entry is treated as a miss, deleted, and re-decoded — never a hole in the grid. |
| **Never inside a Circle root** | A thumbnail in the synced tree is bytes every Member pays to receive, forever, for a derived artefact. The cache lives outside every Circle by construction. |
| **Bounded** | On startup, a background sweep deletes entries whose hash is in no Collection and entries not read for 30 days, then evicts by oldest access time until the directory is under **512 MB**. `kith doctor`'s `cache.writable` check prints the current size. |
| **Optional** | If the cache directory is unwritable or the filesystem is full, kith decodes into memory, renders normally, and emits one `warn` note (`cache.unwritable`). Preview never fails because a cache could not be written. |

#### 2.3 Scheduling

```rust
pub enum Class { Thumb, Full }

pub struct Thumbs { /* cache dir, memory LRU, worker channel */ }

impl Thumbs {
    /// Never blocks the render loop. `Some` = ready to draw now.
    /// `None` = a job is queued or running; the caller draws a placeholder (§2.4).
    pub fn get(&mut self, item: &ItemView, class: Class) -> Option<Arc<DynamicImage>>;
    /// Queue ahead of need, at lower priority than anything visible.
    pub fn prefetch(&mut self, items: impl Iterator<Item = ItemId>, class: Class);
    /// Drop queued jobs for Items no longer near the viewport. Running jobs finish.
    pub fn retain(&mut self, keep: &[ItemId]);
}
```

- Decoding runs on `tokio::task::spawn_blocking`, which is also where every Provider call
  goes (ADR-0003 §1). Worker count is `min(4, available_parallelism())` — enough to fill a
  screen quickly, few enough that a scroll does not saturate a laptop.
- **Priority is visual.** Jobs are ordered by distance from the selection, so the tile a
  Person is looking at resolves first and the rest of the screen fills outward.
- Scrolling calls `retain` with the viewport plus one screen of margin; queued jobs outside
  it are dropped, not killed. A decode already running finishes and is cached — the work is
  paid for either way.
- Prefetch: one screenful ahead in the direction of travel for `Thumb`; the previous and
  next Item for `Full` while in Preview, so `j` and `k` are instant.
- Memory: an LRU of decoded images capped at **128 entries or 64 MB**, whichever binds
  first, plus at most **3** `Full` images (current, previous, next).
- `ratatui-image` protocol state (the encoded escape payload) is cached per visible tile,
  keyed by `(ItemId, tile_w, img_h)`, and dropped when the tile leaves the viewport. This
  is what keeps a scroll from re-encoding every image every frame.
- **Budget:** with a warm cache, a full screen of tiles paints in under 100 ms. With a cold
  cache, placeholders paint immediately and images land as they decode. There is no
  loading screen and no blocking spinner over the grid.

#### 2.4 Placeholder states

Three of them, visually distinct, all drawable on every rung:

| State | Tile shows |
|---|---|
| Bytes present, thumbnail not decoded yet | A dim `░` field. No spinner — a screenful of spinners is noise, and the image is about to appear. |
| Bytes not here yet (§4) | A dim `▒` field with a centred `↓`. |
| Bytes present, not decodable | A dim `▒` field with a centred `!`; Preview explains (§4.2). |

The caption row is always drawn, in every state. **The grid never has a hole**, which is
ADR-0003 §5's text-tier rule applied to tiles: an Item kith cannot picture is still an
Item kith can name and attribute.

---

### 3. Seen and unseen

Noticing that something new arrived *is* the wedge (ROADMAP Gallery row). The dot is the
whole notification system in v0.1, and it has to be right in two directions: it must not
cry wolf, and it must not tell the Circle anything.

#### 3.1 What marks an Item seen

An Item is marked seen when the Person **opens it in Preview**, or **performs any Action on
it** (Apply, Favourite, Delete, Reveal, Copy path). Both are deliberate engagements with
that specific Item.

Selecting a tile does not mark it seen; neither does scrolling past it, nor does its
thumbnail decoding. A dot that clears because a grid reflowed under a held-down `j` is a
dot nobody can trust.

There is no mark-all-seen key: cli-tui.md's keymap is closed and this spec may not add
one. The bulk path is Preview's adjacent-Item movement — `Enter`, then hold `j` — which
marks each Item as it is shown. A `mark all seen` affordance is v0.2, with Notifications.

#### 3.2 Where the state lives

Per-Circle, in `$XDG_STATE_HOME/kith/state.toml` — the local, unsynced, cheap-to-lose file
cli-tui.md §8.3 already defines as `last_circle` plus TUI leftovers.

```toml
# $XDG_STATE_HOME/kith/state.toml
last_circle = "kith-4npq7x2b"

[seen."kith-4npq7x2b"]
established = "1970-01-01T00:00:00.000Z"
items = [
  "01K1YFQ2M9CQ2E7B5NK0YH3RVD",
  "01K1YG04XKPM7A2ZQ0B8N6TE1F",
]
```

```rust
/// Unseen iff: not individually marked, not older than the baseline,
/// and not added by this Person — counting aliased duplicate `add`
/// records (ADR-0004 §4.4 step 3), so adopting a tree never dots itself.
fn is_unseen(seen: &Seen, circle: &CircleId, v: &ItemView, me: PersonId, now: Timestamp) -> bool {
    let (at, _) = sort_key(v, now);
    !seen.items(circle).contains(&v.id)
        && at > seen.established(circle)
        && !v.added_by_any_of(me)
}
```

| Rule | |
|---|---|
| **Baseline at create/join** | `kith create` and a completed `kith join` write `established = 1970-01-01T00:00:00Z` with an empty `items`. Everything the Circle contains is new to this Device, so everything arrives unseen — walkthrough step 9. |
| **Baseline on repair** | A Circle with no `[seen]` table (state lost, file deleted, first run of a kith that has one) gets `established = now` and an empty `items`, i.e. **everything currently known is treated as seen.** After losing local state kith cannot honestly claim anything arrived since the Person last looked, and manufacturing 200 dots would destroy the signal rather than preserve it. Degradation is toward quiet. |
| **Own content is born seen** | No write is needed; it falls out of the `added_by` clause. |
| **Ids are canonical** | The set stores canonical Item ids. If an alias merge changes which id is canonical, the group's marks are unioned — seen wins — so a duplicate tile that merges (ADR-0004 §4.5) does not re-dot itself. |
| **Compacted on write** | Ids no longer in the `CollectionView`, and ids whose `sort_at` is at or below `established`, are dropped. After a repair baseline the list is empty again. |
| **Written atomically, debounced** | Marking updates memory immediately — the dot vanishes on the keystroke — and flushes via temp-file-plus-`rename(2)` (ADR-0004 §3's descriptor protocol) after 2 seconds idle, on screen change, and at exit. |
| **Size** | ~30 bytes per marked Item. A 1000-Item Collection browsed exhaustively costs 30 KB, rewritten in one `write(2)`. |

*Call recorded here:* ADR-0004 §2 states that **kith writes exactly two files it cannot
rebuild** — `identity.toml` and `favourites.jsonl`. Seen-state does not become a third,
because its loss has a defined, honest resolution (the repair baseline above) rather than
being unrecoverable. It lives in `state.toml` rather than in `cache.sqlite3` for one
reason: cache rebuild is a routine repair (`Change::Desynced`, a schema bump, a corrupt
database), and silently clearing every dot on a routine repair would break the one feature
the wedge depends on. `state.toml` survives those.

#### 3.3 It is per-Person, and the Circle never learns it

The dot is a private local fact and is enforced the same way Favourites are (ADR-0004 §7):
**it is never written into a directory the Sync Engine watches.** Not into `.kith/`, not
into `.kith/local/`, not into a record log. No mis-edited ignore pattern, no engine
upgrade, and no configuration change can leak it, because there is nothing inside the tree
to leak. ADR-0004 §10 already committed to the consequence: *nothing is recorded about
reading* — no views, no "Ana looked at this", and "new since you last looked" is a local
notion, never a shared one.

The title row's count (`42 Items · 3 unseen`) is likewise local. It is a count of this
Person's dots on this Device, and it is not sent anywhere, shown to anyone, or derivable
by anyone.

---

### 4. Partially-synced and missing-byte Items

#### 4.1 The record is here and the bytes are not

There is no half-drawn image, ever, and the reason is structural: the Sync Engine stages
incoming content in temporary paths and `rename(2)`s it into place, so a path either has
its complete bytes or does not exist. Partial sync therefore shows up at exactly one
level — an Item whose record has arrived and whose bytes have not — and that is
`ItemView.bytes == None` (ADR-0004 §4.4 step 6).

| Surface | Rendering |
|---|---|
| Tile | §2.4's `▒` field with `↓`, plus the normal caption: markers, title. The Item is a full citizen of the grid — it sorts, it selects, it previews. |
| Preview | No image pane. The fact block renders every fact kith has, and the qualifier line reads `bytes not here yet`. |
| Title row | Unchanged. Circle-level transfer state is the status row's job. |

**kith shows no per-Item progress bar, because there is no per-Item figure to show.** The
Sync Engine reports completion per Circle and per peer (`PeerProgress`,
`FolderCompletion` — ADR-0002 §5), not per path. Inventing a per-Item percentage from a
Circle-level one would be a fabricated number, which cli-tui.md §7.5 forbids. The status
row carries the real one: `receiving 118 MB · 62%`.

Actions on a byte-less Item:

| Action | Availability |
|---|---|
| Favourite | **Available.** A Favourite is a mark on the Item, not on its bytes, and it must work before the bytes land — that is exactly when a Person decides they want it. |
| Delete | Available. Appends the tombstone (ADR-0004 §6); there are no bytes to remove. The confirm says so: `sunset has not arrived on this Device yet. Deleting removes it for every Member.` |
| Apply | `Unavailable: the bytes for this Item have not arrived yet.` |
| Reveal | `Unavailable: the bytes for this Item have not arrived yet.` |
| Copy path | `Unavailable: nothing is at that path yet.` The Item's recorded path is shown in the reason, so it can still be read. |

When the bytes land, `ItemsChanged` fires, the tile's placeholder is replaced by a
thumbnail, and every Action becomes available with no further interaction.

The reverse window is equally survivable and equally quiet: bytes deleted before their
tombstone arrives leave `bytes: None`, the tile becomes a placeholder, and it resolves
into a clean disappearance when the record lands (ADR-0004 §6).

#### 4.2 Bytes present but not renderable

A truncated download that finished, a `.png` that is not a PNG, an image beyond the decode
guard. The Provider returns `Err(ProviderError)`, and per ADR-0003 §5 the core synthesises
a text card from Sidecar facts rather than leaving a hole:

```
  !  sunset-4k
     added by Ana · today 09:14 · 3840×2160 · 1.9 MB · png
     preview unavailable — these bytes are not a readable image
     press ! for detail
```

**Decode guard:** kith refuses to decode content above **128 megapixels** or **512 MB** on
disk and renders the same card with `too large to preview (30000×30000)`. Apply is still
offered — the backend, not kith, decides whether it can set it, and refusing to try would
be kith inventing a limit it does not own.

---

### 5. Preview

`Enter` on a tile. One Item, as large as the terminal allows, with its Sidecar facts.
Entering Preview marks the Item seen (§3.1). ROADMAP: no zoom, no hash panel.

#### 5.1 Layout

```
┌ kith · walls ─────────────────────────────────── 42 Items · 2 unseen ┐
│                                                                      │
│                                                                      │
│                   [ image — centred, aspect preserved,               │
│                     letterboxed, never cropped, never                │
│                     upscaled past its own resolution ]               │
│                                                                      │
│                                                                      │
│  ★ sunset-4k                                                         │
│  added by Ana · today 09:14 · 3840×2160 · 1.9 MB · png               │
│  Attribution is what the adding Device claimed; kith cannot prove it.│
├──────────────────────────────────────────────────────────────────────┤
│ ● idle · 2 Members, 1 online                        kitty · swww     │
│ j k next/prev · a apply · f fav · d delete · esc back · ? keys       │
└──────────────────────────────────────────────────────────────────────┘
```

The fact block is three rows at the bottom of the content area; the image gets everything
above it. An image smaller than the pane is centred at its own size, never upscaled —
upscaling a 1280×720 wallpaper to fill a 4K terminal pane is a lie about what the Person
is about to apply.

#### 5.2 The facts

Row 1 — markers and title: `★ ● ? ↓` in that order where they apply, then the Sidecar
title, truncated with `…`.

Row 2 — the fact line, `·`-separated, in this fixed order:

| Field | Source | Rendering |
|---|---|---|
| attribution | `ItemView.added_by`, resolved to a Person through the Membership claims (ADR-0004 §5) | `added by Ana`; this Person → `added by you`; `adopted: true` → **`found by Ana`**, because she did not add it, she was the first to find it on disk; no Membership claim → `added by unknown Person (P56IOI7…)`, never blank |
| when | `ItemView.added_at` | `today 09:14`, `yesterday 21:03`, `3 days ago`, then absolute: `3 Aug 2026`. Clock-skewed (§1.3) → `dated 9 Aug 2026 (?)` |
| resolution | `facts.width × facts.height` | `3840×2160`; facts absent (a record from a newer or foreign writer) → `resolution unknown` |
| byte size | `ByteBinding.size` | SI: `847 B`, `12 kB`, `1.9 MB`, `12 MB`. One decimal below 10, none above. JSON surfaces carry integers (cli-tui.md §3.2) |
| format | `facts.format` | `png`, `jpeg`, `webp`; absent → omitted rather than guessed |

Row 3 — the qualifier line, dim. It carries the most specific honest thing there is to say
about *this* Item; when there is nothing specific, it carries the standing attribution
caveat. In priority order:

| Condition | Line |
|---|---|
| Bytes absent | `bytes not here yet` |
| Not decodable | `preview unavailable — these bytes are not a readable image` |
| Removed while open (§5.4) | `removed by Ben, 2 minutes ago — other Devices keep versions for 30 days` |
| Clock-skewed | `that Device's clock is ahead of yours; kith sorted this by arrival` |
| Adopted | `found on disk when this Circle was adopted — nobody added it through kith` |
| otherwise | `Attribution is what the adding Device claimed; kith cannot prove it.` |

That last line is the attribution equivalent of cli-tui.md §7.2's Role caveat, and
ADR-0004 §5 requires it: *the product says so wherever attribution is shown, in the same
voice it uses for Roles.* One dim line, one place, never a modal.

#### 5.3 Movement inside Preview

`j` `k` `←` `→` move to the adjacent Item in the Gallery's order without leaving Preview
(cli-tui.md §6.4), each marking the newly shown Item seen. `Full` images for the
neighbours are prefetched (§2.3), so held-down movement is smooth. `q`, `Esc` and `Enter`
return to the Gallery with that Item selected — the Gallery's viewport scrolls to it if
the Person walked out of view.

The favourites filter, if active, constrains Preview's movement to the same set. Anything
else would be a filter that does not filter.

#### 5.4 Removed while open

A tombstone can arrive for the Item currently in Preview. kith does not yank the screen:
the image is dimmed, the qualifier line becomes `removed by Ben, 2 minutes ago — other
Devices keep versions for 30 days`, and every Action except Copy path becomes
`Unavailable: this Item was removed from the Collection.` Returning to the Gallery finds
the tile gone. Being shown *what* disappeared and *who* removed it is the difference
between a sync product and a haunting.

---

### 6. Actions

#### 6.1 The v0.1 set

Five. ADR-0003 §3 splits them by implementer; the surface presents them identically.

| Action | `ActionId` | Key | Implemented by | Needs target |
|---|---|---|---|---|
| Apply | `wallpaper.apply` | `a` | the wallpaper Provider | yes |
| Favourite | `core.favourite` | `f` | the core | no |
| Delete | `core.delete` | `d` | the core | no |
| Reveal | `core.reveal` | `r` | the core | no |
| Copy path | `core.copy_path` | `y` | the core | no |

ADR-0003 §3's core list also names **open** (`xdg-open`). It is not in ROADMAP's Actions
row and not in cli-tui.md's keymap, so *it does not ship in v0.1* — recorded in §"Out of
scope" with its milestone rather than smuggled in because an ADR mentions it.

`Space` opens the action menu: Provider Actions first (Apply is the point of the product),
then core Actions in key order. Every entry shows its label, its key and its availability.
**Unavailable entries are shown, greyed, with their reason** — never omitted (ADR-0003 §3).
Pressing `Enter` on one prints the reason on the status row instead of doing nothing.

```
  ┌ Actions — sunset-4k ────────────────────────────────────────┐
  │ > Apply                                                  a  │
  │   Favourite                                              f  │
  │   Reveal on disk    unavailable: no desktop session      r  │
  │   Copy path                                              y  │
  │   Delete                                                 d  │
  │ j k move · enter perform · esc cancel                       │
  └─────────────────────────────────────────────────────────────┘
```

Every Action runs through `spawn_blocking` (ADR-0003 §1); the TUI never stalls. At most one
Action is in flight per Item (ADR-0003 §2); a second press reports
`apply already running for sunset-4k`.

#### 6.2 Favourite — `f`

Toggles the Person's private mark. Appends a `fav` or `unfav` record to
`$XDG_DATA_HOME/kith/favourites.jsonl` under the append protocol of ADR-0004 §§3, 7 —
`flock`, one `write(2)`, `fdatasync`.

- Instant: the `★` appears on the keystroke; the append is local and cannot fail for a
  remote reason.
- Status row: `★ favourited — private to you; nothing is announced.` on the first toggle of
  a session, then just `★ favourited` / `☆ unfavourited`. The promise is worth stating; it
  is not worth repeating forty times.
- Works with the Sync Engine down. Works before the bytes arrive. Never crosses the seam
  (ADR-0002 §2), and cannot: the file is not in a watched directory.
- Leaving a Circle does not consume Favourites (ADR-0004 §7); records for Items in a Circle
  that no longer exists are inert and dropped at the next compaction.

#### 6.3 Reveal — `r`

Shows the Item's bytes in the Person's desktop, in this order:

1. `org.freedesktop.FileManager1.ShowItems([file:///…], "")` on `/org/freedesktop/FileManager1`
   over the session bus — the freedesktop standard, honoured by Nautilus, Dolphin, Nemo and
   Thunar, and the only one that *selects* the entry rather than just opening its directory.
2. `xdg-open <containing directory>`.
3. `Unavailable: no desktop session on this Device — press y to copy the path instead.`

Rung 3 is the normal outcome over SSH, and the wording treats it as an environment fact,
not a defect.

#### 6.4 Copy path — `y`

Puts the absolute path of the Item's bytes on the clipboard: `wl-copy` under Wayland,
`xclip -selection clipboard` / `xsel -ib` under X11, OSC 52 otherwise (which is what works
over SSH and, with passthrough on, under tmux). If all three fail, the path is printed on
the status row so it can be selected with the mouse, and the status row says so.

#### 6.5 Delete — `d`

Always confirms, and the confirmation defaults to no (cli-tui.md §6.3). The honesty lives
in the confirm, not in a footnote:

```
Delete sunset-4k from walls? This deletes it for every Member.
Other Devices keep the last 5 versions for 30 days; v0.1 has no restore. [y/N]
```

When the Item was added by someone else, ADR-0004 §6's rule applies — kith warns and
confirms, never refuses, and never pretends a refusal would have stopped anyone:

```
sunset-4k was added by Ana. Deleting removes it for her too.
Roles are agreements, not enforcement — kith cannot prevent this, only recover it.
Other Devices keep the last 5 versions for 30 days; v0.1 has no restore. [y/N]
```

On confirm, in ADR-0004 §3's order — **record before bytes on remove**:

1. Append a `remove` record to this Device's own log.
2. Delete the bytes on disk.
3. Drop the tile, move the selection (§1.5), status row `removed sunset-4k from walls`.

Step 1 succeeding and step 2 failing (a read-only filesystem, bytes this Device may not
remove) leaves
a tombstoned Item whose bytes are still on disk. That is a state ADR-0004 §6 already
handles: the Gallery is clean because the tombstone is authoritative, and `kith doctor`
reports `N removed Items still have bytes on disk`. The status row says
`removed from the Collection, but the bytes could not be deleted — see kith doctor`.

#### 6.6 Apply — `a`

The Action the product exists for, and the one with the most ways to go wrong.

```
press a
  │
  ├─ actions() says Unavailable ──────────► status row: the reason, verbatim (§6.8). Nothing happens.
  │
  ├─ bytes not here yet ──────────────────► "the bytes for this Item have not arrived yet"
  │
  └─ apply_targets()  (called now, never cached — monitors hotplug)
       ├─ 0 targets ──────────► Unavailable: the backend reports no outputs
       ├─ 1 target  ──────────► apply straight to it; no picker for a decision with one answer
       └─ ≥2 targets ─────────► Monitors overlay
```

```
  ┌ Apply sunset-4k to ─────────────────┐
  │ > All monitors                      │
  │   DP-1        Desk left             │
  │   HDMI-A-1    TV                    │
  │ j k move · enter apply · esc cancel │
  └─────────────────────────────────────┘
```

- `All monitors` is first and is the default selection: it is what most People mean, and it
  is the only option every backend can honour.
- Labels come from `[provider.wallpaper.monitors]` in config (cli-tui.md §8.2); an output
  with no configured label shows its raw name alone.
- Backends that cannot address a monitor (GNOME, KDE, feh in v0.1 — ADR-0003 §4) return a
  single `AllMonitors` target, so the picker never opens and the Person is never offered a
  choice the backend cannot execute. The Monitors row of `kith doctor` states this plainly.
- `Esc` cancels. **Cancelling is not a failure**: no status line, no note, no record.

#### 6.7 Apply feedback

| Phase | Status row |
|---|---|
| In flight | `applying sunset-4k to Desk left…` with a spinner (`Tick` at 250 ms, cli-tui.md §6.2) |
| Succeeded | `✓ applied sunset-4k to Desk left` for 4 seconds, then the row reverts |
| Failed | `✗ Apply failed: swww exited 1 — press ! for detail` for 4 seconds, then the row reverts; the detail persists |
| Timed out | `✗ Apply timed out after 120s — press ! for detail` |
| Cancelled | nothing |

`!` opens the Detail overlay (cli-tui.md §6.4) carrying `ActionError.detail` — the
backend's stderr tail (ADR-0003 §3). `y` inside it copies the text, because the next thing
that happens is a Person pasting it into an issue.

**A failed Apply changes nothing and records nothing** (ADR-0003 §1): the previous
wallpaper is still on screen, no state moved, and pressing `a` again is safe. There is no
half-applied state to recover from because the Provider contract forbids one.

**A successful Apply also records nothing shared.** No `applied` record, no Activity entry,
no marker on the tile. ADR-0004 §10 is categorical — nothing is recorded about reading or
using — and a "currently applied" marker would be the first exception. It is listed out of
scope with its milestone rather than added quietly.

#### 6.8 When no backend exists, or it fails

The distinction matters and the surface keeps it:

| | Detection | What the Person sees |
|---|---|---|
| **Missing** | ADR-0003 §4's ladder found nothing, or config names a backend that is not detected | `Apply` is declared `Unavailable` with its reason: `no wallpaper backend found (probed gsettings, plasma-apply-wallpaperimage, swww, hyprpaper, swaybg, xwallpaper, feh). Set [provider.wallpaper.custom] in ~/.config/kith/config.toml.` The status row's right side permanently reads `no apply backend`. |
| **Failed** | A detected backend ran and returned non-zero, or timed out | `✗ Apply failed:` plus the first line of stderr, with the full tail under `!`. The backend name is always in the message: a Person who can see `swww exited 1` can search for it. |

A configured-but-undetected backend never silently falls back to another one
(cli-tui.md §8.2): `configured backend "swww" not detected` is the reason, because a Person
who configured `swww` and got `feh` has been lied to.

**Nothing else degrades.** With no backend at all, browsing, thumbnails, Preview,
Favourites, Delete, Reveal, Copy path and sync are untouched — kith without an apply
backend is still a shared gallery, and the wording never implies the product is broken
(ADR-0003 §4, cli-tui.md §7.4).

---

### 7. Apply is always local and always deliberate

CONTEXT.md's rule: *content arriving from a Circle never changes what is on a Person's
screen without that Person having asked for it.* In v0.1 this is satisfied **structurally**
— not by a policy, not by a setting, and not by a consent dialogue. Five layers, each of
which a reviewer can check:

1. **The Sync Engine module cannot reach the Provider registry.** ADR-0003 §6.1 makes the
   dependency direction one-way: `engine` does not depend on `provider`. "Arriving content
   applies itself" is not forbidden, it is *unrepresentable*. The Gallery is the only module
   that holds both, and the Gallery reads the engine and performs on the Provider — never
   the reverse.

2. **`Cmd::Perform` is constructed in exactly one place: the key handler.**

   ```rust
   pub enum Cmd { Perform { action: ActionId, item: ItemId, target: Option<ApplyTarget> },
                  Decode  { item: ItemId, class: Class },
                  Repaint, Quit, /* … */ }

   impl App {
       fn on_key(&mut self, k: KeyEvent) -> Option<Cmd>;      // the only producer of Cmd::Perform
       fn on_sync(&mut self, c: Change)   -> Option<Cmd>;     // returns Decode / Repaint only
       fn on_tick(&mut self)              -> Option<Cmd>;     // returns Repaint only
   }
   ```

   `grep -n 'Cmd::Perform' src/` must only ever hit `on_key` and its overlay handlers. That
   is a one-line invariant a reviewer can enforce and a test can assert, which is worth more
   than a paragraph of intent.

3. **There is no automation to consent to.** v0.1 has no rotation, no scheduler, no import
   watch, no `--apply` flag on `kith add`, no `apply_on_arrival` config key, and no
   scriptable `kith apply` (all deferred; ROADMAP §2's cut table, cli-tui.md "Out of
   scope"). The config file is closed at three settings and adding a fourth is a ROADMAP
   change, not an implementation detail. **The design's obligation is not to create one** —
   which is why this spec adds no "apply newest", no "apply on favourite", and no
   currently-applied marker whose natural next feature is keeping it up to date.

4. **Looking is not acting.** Opening Preview marks an Item seen in a local file (§3) and
   does nothing else. Arrival changes exactly one thing on this Device: a dot appears.

5. **Selection is stable under arrival** (§1.5). An Item landing at the top of the sort
   while a Person's finger is over `a` does not become the thing that gets applied. Consent
   is about the specific Item a Person chose, and a race that substitutes a different one
   violates the rule just as surely as an auto-apply feature would.

The v1.0 consent framework (approve-before-apply as machinery) exists for the day something
can push. In v0.1, nothing can, and the honest implementation of "consent" is the absence
of a mechanism rather than a checkbox in front of one.

---

### 8. Down the preview ladder

`ratatui-image` chooses the rung: environment variables, then a terminal query, then
halfblocks (ADR-0001; `research/tui-landscape` §4.1). Detection runs once at startup. Cell
size is re-queried on `Resize`; the rung is not, because a terminal does not change
protocol mid-session.

#### 8.1 What the Person actually sees

| Rung | Where it lands (`research/tui-landscape` §3) | Gallery | Preview |
|---|---|---|---|
| **kitty** | kitty, Ghostty, Konsole (subset), WezTerm (opt-in) | Full-colour, full-resolution tiles. Images are placed by cell and do not tear on scroll. Under `$TMUX`, kith uses Unicode-placeholder mode, the one pixel path designed to survive a multiplexer. | Full-pane image at the terminal's real pixel resolution. |
| **iTerm2** | WezTerm, Konsole | Full-colour tiles, indistinguishable from kitty at these sizes. | Full-pane image. |
| **sixel** | foot, xterm, Konsole, WezTerm | Full-colour but palette-limited — smooth gradients band visibly, which on wallpapers is the most noticeable artefact. The bottom content row is reserved (§1.2). | Full-pane image, same banding. |
| **halfblocks** | GNOME Terminal, Alacritty, anything under plain tmux — the largest population on desktop Linux | Two pixels per cell: a 24×6 tile is 24×12 pixels of image. Composition and palette are legible; detail is not. Tiles are deliberately larger on this rung (§1.2) to spend the terminal's budget on fewer, better pictures. | An 80×20 content area gives 80×40 pixels. Coarse — and the fact block carries every detail the pixels cannot. |

Halfblocks is the **shipped fallback**, not a defect: ADR-0001 promises kith is never
unusable because of a terminal, and this is that promise. The words *unsupported*, *error*
and *failed* are banned in this context (cli-tui.md §7.3).

#### 8.2 How kith says which rung — and how it stops there

Three places, all passive:

1. **The status row's right side, permanently**: `kitty`, `iterm2`, `sixel`, or
   `halfblocks (degraded)`, sitting next to the apply backend. Both degradations are always
   visible, so neither is ever discovered at the moment of failure.
2. **`kith doctor`**, which explains it once, in full, with the way out (cli-tui.md §5's
   `preview.protocol` check): *Images render as coloured blocks. Degraded, never broken.
   kitty, WezTerm, foot and Ghostty give full-resolution previews.* Under tmux with a
   pixel-capable terminal, `doctor`'s fix line names `allow-passthrough on` — the setting is
   off by default and is the single most common cause of an unexpectedly low rung.
3. **`?` help**, one line.

And the rules that keep it from nagging:

- No modal, no toast, no first-run banner, no repetition. The rung is a **fact on the
  status row**, in the same voice as the Circle's sync state.
- Never a blocking dialogue, never a "your terminal is not supported" screen, never a
  prompt to switch terminals.
- The rung is never mentioned in an Action failure message; it has nothing to do with Apply.
- kith does not degrade its own behaviour beyond the pictures. Every fact, marker, Action
  and key is identical on all four rungs. A Person on GNOME Terminal is using the same
  product, at lower resolution.

There is **no configuration key to force a rung** in v0.1: cli-tui.md §8.2's file is closed
at three settings, and detection that misfires is a `doctor` report, not a knob. Deferred
with its milestone.

---

## Edge cases & failure honesty

| Situation | What happens |
|---|---|
| Terminal resized mid-decode | Geometry recomputes, the selection is preserved, in-flight decodes finish into the cache (they are geometry-independent, §2.1) and are re-scaled to the new tile size. Nothing is discarded. |
| Terminal resized below 60×18 | The grid stops drawing; the content area carries the measured size (§1.2). The frame stays. Resizing back restores the selection. |
| Cache directory unwritable or disk full | Decode into memory, render normally, one `warn` note (`cache.unwritable`). Never a failure (§2.2). |
| Truncated or corrupt cache entry | Treated as a miss: deleted and re-decoded. Never a hole (§2.2). |
| Content that is not a readable image | Text card with the Sidecar facts and `preview unavailable` (§4.2). Apply is still offered. |
| A 30000×30000 px image | Decode guard: `too large to preview (30000×30000)`. kith does not attempt it, does not hang, and does not stop the Person applying it (§4.2). |
| Animated GIF or APNG | First frame only. There is no animation in the Gallery in v0.1, and none is planned before the second Provider. |
| Portrait content shot on a phone | EXIF orientation is applied at decode, so it is not sideways. `facts.width/height` are the *oriented* dimensions, so the Preview line and the pixels agree. |
| SVG, or anything else the Provider does not `claims()` | Not an Item, so not a tile (§1.1). `kith add` already refused it with a reason (cli-tui.md §4.6). |
| Two tiles for one wallpaper during adoption | Real and expected while two Devices' logs cross (ADR-0004 §4.5). They merge into one tile when the peer's log arrives; the surviving tile keeps the union of the pair's seen and favourite marks (§3.2). No warning is shown for a state that resolves itself in seconds. |
| An Item whose bytes are re-encoded in place | The content hash changes, so a new thumbnail is decoded and the old one is swept by the 30-day sweep. The Item id, its attribution and its Favourite are unaffected — which is the whole point of hashing bytes and identifying Items separately (ADR-0004 §4.1). |
| Item removed by another Member while it is in Preview | The image dims, the qualifier line names who and when, Actions go unavailable except Copy path (§5.4). |
| Item removed while it is merely selected in the Gallery | The tile disappears; the selection moves to the next Item in sort order. No dialogue — a Person who was not looking at it does not need to be interrupted. |
| Favourite on an Item that is later removed | The `fav` record survives; the Item does not appear in any view; the record is dropped at the next compaction. Harmless, and re-favouriting a restored Item is idempotent. |
| Two kith processes running at once | Favourites append under `flock` (ADR-0004 §3). `state.toml` is temp-file-plus-rename, last writer wins; the cost is a few dots, which is exactly what §3.2's degradation rule already tolerates. |
| Sync Engine offline | Everything in this document works: browse, thumbnails, Preview, Favourite, Apply, Delete, Reveal, Copy path. All of it reads the tree and the cache (ADR-0002 §6). The status row carries cli-tui.md §7.1's line; deletions and additions sync when the daemon returns. |
| `Change::Desynced` | `resynchronising…` on the status row; the grid keeps rendering the last good view; the seen set survives, because it is not in the cache (§3.2). |
| Every Item unseen after joining a Circle with 200 Items | Correct and intended: they are all new to this Device (§3.2). Clearing them is Preview plus held `j`; a bulk affordance is v0.2. |
| No dots at all after `~/.local/state` was wiped | Correct and intended: kith cannot honestly claim anything arrived since the Person last looked, so it says nothing (§3.2's repair baseline). |
| An unbound key | `no binding for 'z' — press ? for keys` (cli-tui.md §6.3). Grid movement at a boundary is not an unbound key and stays silent. |

---

## CLI/TUI surface touchpoints

The keymap, the frame, the screen stack, the overlays, the exit codes and the wording rules
are `docs/spec/cli-tui.md`. This spec supplies the semantics behind the keys it uses and
adds none.

**Keys this spec gives meaning to** (all defined in cli-tui.md §6.4):

| Key | Screen | This spec's section |
|---|---|---|
| `h j k l` `← ↓ ↑ →`, `gg` `G`, `Ctrl-d` `Ctrl-u`, `PgDn` `PgUp`, `Home` `End` | Gallery | §1.5 grid semantics |
| `Enter` | Gallery → Preview; Preview → Gallery | §5, and it is what marks an Item seen (§3.1) |
| `j k ← →` | Preview | §5.3 adjacent-Item movement |
| `F` | Gallery | §1.6 favourites filter |
| `a` | Gallery, Preview | §6.6 Apply, §6.7 feedback |
| `f` | Gallery, Preview | §6.2 Favourite |
| `d` | Gallery, Preview | §6.5 Delete |
| `r` | Gallery, Preview | §6.3 Reveal |
| `y` | Gallery, Preview | §6.4 Copy path |
| `Space` | Gallery, Preview | §6.1 action menu |
| `!` | anywhere | §6.7 last-failure detail |

**CLI equivalents, at synopsis level.** Full specification of each is cli-tui.md's.

```
kith list items [--circle <CIRCLE>] [--json]     # the Gallery's rows: same sort, ● and ★ columns
kith doctor [--json]                             # preview.protocol, preview.cell_size,
                                                 # apply.session/backend/monitors, cache.writable
kith add [--circle <CIRCLE>] <PATH>...           # creates Items; born seen for the adding Person (§3.2)
```

`kith list items` is the only CLI window onto this surface in v0.1, and it is deliberately
a *view*, not a controller: it can show that an Item is a favourite and unseen, and it
cannot change either. The Actions in §6 are TUI-only.

**Scriptable Actions are v0.2, not v0.1.** `kith apply <item-ref> [--on <monitor>]`,
`kith fav <item-ref>` and `kith browse` are named in ADR-0003 §3 and placed in v0.2 by
ROADMAP §2 (CLI parity arrives when rotation needs it) and by cli-tui.md's "Out of scope"
table. Item-ref grammar is already fixed (cli-tui.md §1.4) so those verbs will not have to
redesign addressing when they land.

---

## Coverage — ROADMAP → this spec

Every clause of ROADMAP §2's Gallery, Preview and Actions rows, and nothing that is not one.

| ROADMAP clause | Section |
|---|---|
| Gallery — TUI grid of the Collection's Items | §1.1, §1.2 |
| Gallery — image thumbnails on the preview ladder | §2, §8 |
| Gallery — sorted by date added | §1.3 |
| Gallery — favourite marker | §1.4, §6.2 |
| Gallery — unseen-Item dot | §3 |
| Gallery — no grouping, no filtering beyond a favourites toggle, no infinite scroll | §1.6; the whole Collection is held in one `Vec<ItemId>` and the grid scrolls it — nothing is paged, and there is nothing to page |
| Preview — fullscreen, one Item | §5.1 |
| Preview — Sidecar facts: title, who added it, when, resolution, byte size | §5.2 |
| Preview — no zoom, no hash panel | Out of scope, below |
| Actions — Apply with a monitor picker | §6.6 |
| Actions — Favourite, private to the Person | §6.2, §3.3 |
| Actions — Reveal on disk | §6.3 |
| Actions — Delete | §6.5 |
| Actions — nothing a Member adds ever changes another Person's screen | §7 |
| Actions — no share, duplicate, move, restore | Out of scope, below |
| Providers — unavailable Apply explains itself | §6.8 |

Against the §3 walkthrough: step 9 (tiles arriving, marked unseen) is §1.7 plus §3.2's
join baseline; step 10 (`Enter`, the fact line, `f`, and Ana learning nothing) is §5.2,
§6.2 and §3.3; step 11 (`a`, two monitors, the picker) is §6.6; step 12 (Ana's Gallery
shows it unseen and her screen does not change) is §3 plus §7.

---

## Out of scope for v0.1

Named with the milestone that gets them, so each can be refused by pointing at a line.

| Deferred | Why | Returns |
|---|---|---|
| Grouping, tags, search, sort flags, alternative orders | ROADMAP Gallery and Search rows: a v0.1 Collection is tens of wallpapers | v0.3 |
| A second filter of any kind | Favourites is the one ROADMAP names | v0.3 with Search |
| Mark-all-seen, per-Circle unseen badges, desktop notifications | ROADMAP's Notifications cut; the dot is the whole mechanism in v0.1 | v0.2 |
| Zoom, pan, hash panel, full metadata inspector in Preview | ROADMAP Preview row states both exclusions | v0.3 |
| Title editing and rename | ADR-0004's `meta` record is reserved and unwritten in v0.1 | v0.3 |
| The `open` Action (`xdg-open` on the Item) | In ADR-0003 §3's core list, in neither ROADMAP's Actions row nor cli-tui.md's keymap | v0.2 with CLI Action parity |
| Share, duplicate, move between Collections, restore/undo | ROADMAP Actions row; restore needs History and Role honesty designed together | v0.3 |
| A "currently applied" marker on a tile | Nothing is recorded about using an Item (ADR-0004 §10), and the marker's natural next feature is keeping it live | v0.2 with rotation |
| Per-Item sync progress | There is no per-Item figure; the engine reports per Circle and per peer (§4) | not planned |
| Multi-select and bulk Actions | Every v0.1 Action is safe on one Item and dangerous on forty | v0.2 |
| Mouse support (click to select, scroll wheel) | ROADMAP's TUI surface is keyboard-first; a second input model is a second thing to keep true | v0.2 |
| Conflict copies shown as an Item or a resolve affordance | ADR-0002 §2 defers resolution to the Health screen; v0.1 counts them in `status`/`doctor` | v0.2 |
| Animation (GIF/APNG playback), Überzug++ as a fifth rung, a config key to force a rung | ADR-0001 defers Überzug++ until a real gap is reported; the config file is closed at three settings | v0.2 at the earliest |
| Thumbnails shared through the Circle | Derived bytes every Member would pay to receive, forever (§2.2) | not planned |
| Scriptable `kith apply`, `kith fav`, `kith browse` | ROADMAP puts CLI Action parity behind rotation needing it | v0.2 |
