# SPEC — periphery modules (shallow)

- **Status:** Accepted
- **Date:** 2026-08-07
- **Resolves:** [#17 Spec: periphery modules (shallow)](https://github.com/opx0/wp-sync/issues/17)
- **Informed by:** `ROADMAP.md` §§2 and 4, `CONTEXT.md`, ADR-0001 – ADR-0004, the product universe (map #1)

## Purpose

Seven modules are named in the product universe, cut from v0.1 by ROADMAP §2, and therefore
unspecced: Activity, Notifications, Health, Automation, Search, Configuration and the Plugin
system. Unspecced is dangerous in exactly one way — it is indistinguishable from *forgotten*,
and a forgotten module does not stay unbuilt. It gets built accidentally, one convenient hook
at a time, inside the modules that did ship.

This file makes the periphery **consciously shallow**. Per module it fixes four things and
nothing else: what it is in kith's vocabulary, which milestone lands it, the one constraint it
inherits from a locked ADR, and what it would be a mistake to build early. It fixes no flows,
no screens, and no signatures beyond the ones that already exist.

**Depth budget: roughly 25 lines of prose per module.** A section that wants to grow is not one that needs
more room; it is a module whose milestone has arrived. When it does, it **graduates** to its own
`docs/spec/<module>.md` and the section here collapses to a pointer line. Growing a section in
place is the exact failure this document was written to prevent.

Nothing in this file is in v0.1. That is restated formally at the end so any of it can be
refused by pointing at a line.

## The organizing fact: the periphery reads

Every module below is a *view* over state that v0.1 already writes. That is not a coincidence —
it is the property that makes the periphery cheap to defer and safe to add.

| Module | Lands in | Reads (all of it exists at v0.1) | Writes |
|---|---|---|---|
| **Activity** | v0.2 | `add`/`remove` records, member claims, the change feed | nothing |
| **Notifications** | v0.2 | the change feed | nothing |
| **Health** | v0.2 (the screen) | `MetadataHealth`, `SyncEngine::status`/`devices`, the version archive | nothing |
| **Automation I** | v0.2 | Favourites | the rotation cursor, in the rebuildable cache |
| **Automation II** | v0.3 | config rules, a watched local directory | Items, through the existing import path |
| **Search** | v0.3 | the derived `CollectionView` | a cache index, rebuildable |
| **Configuration** | v0.1 minimal → v1.0 as a module | `config.toml` | nothing — kith never writes it |
| **Plugins** | v1.0 | provider manifests | nothing inside `.kith/` |

Exactly two writes, both narrow: rotation moves a cursor in `$XDG_CACHE_HOME/kith/cache.sqlite3`
(ADR-0003 §7), and import watch calls the same import path `kith add` already uses (ADR-0004
§3). **No periphery module introduces a new writer into `.kith/`.** That is what keeps ADR-0004's
W1 — one writer per record log — intact as the product grows. A periphery feature that genuinely
needs a new record kind may have one, additively per ADR-0004 §11; it is a reviewable decision,
never a convenience.

## Domain objects involved

| Object | How the periphery touches it |
|---|---|
| **Circle** | The scope of every view here. It is the boundary of trust, and therefore also the boundary of a result list, a timeline and a health panel. |
| **Person / Member / Role** | Named in Activity and Notifications by attribution resolved through the roster (ADR-0004 §5); every Role shown carries the caveat (ADR-0002 §4). |
| **Item / Sidecar** | The derived `ItemView` is the row Search filters, the line Activity narrates, and the placeholder Health explains. |
| **Favourite** | Automation's entire pool, and the mechanism by which consent is structural (ADR-0003 §6). Local, never synced, never announced. |
| **Provider / Action / Apply** | The Plugin system's first two capabilities, already designed as ADR-0003 §2's frozen protocol. Rotation is the wallpaper Provider's, not Automation's. |
| **Sync Engine** | Health's live half and Notifications' only event source, through `Change` (ADR-0002 §5). No periphery module talks past the seam. |
| **Activity** | The only glossary term in this file that *is* one of these modules — derived, never an authoritative log. |

---

## Behaviour

### 1. Activity — v0.2

**What it is.** The record of what has happened in a Circle: Items added, Items removed, Members
joined (CONTEXT.md). A view, never a log anyone is authoritative about.

**Lands in** v0.2 — ROADMAP §4: *"Activity (timeline derived from Sidecars)"*.

**Constraint carried.** ADR-0004 §10 already fixed both the derivation and its honesty, and left
Activity nothing to invent. The timeline has two halves that must stay visually distinct because
only one survives a cache rebuild: the **durable, Circle-wide** half (`add` and `remove` records,
member claims' `asserted`) replays identically on every Device; the **ephemeral, Device-local**
half (peers connecting, sync errors, arrival times, from the change feed) starts when the process
started. Four limits ship printed next to the feature, not discovered: no happens-before, so
two Members acting in the same second may show in either order (wall-clock order plus §4.4's
24 h skew clamp); **record time, not arrival time**, so a week offline arrives as a week of
correctly dated history at once; nothing is recorded about reading; only what reached this Device.

**Anchor.** Activity reduces from the same `CollectionView` the Gallery already holds, plus the
`Envelope`s retained in memory since process start. No new query path, no new storage.

**Mistake to build early.** An activity log — a synced, append-only stream written *for*
Activity. It adds a writer, duplicates facts the records already carry, and falsifies ADR-0004
§10's rule that no record is written for Activity's benefit. Equally a mistake: unread state
shared across the Circle (the unseen dot is local by construction, and "new since you last
looked" is a local notion), and anything that narrates reading — views, "Ana looked at this",
or a Favourite, which §7 makes structurally impossible rather than merely omitted.

### 2. Notifications — v0.2

**What it is.** A Person being told, away from the screen they are looking at, that something
happened in a Circle: a new Item, a Member joined, a sync error. Not a domain object — a
rendering of Activity's ephemeral half at the moment it happens.

**Lands in** v0.2 — ROADMAP §4: *"Notifications (new Item, Member joined)"*. ROADMAP §2 is
explicit that desktop-notification integration is **a new platform surface**, admitted
deliberately at v0.2 and never by an earlier convenience.

**Constraint carried.** ADR-0003 §6 layer 1 — the Sync Engine module holds no reference to the
Provider registry, so arriving content structurally cannot reach a screen. A notification is a
sentence, never an Action: it may open the Gallery on an Item and may never Apply one, and its
payload carries an Item ref, not a path to bytes. ADR-0003 §7's daemonless rule bounds delivery
just as hard: kith is a binary you run. **Notifications exist only while a kith process is
alive.** Nothing is queued for later delivery; the Gallery's unseen-Item dot stays the durable
channel and the docs say so plainly rather than letting People infer it from silence.

**Anchor.** One seam, `trait Notifier { fn notify(&self, note: &Note); }`, with two implementors:
an in-frame toast (always present) and a desktop notifier over the freedesktop notification
interface, opt-in through a `[notifications]` block that arrives with this feature (§6).

**Mistake to build early.** A background agent that watches Circles while kith is closed — a
daemon wearing a different noun, and the maintenance class ADR-0001 identifies as this product
type's killer. Also: delivery or read receipts per Member (nothing about reading is recorded);
and one notification per Item during a 400-Item first sync, when the honest unit is one per
Circle per burst.

### 3. Health — v0.2 (the screen)

**What it is.** The Circle's answer to *is this working right now*: sync state, per-Member
completion, conflict copies, removed Items whose bytes are still on disk, orphans, and how much
the version archive is holding.

**Lands in** v0.2 — ROADMAP §4: *"the Health screen and a Dashboard"*. The diagnosis half already
shipped: `kith doctor` and `kith status` are v0.1 under the Sync Engine, specced in full in
`docs/spec/cli-tui.md` §5 (sixteen checks, seven sections). Health does not re-ask them.

**The division of labour, fixed here because it decides what goes where:** *`doctor` asks whether
this **Device** is set up correctly; Health asks whether this **Circle** is healthy.* One is a
one-shot, stateless, exit-coded check a stuck Person runs before anything works; the other is a
live view of state the TUI is already subscribed to. `doctor` never grows a live Circle panel,
and Health never re-implements a setup check.

**Constraint carried.** ADR-0004 §4.4's reducer already emits `MetadataHealth` — unparseable
lines, records from a newer schema, sequence gaps, forked logs, absorbed conflict copies,
records written by a Device that is not a Member (§5), clock-skewed records. Health is that
struct beside `SyncEngine::status`/`devices`, rendered. ADR-0004 §8's **absorb, never resolve**
rule then sets the ceiling on what the screen may *do*: it reports, and it points at the version
archive. Only the owning Device ever deletes its own conflict copy, and no surface rewrites a
record log — that is W2, and it is what makes conflicts absorbable at all.

**Mistake to build early.** A repair engine. A "resolve conflicts" button that merges or rewrites
logs destroys the append-only property the whole metadata design rests on. Also: a storage
manager (pruning the version archive is the engine's `cleanoutDays`, not kith's business), and a
health *score* — a number that averages "0 Members online" with "a forked log" is worse than the
two sentences it replaces.

### 4. Automation — v0.2 (I) and v0.3 (II)

**What it is.** The module that reduces manual work. Its most visible member is **not here**:
rotation belongs to the wallpaper Provider and is already specced in ADR-0003 §7 as `kith
rotate`, a one-shot verb driven by the host scheduler. What remains for this module is the rest —
rules ("apply a Favourite from *walls* at sunrise"), import watch (a local directory outside any
Circle whose new content becomes Items), cleanup and duplicate detection.

**Lands in** v0.2 for **Automation I** — ROADMAP §4: *"local wallpaper rotation (Automation I),
and CLI parity (`kith apply`, `kith browse`) so rotation is scriptable"*. v0.3 for **Automation
II** — *"Automation II (rules, import watch)"*.

**Constraint carried, twice.** (1) **Consent is structural** (ADR-0003 §6): any automation whose
effect reaches a screen draws only from the Person's Favourites. A rule may choose *which*
Favourite and *when*; it may never widen the pool, and unfavouriting withdraws consent
immediately. ADR-0002 §7 already applied this once — `kith adopt` retires wp-sync's systemd path
unit precisely because auto-apply without consent contradicts the rule. Import watch may create
Items; it may never Apply one. (2) **kith is a verb, not a daemon** (ADR-0003 §7): the schedule
lives in the host scheduler, and rules are evaluated by a `kith` process something else started.
Automation II ships as declarative TOML plus a foreground `kith watch` that dies with its
terminal, with example systemd user units in packaging — never a resident kith.

**Mistake to build early.** A rules DSL, a cron parser, or a trigger/condition/effect engine
inside kith: the scheduler already exists, is someone else's tested product, and composition over
reinvention is a product principle. Also a mistake — and this one is a *rejection*, not a
deferral — cleanup and duplicate detection as the universe names them. Destructive automation
over content other Members added is exactly where "a Role is a policy, not an enforcement"
(ADR-0002 §4) turns dangerous, and duplicates are already the reducer's job: ADR-0004 §4.4 step 3
aliases Items by content hash, so a separate detector would re-answer a settled question,
differently, with `rm`.

### 5. Search — v0.3

**What it is.** Finding Items in a Collection by facts kith already holds: title, tags, who added
it, when, resolution, favourite.

**Lands in** v0.3 — ROADMAP §4: *"tags and Search"*. Tags are the gate, not a companion: ADR-0004
reserves the `meta` record and writes none in v0.1 (§4.2, §11), so until that record ships the
only searchable fields are the ones the Gallery already sorts and shows.

**Constraint carried.** ADR-0001's authority rule. Search reads the derived `CollectionView` and
may keep an index in `$XDG_CACHE_HOME/kith/cache.sqlite3` — where an index is welcome precisely
because it is rebuildable and deletable (ADR-0004 §9). **No search artefact ever enters the
synced tree.** A shared index would be a multi-writer path, which is the one thing ADR-0004
exists to make impossible, and it would leak one Person's queries into a directory every Member
holds byte-for-byte — the same objection that put Favourites outside the tree (§7).

**Anchor.** A filter struct over the view, not a language: `kith list items --tag <t> --by <person>
--since <date>` on the CLI, and a `/` filter line over the Gallery grid. Matching is
substring, case-insensitive, over already-materialised fields; results are Items in the current
Circle.

**Mistake to build early.** A query language, a full-text engine, or fuzzy ranking. A v0.1
Collection is tens of Items and a v0.3 one is hundreds — a linear scan over a materialised view
beats the index that would replace it, and beats it while being deletable. Also a mistake:
results spanning Circles in one list, because the Circle is the boundary of trust and a result
row that crosses it teaches the wrong mental model; and saved searches as synced state.

### 6. Configuration — minimal in v0.1, a module at v1.0

**What it is.** One human-authored TOML at `$XDG_CONFIG_HOME/kith/config.toml`. v0.1's file is
already specced in full — `docs/spec/cli-tui.md` §8 — and is three things: apply backend and
custom command, monitor labels, daemon address and API-key override. Nothing else.

**Lands in** v1.0 as a *module* — ROADMAP §4: *"Configuration growth (themes, keybindings)"*. But
the *file* grows earlier, and the rule that keeps that honest is this section's real content:

> **Config keys arrive with the feature that needs them. The Configuration module is the moment
> configuration becomes a surface of its own.**

So `[notifications]` lands with Notifications in v0.2, `[rotation]` with Automation I in v0.2
(pool scoping per ADR-0003 §6 layer 3), and each is specced by its own feature's spec, each
defaulted so the file stays optional. What waits for v1.0 is configuration *as an activity*:
themes, keybinding remap, an in-TUI settings screen, and `kith config` as a verb — which
cli-tui.md already refuses outright rather than deferring: edit the TOML, `kith doctor` validates
it.

**Constraint carried.** cli-tui.md §8.1's contract binds every key ever added: a missing file is
not an error, every key has a default, unknown keys warn and never fail, a wrong type is fatal at
exit 78 with the line named. And ADR-0001's split holds — the config is Person-owned and **kith
never writes it**; anything kith must remember goes to state or to the rebuildable cache, never
back into this file.

**Mistake to build early.** A settings screen: it writes the file, which breaks the rule above,
and every knob it exposes is a knob supported forever. Also: config profiles, includes, or
per-Circle overrides before anyone has two Circles' worth of divergent settings; and moving
Circle roots into config, when they live in the engine's own record and are reachable through
`CircleRef.root`.

### 7. Plugin system — v1.0, and this section is vision only

**What it is.** The moment kith's internal seams become a contract with strangers. The universe
names seven capabilities: Preview Provider, Action Provider, Automation, Notification, Importer,
Exporter, Sync Engine.

**Lands in** v1.0 — ROADMAP §4: *"open the seams"* — and not one milestone earlier. §2's cut
carries the whole argument: *a plugin API is a compatibility contract signed with strangers; the
wrapper graveyard says never sign it before the core is stable.*

**What already exists**, and is most of the v1.0 delivery: ADR-0003 §2 **froze external-Provider
protocol v1 during v0.1** — manifest discovery at `$XDG_DATA_HOME/kith/providers/*/provider.toml`,
one process per request, `exec <verb>` with one JSON request on stdin and one response on stdout,
three verbs (`metadata`, `preview`, `act`), sysexits-flavoured codes, fixed timeouts, and no
shadowing of built-ins. Preview Provider and Action Provider are therefore *designed*; v1.0 ships
the `ExternalProvider` adapter that reads them, the docs, and the compatibility promise. "Vision
only" means the design is done and the promise is what waits.

**Constraints carried**, bounding the five capabilities that remain. **No plugin ever writes into
`.kith/`** — W1 is what makes shared state conflict-free, and a plugin appending to a Device's
record log is a second writer in a costume; plugins produce candidate bytes and facts, the core
writes records. **No method may enter `Provider` that a one-shot exec cannot express** (ADR-0003
§2) — binding today, which is why the protocol was frozen with zero implementations. **Any
capability that can reach a screen goes through the formal consent framework**, which ROADMAP §4
places in this same milestone deliberately: Automation and Notification plugins are gated on it,
and that ordering is the design rather than an accident of scheduling. **The Sync Engine
capability is proven in-tree, not by plugin** — v1.0's second implementation is a second
implementor of the trait inside the binary, because a transport behind a one-shot process is a
different product.

**Mistake to build early.** Shipping the `ExternalProvider` adapter before a second content type
exists to exercise it — ADR-0003 defers it for exactly that reason. Also: a plugin registry,
index or marketplace; a long-lived plugin daemon (rejected in ADR-0003's alternatives — kith is
not a daemon and neither are its Providers); and native `dlopen` plugins, which Rust's unstable
ABI makes a tarpit and which would gate every contribution on the toolchain ADR-0001 promised to
keep optional.

---

## Edge cases & failure honesty

Five properties are shared by everything above, and each is a place a periphery module could lie
comfortably.

- **The periphery inherits every honesty limit and must restate it locally.** Activity's
  wall-clock ordering, Health's report-never-repair, Notifications' process-lifetime delivery,
  Search's only-what-arrived, and the Role caveat on every attributed line. A limit stated once
  in an ADR and nowhere on screen has not been stated.
- **Derived views can be right about content that has not arrived.** Metadata outruns bytes
  (ADR-0004 §4.4 step 6): Activity may narrate an Item whose bytes are still in flight, Search
  matches it because its facts are present, and the Gallery shows a placeholder. Health names
  that state as normal rather than as an error — it is the designed behaviour behind walkthrough
  step 9.
- **A cache rebuild erases the ephemeral half and nothing else.** After `Change::Desynced` or a
  rebuild, the durable timeline replays identically while notification history, arrival times and
  peer-connection events start empty. No surface may imply the timeline is complete; ADR-0004
  §9's promise is that losing the cache loses time, and this is the visible shape of that.
- **No periphery module is a reason to widen the seam.** ROADMAP §6 rule 2 budgets every Sync
  Engine endpoint kith touches. Health is the module most likely to want one more; adding it is a
  reviewable decision with a domain operation behind it, exactly as in ADR-0002's method table.
- **A degraded periphery is never a broken kith.** No desktop notifier, no rules, no index: the
  Gallery, Preview, Favourites, Apply and sync are untouched. Same posture as the preview ladder
  and the apply matrix — degraded, never broken.

## CLI / TUI surface touchpoints

**Nothing in this file touches the v0.1 surface.** ROADMAP §2 fixes that surface at eleven verbs
plus bare `kith`, and at Gallery, Preview, Members, the pending-join prompt and the Circle
switcher; `docs/spec/cli-tui.md` closes it. This table is the *future* touchpoint per module, so
that when each arrives it lands in a known slot rather than wherever it fits.

| Module | CLI | TUI | Milestone |
|---|---|---|---|
| Activity | — | Activity screen; a Dashboard panel | v0.2 |
| Notifications | — | toast in the frame's status line; the desktop notifier | v0.2 |
| Health | `kith doctor`, `kith status` **already ship** (v0.1, cli-tui.md §5) | Health screen, including ADR-0002's conflict-resolve affordance | screen v0.2 |
| Automation | `kith rotate` (v0.2); `kith apply` / `kith fav` / `kith browse` parity (v0.2); `kith watch` (v0.3) | — | v0.2 / v0.3 |
| Search | `kith list items --tag/--by/--since` (v0.3) | `/` filter line over the Gallery (v0.3) | v0.3 |
| Configuration | `kith doctor`'s `config.file` check **already ships**; no `kith config`, ever | settings screen | v1.0 |
| Plugins | a `doctor` Plugins section — manifests found, protocol versions, shadowing rejected per ADR-0003 §2 | Plugins screen | v1.0 |

One call recorded rather than left implicit: ADR-0003 §2 says `kith doctor` reports a manifest
that collides with a built-in, but cli-tui.md's sixteen v0.1 checks contain no plugin section.
That is correct and deliberate — v0.1 scans no manifests because it ships no adapter, so the
check arrives with the adapter in v1.0.

## Out of scope for v0.1

All of it. Restated so each line can be pointed at, with where it graduates when its milestone
opens:

| Deferred | Returns | Graduates to |
|---|---|---|
| Activity timeline, Dashboard panel | v0.2 | `docs/spec/activity.md` |
| Notifications, in-frame and desktop | v0.2 | `docs/spec/notifications.md` |
| Health screen, conflict-resolve affordance | v0.2 | `docs/spec/health.md` |
| Automation I — rotation, CLI Action parity | v0.2 | ADR-0003 §7 plus `docs/spec/automation.md` |
| Automation II — rules, import watch | v0.3 | `docs/spec/automation.md` |
| Search, and the tags it needs | v0.3 | `docs/spec/search.md` (tags: ADR-0004 §11's `meta` record) |
| Configuration as a module — themes, keybindings, settings screen | v1.0 | `docs/spec/configuration.md` |
| Plugin system — adapter, docs, compatibility promise | v1.0 | `docs/spec/plugins.md` (protocol: ADR-0003 §2) |
| Cleanup, duplicate detection | not planned | rejected above, not deferred |
| `kith config` as a verb | not planned | cli-tui.md's out-of-scope table |

And the standing rule this file is an instance of, from ROADMAP §6: **the universe is not a TODO
list.** A module leaves this document by having its milestone arrive, never by having its section
get longer.
