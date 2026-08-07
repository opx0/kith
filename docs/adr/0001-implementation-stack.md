# ADR-0001: Implementation stack — Rust, ratatui, Syncthing over REST

- **Status:** Accepted
- **Date:** 2026-08-06
- **Resolves:** [#8 ADR: implementation stack](https://github.com/opx0/wp-sync/issues/8)
- **Informed by:** `research/tui-landscape`, `research/syncthing-api`, `research/prior-art`

## Context

kith is a keyboard-first TUI over a peer-to-peer Sync Engine, whose single most
distinctive feature — a gallery of images you can see in your terminal — is also its
riskiest technical dependency. The stack choice is therefore driven primarily by *which
ecosystem has a battle-tested terminal image story*, and secondarily by packaging,
startup time and dependency weight.

Three candidate stacks were surveyed:

| Stack | TUI | Terminal images | Packaging |
|---|---|---|---|
| Rust | ratatui + crossterm | **`ratatui-image`** — kitty (incl. unicode placeholders), iTerm2, sixel, halfblocks, with terminal capability querying | single static-ish binary |
| Go | Bubbletea / tview | no maintained general-purpose image widget; each project hand-rolls protocol detection | single static binary |
| Python | Textual | rich ecosystem, but image support means shelling out to chafa/Überzug++ | interpreter + venv, worst startup |

The prior-art scan also surfaced two hard constraints. First, **no pixel protocol covers
all terminals** — yazi, the state of the art, ships kitty/sixel/iTerm2 *plus* Überzug++
*plus* a chafa ASCII fallback, and that degradation ladder is non-negotiable rather than a
nicety. Second, the Syncthing wrapper graveyard (SyncTrayzor, syncthing-gtk, the official
Android app) shows the failure mode for this class of project is **maintenance burden**,
not competition — which argues for a small dependency surface and no GUI toolkit.

## Decision

**Rust**, as a single binary named `kith`.

| Concern | Choice | Rationale |
|---|---|---|
| TUI framework | `ratatui` + `crossterm` | The only stack with a maintained, reusable image widget |
| Image preview | `ratatui-image` | Packages the protocol matrix and capability querying that would otherwise be hand-rolled |
| Preview ladder | kitty → iTerm2 → sixel → **halfblocks** | Halfblocks are mandatory and always available; kith must never be unusable because of a terminal |
| CLI | `clap` (derive) | Same binary serves CLI and TUI; `kith` with no args opens the TUI |
| Sync Engine client | `tokio` + `reqwest` | Syncthing is a separately-running daemon spoken to over REST + the long-poll event stream |
| Config | TOML via `serde` | Human-editable, conventional on Linux |
| Local state | `rusqlite` (bundled feature) | **Rebuildable cache only** — never authoritative, deletable at any time without data loss |
| Synced state | files in the Circle's synced tree | Source of truth; format is ADR-0004's problem, not this one |

**The Sync Engine daemon is never owned by kith.** kith talks to an independently-running
Syncthing over REST and auto-discovers its credentials, following the pattern of every
wrapper that survived (Syncthing Tray, `stc`, Syncthing's own `cli` subcommand). The one
wrapper that owned the daemon process and rewrote its GUI address and API key —
SyncTrayzor — aged worst and is unmaintained.

**Authority rule.** Anything in SQLite must be derivable from the synced tree. If the two
disagree, the synced tree wins and the cache is rebuilt. This keeps the local database an
optimisation rather than a second source of truth that can drift out of sync across
Devices — a distinction that gets violated silently unless it is stated as a rule.

## Consequences

**Accepted:**
- Contributions require Rust, a narrower pool than Python or bash. Mitigated by keeping
  Providers scriptable at the seam (ADR-0003) so the most likely contributions — support
  for another wallpaper backend, another content type — do not require touching the core.
- Compile times and a build toolchain, versus `styli.sh`-style instant hackability.
- The existing `wp-sync-setup.sh` is superseded, not ported. Existing installs adopt their
  Syncthing folder and configuration rather than recreating them.

**Gained:**
- The image story is de-risked on day one by an off-the-shelf widget rather than by
  hand-rolled terminal capability detection, which the research identifies as the single
  fiddliest part of this product.
- Single-binary distribution: a tarball, an AUR package, `cargo install`. No interpreter,
  no venv, no GUI toolkit — directly addressing the maintenance failure mode.
- Fast startup and small memory, which the non-functional goals call for and which a
  keyboard-first tool people open dozens of times a day genuinely needs.

**Deliberately deferred:**
- Überzug++ as an additional fallback rung. `ratatui-image`'s four rungs cover the modern
  terminal matrix; adding an X11-only external process is only worth it if real users
  report a gap.
- Any GUI, mobile, or non-Linux behaviour. Seams stay portable; behaviour is Linux-first.

## Alternatives considered

**Go + Bubbletea.** Genuinely attractive: better cross-compilation, faster builds, and
`syncthingtui` proves the Syncthing-TUI pairing works there. Rejected because every Go
project that shows images in a terminal reimplements protocol detection, and that code —
not the domain logic — is where this product's early bugs would live.

**Python + Textual.** Best developer velocity and the largest contributor pool. Rejected
on two counts: image support means shelling out to external binaries the user must
install, and the two most prominent Python/GTK tools in the adjacent space — Variety and
syncthing-gtk — both stalled under exactly the maintenance load this product would take on.

**Staying in bash.** `styli.sh` proves the whole wallpaper apply-matrix fits in a shell
script, and kith's own origin is a bash script. Rejected: a gallery, an event-driven sync
client, and conflict-tolerant metadata are not shell-shaped problems.
