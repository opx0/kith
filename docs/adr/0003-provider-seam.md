# ADR-0003: Provider seam — one trait, built-in wallpaper Provider, scriptable edges

- **Status:** Accepted
- **Date:** 2026-08-06
- **Resolves:** [#9 ADR: provider seam (wallpaper provider first)](https://github.com/opx0/wp-sync/issues/9)
- **Informed by:** `research/prior-art` §§3–4, `research/tui-landscape`, ADR-0001

## Context

The Provider is the content-type-aware layer: the core knows People, Circles,
Collections and Items; everything about *wallpapers* — preview, Apply, rotation,
metadata — is the wallpaper Provider's. This ADR fixes the seam between the two.

Prior art gives two proven templates to merge. **Waypaper** is one frontend over ~11
interchangeable apply-backends — the shape of the wallpaper Provider's inside. **ranger's
`rifle` and `scope.sh`** show that content-type dispatch and preview can be plain
executables — the shape of the seam's outside. Three more precedents constrain the
design: **Azote and HydraPaper** exist solely because per-monitor apply is table stakes;
**Variety** is in maintenance mode under the weight of supporting every DE and every
source in one process; **Walltaker** — the only successful "friends control your
wallpaper" product — leads with enforced consent controls.

Two earlier commitments bind this ADR. ADR-0001 promises that Providers stay scriptable
so the most likely contributions — another wallpaper backend, another content type — never
touch the core. And the glossary's domain rule is absolute: *Apply is always local and
always deliberate*; content arriving from a Circle never changes a screen unasked.

## Decision

### 1. The seam: one synchronous, object-safe trait

One trait, held by the core as `Box<dyn Provider>` in a registry keyed by `ProviderId`.
Every Collection is bound to exactly one Provider (glossary); v0.1 registers one:
`wallpaper`.

```rust
/// Everything kith knows about one kind of content. Object-safe.
/// The seam is synchronous: Providers do plain I/O and CPU work; the core
/// runs every call on `tokio::task::spawn_blocking`. Concurrency is the
/// core's problem, which keeps the seam free of runtime types and makes the
/// external-process adapter (§2) trivial.
pub trait Provider: Send + Sync {
    /// Stable identifier ("wallpaper"). Recorded in Collection metadata.
    fn id(&self) -> ProviderId;

    /// Does this Provider claim these bytes? Consulted at import into a
    /// Collection and at cache rebuild. Must be cheap and pure: extension
    /// match plus the bounded magic-byte prefix the core has already
    /// sniffed. Never reads the whole content. (rifle's MIME dispatch.)
    fn claims(&self, candidate: &ImportCandidate) -> bool;

    /// Facts read from the content itself at import time (for wallpaper:
    /// width, height, format). Pure — no network, no mutation. The facts
    /// land in the Item's Sidecar under the Provider's namespace; the
    /// Sidecar's on-disk shape is ADR-0004's problem, producing the facts
    /// is this seam's.
    fn extract_metadata(&self, candidate: &ImportCandidate)
        -> Result<ProviderFacts, ProviderError>;

    /// Produce a preview within `budget` pixels. Returns pixels or text —
    /// never escape sequences. The core owns all terminal encoding (§5).
    fn preview(&self, item: &Item, budget: PixelBudget)
        -> Result<Preview, ProviderError>;

    /// Actions this Provider offers on a claimed Item *on this Device*.
    /// Availability is per-Device: no detected backend means Apply is
    /// declared Unavailable with a reason, not omitted (§3).
    fn actions(&self, item: &Item) -> Vec<ActionDecl>;

    /// Targets Apply can address on this Device, enumerated at call time
    /// (monitors hotplug; nothing is cached).
    fn apply_targets(&self) -> Result<Vec<ApplyTarget>, ProviderError>;

    /// Execute a declared Action. Must leave no half-state on failure: a
    /// failed Apply changes nothing on screen and records nothing.
    fn perform(&self, action: &ActionId, item: &Item, target: Option<&ApplyTarget>)
        -> Result<ActionOutcome, ActionError>;
}

pub struct ImportCandidate<'a> {
    pub path: &'a Path,       // where the bytes currently sit
    pub mime: Option<Mime>,   // core-sniffed from a bounded prefix
}

pub enum Preview {
    /// Decoded pixels, already scaled to fit the budget.
    Image(image::DynamicImage),
    /// Text card — the tier that must never fail (§5).
    Text(String),
}

pub struct PixelBudget { pub w_px: u32, pub h_px: u32 }  // core-computed from cell size

pub struct ActionDecl {
    pub id: ActionId,               // namespaced: "wallpaper.apply"
    pub label: String,              // "Apply"
    pub needs_target: bool,         // true → TUI/CLI offer target selection
    pub availability: Availability, // Available | Unavailable { reason: String }
}

pub enum ApplyTarget { AllMonitors, Monitor(String) }
```

`claims` is also the import gate: importing into a Collection whose Provider does not
claim the candidate is refused with a message, not silently accepted.

Inside the wallpaper Provider — **not** part of the public seam — a second small trait
carries the Waypaper pattern:

```rust
/// One way to set a wallpaper. Private to the wallpaper Provider.
trait ApplyBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn detect(&self, env: &SessionEnv) -> bool;
    fn targets(&self) -> Result<Vec<ApplyTarget>, ApplyError>;
    fn apply(&self, bytes: &Path, target: &ApplyTarget) -> Result<(), ApplyError>;
}
```

### 2. Built-in and external Providers: both, with a frozen protocol

**Decision: both tiers exist; v0.1 ships only the built-in wallpaper Provider.**

The wallpaper Provider is compiled in. It is v0.1's only Provider, it sits on the gallery
hot path, and its preview work must feed `ratatui-image` without process-per-thumbnail
overhead. Making the flagship flow cross an IPC boundary would buy nothing.

External Providers are one-shot executables behind a versioned JSON protocol, adapted
into the trait by a single `ExternalProvider` struct. **The protocol is frozen now at
version 1, even though the adapter ships only when the first second content type demands
it.** Freezing it now is a design constraint on the trait: no method may ever be added to
`Provider` that cannot be expressed as a one-shot exec verb. That is the discipline that
keeps ADR-0001's scriptability promise true instead of aspirational.

Scriptability where contributions will actually arrive — *another wallpaper backend* — is
even cheaper and ships in v0.1: a config-declared command template (§4), no protocol
needed. styli.sh proves the entire apply matrix is shell-shaped.

**Protocol v1, precisely:**

*Discovery.* Scan `$XDG_DATA_HOME/kith/providers/*/provider.toml`, then each
`$XDG_DATA_DIRS/kith/providers/*/provider.toml`. First manifest per Provider id wins. A
manifest whose id collides with a built-in is ignored and reported by `kith doctor` —
built-ins cannot be shadowed.

```toml
# provider.toml
[provider]
id       = "comics"
protocol = 1
exec     = "kith-provider-comics"   # relative to manifest dir unless absolute

[claims]
extensions = ["cbz", "cbr"]
mime       = ["application/vnd.comicbook+zip"]

[[action]]
id           = "comics.open"
label        = "Open in reader"
needs_target = false
```

Claims and Action declarations are static in the manifest, so kith never spawns a process
just to ask what a Provider handles — a gallery scroll costs zero forks.

*Invocation.* One process per request, no plugin daemon (kith is not a daemon; neither
are its Providers). `exec <verb>` with a single JSON request on stdin, a single JSON
response on stdout, human diagnostics on stderr. cwd is the manifest directory;
`KITH_PROTOCOL=1` is set in the environment.

| Verb | Request (stdin) | Response (stdout) |
|---|---|---|
| `metadata` | `{"protocol":1, "item":{"path":…}}` | `{"ok":true, "facts":{…}}` |
| `preview` | `{"protocol":1, "item":{…}, "budget":{"w":…,"h":…}, "out":"/…/tmp.png"}` | `{"ok":true, "kind":"image"}` after writing a PNG to `out`, or `{"ok":true, "kind":"text", "text":"…"}` |
| `act` | `{"protocol":1, "action":"comics.open", "item":{…}, "target":null}` | `{"ok":true}` or `{"ok":false, "error":"…"}` |

Preview pixels travel by file, never by pipe, and never as escape sequences — the core
encodes for whatever terminal is present (§5).

*Exit codes* (sysexits-flavoured, and the kith CLI maps Action results to the same
numbers for coherence):

| Code | Meaning |
|---|---|
| 0 | stdout JSON is authoritative (including `"ok":false`) |
| 64 | protocol error: unknown verb, unparseable request |
| 65 | cannot process these bytes (claim was wrong) |
| 69 | unavailable on this Device (missing tool, no session) |
| other | crash — kith reports the stderr tail as the failure detail |

*Timeouts.* `metadata` 10 s, `preview` 20 s, `act` 120 s. Overruns get SIGTERM, then
SIGKILL after 2 s, reported as `TimedOut`. kith may run previews concurrently but never
two `act`s on the same Item.

### 3. Action model: core Actions and Provider Actions, one surface

Actions form a single namespace presented identically everywhere, but split by
implementer:

| | Actions | Implemented by | Why |
|---|---|---|---|
| **Core** | favourite, delete, copy path, reveal, open | the core, uniformly on every Item | They mutate domain state (Favourite, Collection membership) or are content-blind. Delete goes through the core because it edits the Collection and Sidecar under Role policy — with the Sync Engine's file versioning as the safety net, per ADR-0002/0004. Open is `xdg-open`: which application handles which content is the OS's dispatch problem, not kith's. |
| **Provider** | apply (+ targets); future types add their own | the claiming Provider via `actions()`/`perform()` | Only the Provider knows what "make this Item active" means. |

**Surfacing.** In the TUI, the selected Item's action menu is the union of core Actions
and `actions()`, with single-key bindings for the common five (favourite, apply, delete,
copy path, reveal). Unavailable Actions render greyed **with their reason** — "no
wallpaper backend found", not a missing menu entry. Honesty in the UI mirrors the Role
honesty rule. In the CLI, each Action is a verb: `kith apply <item> [--on <monitor>]`,
`kith fav <item>`, etc. Item addressing syntax is the CLI/TUI spec's problem (#16).

**Failure reporting.** `ActionError { kind: Unavailable | Failed | TimedOut | Cancelled,
message, detail }` where `detail` carries the backend/provider stderr tail. TUI: status
line, expandable detail. CLI: message on stderr, exit 0/1/69 matching §2's table. Never
silent, never half-applied: a failed Apply leaves the previous wallpaper and records
nothing.

### 4. Apply targets and the backend matrix

Monitors are first-class: `ApplyTarget::Monitor(name)` or `AllMonitors`, enumerated by
the selected backend at Action time (hotplug-safe, nothing cached). Azote and HydraPaper
exist because this is table stakes; kith treats it as such where the backend can.

**v0.1 built-in backends**, detection precedence top to bottom. DE checks run before the
session-generic ladders because GNOME and KDE on Wayland do not speak wlr-layer-shell —
`WAYLAND_DISPLAY` alone proves nothing.

| # | Backend | Detect | Per-monitor | Invocation |
|---|---|---|---|---|
| 0 | custom (config) | `[provider.wallpaper.custom]` present | template's problem | command template below |
| 1 | GNOME `gsettings` | `XDG_CURRENT_DESKTOP` ∋ GNOME | **no** (why HydraPaper exists; deferred) | `gsettings set org.gnome.desktop.background picture-uri` (+ `picture-uri-dark`) |
| 2 | KDE | `XDG_CURRENT_DESKTOP` ∋ KDE | no in v0.1 | `plasma-apply-wallpaperimage <path>` (Plasma ≥ 5.26) |
| 3 | swww | Wayland + `swww query` succeeds | yes (`-o`) | `swww img <path> -o <output>` |
| 4 | hyprpaper | `HYPRLAND_INSTANCE_SIGNATURE` + hyprpaper socket | yes | `hyprctl hyprpaper preload` + `wallpaper "<mon>,<path>"` |
| 5 | swaybg | `WAYLAND_DISPLAY` + binary | yes (`-o`) | detached `swaybg -o <out> -i <path> -m fill`; kith kills its previously-spawned instance |
| 6 | xwallpaper | `DISPLAY` + binary | yes (`--output`) | `xwallpaper --output <out> --zoom <path>` |
| 7 | feh | `DISPLAY` + binary | no in v0.1 | `feh --bg-fill <path>` |

xwallpaper outranks feh because it can address a monitor. swww outranks hyprpaper
because a running swww daemon is a Person's explicit choice. swaybg is the one backend
that must stay resident; preferring swww keeps kith's hands clean of process management
on the happy path.

**Deliberately not built in:** XFCE, MATE, Cinnamon, Enlightenment, and the rest of the
long tail. Variety's maintenance mode is what supporting every DE costs. The escape
hatch covers them without touching the core:

```toml
[provider.wallpaper.custom]
apply   = "xfconf-query -c xfce4-desktop -p {prop} -s {item}"
targets = "some-command-listing-outputs"   # optional; omitted → AllMonitors only
```

`{item}` is the path to the Item's bytes, `{target}` the chosen monitor. This one
template — plus external-Provider scripts — is ADR-0001's "contributions don't touch the
core" promise made concrete.

**When nothing is found**, Apply degrades honestly rather than breaking kith: the Action
is declared `Unavailable` with the list of backends probed and a pointer to the custom
template; CLI exits 69. Browsing, preview, Favourites and sync are untouched — kith
without a backend is still a shared gallery. `kith doctor` reports detected session,
chosen backend, and enumerated monitors alongside its terminal-protocol report.

### 5. Preview: Providers make pixels, the core talks to terminals

The single most important seam rule: **Providers never emit escape sequences.** A
Provider produces decoded pixels (or text) within a pixel budget; the core encodes them
through the `ratatui-image` ladder (kitty → iTerm2 → sixel → halfblocks) chosen in
ADR-0001. This keeps the fiddliest code in the product in exactly one place and makes
every Provider — built-in or script — automatically portable across the whole terminal
matrix.

- **Budgets.** Two classes: `Thumb` (gallery tile) and `Full` (preview pane). The core
  computes pixel budgets from the terminal's queried cell size, so Providers pre-scale
  and a 4–8 MB wallpaper is never re-encoded per redraw — the known performance trap.
- **Thumbnail cache.** `$XDG_CACHE_HOME/kith/thumbs/<content-hash>-<class>.png`, written
  by the core on preview miss (Providers stay stateless). Keyed by content hash so Sync
  Engine renames and moves never invalidate. Under ADR-0001's authority rule it is
  rebuildable and deletable at any time: losing it costs re-decodes, never data.
- **Text tier never fails.** If a Provider returns `Preview::Text`, the core renders the
  card; if a Provider *errors*, the core synthesises the card itself from Sidecar facts
  (title, Member who added it, date, size). The gallery never has holes.

### 6. Consent: approval is structural, not policy

The domain rule stands: Apply is always local and always deliberate. Enforced
structurally, in three layers:

1. **No path from sync to screen.** The Sync Engine module has no reference to the
   Provider registry — the dependency direction makes "arriving content applies itself"
   unrepresentable, not merely forbidden. New Items from Members land in the gallery and
   Activity, never on a screen.
2. **Rotation draws only from Favourites.** The rotation pool (§7) is the Person's
   Favourites, nothing else. A Favourite can only be set by a Person looking at an Item —
   so approve-before-apply is the mechanism itself, not a checkbox in front of it.
   Walltaker, the one successful friends-push-wallpapers product, leads with enforced
   blacklists; kith's stance is stronger: Members cannot push to a screen at all.
   Withdrawal is symmetric: unfavourite an Item and rotation forgets it.
3. **Per-Circle scope and explicit opt-in.** `rotation.circles` in config limits the pool
   to chosen Circles' Collections. And rotation only runs when the Person schedules it or
   invokes it — kith never schedules itself (§7).

### 7. Rotation: a verb, not a daemon

Rotation lives entirely inside the wallpaper Provider and is driven from outside kith:

- **`kith rotate`** is a one-shot CLI verb: take the next Item from the pool (Favourites,
  optionally scoped per §6), perform `wallpaper.apply` on the configured target, exit.
  Ordering is a shuffled cycle whose cursor lives in the SQLite cache — rebuildable;
  losing it merely reshuffles.
- **The schedule lives in the host scheduler.** Packaging ships an example systemd user
  timer (`kith-rotate.timer` + service) and documents the cron one-liner — the styli.sh
  `@hourly` precedent, and the Linux-first convention. For scheduler-less setups,
  `kith rotate --every 30m` runs a plain foreground loop: visible, killable, dies with
  the terminal — still not a daemon.
- **Why not a daemon:** ADR-0001 identifies maintenance burden as this product class's
  killer, and a resident process is a whole class of it (lifecycle, restarts, sockets,
  packaging). waypaperd exists, and it is an always-on process doing what a timer does.
  The pool definition lives in TOML config; the schedule in the scheduler; the cursor in
  the rebuildable cache. Nothing about rotation is authoritative state.

## Consequences

**Accepted:**
- The seam is synchronous, so the core must route every Provider call through
  `spawn_blocking` — one discipline, one choke point, enforced in the registry.
- Protocol v1 is frozen with zero external Providers shipped. The cost is discipline
  (the trait can never grow an exec-inexpressible method); the payoff is that
  scriptability stays true rather than decaying into "recompile the core".
- GNOME and KDE Members get all-monitors Apply only in v0.1, stated plainly in docs.
- swaybg requires kith to manage one detached process — the single impure spot in the
  daemonless story, mitigated by swww outranking it.
- Two extension surfaces to document and keep honest: the custom-backend template and
  the Provider protocol.

**Gained:**
- ADR-0001's contribution promise is now concrete: a new wallpaper backend is a TOML
  template or a small PR against one private trait; a new content type is an executable
  plus a manifest. Neither touches the core.
- Terminal-graphics knowledge lives in exactly one place; every Provider rides the whole
  kitty→halfblocks ladder for free.
- Consent is structural — favourites-only rotation and a sync module that *cannot* reach
  a screen — not a policy hoping clients behave.
- kith remains a binary you run, never a service you operate.

**Deliberately deferred:**
- The `ExternalProvider` adapter, until the first second content type exists.
- Per-monitor Apply on GNOME (HydraPaper-style composite spanning) and KDE (qdbus
  scripting) — real, known techniques, not v0.1.
- XFCE/MATE built-in backends; the custom template covers them today.
- An `export` method on the seam — copy path and reveal cover v0.1; the verb slot is
  reserved in protocol v2 if a content type ever needs transformation on the way out.
- Tag-based rotation filters — blocked on ADR-0004's metadata model growing tags.
- Provider-contributed CLI verbs as a generic mechanism; `rotate` is wallpaper-internal.

## Alternatives considered

**Everything external (scope.sh purism).** Every Provider a script, including wallpaper.
Rejected: the gallery hot path would fork per thumbnail, and v0.1's only Provider would
cross an IPC boundary for zero flexibility gained. The protocol exists for the second
content type, not the first.

**Everything compiled in — native plugins via `dlopen`.** Rejected twice over: Rust has
no stable ABI, making native plugins a known tarpit; and it breaks ADR-0001's promise by
gating every contribution on the Rust toolchain.

**WASM plugins.** Sandboxed and fashionable, but Apply's entire job is touching the host
— spawning swaybg, talking to hyprctl — so the sandbox subtracts the point. Adds a
runtime dependency squarely against the small-surface maintenance stance.

**Long-lived plugin daemon (LSP-style JSON-RPC).** Persistent processes for stateless
per-request work. kith is not a daemon; its Providers don't get to be either.

**Support every DE in built-ins.** Variety did; Variety is in maintenance mode. The
custom template plus a narrow built-in matrix serves the tail without the core paying
for it.

**In-kith rotation daemon (waypaperd's shape).** An always-on process doing what a
systemd user timer does. Rejected per the maintenance stance.

**Rotation pool = whole Collection with per-Circle opt-in.** Simpler to explain, but it
puts unseen content on screens — a Member adds an Item and it may appear before anyone
looked at it. Rejected: consent must be structural, and favourites-only is the design
where the safe behaviour is the only representable one.
