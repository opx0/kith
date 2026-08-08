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

**v0.1 runs.** The full walkthrough has been executed across two Devices: Ana creates a
Circle and adds a wallpaper, Ben joins with a printed Invite code, Ana approves him
after matching the fingerprint both Devices print, and the wallpaper arrives in Ben's
Gallery attributed to Ana — then Ben adds one and it lands in hers. Verified again on a
network with local discovery, global discovery, relays and NAT traversal all disabled.

```
kith doctor          # is this Device set up?
kith init <name>     # mint your Identity
kith create walls    # a Circle, and its Collection
kith add ~/Pictures/Wallpapers/*
kith invite          # a code to send over a channel you already trust
kith                 # the TUI: Gallery, Preview, Members
```

Already syncing a wallpaper folder? `kith create <name> --path <dir> --adopt` takes it
over in place — no copying, no second directory.

Apply backends: caelestia, swww, hyprpaper, feh, or your own command in
`~/.config/kith/config.toml`. Build with `cargo build --release`; you need a Syncthing
daemon already running, and `kith doctor` names anything missing.

Not yet done: a second Device per Person, Activity, Notifications, a Health screen,
rotation, Search.

## Previously: wp-sync

This repo began as **wp-sync**, a one-script bash wallpaper syncer. kith is its
successor, not a port. The script stays at
[`wp-sync-setup.sh`](wp-sync-setup.sh) and keeps working; existing installs are not
stranded — `kith create --adopt` takes over the wallpaper directory you already sync
rather than recreating it.

## License

MIT
