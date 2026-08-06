# Prior-Art Scan: Local-First P2P Shared-Collections App on Syncthing

## Question

Prior-art scan for a local-first P2P shared-collections app built on Syncthing. Cover four categories: (1) Syncthing wrappers/frontends — syncthing-gtk, syncthingtray, synctrayzor, syncthing-android, any TUI/CLI wrappers; (2) P2P/local-first sharing tools with a social "share with friends" workflow layer — e.g. anything building group-sharing UX on Syncthing or similar (also note Resilio Sync as proprietary comparison); (3) Linux wallpaper managers — variety, waypaper, azote, hydrapaper, styli.sh: what workflow features they offer (auto-rotation, sources, per-monitor) and whether any do sharing; (4) TUI media galleries/file pickers with image preview. For each: what it does well worth learning from, what to avoid, and whether it overlaps this product. End with a differentiation assessment: is "private collections shared inside trusted circles, with content-type-aware actions, on top of Syncthing" genuinely unoccupied space? Primary sources: official repos/sites.

## TL;DR

- **Every existing Syncthing wrapper is an *instance manager*** (folders, devices, status, restarts) talking to the REST API — none presents synced *content* as a browsable gallery or attaches content-type actions. The niche "content-level UX on Syncthing" is empty [1][4][5][7][8][9].
- **No open-source project builds a social "circles/collections" layer on Syncthing.** The closest analogues are proprietary (Resilio Sync — folder-centric, per-user read/write permissions [10]), server-mediated (Peergos [11]), infra-heavy protocols (any-sync needs sync/file/consensus/coordinator nodes [12]), or one-off transfer tools (LocalSend [13]).
- **Wallpaper managers have rich apply-workflow features (rotation, per-monitor, multi-backend) and exactly zero sharing.** Waypaper's frontend-over-many-backends design is a proven template for the "provider" seam [15]; Variety shows the maintenance cost of supporting every DE (it is in maintenance mode) [14].
- **The two "share wallpapers with friends" projects that exist are both centralized:** DynamicWallpaper (Rust web server + polling client, self-described beginner project [23]) and Walltaker (hosted Rails service where friends push wallpapers to you, furry-community-specific [24]). Neither is local-first or P2P — the wp-sync model has no direct competitor.
- **TUI image browsing is a solved rendering problem:** yazi ships built-in kitty/sixel/iTerm2 support with Überzug++/chafa fallbacks [19], and ratatui-image packages the same protocol matrix as a reusable widget [21]. No TUI gallery is collection- or sync-aware.
- **Verdict: the combination is genuinely unoccupied.** The risk is not competition; it is the graveyard pattern of single-maintainer Syncthing wrappers (SyncTrayzor, syncthing-gtk, official Android all died or were handed off) [2][4][5].

## Findings

### 1. Syncthing wrappers and frontends

All of these manage the Syncthing *daemon* — none of them treat the synced files themselves as first-class content.

| Project | Platform / UI | Integration | Status |
|---|---|---|---|
| Syncthing Tray (Martchus) | Linux/Win/macOS/Android; Qt tray + Qt Quick UI + Plasmoid + Dolphin integration + `syncthingctl` CLI | REST API against a running instance; optional launcher/embedded lib | Actively maintained, ~2.9k stars [1] |
| Syncthing-GTK (kozec) | Linux GTK3/Python | Web UI API | Original abandoned (Python 2-era; Py3 issue open since 2018); community fork `syncthing-gtk/syncthing-gtk` released 0.10.0 with Syncthing v2 compatibility [2][3] |
| SyncTrayzor | Windows tray; hosts the web UI in an embedded browser | Hosts/wraps the Syncthing process, overrides GUI listen address + API key | **Unmaintained**; README points to community fork "SyncTrayzor v2" [4] |
| syncthing-android (official) | Android wrapper | Bundles the daemon | **Discontinued Dec 2024**, repo archived; cited "Google making Play publishing something between hard and impossible" + lack of maintenance [5] |
| Syncthing-Fork (Catfriend1 lineage) | Android | Continuation of the official app | Actively maintained, ~2.5k stars, distributed via GitHub/F-Droid/Obtainium, MPL-2.0 [6] |
| syncthingtui (Evidlo) | Go + Bubbletea TUI | Minimal REST client; auto-discovers credentials from `config.xml` | Tiny (18 stars, 19 commits), self-described "vibe-engineered"; tabs for Folders/Device/Remote Devices/Alerts/Actions [7] |
| stc (tenox7) | CLI | REST API; "easy mode" auto-finds config, JSON output for `jq` | Maintained; status/rescan/restart/override/events scope [8] |
| `syncthing cli` (built-in) | CLI subcommand of Syncthing itself | Wraps the REST API — "saves the hassle of handling HTTP connections and API authentication"; config get/set, system status, folder ops, errors, debug; stdin batch mode | Official, part of Syncthing [9] |

**Worth learning from:**
- The universal integration pattern is **talk to the REST API of a separately-running daemon** (Syncthing Tray explicitly "does not launch Syncthing itself by default", with an optional launcher) [1]. SyncTrayzor's approach — owning the process and overriding its GUI address/API key — made it more fragile and Windows-bound [4].
- Credential auto-discovery from `config.xml` (stc "easy mode", syncthingtui) removes the worst onboarding step [7][8].
- syncthingctl exists because even wrapper authors wanted a CLI companion — a CLI/TUI pairing is a validated shape [1].

**What to avoid:** single-maintainer coupling. Three of the five best-known wrappers died or were handed to forks (SyncTrayzor, syncthing-gtk, official Android) [2][4][5]. Also: Syncthing v2 broke syncthing-gtk until the fork's 0.10.0 — the REST surface does churn, so the sync-engine seam should be narrow [3].

**Overlap with wp-sync:** low. These are ops dashboards. None does gallery browsing, content actions, or any social framing. The built-in `syncthing cli` + REST API is *infrastructure* wp-sync builds on, not competition [9].

### 2. P2P / local-first sharing with a social layer

- **Resilio Sync** (proprietary, the direct commercial comparison): P2P folder sync; its "Advanced Folders" let an owner "assign ownership to another user, revoke access, or change read and write permissions on the fly", plus selective sync via placeholder files. Formerly paid Pro features are now in the free tier [10]. It is *folder-centric with per-user ACLs* — richer permissioning than Syncthing's device-level model — but has no collections/social/content-type concept, and it's closed source.
- **Peergos** (open source): "sharing between friends only", E2E post-quantum encryption including metadata, secret links for outsiders, self-hostable servers with portable identity, audited (Cure53, Radically Open Security). But it is a *hosted-or-self-hosted server* architecture with a web UI — not device-to-device local-first, not Syncthing-based [11].
- **any-sync / Anytype**: an MIT-licensed protocol for "peer-to-peer synchronization of encrypted communication channels (spaces)", CRDT-based, local-first — the closest *conceptual* match to "shared collections". But it requires a four-role node infrastructure (sync, file, consensus, coordinator nodes) [12]. This is the heavyweight path wp-sync explicitly avoids by reusing Syncthing.
- **LocalSend** (Apache-2.0, 70k+ stars): zero-config LAN discovery, cross-platform, offline — but strictly **one-off transfers**, no persistent folders or collections [13].
- **Built *on Syncthing* specifically:** searches surfaced no open-source project layering group-sharing UX on Syncthing. Community forum threads about "share photos among family members" resolve to manual folder-sharing recipes, not products. The Syncthing ecosystem around content is empty above the folder level.

**Worth learning from:** Resilio's per-user read/write + ownership transfer is the permissions bar users will expect and Syncthing cannot natively express (Syncthing has folder types like send-only/receive-only per *device*, not per *user*) [10]. Peergos shows "friends-only" as an explicit product stance is viable [11]. LocalSend shows how much adoption zero-config discovery buys [13].

**What to avoid:** any-sync's lesson is that a bespoke protocol drags in node infrastructure and kills "zero cloud dependency" [12]. Peergos' lesson: server-mediated identity re-centralizes trust.

**Overlap:** Resilio overlaps on sync mechanics but not on collections/actions/TUI and is proprietary. Peergos/any-sync overlap on vision but not architecture. None occupies "social layer on Syncthing."

### 3. Linux wallpaper managers

| Tool | Rotation | Sources | Per-monitor | Sharing | Status |
|---|---|---|---|---|---|
| Variety [14] | Yes, interval-based | Wallhaven, Unsplash, Bing, Reddit, local folders; ImageMagick filters; quotes/clock overlays | Not documented | **No** | GPL-3.0, Python/GTK3, **maintenance mode** |
| Waypaper [15] | `--random` + `waypaperd` slideshow daemon | Local files | Yes (with awww/swaybg/hyprpaper/mpvpaper) | **No** | Active; Python/GTK3; frontend for ~11 backends incl. swaybg, swww, hyprpaper, mpvpaper, feh, xwallpaper |
| Azote [16] | No | Local browser | Yes — split one image across displays, per-monitor scaling/crop; color picker/palette tools | **No** | Active; Python/GTK3; swaybg (wlroots) + feh (X11); GNOME unsupported |
| HydraPaper [17] | No | Local | Yes — its entire purpose (GNOME lacks per-monitor natively); GNOME/MATE/Budgie | **No** | Python/GTK; primary development moved to GitLab |
| styli.sh [18] | Via cron (`@hourly styli.sh`) | Picsum, Reddit/custom subreddits, local dirs | No | **No** | Active bash script; applies via feh, Nitrogen, GNOME, XFCE, KDE, Sway, swww, hyprpaper |

**Worth learning from:**
- **Waypaper is the architectural precedent for the provider seam**: one frontend, ~11 interchangeable apply-backends, per-monitor where the backend supports it [15]. wp-sync's "wallpaper provider" is the same pattern one level up.
- Variety's source-pipeline (fetch → filter junk → rotate) is the workflow vocabulary users know; its quotes/clock overlays show scope creep [14].
- styli.sh proves the whole apply-matrix (feh/gnome/sway/hyprpaper/...) fits in a bash script — which is where wp-sync already is [18].
- Azote/HydraPaper: per-monitor assignment is a real, recurring need — the wallpaper provider spec should treat monitor as a first-class target [16][17].

**What to avoid:** Variety's maintenance mode is the cost signal for supporting every DE plus every online source in one Python/GTK app [14]. None of these has tests of a social feature to learn from — **zero of the five do sharing**.

**Overlap:** high on the *apply* half (wp-sync's provider must match table-stakes: rotation, per-monitor, DE matrix), zero on the *sharing* half.

**Near-neighbors that do share wallpapers (both centralized):**
- **DynamicWallpaper** (Urpagin): Rust server + web UI to upload images; a client polls at intervals to mirror the server folder locally; friends share by pointing at the same server. Author: "rsync is simpler and better", "I am a beginner, so I cannot guarantee there are no vulnerabilities". 39 commits, Arch/systemd-focused [23].
- **Walltaker** (PawCorp): hosted Rails service (walltaker.joi.how) where you create a "link" granting a friend control of your wallpaper; clients poll `/api/links/[id].json` ~every 10s. Content restricted to e621.net with enforced blacklists; community clients for Windows/Mac/Linux/Android; built for the furry fandom [24].

Both validate demand for "wallpapers as a social object between friends," and both are central-server designs — confirming the local-first P2P slot is open. Walltaker's enforced blacklist is a notable consent/content-control precedent for any "friends can put images on my screen" product [24].

### 4. TUI media galleries / pickers with image preview

- **yazi** (Rust, 41k+ stars, "public beta, can be used as a daily driver"): fully async, built-in support for Kitty (incl. unicode placeholders), Sixel (foot, Windows Terminal, patched st), iTerm2 protocol (WezTerm, Tabby, VSCode), Konsole, plus Überzug++ for X11/Wayland and chafa ASCII fallback; Lua plugin system covering previewers/preloaders/UI; vim-like input components [19]. This is the state of the art for keyboard-first image browsing in a terminal.
- **ranger** (Python/curses, 17k stars): miller columns + vi bindings; previews via `scope.sh` pluggable script (w3mimgdisplay, ueberzug, kitty, iTerm2, urxvt, img2txt fallback); ships `rifle`, a MIME-driven "which program opens this file type" launcher [20]. `rifle` is conceptually a content-type action dispatcher — the closest existing cousin of wp-sync "providers."
- **ratatui-image**: a Ratatui widget that "unifies terminal image rendering across Sixels, Kitty, and iTerm2 protocols", including terminal capability querying — the off-the-shelf building block if the TUI is Rust/Ratatui [21].
- **timg**: terminal image/video viewer using Sixel/Kitty/iTerm2 with unicode-block fallback; useful as a preview subprocess for shell-first iterations [22].
- Assorted single-purpose viewers (tpix, pymv, garou) exist but are thin; **no TUI gallery is collection-aware, metadata-aware, or sync-aware** — they all browse the filesystem as-is.

**Worth learning from:** yazi's protocol matrix + graceful degradation (protocol → Überzug++ → chafa) is the compatibility recipe to copy [19]; ranger's `scope.sh`/`rifle` show that pluggable preview and pluggable open-actions can be plain scripts — friendly to wp-sync's bash heritage [20].

**What to avoid:** hand-rolling terminal graphics detection (ratatui-image exists precisely because it's fiddly) [21]; w3mimgdisplay-era hacks.

**Overlap:** these are components/inspiration, not competitors. A generic file manager will never know "this folder is Kai's circle" or "apply as wallpaper on monitor 2."

## Implications for the spec

1. **Integration architecture is settled by precedent: separate daemon + REST API.** Every surviving wrapper (Syncthing Tray, stc, built-in CLI) talks REST to an independently-running Syncthing; the process-owning approach (SyncTrayzor) aged worst [1][4][8][9]. The sync-engine seam should be a thin REST-shaped interface — which also keeps it replaceable, and insulates against REST churn like the v2 breakage that hit syncthing-gtk [3].
2. **The product's job is the layer nobody built: content + people.** Concretely: map *circle → Syncthing folder* (the folder is Syncthing's unit of sharing), and put collection metadata (who added an item, captions, favorites) in synced sidecar files with a conflict-tolerant convention — Syncthing syncs bytes, not semantics, and no prior art solves per-item metadata on top of it. This metadata layer is simultaneously the moat and the hardest design problem (any-sync solves it with CRDTs + node infra; wp-sync must solve it with files) [9][12].
3. **Permissions honesty.** Users coming from Resilio expect per-user read/write and revocation [10]; Syncthing offers per-device folder types only. The spec should state what a "circle" can and cannot enforce (e.g., removal from circle stops future sync but cannot claw back files) rather than imply ACLs that don't exist.
4. **Provider design has a proven template.** Waypaper's one-frontend/many-backends model [15] and ranger's `rifle` MIME dispatch [20] are the two patterns to merge: providers declare (a) how to preview, (b) how to apply, (c) apply targets (per-monitor is table stakes — Azote/HydraPaper exist solely for it [16][17]). Rotation/scheduling (Variety, `waypaperd`, styli.sh cron) belongs in the wallpaper provider, not the core [14][15][18].
5. **TUI feasibility is de-risked.** Kitty/Sixel/iTerm2 + chafa fallback is a known-good stack (yazi), and ratatui-image packages it if the TUI is Rust; a Bubbletea path exists too (syncthingtui) [7][19][21]. Choose based on team language, not rendering risk.
6. **Consent and safety are a real feature, not an afterthought.** Walltaker — the only successful "friends control your wallpaper" product — leads with enforced blacklists [24]. Anything that lets circle members push images onto screens needs approve-before-apply and per-circle content controls in v1 of the spec.
7. **Sustainability lesson from the wrapper graveyard:** official Android died over store friction, SyncTrayzor and syncthing-gtk over maintainer bandwidth [2][4][5]. Keep the core small (the bash script heritage is an asset), make providers/plugins external scripts, and avoid platform surfaces (mobile, GUI toolkits) that multiply maintenance.
8. **Differentiation assessment: the space is genuinely unoccupied.** Syncthing wrappers stop at instance management; local-first social tools don't use Syncthing; wallpaper managers don't share; TUI galleries aren't sync-aware; the only wallpaper-sharing apps are centralized hobby/community servers [23][24]. No project combines "private collections + trusted circles + content-type actions + Syncthing + TUI." The corollary: absence of competitors also reflects a niche audience — the spec should lean into the wedge (wallpapers between friends, where Walltaker proves demand) rather than pitch a generic sharing platform on day one.

## Sources

1. https://github.com/Martchus/syncthingtray — Syncthing Tray README (features, REST integration, syncthingctl)
2. https://github.com/kozec/syncthing-gtk — original Syncthing-GTK repo (feature set, Python-2-era status)
3. https://github.com/syncthing-gtk/syncthing-gtk — maintained community fork (0.10.0, Syncthing v2 compatibility)
4. https://github.com/canton7/SyncTrayzor — SyncTrayzor README (feature set, unmaintained notice, v2 fork pointer)
5. https://github.com/syncthing/syncthing-android — archived official Android app (discontinuation notice, Dec 2024)
6. https://github.com/Catfriend1/syncthing-android — Syncthing-Fork Android continuation (status, distribution)
7. https://github.com/Evidlo/syncthingtui — Bubbletea-based Syncthing TUI (scope, maturity)
8. https://github.com/tenox7/stc — Syncthing CLI tool (commands, config auto-discovery)
9. https://docs.syncthing.net/users/syncthing.html — official docs: built-in `syncthing cli` subcommand and REST wrapping
10. https://www.resilio.com/sync/ — Resilio Sync (Advanced Folders permissions, selective sync, free tier)
11. https://peergos.org/ — Peergos (friends-only sharing model, E2EE, self-hosting, audits)
12. https://github.com/anyproto/any-sync — any-sync protocol (CRDT spaces, required node roles, MIT, Anytype)
13. https://localsend.org/ — LocalSend (one-off LAN transfers, zero-config discovery, Apache-2.0)
14. https://github.com/varietywalls/variety — Variety (sources, rotation, effects, maintenance mode)
15. https://github.com/anufrievroman/waypaper — Waypaper (backend list, per-monitor, waypaperd)
16. https://github.com/nwg-piotr/azote — Azote (swaybg/feh, multi-display splitting, palette tools)
17. https://github.com/GabMus/HydraPaper — HydraPaper (per-monitor on GNOME/MATE/Budgie, GitLab move)
18. https://github.com/thevinter/styli.sh — styli.sh (sources, apply matrix, cron rotation)
19. https://github.com/sxyazi/yazi — Yazi (graphics-protocol matrix, async architecture, Lua plugins)
20. https://github.com/ranger/ranger — ranger (miller columns, scope.sh previews, rifle opener)
21. https://github.com/ratatui/ratatui-image — Ratatui image widget (unified Sixel/Kitty/iTerm2 rendering)
22. https://github.com/hzeller/timg — timg terminal image/video viewer
23. https://github.com/Urpagin/DynamicWallpaper — centralized wallpaper-sharing web UI + sync client
24. https://github.com/PawCorp/walltaker — Walltaker (central Rails service, link-based wallpaper control, blacklists)
