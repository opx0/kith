# TUI Application Stack Landscape for wp-sync

Research ticket resolved 2026-08-06. All version numbers and dates verified against package registries and official repos on this date.

## Question

Survey the TUI application stack landscape for a keyboard-first, high-performance, easily-packaged Linux TUI app with image preview. Candidate stacks: Rust (ratatui), Go (bubbletea/lipgloss), Python (textual) — plus briefly any other serious contender. For each: maturity/activity, single-binary distribution story, startup time / memory footprint reputation, ecosystem for the pieces this app needs (REST client, SQLite or embedded store, image decoding). Then the terminal image preview question in depth: kitty graphics protocol vs sixel vs iTerm2 vs unicode halfblocks — terminal support matrix (kitty, foot, wezterm, alacritty, konsole, gnome-terminal, tmux passthrough), and per-stack library support (e.g. ratatui-image for Rust, what exists for bubbletea, textual-image / rich pixels for Python). Primary sources: official repos, official docs, protocol specs (sw.kovidgoyal.net/kitty/graphics-protocol).

## TL;DR

- All three candidate stacks are mature and actively maintained as of mid-2026: **ratatui 0.30.2** (Jun 2026, 42M crates.io downloads), **Bubble Tea v2.0.8** (Jul 2026, "18,000+ apps built with it"), **Textual 8.2.8** (Jun 2026, still shipping despite the Textualize company winding down in May 2025).
- **Terminal image preview is a solved problem only in Rust.** `ratatui-image` (v11.0.6, ~652K downloads) supports all four protocols (kitty, sixel, iTerm2, halfblocks) with automatic terminal detection. Go's equivalents (`go-termimg`, `rasterm`) are young and unproven (≤ ~150 commits, 64 stars); Charm ships nothing official. Python's `textual-image` covers kitty+sixel but is third-party, LGPL-3.0, and documents sixel performance problems inside Textual; Textual's own FAQ says images are "on the Roadmap", i.e. not built in.
- **No single image protocol covers the Linux terminal fleet.** Kitty graphics: kitty, Ghostty, WezTerm (opt-in), Konsole (subset). Sixel: foot, Konsole, xterm, WezTerm, tmux (if compiled with `--enable-sixel`). GNOME Terminal and stock Alacritty support *neither* — a Unicode-halfblock fallback is mandatory, and a detection ladder (kitty → sixel → iTerm2 → halfblocks) is the industry-standard approach all surveyed libraries implement.
- Single-binary distribution: Rust and Go are both excellent (static binary, `rusqlite` `bundled` feature / pure-Go `modernc.org/sqlite` v1.56.0 keep even SQLite dependency-free). Python has no first-party single-binary story — Textual's official packaging guide is wheel/Hatch-based.
- tmux is workable but config-dependent: `allow-passthrough` (added tmux 3.3, **default off**), basic sixel only when built with `--enable-sixel` (tmux 3.4). The kitty protocol's Unicode-placeholder mode (kitty ≥ 0.28.0) was designed specifically to survive multiplexers.
- For this product (keyboard-first gallery + image preview + REST client to Syncthing + embedded index + AUR-friendly packaging), **Rust/ratatui has the strongest end-to-end story**, with Go/bubbletea a close second that trades image-preview maturity for developer velocity.

## Findings

### 1. Candidate stack profiles

#### 1.1 Rust — ratatui

- **Maturity/activity**: ratatui 0.30.2, published 2026-06-19 on crates.io; 42.1M total downloads; 22.1k GitHub stars; 2,301 commits; active fork (2023) of the proven `tui-rs`. MIT license. [1][2][3]
- **Architecture**: immediate-mode rendering; modular workspace since 0.30 (`ratatui-core`, `ratatui-widgets`, backend crates). Backends: crossterm (primary), termion, termwiz, and the new termina backend added in 0.30.2. [1][2]
- **Single-binary story**: excellent — native static-ish binary by default; fully static with musl target. SQLite via `rusqlite` 0.40.1 with the `bundled` feature: "'bundled' uses a bundled version of SQLite… a good option for cases where linking to SQLite is complicated" — no system dependency. [26]
- **Startup/memory reputation** (qualitative, no official benchmark): no runtime/VM; startup in single-digit milliseconds, RSS in the low MB. Stripped release binaries for TUI apps typically land 2–8 MB.
- **Ecosystem pieces**: REST client — `reqwest` (async, tokio) or `ureq` (blocking, tiny) are the standard choices for hitting Syncthing's localhost REST API. Embedded store — `rusqlite` (above) or pure-Rust `redb`/`sled`. Image decoding — the `image` crate decodes "AVIF, BMP, EXR, FF, GIF, HDR, ICO, JPEG, PNG, PNM, QOI, TGA, TIFF, and WebP" with default features; dual MIT/Apache-2.0; 5.8k stars, very active. [27]
- **Trade-off**: highest implementation effort of the three; immediate-mode means you build list/gallery state handling yourself (though `ratatui-widgets` and third-party widget crates cover much of it).

#### 1.2 Go — Bubble Tea / Lip Gloss

- **Maturity/activity**: v2.0.8 published 2026-07-03 (Go module proxy timestamp); 44.2k stars; MIT. v2 is the current stable line with a "high-performance cell-based renderer, built-in color downsampling, declarative views, high-fidelity keyboard and mouse handling". README claims "over 18,000 applications built with Bubble Tea", with enterprise users (Microsoft, NVIDIA, AWS, Cockroach Labs, Canonical). [4][5][6]
- **Note**: the v2 module uses the vanity import path `charm.land/bubbletea/v2` (repo remains `github.com/charmbracelet/bubbletea`); 961 known importers of v2 on pkg.go.dev. [7]
- **Architecture**: The Elm Architecture (Model/Update/View). Companion libraries: **Bubbles** (reusable components: lists, viewports, spinners, text inputs), **Lip Gloss** (styling/layout), Harmonica, BubbleZone. [4]
- **Single-binary story**: best-in-class — `CGO_ENABLED=0` static binary, trivial cross-compilation. SQLite without cgo via `modernc.org/sqlite` v1.56.0 (2026-08-03): "CGo-free, pure-Go port of SQLite3", ships SQLite 3.53.3, 3,518+ importers, BSD-3-Clause. Caveat: must pin the matching `modernc.org/libc` version (documented fragility). [8]
- **Startup/memory reputation** (qualitative): compiled binary, startup in low tens of ms; Go runtime baseline RSS ~10–20 MB; binaries typically 5–15 MB.
- **Ecosystem pieces**: REST client — `net/http` in the standard library, zero deps. Embedded store — `modernc.org/sqlite` (above) or pure-Go bbolt/badger. Image decoding — stdlib `image/png`, `image/jpeg`, `image/gif`; WebP decode via `golang.org/x/image/webp`; AVIF support is weak in pure Go (relevant if collections accept arbitrary image formats).
- **Trade-off**: the image-preview library situation (see §3.2) is the one weak link.

#### 1.3 Python — Textual

- **Maturity/activity**: 8.2.8 on PyPI, released 2026-06-30; requires Python ">=3.9, <4.0"; classified Production/Stable; 36.9k stars; 13,103 commits; MIT. [9][10][11]
- **Company status**: Textualize (the company) announced it was winding down in the official blog post "The future of Textualize" (2025-05-07); the open-source project explicitly continues and the 2026 release cadence confirms it — v8.2.x releases through June 2026, with release notes crediting sponsorship (Mistral AI named in v8.2.0/v8.2.7). Bus-factor is real but current activity is healthy. [12][11]
- **Architecture**: retained-mode widget framework with CSS-like styling, rich built-in widget set (DataTable, Tree, Input, TextArea…), dev console (`textual-dev`), built-in command palette, and `textual serve` to run the same app in a browser. [9]
- **Single-binary story**: weakest of the three. The official packaging guide is "Package a Textual app with Hatch" (wheel + PyPI/pipx/uv distribution); the FAQ has no standalone-executable guidance. PyInstaller/Nuitka work in practice but are community-supported, produce 40–80 MB bundles, and add per-platform build complexity. On Arch specifically, distribution means AUR `python-*` dependency chains or a pipx/uv instruction. [13]
- **Startup/memory reputation** (qualitative): interpreter start + framework import puts cold start in the ~100–400 ms range on typical hardware; RSS commonly 40–80 MB for a running Textual app. Fine for an interactive gallery, but the heaviest candidate, and it makes a snappy `wp-sync apply` CLI subcommand share an interpreter tax.
- **Ecosystem pieces**: REST client — `httpx`/`requests`. Embedded store — `sqlite3` in the standard library (zero extra deps; best SQLite ergonomics of the three stacks). Image decoding — Pillow (ubiquitous, all relevant formats).
- **Image support**: Textual FAQ (fetched 2026-08-06): "Textual doesn't have built-in support for images yet, but it is on the Roadmap." Note: Textual v8.2.7 "The more Kitty Release" is about the kitty **keyboard** protocol, not graphics. [13][11]

#### 1.4 Other serious contenders (brief)

- **Zig — libvaxis**: MIT, 1.9k stars, 787 commits, active; notable because **kitty graphics protocol image support is built into the framework itself**, plus kitty keyboard protocol, and it detects features by terminal query rather than terminfo. Requires Zig 0.16.0 — the language itself is still pre-1.0, which is the disqualifier for a long-lived product. [14]
- **Notcurses (C)**: performance-oriented TUI/character-graphics library with multimedia support across sixel/kitty protocols; listed in the kitty spec as a client-side implementation. Serious but C, with correspondingly higher integration cost. [15]
- **FTXUI (C++)**: solid immediate-mode C++ TUI library, but no terminal graphics-protocol support built in — you'd be hand-rolling the entire §3 story.
- Not contenders for this product: OpenTUI (TypeScript; image support is an open issue), Ink (React/Node; no pixel graphics).

### 2. Terminal image protocols

Four mechanisms, in descending fidelity:

1. **Kitty terminal graphics protocol** (spec: sw.kovidgoyal.net/kitty/graphics-protocol). Escape structure `ESC _G <control data>;<payload> ESC \`, base64 payload; pixel formats RGB (`f=24`), RGBA (`f=32`), PNG (`f=100`); transmission direct/file/temp-file/shared-memory; zlib compression (`o=z`); placements sized in cells with z-indexing; animation (since kitty 0.20.0); **capability query** (`a=q` + device-attributes request): "If you get back a response to the graphics query, the terminal emulator supports the protocol." The killer feature for this product: **Unicode placeholders** (since kitty 0.28.0) — images anchored to `U+10EEEE` placeholder cells with row/column encoded in diacritics, enabling "using images inside any host application that supports Unicode, foreground colors (tmux, vim, weechat, etc.)". Implementations listed by the spec: kitty; official support in **Ghostty, Konsole, WezTerm**; community patches for st, Warp, wayst, iTerm2, xterm.js. [15]
2. **Sixel** (DEC VT3xx legacy, widely revived). Paletted pixel graphics in an escape sequence; no alpha, palette-limited, no placement model — images scroll with text. Broadest old-guard support (xterm, foot, mlterm, Konsole, WezTerm, Windows Terminal ≥1.22 per textual-image's compatibility table). [18][19]
3. **iTerm2 inline images** (`ESC ] 1337 ; key=value ^G`, base64 file payload; "Any image format that macOS supports"; multipart variant added in iTerm2 3.5 for tmux byte limits). Origin is macOS; on Linux, WezTerm implements it ("wezterm implements support for the iTerm2 inline images protocol", plus a `doNotMoveCursor` extension) and Konsole accepts it. [16][17][20]
4. **Unicode halfblocks**: render `▀`/`▄` cells with fg/bg colors — 2 "pixels" per cell, no protocol needed, works in literally every color terminal including GNOME Terminal, Alacritty, and over any multiplexer. This is the universal fallback every serious library ships.

### 3. Terminal support matrix

Compiled from the kitty spec's implementation list [15], foot's README [21], WezTerm docs/issues [17][22], Alacritty issue #910 [23], Konsole MR !594 [20], VTE API docs [24], Are We Sixel Yet [19], textual-image's compatibility table [18], and tmux 3.4 CHANGES [25]:

| Terminal | Kitty graphics | Sixel | iTerm2 | Halfblocks | Notes |
|---|---|---|---|---|---|
| **kitty** | ✅ native (reference) | ❌ (refused; "won't implement SIXEL") | ❌ | ✅ | Protocol author; animation, placeholders, z-index [15][19] |
| **Ghostty** | ✅ official | ❌ (open discussion) | ❌ | ✅ | Listed as official implementation in kitty spec [15] |
| **foot** | ❌ | ✅ (since 1.2.0) | ❌ | ✅ | README lists "Sixel image support"; no kitty graphics [21][19] |
| **WezTerm** | ⚠️ opt-in `enable_kitty_graphics=true` (default off; known conformance gaps, issue #3817) | ✅ (since 2020-06-20 release) | ✅ (native + `imgcat`) | ✅ | Only terminal with all three pixel protocols [17][22][19] |
| **Alacritty** | ❌ | ❌ (issue #910 open since Nov 2017, no merged work) | ❌ | ✅ | No graphics of any kind in mainline [23] |
| **Konsole** | ⚠️ subset ("Animation and transfer modes other than direct are not supported") | ✅ (KDE Gear 22.04, MR !594) | ✅ | ✅ | One MR added all three, marked "experimental" but shipped [20][19] |
| **GNOME Terminal (VTE)** | ❌ | ❌ in practice — VTE has had `set_enable_sixel` API since 0.62 but it's compile-time/off; Are We Sixel Yet: "Unsupported – blocked by upstream VTE issue"; textual-image: "GNOME Terminal (no protocol support)" | ❌ | ✅ | The largest halfblocks-only population on desktop Linux [24][19][18] |
| **xterm** | ❌ | ✅ (default since patch #359) | ❌ | ✅ | [19] |
| **tmux** (as middleman) | ⚠️ via `allow-passthrough` (3.3+, **default off**; "all" state added in 3.4) or, best, kitty Unicode placeholders which flow through as ordinary text | ⚠️ "Add basic support for SIXEL if built with --enable-sixel" (3.4) — distro builds vary | ⚠️ passthrough / iTerm2 3.5 multipart | ✅ always | Halfblocks are the only zero-config option under tmux [25][15] |

Key takeaways from the matrix:

- There is **no protocol with universal Linux coverage**. Kitty-protocol reach (kitty, Ghostty, Konsole-partial, WezTerm-opt-in) and sixel reach (foot, Konsole, xterm, WezTerm, Windows Terminal) are complementary, not nested.
- **GNOME Terminal (the default on the biggest distros) and stock Alacritty render no pixel protocol at all** — halfblock fallback is not optional.
- Under tmux, everything except halfblocks requires user configuration (`allow-passthrough on`) and/or a special tmux build (`--enable-sixel`); kitty's Unicode-placeholder mode is the only pixel path designed to work through a multiplexer without terminal-side passthrough tricks. [15][25]

### 4. Per-stack image-preview library support

#### 4.1 Rust: `ratatui-image` — the mature option

- v11.0.6 on crates.io (2026-06-25), ~652K downloads, MIT, active (394 commits). [28][29]
- Supports **all four**: kitty (kitty, Ghostty), sixel (xterm, foot, mlterm…), iTerm2 (iTerm2, WezTerm, Rio, Bobcat), halfblocks fallback. Detection: "Guess by env vars. If that fails, query the terminal with some control sequences. Fallback to 'halfblocks.'" Also queries the terminal font size in pixels to map image pixels to cells. [29]
- Ratatui-native API: `Image` (stateless) and `StatefulImage` (resizes at render time) widgets; prevents the TUI from overwriting image cells; uses the `image` crate for decoding; optional chafa linkage for ASCII-art rendering. Documented caveats: termwiz backend incompatible; sixel on the last terminal line can scroll; notes that some terminals (Alacritty, Konsole) have absent/incomplete implementations. [29]

#### 4.2 Go: young, third-party, no official Charm solution

- Charm ships **no official image widget** for Bubble Tea (nothing in bubbles/lipgloss).
- **`blacktop/go-termimg`** is the most complete community option: kitty (incl. virtual placement, z-index, compression), sixel (palette optimization, dithering), iTerm2, halfblocks; protocol auto-detection (`DetectProtocol()`, terminal feature/font-size queries with caching); and a ready-made **`ImageWidget` for Bubble Tea**. But it is early-stage: 64 stars, 148 commits, no tagged stable release visible. [30]
- **`BourgeoisBear/rasterm`**: encoder-level library ("encode images to iTerm / Kitty / SIXEL (terminal) inline graphics protocols") — solid primitive, but you build the widget/detection layer yourself. [31]
- Older `trashhalo/imgcat` demonstrates bubbletea image display but is not a maintained widget library.
- Net: feasible, but the team would own more of the detection/rendering edge cases than in Rust.

#### 4.3 Python: third-party, capable but caveated

- **Textual core**: no image support; FAQ: "Textual doesn't have built-in support for images yet, but it is on the Roadmap," pointing to rich-pixels as the workaround. [13]
- **`textual-image`** (lnqs/textual-image): the serious option — kitty TGP + sixel with Unicode/halfblock fallback; ships both a Rich renderable and a Textual `Image` widget; Pillow-based. Compatibility per its README: kitty, WezTerm, Windows Terminal ≥1.22, xterm, iTerm2, foot, Konsole supported; "GNOME Terminal (no protocol support)"; Warp explicitly unsupported; tmux requires `allow-passthrough on` plus terminal-features config. Documented limitations: sixel in Textual has "performance issues with scrolling and style changes; best for static images." **License: LGPL-3.0** (fine for a Python app importing it dynamically, but a policy decision to record). 182 stars, active. [18]
- **`rich-pixels`** (darrenburns): MIT; Rich-renderable pixel grids from images — cell/halfblock-style rendering only, **no kitty/sixel protocol support**; small (46 commits), best treated as fallback-only. [32]
- Also exists: `textual-kitty` (TGP for Rich/Textual; predecessor in spirit to textual-image). [33]

## Implications for the spec

1. **Stack recommendation ordering.** For this product's hard requirements (fast keyboard-first gallery, image preview across heterogeneous friend machines, single-file install, Arch/AUR-first), **Rust + ratatui + ratatui-image** is the lowest-total-risk stack: the only ecosystem where the four-protocol image story is mature and battle-tested (652K downloads), plus `rusqlite(bundled)` + `reqwest`/`ureq` + `image` cover every other need with static-binary packaging. **Go + Bubble Tea** wins on iteration speed and effortless cross-compilation (pure-Go SQLite, stdlib HTTP) and is the right choice *if* the team accepts adopting/hardening `go-termimg` (64 stars) or writing a thin widget over `rasterm` — schedule the image-preview spike early if Go is chosen. **Python + Textual** maximizes UI velocity (richest widgets, CSS, `textual serve` as a free future web view) but pays in cold-start latency, memory, third-party+LGPL image dependency with documented sixel performance caveats, and — decisive for this product — no first-party single-binary distribution.
2. **Spec the preview subsystem as a protocol ladder, not a protocol choice.** Detection order: kitty TGP (query `a=q`) → sixel (device attributes) → iTerm2 (env/heuristics) → halfblocks. Every surveyed library already implements this ladder; the spec should require it and require that **halfblocks always work** — GNOME Terminal and stock Alacritty users are a large share of desktop Linux and get no pixel protocol at all.
3. **Treat tmux as a first-class documented environment.** The spec should state: halfblocks work everywhere with zero config; pixel preview under tmux requires `allow-passthrough on` (off by default) and, for sixel, a `--enable-sixel` tmux build; kitty's Unicode-placeholder mode is the most robust pixel path under tmux. A doctor/diagnose command (`wp-sync doctor`) that reports detected terminal, protocol, and tmux passthrough state would eliminate the top support-burden category.
4. **Thumbnails belong in the app, not the protocol.** Full-resolution wallpapers (often 4–8 MB PNGs) re-encoded to base64 per redraw is the known performance trap; the spec should include a disk thumbnail cache (pre-scaled to cell-pixel budget) feeding whichever protocol is active — this also makes halfblock rendering cheap.
5. **The embedded store decision is stack-neutral.** All three stacks have a dependency-free SQLite path (`rusqlite bundled` / `modernc.org/sqlite` / stdlib `sqlite3`); the spec can commit to SQLite for the collection index without constraining the stack choice.
6. **Syncthing REST integration is stack-neutral** — localhost HTTP + API key; every candidate covers it with a standard library or a single dependency, so the sync-engine abstraction seam can be specified independent of stack.
7. **Ecosystem-health flags to record in the spec's risk register**: Textual's single-maintainer/sponsorship model post-Textualize-shutdown (May 2025); Bubble Tea's v2 vanity-import migration (`charm.land/bubbletea/v2`); `go-termimg`'s immaturity; `textual-image`'s LGPL-3.0; WezTerm's kitty-graphics support being off by default with conformance gaps (don't advertise "kitty protocol = WezTerm works" without the config note).

## Sources

1. https://github.com/ratatui/ratatui — repo, README, backends, stars/activity
2. https://github.com/ratatui/ratatui/releases — 0.30.x release notes
3. https://crates.io/api/v1/crates/ratatui — version 0.30.2 (2026-06-19), 42.1M downloads
4. https://github.com/charmbracelet/bubbletea — README, ecosystem, adoption claims
5. https://github.com/charmbracelet/bubbletea/releases — v2 release history
6. https://proxy.golang.org/github.com/charmbracelet/bubbletea/v2/@latest — v2.0.8, 2026-07-03
7. https://pkg.go.dev/charm.land/bubbletea/v2 — vanity module path, importers
8. https://pkg.go.dev/modernc.org/sqlite — v1.56.0 (2026-08-03), pure-Go, SQLite 3.53.3
9. https://github.com/Textualize/textual — repo, features, stats
10. https://pypi.org/project/textual/ — 8.2.8 (2026-06-30), Python >=3.9
11. https://github.com/Textualize/textual/releases — 2026 release cadence, sponsorship notes
12. https://textual.textualize.io/blog/2025/05/07/the-future-of-textualize/ — company wind-down, project continues
13. https://textual.textualize.io/FAQ/ — "doesn't have built-in support for images yet"; packaging guidance
14. https://github.com/rockorager/libvaxis — Zig contender, built-in kitty graphics
15. https://sw.kovidgoyal.net/kitty/graphics-protocol/ — protocol spec: formats, query, placements, Unicode placeholders (0.28.0), animation (0.20.0), implementations list
16. https://iterm2.com/documentation-images.html — iTerm2 inline images protocol spec
17. https://wezterm.org/imgcat.html — WezTerm iTerm2-protocol support statement
18. https://github.com/lnqs/textual-image — protocols, terminal compatibility table, limitations, LGPL-3.0
19. https://www.arewesixelyet.com/ — sixel status aggregator (foot 1.2.0, Konsole 22.04, xterm #359, kitty refusal, VTE blocked, tmux --enable-sixel)
20. https://invent.kde.org/utilities/konsole/-/merge_requests/594 — Konsole sixel/iTerm2/kitty-subset MR, limitations quote
21. https://codeberg.org/dnkl/foot — sixel yes, kitty graphics absent
22. https://github.com/wezterm/wezterm/issues/1406 (+ issue #3817 via search) — enable_kitty_graphics off by default; conformance gaps
23. https://github.com/alacritty/alacritty/issues/910 — graphics request open since 2017, no merged support
24. https://gnome.pages.gitlab.gnome.org/vte/gtk4/method.Terminal.set_enable_sixel.html — VTE sixel API since 0.62 (not enabled in practice)
25. https://raw.githubusercontent.com/tmux/tmux/3.4/CHANGES — "Add basic support for SIXEL if built with --enable-sixel"; allow-passthrough history
26. https://github.com/rusqlite/rusqlite — bundled feature quote, v0.40.1
27. https://github.com/image-rs/image — decode format list, license, activity
28. https://crates.io/api/v1/crates/ratatui-image — v11.0.6 (2026-06-25), 652K downloads
29. https://github.com/benjajaja/ratatui-image — protocols, detection strategy, widgets, limitations
30. https://github.com/blacktop/go-termimg — protocols, Bubble Tea ImageWidget, maturity
31. https://github.com/BourgeoisBear/rasterm — kitty/iTerm/sixel encoder library
32. https://github.com/darrenburns/rich-pixels — MIT, halfblock-style rendering, no pixel protocols
33. https://pypi.org/project/textual-kitty — TGP renderable for Rich/Textual (search-verified)
