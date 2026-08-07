# ROADMAP — kith

- **Status:** Accepted
- **Date:** 2026-08-06
- **Resolves:** [#10 Decide: v0.1 scope & roadmap](https://github.com/opx0/wp-sync/issues/10)
- **Informed by:** product universe (map #1, first comment), `research/prior-art`, `CONTEXT.md`, ADR-0001

The product universe is the reference for what kith *could* be. This roadmap is the
contract for what it *will* be, in order. Its job is to let anyone say "no" by pointing
at a line. If a feature is not named in §2, it is not in v0.1 — including good ideas.

---

## 1. The wedge

v0.1 delivers exactly one story, end to end:

> **Two friends share wallpapers with each other. No cloud, no account, no server.**
> I create a Circle and invite a friend. From then on, every wallpaper either of us adds
> appears in the other's Gallery — previewable in the terminal, applied to the desktop
> with one keystroke.

Everything in v0.1 exists because this story fails without it. Nothing in v0.1 exists
for any other reason.

What makes it finishable:

- **One Provider, one Collection per Circle, both hardcoded.** The seams exist in code;
  nothing is pluggable from outside.
- **The riskiest part is bought, not built.** Terminal image rendering is
  `ratatui-image`'s protocol matrix (ADR-0001), not our research project.
- **The daemon is someone else's product.** kith adapts a separately-running Sync Engine
  daemon over its API; it never launches, embeds, or configures one.
- **No server component, no plugin API, no second platform.** Each of these is a standing
  maintenance contract; v0.1 signs none.
- The prior-art scan is unambiguous: the space is unoccupied and the failure mode for
  this class of project is **maintainer burnout, not competition**. The wedge is cut to
  be shippable by a very small team and maintainable by one person.

---

## 2. v0.1 in scope

The minimum column below is also the maximum. Shipping less breaks the walkthrough (§3);
shipping more steals time from it.

| Module | v0.1 minimum — and ceiling |
|---|---|
| **Identity** | Create a Person profile (a name) on first run; bind this Device to it via the Sync Engine's device identity. One Device per Person in v0.1 — the Person/Device split is modelled from day one so a second Device lands later without migration. No avatars, no device grouping, no rename. |
| **Circles** | Create, list, join via Invite. The founding Member is the Circle's Steward and its one admin. No rename, no delete, no Circle settings. |
| **Collections** | Exactly one per Circle, created with it. Import an existing directory of wallpapers as Items (adopting the current wp-sync tree rather than recreating it, per ADR-0001). List Items. Modelled one-to-many from the start; opened up in v0.3. |
| **Members** | List Members with name and Presence. Two Roles: admin, member — policy, not enforcement, stated honestly wherever shown. Leave a Circle. No kick, no role editing. |
| **Invites** | `kith invite` prints a time-bounded invite code; `kith join <code>`; the admin approves or rejects the pending join. Codes expire; no revoke (let it expire), no QR, no links. |
| **Gallery** | TUI grid of the Collection's Items with image thumbnails on the preview ladder (kitty → iTerm2 → sixel → halfblocks), sorted by date added. Favourite marker and an unseen-Item dot — noticing new content *is* the wedge. No grouping, no filtering beyond a favourites toggle, no infinite scroll. |
| **Preview** | Fullscreen Preview of one Item with its Sidecar facts: title, who added it, when, resolution, byte size. No zoom, no hash panel. |
| **Actions** | Apply (with a monitor picker — per-monitor is table stakes per prior art), Favourite (private to the Person), Reveal on disk, Delete. Nothing a Member adds ever changes another Person's screen without that Person pressing Apply — v0.1 satisfies the consent rule structurally by containing no automation. No share, duplicate, move, restore. |
| **Providers** | The wallpaper Provider, compiled into the binary. Apply backends: a small matrix the maintainers can actually test (swww and hyprpaper on Wayland, feh on X11) plus a custom apply-command escape hatch in config. The matrix grows by contribution, never by promise. |
| **Sync Engine** | Adapter over the separately-running daemon (Syncthing, per ADR-0001): discover its credentials, map Circle to synced space, admit a Device on approved join, watch the event stream for arriving Items, and expose state via `kith status` and `kith doctor` (daemon reachable, Identity present, Circle healthy, preview rung detected). Silent sync failure would kill the wedge between two non-expert friends, so diagnosis ships in v0.1 even though the Health module does not. |
| **Configuration** | One TOML file: apply backend and custom command, monitor names, daemon address/API-key override. No themes, no keybinding remap. |

**CLI surface v0.1:** `init`, `create`, `join`, `invite`, `approve`, `reject`, `add`,
`list`, `status`, `doctor`, `version` — and bare `kith` opens the TUI.
**TUI surface v0.1:** Gallery, Preview, Members, a pending-join prompt, and a plain
Circle switcher when a Person has more than one Circle. No Dashboard.

### Cut from v0.1

Every cut is deliberate, and every one returns (or is ruled out in §5):

| Module | Why it is cut | Returns |
|---|---|---|
| **Activity** | The wedge needs content to arrive, not to be narrated. The `add` and `remove` records already carry who did what and when, so the timeline is derivable later with no migration. | v0.2 |
| **Notifications** | The Gallery's unseen-Item dot covers the wedge; desktop-notification integration is a new platform surface. | v0.2 |
| **Health** | `kith doctor`/`status` ship in v0.1 under the Sync Engine (see above); the Health *screen* — conflicts, storage, per-Member sync state — waits. | v0.2 |
| **Automation** | Rotation and scheduling belong to the wallpaper Provider and need a scriptable `kith apply`; nothing in the wedge story rotates. | v0.2 (local rotation); v0.3 (rules, import watch) |
| **Search** | A v0.1 Collection is tens of wallpapers; scrolling a Gallery beats building a query language. | v0.3 |
| **History** | Restore/undo leans on the Sync Engine's versioning, which is also the honesty backstop for Roles — they must be designed together, later. | v0.3 |
| **Plugins** | A plugin API is a compatibility contract signed with strangers; the wrapper graveyard says never sign it before the core is stable. The Provider seam exists internally from day one. | v1.0 |

---

## 3. The v0.1 walkthrough

This is the acceptance criterion. v0.1 is done when this passes on two real machines —
one Wayland session, one X11 — and not before. Nothing beyond it is required.

1. Ana and Ben each install the `kith` binary and have the Sync Engine daemon running
   (Syncthing from their distro — the one concession to setup, and `kith doctor` names
   it plainly when it is missing).
2. Both run `kith doctor`: daemon reachable, credentials discovered, preview rung
   reported (kitty in Ana's terminal; halfblocks in Ben's — degraded, never broken).
3. Ana runs `kith init` and gives her name. Her Identity now lives on this Device.
4. Ana runs `kith create walls`. The Circle exists, with its Collection; behind the
   seam, a synced space is created.
5. Ana runs `kith add ~/Pictures/walls/*`. Her existing wallpapers become Items, each
   with an `add` record attributing them to Ana.
6. Ana runs `kith invite` and sends the printed code to Ben over any channel she
   already trusts. kith has no messaging and wants none.
7. Ben runs `kith init`, then `kith join <code>`.
8. Ana sees the pending join and approves it (TUI prompt or `kith approve`). As the
   Circle's admin she admits Ben's Device; sync begins.
9. Ben runs `kith`. The TUI opens on the walls Gallery; thumbnails appear as bytes
   arrive, marked unseen.
10. Ben moves with the arrow keys, presses Enter for fullscreen Preview — *added by
    Ana · today · 3840×2160 · 1.9 MB* — and presses `f` to Favourite it. Ana learns
    nothing; Favourites are private.
11. Ben presses `a` to Apply. He has two monitors, so the picker asks which; the
    wallpaper Provider sets his background there.
12. Ben runs `kith add ~/Downloads/sunset.png`. It syncs to Ana; her Gallery shows it
    unseen. Her screen does not change until she chooses Apply — it never will.

---

## 4. After v0.1

Precision decays with distance, deliberately: v0.2 is a plan, v0.3 is a direction,
v1.0 is a horizon. Each gets re-cut against reality when its own ticket opens.

**v0.2 — the Circle feels alive.** Activity (timeline derived from Sidecars),
Notifications (new Item, Member joined), the Health screen and a Dashboard, Member
removal and Invite revocation, role editing, local wallpaper rotation (Automation I),
and CLI parity (`kith apply`, `kith browse`) so rotation is scriptable.

**v0.3 — more than one of everything.** Multiple Collections per Circle, a second
Device per Person, tags and Search, History/restore built on Sync Engine versioning
(designed together with Role honesty), Automation II (rules, import watch), and a
second content-type Provider (plain images) to prove the Provider seam with a
still-internal implementation.

**v1.0 — open the seams.** The Plugin system: Providers and Actions as external
scripts (the Waypaper/rifle pattern), a second Sync Engine implementation to prove the
transport seam, Configuration growth (themes, keybindings), a formal consent framework
(approve-before-apply as machinery, prerequisite for any push-style feature), and
packaging/docs polish (tarball, AUR, `cargo install`).

---

## 5. Not planned

The universe's non-goals, restated as commitments, plus cuts this roadmap makes
consciously:

- **Not Dropbox, not Google Drive, not Nextcloud, not cloud storage.** kith operates no
  server and never will; there is nothing to sign up for.
- **Not a Syncthing replacement, not a generic NAS manager.** kith adapts a transport;
  it does not manage, bundle, or reimplement one. Daemon lifecycle is out of scope
  forever.
- **Not a file explorer.** Collections are logical spaces with content-aware Actions; a
  generic tree browser is a different (well-served) product.
- **Not a social network.** No feeds, no comments, no chat, no reactions, no public
  discovery of Circles. Invites travel over channels people already trust.
- **No accounts, no registry, no identity recovery.** A Person exists because other
  People trust them; kith has no authority to restore what it never issued.
- **No web-source scraping** (Wallhaven, Reddit, Unsplash fetchers). That is
  Variety's scope creep and Variety already exists. Any downloader composes with
  `kith add` — composition over reinvention.
- **No mobile apps, no GUI toolkit.** The official Android wrapper died of store
  friction; GUI stacks are where adjacent projects stalled. kith is a terminal program.
- **No home-grown cryptography.** Transport security is the Sync Engine's job;
  Identity uses what the transport provides.
- **Linux-first, indefinitely.** Seams stay portable (ADR-0001), but no other OS gets
  tested, promised, or debugged until well past v1.0.

---

## 6. Sustainability rules

Standing rules, drawn from the wrapper graveyard. They outlive every milestone above:

1. **Never own the daemon.** Every surviving wrapper adapts a separately-running
   daemon; the one that owned the process aged worst and died. This rule has no
   exceptions, including "just for onboarding".
2. **The Sync Engine seam stays narrow and budgeted.** The REST surface churns
   (a major-version bump broke syncthing-gtk). Every endpoint kith touches is a
   liability; adding one is a reviewable decision, not a convenience.
3. **No new platform surfaces before v1.0.** No mobile, no GUI, no web view. Each
   surface multiplies maintenance; surfaces killed more prior art than missing
   features ever did.
4. **The most likely contribution must not touch the core.** Another apply backend,
   another content type: these arrive as configuration or scripts at a seam, never as
   core patches. If they can't yet, the seam is the bug.
5. **The universe is not a TODO list.** A feature enters a milestone only when the
   milestone's walkthrough fails without it, or by displacing something that leaves.
   When in doubt, the answer is the wedge.
