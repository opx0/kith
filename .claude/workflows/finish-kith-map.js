export const meta = {
  name: 'finish-kith-map',
  description: 'Resolve all remaining Wayfinder tickets (ADRs, specs, roadmap, README) with dependency-ordered parallel agents',
  phases: [
    { title: 'Wave 1', detail: 'sync-engine ADR, provider ADR, roadmap' },
    { title: 'Wave 2', detail: 'metadata ADR, identity spec, circles spec, CLI/TUI spec' },
    { title: 'Wave 3', detail: 'collections spec, gallery spec, periphery, README' },
  ],
}

const CTX = `You are resolving a Wayfinder ticket on the repo at /home/opx/Projects/wp-sync (GitHub: opx0/wp-sync). Work fully autonomously — senior-engineer judgment calls, never ask questions.

## Product context (DECIDED — authoritative, do not relitigate)
- Product: **kith**, binary \`kith\`. Identity: "Local-first, peer-to-peer collections shared with the people you trust." v0.1 wedge: share wallpapers with friends.
- Read /home/opx/Projects/wp-sync/CONTEXT.md first — the domain glossary. **Use its vocabulary exactly** (Person, Device, Identity, Circle, Member, Role, Invite, Collection, Item, Sidecar, Favourite, Provider, Action, Apply, Sync Engine, Activity). Never say "user", "folder", "file", or "Syncthing" in a domain position; Syncthing vocabulary belongs only behind the Sync Engine seam.
- Read /home/opx/Projects/wp-sync/docs/adr/0001-implementation-stack.md — Rust, ratatui + ratatui-image (kitty→iTerm2→sixel→halfblocks), clap, tokio+reqwest against a separately-owned Syncthing daemon, SQLite as rebuildable cache only (synced tree is the sole source of truth), TOML config.
- v0.1 constraints: every Circle has exactly one Collection; one Provider (wallpaper).
- Research is on branches — read with: git show research/syncthing-api:docs/research/syncthing-api.md ; git show research/prior-art:docs/research/prior-art.md ; git show research/tui-landscape:docs/research/tui-landscape.md
- Established research facts: every Sync Engine op maps to Syncthing REST + events but enforcement is local-only (Roles = policies + file versioning, not ACLs); exactly one introducer per Circle is the circle-admin primitive; Circle→Syncthing folder; REST surface churns so the seam must be narrow; hardest open problem is conflict-tolerant per-Item metadata on bytes-only sync; per-monitor apply is table stakes; consent (approve-before-apply) is mandatory for anything friends can push.

## Rules
- Write ONLY the file(s) your ticket names. Do NOT edit CONTEXT.md, the map issue (#1), or other files. Do NOT git commit.
- Match the tone/density of docs/adr/0001-implementation-stack.md: decisive, dense, no hedging, tables where they earn their place. ADRs use Status/Date(2026-08-06)/Resolves/Informed-by header then Context/Decision/Consequences/Alternatives considered. Specs use a tight structure: Purpose, Domain objects involved, Behaviour (the bulk — concrete flows), Edge cases & failure honesty, CLI/TUI surface touchpoints, Out of scope for v0.1.
- When done: write a ~12-bullet resolution summary to a temp file under /tmp/claude-1000/-home-opx-Projects-wp-sync/238918d5-dc69-4002-8686-cd3f03c965fc/scratchpad/ and run: gh issue comment <N> --body-file <file> && gh issue close <N> (retry once on network timeout).
- If a decision you need is missing, make the call yourself in the spirit of the existing ADRs and record it in your document.`

const OUT = {
  type: 'object',
  properties: {
    gist: { type: 'string', description: 'One line, max 30 words, summarising the answer for the map decision index' },
    terms: { type: 'string', description: 'New domain terms for CONTEXT.md with one-line definitions, or "none"' },
    file: { type: 'string', description: 'Repo-relative path of the main file written' },
    closed: { type: 'boolean', description: 'Whether gh issue close succeeded' },
  },
  required: ['gist', 'terms', 'file', 'closed'],
}

phase('Wave 1')

const p7 = agent(`${CTX}

## Ticket #7 "ADR: sync-engine seam & Syncthing mapping" → write /home/opx/Projects/wp-sync/docs/adr/0002-sync-engine-seam.md

Required reading beyond CTX: full research/syncthing-api doc. Nail down concretely:
1. The seam interface — actual Rust trait \`SyncEngine\` with named methods/signatures, each justified by a domain operation (create Circle, join, leave, list Members, observe changes, report status). Narrow — this is the churn firewall. Name associated types/errors.
2. The mapping table — every domain concept to its Syncthing counterpart: Circle→folder (folder IDs, labels), Device→device ID, Member→set of a Person's device IDs, Invite→?, Collection→path within folder, Item→file, Sidecar→file.
3. Circle admin via introducer — exactly one introducer per Circle: what it buys (automatic device propagation), what it costs (single point of failure for new joins), recovery/succession when the introducer Device is offline or lost.
4. Roles as policy — honestly what a Role can/cannot do given local-only enforcement; specify file versioning config (name the Syncthing versioning strategy + settings) as the real mitigation for "a Member deleted everything".
5. Change observation — the long-poll event stream: which event types matter (name them), disconnect/replay behaviour.
6. Daemon lifecycle & credential discovery — kith never owns the process; how it finds API key/address, behaviour when Syncthing absent or rejecting, what it must never mutate in existing config.
7. Migration compat note — existing wp-sync installs adopt their existing folder + config (brief subsection).
Resolution comment on issue 7, then close it.`, { label: 'adr:sync-engine #7', phase: 'Wave 1', schema: OUT })

const p9 = agent(`${CTX}

## Ticket #9 "ADR: provider seam (wallpaper provider first)" → write /home/opx/Projects/wp-sync/docs/adr/0003-provider-seam.md

Required reading beyond CTX: research/prior-art sections 3-4, research/tui-landscape. Precedents: Waypaper (one frontend over ~11 apply-backends), ranger's rifle (MIME action dispatch), scope.sh (script previews), Azote/HydraPaper (per-monitor is table stakes), Variety maintenance-mode warning, Walltaker enforced-blacklist consent precedent. ADR-0001 promises Providers stay scriptable so contributions don't touch the core — honour it. Nail down concretely:
1. The seam interface — actual Rust trait(s) with named methods/signatures: which Items a Provider claims (MIME/extension), preview production, declared Actions, Action execution, metadata extraction on import, Apply target declaration.
2. Built-in vs external Providers — decide (compiled-in, external executables with a declared protocol, or both) and justify; if external, specify the protocol precisely (discovery path, invocation, stdin/stdout contract, exit codes).
3. Action model — core Actions (favourite, delete, copy path, reveal) vs Provider Actions (apply + targets); how Actions surface in TUI/CLI; failure reporting.
4. Apply targets & backend matrix — monitors first-class; backend detection/precedence across swaybg/swww/hyprpaper/feh/xwallpaper/gnome/kde; defensible v0.1 subset + fallback when none found. Don't support every DE.
5. Preview — cooperation with the ratatui-image ladder, thumbnail caching (rebuildable, never authoritative), non-visual content fallback.
6. Consent — Apply is deliberate by domain rule; specify how auto-apply/rotation stays safe: approve-before-apply, per-Circle content controls, explicit opt-in.
7. Rotation/scheduling — lives in the wallpaper Provider; where the schedule lives and what drives it (kith is not a daemon by default — decide and justify).
Resolution comment on issue 9, then close it.`, { label: 'adr:provider #9', phase: 'Wave 1', schema: OUT })

const p10 = agent(`${CTX}

## Ticket #10 "Decide: v0.1 scope & roadmap" → write /home/opx/Projects/wp-sync/ROADMAP.md

Required reading beyond CTX: the product universe doc — first comment on the map: gh issue view 1 --comments --json comments --jq '.comments[0].body' — it is explicitly NOT a TODO list. Also prior-art "Implications for the spec" (failure mode is maintainer burnout; lean into the wedge). Structure:
1. The wedge — what v0.1 is and what makes it finishable; the single user story delivered end-to-end.
2. v0.1 in scope — every included module with the minimum it must do. Be ruthless: cut Activity, Notifications, Search, Health, Automation, Plugins, History if not needed for the wedge — but for each cut module state why and which release it returns in. Every universe module must appear somewhere in the file (v0.1, later milestone, or not planned).
3. The v0.1 walkthrough — numbered end-to-end flow of actual kith commands + TUI steps, from "two friends install it" to "a wallpaper added by one is applied by the other". This is the acceptance criterion.
4. v0.2, v0.3, v1.0 — one-line theme + modules each; coarser the further out, and say so.
5. Not planned — consciously ruled out + explicit non-goals (Not Dropbox, Not a Syncthing replacement, Not a file explorer, Not a social network...).
6. Sustainability rules — 3-5 standing rules from the wrapper-graveyard research that keep the project finishable.
Tone: this document's job is to let someone say "no" by pointing at a line. Resolution comment on issue 10, then close it.`, { label: 'roadmap #10', phase: 'Wave 1', schema: OUT })

log('Wave 1 launched: #7 sync-engine ADR, #9 provider ADR, #10 roadmap')

// Wave 2 — each agent starts the moment its actual dependency lands (no global barrier)
const p11 = p7.then(r7 => agent(`${CTX}

## Ticket #11 "ADR: metadata & shared-state model" → write /home/opx/Projects/wp-sync/docs/adr/0004-metadata-shared-state.md

This is the hardest design problem in the product (prior-art research: "conflict-tolerant per-item metadata on bytes-only sync" is the moat and the risk). Read /home/opx/Projects/wp-sync/docs/adr/0002-sync-engine-seam.md first (just written; gist: ${r7 ? r7.gist : 'see file'}) and the syncthing-api research (sync conflict behaviour, .stversions, conflict-file naming). Nail down concretely:
1. Where shared state lives — the on-disk layout inside a Circle's synced tree: Items' bytes, Sidecars, Circle/Collection metadata, membership records. Name actual paths/conventions (e.g. a dotted metadata directory) and what is hidden from the gallery.
2. The Sidecar format — exact file format + schema for per-Item metadata (who added, when, title, tags). Design for conflict tolerance on a transport that syncs bytes with last-writer-wins + conflict copies: the recommended shape is per-Person append-friendly files or per-(Item,Person) records so two Members never write the same file; decide and specify precisely, with merge/read semantics (how kith derives one view from many records) and what happens when a Syncthing conflict copy of a metadata file DOES appear.
3. Identity & attribution records — how a Person (display name, avatar hash, their Device set) is represented in synced state so attribution survives Devices coming/going; how records are trusted (signed or convention-only — decide honestly).
4. Favourites — per-Person and private by domain rule: where they live (local-only vs synced-but-namespaced; decide, justify).
5. Tombstones & removal — how "Item removed from Collection" propagates without a coordinator; interaction with Roles-as-policy and file versioning.
6. Activity derivation — Activity is derived, not logged authoritatively: what it is derived from and its consistency limits, stated honestly.
7. Cache interaction — restate the authority rule (SQLite rebuildable from tree) and specify the rebuild path.
8. Versioning/evolution — schema version field, forward-compat rule for older clients.
Resolution comment on issue 11, then close it.`, { label: 'adr:metadata #11', phase: 'Wave 2', schema: OUT }))

const p12 = p7.then(r7 => agent(`${CTX}

## Ticket #12 "Spec: identity & devices" → write /home/opx/Projects/wp-sync/docs/spec/identity-devices.md (deep, v0.1-core)

Read docs/adr/0002-sync-engine-seam.md first (gist: ${r7 ? r7.gist : 'see file'}). Spec the identity & devices module: first-run experience (creating a Person: display name, optional avatar; what is generated and where it lives), Device registration and naming/renaming, how a Person's several Devices are linked as one identity (concretely, over the Sync Engine seam), trust management (what trusting a Device means and when a Person is asked), identity loss honesty (no recovery authority — spell out what losing all Devices means and what the docs tell People), and the exact CLI verbs + TUI touchpoints for all of this. Include edge cases: same Person joins a Circle from two Devices; a Device is re-installed; two People share a machine. Resolution comment on issue 12, then close it.`, { label: 'spec:identity #12', phase: 'Wave 2', schema: OUT }))

const p13 = p7.then(r7 => agent(`${CTX}

## Ticket #13 "Spec: circles, members & invites" → write /home/opx/Projects/wp-sync/docs/spec/circles-members-invites.md (deep, v0.1-core)

Read docs/adr/0002-sync-engine-seam.md first (gist: ${r7 ? r7.gist : 'see file'}). Spec the social core: creating a Circle (what happens underneath, one Collection auto-created per v0.1 rule), the full Invite lifecycle (issue → transmit — decide the v0.1 invite artifact: code string vs QR vs link, pick and justify → accept → introducer approves → propagation; expiry and revocation), joining flow from the invitee's side step by step, Member listing with online status honesty (what kith can actually know), Roles in v0.1 (pick the minimal set — e.g. just admin-vs-member, admin = introducer holder — and state the policy-not-enforcement caveat surfaces in UI copy), removing a Member (what it does and does NOT do — cannot claw back files; versioning protects the rest), leaving a Circle, deleting a Circle. Edge cases: invite accepted after expiry; introducer offline during a join; two Members invite the same Person concurrently. Exact CLI verbs + TUI touchpoints. Resolution comment on issue 13, then close it.`, { label: 'spec:circles #13', phase: 'Wave 2', schema: OUT }))

const p16 = p10.then(r10 => agent(`${CTX}

## Ticket #16 "Spec: CLI & TUI surface" → write /home/opx/Projects/wp-sync/docs/spec/cli-tui.md (deep, v0.1-core)

Read /home/opx/Projects/wp-sync/ROADMAP.md first (just written; gist: ${r10 ? r10.gist : 'see file'}) and scope this spec to its v0.1 cut. Spec the complete surface: the full \`kith\` command tree for v0.1 (every subcommand with synopsis, key flags, exit codes, and JSON output mode for scripting), the bare-\`kith\` TUI: screen inventory for v0.1, navigation model, keybinding philosophy (vim-flavoured, keyboard-first, discoverable via which-key-style hints — decide), the gallery/preview screens only at pointer level (deep spec is ticket #15's file — reference it, don't duplicate), status/error surfacing conventions (including the honesty rules: Sync Engine down, Role-policy caveats), config file location + TOML shape sketch, and \`kith doctor\` diagnostics. Include a table mapping every ROADMAP v0.1 capability → CLI verb + TUI screen so coverage is checkable. Resolution comment on issue 16, then close it.`, { label: 'spec:cli-tui #16', phase: 'Wave 2', schema: OUT }))

// Wave 3 — chained on actual dependencies
const p14 = Promise.all([p11]).then(([r11]) => agent(`${CTX}

## Ticket #14 "Spec: collections" → write /home/opx/Projects/wp-sync/docs/spec/collections.md (deep, v0.1-core)

Read docs/adr/0002-sync-engine-seam.md and docs/adr/0004-metadata-shared-state.md first (metadata gist: ${r11 ? r11.gist : 'see file'}). Spec the Collections module for v0.1 (one Collection per Circle): what a Collection is on disk vs in the domain, adding Items (import flow: copy vs move, metadata extraction via Provider, Sidecar creation), importing an existing directory of wallpapers, removing Items (tombstones per ADR-0004), Item naming/dedup (same image added by two Members — decide: content-hash identity), size/statistics honesty (local knowledge only), the wp-sync migration path (existing installs adopt their synced folder — concrete steps), and CLI verbs + TUI touchpoints. Edge cases: huge directories, non-image files dropped into the tree by a Member, disk-full, partially-synced Items appearing in the gallery. Resolution comment on issue 14, then close it.`, { label: 'spec:collections #14', phase: 'Wave 3', schema: OUT }))

const p15 = Promise.all([p9, p11]).then(([r9, r11]) => agent(`${CTX}

## Ticket #15 "Spec: gallery, preview & actions" → write /home/opx/Projects/wp-sync/docs/spec/gallery-preview-actions.md (deep, v0.1-core)

Read docs/adr/0003-provider-seam.md (gist: ${r9 ? r9.gist : 'see file'}) and docs/adr/0004-metadata-shared-state.md (gist: ${r11 ? r11.gist : 'see file'}) first, plus research/tui-landscape for the preview ladder. Spec the browse experience: gallery grid (layout, thumbnail pipeline + cache rules per the rebuildable-cache authority rule, sort orders, filtering by Member/favourite/date for v0.1), selection & navigation keys, the Preview screen (full metadata display: who added, when, resolution, size, applied-state), the Action palette (core Actions + wallpaper Provider Actions incl. per-monitor Apply target picking), apply feedback and failure surfacing, approve-before-apply consent flow appearing in the gallery for newly-arrived Items, partially-synced/missing-bytes Item rendering, and graceful degradation per preview rung down to halfblocks. CLI equivalents (kith browse, kith preview, kith apply) at synopsis level referencing #16's file. Resolution comment on issue 15, then close it.`, { label: 'spec:gallery #15', phase: 'Wave 3', schema: OUT }))

const p17 = Promise.all([p10, p11]).then(([r10, r11]) => agent(`${CTX}

## Ticket #17 "Spec: periphery modules (shallow)" → write /home/opx/Projects/wp-sync/docs/spec/periphery.md

Read ROADMAP.md (gist: ${r10 ? r10.gist : 'see file'}) and docs/adr/0004-metadata-shared-state.md (gist: ${r11 ? r11.gist : 'see file'}) first. Concept-level shallow specs — one file, one section per module, each ~15-25 lines: Automation (rotation/rules — rotation itself is the wallpaper Provider's per ADR-0003; this section covers the rest), Notifications, Search, Health (kith doctor + sync status), Activity (derived per ADR-0004), Configuration, Plugin system (capabilities vision only; explicitly deferred). Each section: what it is in kith's domain language, which milestone lands it (align exactly with ROADMAP.md), key design constraint carried from the ADRs, and what would be a mistake to build early. This file's job is to make the periphery consciously shallow rather than accidentally unspecced. Resolution comment on issue 17, then close it.`, { label: 'spec:periphery #17', phase: 'Wave 3', schema: OUT }))

const p18 = p10.then(r10 => agent(`${CTX}

## Ticket #18 "Task: README & repo identity rewrite for kith" → rewrite /home/opx/Projects/wp-sync/README.md

Read the existing README.md fully first (you may overwrite it after reading), plus ROADMAP.md (gist: ${r10 ? r10.gist : 'see file'}). Rewrite README.md as kith's front door: name + one-line identity ("Local-first, peer-to-peer collections shared with the people you trust"), the wedge pitch (share wallpapers with your circle — no cloud, no account, no server), a "how it works" paragraph honest about Syncthing underneath, current status (spec-complete, pre-build — link CONTEXT.md, ROADMAP.md, docs/adr/, docs/spec/), a note that this repo was previously the wp-sync bash script (kept at wp-sync-setup.sh until v0.1 lands; existing installs will adopt their synced folder), and a Handoff section listing the repo rename opx0/wp-sync → opx0/kith as a to-do at build start (GitHub redirects old URLs). Keep it tight — a README, not a spec. Resolution comment on issue 18, then close it.`, { label: 'readme #18', phase: 'Wave 3', schema: OUT }))

const results = await parallel([
  () => p7, () => p9, () => p10,
  () => p11, () => p12, () => p13, () => p16,
  () => p14, () => p15, () => p17, () => p18,
])

const names = ['#7 sync-engine ADR', '#9 provider ADR', '#10 roadmap', '#11 metadata ADR', '#12 identity spec', '#13 circles spec', '#16 cli-tui spec', '#14 collections spec', '#15 gallery spec', '#17 periphery', '#18 readme']
return results.map((r, i) => ({ ticket: names[i], ...(r || { gist: 'AGENT FAILED — needs rerun', terms: 'none', file: '', closed: false }) }))