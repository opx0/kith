# kith

**Local-first, peer-to-peer collections shared with the people you trust.**

v0.1 does exactly one thing: **share wallpapers with your circle — no cloud, no
account, no server.** You create a Circle and invite a friend. From then on, every
wallpaper either of you adds appears in the other's Gallery: previewed as real images
in the terminal, applied to the desktop with one keystroke. Nothing a friend adds ever
touches your screen until *you* press Apply.

## How it works

kith is a single Rust binary — a CLI and a keyboard-first TUI with image previews on a
degradation ladder (kitty → iTerm2 → sixel → halfblocks), so it works in any terminal.
The transport underneath is **Syncthing**: kith speaks to a Syncthing daemon you
already run, over its REST API, and never launches, embeds, or configures one. A
Circle maps to a synced folder; kith supplies everything Syncthing deliberately
doesn't — People instead of device IDs, Collections instead of directories, metadata
that travels with each Item, invites and joins, and consent before anything changes
your desktop. Discovery, NAT traversal, and encryption are Syncthing's job. Honesty
note: with no server, nothing is enforced centrally — Roles are policy that
well-behaved clients honour, and the docs say so wherever it matters.

## Status

**Spec-complete, pre-build.** The design is locked; the Rust implementation has not
started. The paper trail:

| Document | What it locks |
|---|---|
| [CONTEXT.md](CONTEXT.md) | The vocabulary — Person, Circle, Collection, Item, and the words we refuse to use |
| [ROADMAP.md](ROADMAP.md) | The v0.1 wedge, its 12-step acceptance walkthrough, and what returns in v0.2+ |
| [docs/adr/](docs/adr/) | Technical decisions — Rust + ratatui, the Sync Engine seam, the Provider seam |
| [docs/spec/](docs/spec/) | Behavioural specs, module by module |

## Previously: wp-sync

This repo began as **wp-sync**, a one-script bash wallpaper syncer. kith is its
successor, not a port. The script stays at
[`wp-sync-setup.sh`](wp-sync-setup.sh) and keeps working until v0.1 ships; existing
installs are not stranded — kith adopts the synced wallpaper folder you already have
rather than recreating it.

## Handoff

- [ ] At build start, rename the repo `opx0/wp-sync` → `opx0/kith`. GitHub redirects
      old URLs (clones, remotes, and the existing `curl | bash` install line keep
      working); update the install instructions after the rename.

## License

MIT
