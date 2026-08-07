export const meta = {
  name: 'kith-v01-build',
  description: 'Implement walkthrough steps 4-12: store layer, engine adapter, commands, TUI, then integrate to green',
  phases: [
    { title: 'Foundations', detail: 'hash, records, claims, descriptors, config, invite, engine adapter' },
    { title: 'Commands', detail: 'create, add, membership, report' },
    { title: 'TUI', detail: 'gallery, preview, members' },
    { title: 'Integrate', detail: 'wire main.rs and the app loop, compile to green' },
  ],
}

const CTX = `You are implementing kith v0.1 in Rust at /home/opx/Projects/wp-sync. Work fully autonomously — senior-engineer judgment calls, never ask questions.

## What kith is
Local-first, peer-to-peer collections shared with the people you trust. v0.1 wedge: two friends share wallpapers, no cloud, no account, no server. The transport is a separately-running Syncthing daemon driven over REST; kith never launches, embeds or configures one.

## Required reading (the design is locked — implement it, do not redesign it)
- /home/opx/Projects/wp-sync/CONTEXT.md — the domain glossary. Use its vocabulary exactly in names, comments and user-facing strings: Person, Device, Identity, Circle, Member, Role, Steward, Invite, Collection, Item, Sidecar, Membership claim, Favourite, Provider, Action, Apply, Sync Engine, Presence. **Never** say "user", "folder", "file" or "online" in a domain position. "Syncthing" appears only inside src/engine/syncthing.rs.
- /home/opx/Projects/wp-sync/ROADMAP.md §2 (the v0.1 scope ceiling) and §3 (the 12-step acceptance walkthrough you are building).
- /home/opx/Projects/wp-sync/docs/adr/0002-sync-engine-seam.md — the SyncEngine trait and the domain→Syncthing mapping.
- /home/opx/Projects/wp-sync/docs/adr/0004-metadata-shared-state.md — **the on-disk layout. This one is load-bearing for the store layer.**
- /home/opx/Projects/wp-sync/docs/adr/0003-provider-seam.md — the Provider trait.
- The spec for your area under /home/opx/Projects/wp-sync/docs/spec/.
- Existing code: src/domain.rs, src/identity.rs, src/engine/mod.rs, src/provider/ — read before writing.

## Fixed module contracts — implement these signatures EXACTLY
Other agents are coding against them concurrently. Changing one breaks their code.

\`\`\`rust
// src/hash.rs
pub fn hash_file(path: &std::path::Path) -> std::io::Result<String>;  // "b3:" + 64 lowercase hex
pub fn short(hash: &str) -> &str;                                     // first 12 chars after "b3:"

// src/store/descriptors.rs
pub struct CircleDescriptor { pub schema: u32, pub id: String, pub name: String,
    pub created: String, pub founder_person: String, pub founder_device: String }
pub struct CollectionDescriptor { pub schema: u32, pub collection: String, pub provider: String }
pub fn write_atomic<T: serde::Serialize>(path: &std::path::Path, value: &T) -> std::io::Result<()>;
pub fn read_circle(root: &std::path::Path) -> std::io::Result<Option<CircleDescriptor>>;
pub fn write_circle(root: &std::path::Path, d: &CircleDescriptor) -> std::io::Result<()>;
pub fn read_collection(root: &std::path::Path, id: &str) -> std::io::Result<Option<CollectionDescriptor>>;
pub fn write_collection(root: &std::path::Path, d: &CollectionDescriptor) -> std::io::Result<()>;
pub fn seed_stignore(root: &std::path::Path, reserved: &[&str]) -> std::io::Result<()>;

// src/store/claims.rs
pub fn publish(root: &std::path::Path, device: &str, id: &crate::identity::Identity, now: &str) -> std::io::Result<()>;
pub fn read_all(root: &std::path::Path) -> std::io::Result<Vec<crate::domain::MembershipClaim>>;
pub fn derive_people(claims: &[crate::domain::MembershipClaim]) -> Vec<crate::domain::Person>;
pub fn stamp_left(root: &std::path::Path, device: &str, now: &str) -> std::io::Result<()>;

// src/store/records.rs
pub enum Record { Add {...}, Bind {...}, Remove {...} }   // serde-tagged on field "t": "add"|"bind"|"remove"
pub fn append(root: &std::path::Path, collection: &str, device: &str, rec: &Record) -> std::io::Result<()>;
pub fn read_all(root: &std::path::Path, collection: &str) -> std::io::Result<Vec<Record>>;
pub fn derive_items(records: &[Record], root: &std::path::Path) -> Vec<crate::domain::Item>;

// src/config.rs
pub struct Config { pub apply_backend: Option<String>, pub apply_command: Option<String>,
    pub monitors: Vec<String>, pub engine_address: Option<String>, pub engine_api_key: Option<String> }
pub fn path() -> Option<std::path::PathBuf>;   // $XDG_CONFIG_HOME/kith/config.toml
pub fn load() -> Config;                        // a missing file is NOT an error

// src/invite.rs
pub fn encode(ticket: &crate::engine::InviteTicket) -> String;
pub fn decode(code: &str) -> Result<crate::engine::InviteTicket, InviteError>;
pub enum InviteError { Malformed, Checksum, Expired, WrongVersion }

// src/cmd/*  — every command returns its process exit code
pub async fn create::run(name: &str, path: Option<&str>, adopt: bool) -> i32;
pub async fn add::run(paths: &[String]) -> i32;
pub async fn membership::invite(new: bool) -> i32;
pub async fn membership::join(code: &str) -> i32;
pub async fn membership::approve(device: Option<&str>) -> i32;
pub async fn membership::reject(device: Option<&str>) -> i32;
pub async fn report::list(subject: Option<&str>, json: bool) -> i32;
pub async fn report::status(json: bool) -> i32;
\`\`\`

Shared helpers you may rely on: \`crate::identity::{load, path, Identity}\`, \`crate::engine::syncthing::{SyncthingEngine, Credentials}\`, \`crate::engine::SyncEngine\` (17 methods), \`crate::provider::wallpaper::WallpaperProvider\`. Timestamps: \`jiff::Timestamp::now().to_string()\` (RFC 3339). Exit codes are sysexits: 0 ok, 1 fail, 64 usage, 65 data, 69 engine unavailable, 70 internal, 78 config.

## Rules
- **Write ONLY the file(s) your task names.** Other agents are editing the others right now. Never touch src/main.rs — the integrator owns it.
- The module tree is already declared and compiles, so your file is in the build. You MAY run \`cargo check\`, but expect errors from other agents' half-written files: **fix only errors whose path is your own file.** Never "fix" someone else's module.
- Write unit tests in your file (\`#[cfg(test)] mod tests\`) for the logic that is yours. Prefer tests over a demo binary. Use \`std::env::temp_dir()\` or a scratch dir under /tmp for filesystem tests, never the user's home.
- Match the existing code's voice: comments explain *why*, doc comments on public items, honesty caveats stated where the design demands them (Roles are policy not enforcement; Presence is a socket not a Person; Apply is always deliberate).
- Do NOT git commit. Do NOT touch any GitHub issue. Do NOT edit files under docs/.
- Where the spec leaves a genuine gap, make the call in the spirit of the ADRs and note it in a comment.`

const OUT = {
  type: 'object',
  properties: {
    summary: { type: 'string', description: 'What you implemented, one clause per public item' },
    contract_changes: { type: 'string', description: 'Any deviation from the fixed contract you were forced into, and why — or "none"' },
    depends_on: { type: 'string', description: 'Anything you needed from another module that does not exist yet — or "none"' },
    tests: { type: 'string', description: 'Test names you added, and whether cargo test passed for them' },
  },
  required: ['summary', 'contract_changes', 'depends_on', 'tests'],
}

const mk = (label, phase, body) => agent(`${CTX}\n\n${body}`, { label, phase, schema: OUT })

phase('Foundations')
log('Wave 1: seven agents — store layer, config, invite codec, and the engine adapter')

const w1 = [
  mk('hash', 'Foundations', `## Your file: src/hash.rs
Implement BLAKE3 content hashing per docs/spec/collections.md. One digest per file, reused as the record's hash, the dedup key and the thumbnail-cache key. Stream the file rather than reading it whole — Collections hold thousands of wallpapers. Render as \`b3:\` + 64 lowercase hex. \`short()\` returns the first 12 characters after the prefix, for display. Test against blake3's known vectors for the empty input and a short input.`),

  mk('store:descriptors', 'Foundations', `## Your file: src/store/descriptors.rs
The Circle and Collection descriptors plus the atomic descriptor protocol, per ADR-0004 §2/§3/§5. \`write_atomic\` writes \`<name>.toml.kith-tmp\` beside the target then renames — the temp name is engine-ignored so a half-written descriptor never replicates. Layout: \`.kith/circle.toml\`, \`.kith/collections/<id>.toml\` (v0.1 always \`main\`, provider \`wallpaper\`). \`seed_stignore\` writes the Circle's \`.stignore\` from the engine's reserved globs plus kith's own temp pattern — take the globs as the argument, never hardcode a Syncthing spelling in this file. Reading a descriptor that is absent is \`Ok(None)\`, not an error; reading a malformed one is an error. Test the tmp-and-rename leaves no stray file, and that read-after-write round-trips.`),

  mk('store:claims', 'Foundations', `## Your file: src/store/claims.rs
Membership claims per ADR-0004 §5 and docs/spec/identity-devices.md. One file per Device at \`.kith/members/<device-id>.toml\`, **written only by the Device it names** — that single-writer rule is the whole reason the layout is Device-keyed, so \`publish\` must refuse to write a claim whose filename is not this Device. Fields are exactly \`schema\`, \`device\`, \`person\`, \`display_name\`, \`asserted\`, optional \`left_at\`. A claim is a *descriptor*: read-modify-write, not append-only. \`derive_people\` folds claims by \`person\`, resolves display-name disagreement by newest \`asserted\`, and excludes anyone whose newest claim carries \`left_at\`. Handle a conflict copy by reading it (newest \`asserted\` wins) rather than failing. Test: two Devices of one Person fold to one Person; \`left_at\` removes them; newest \`asserted\` wins a name disagreement.`),

  mk('store:records', 'Foundations', `## Your file: src/store/records.rs
**The hardest module — read ADR-0004 §§4-6 closely before writing a line.** Per-Device append-only JSONL record logs at \`.kith/items/<collection-id>/<device-id>.jsonl\`, one JSON object per line, serde-tagged on \`t\`. Records: \`add\` (item ULID, by PersonId, at, title, path, hash, size), \`bind\` (item, new path/hash/size), \`remove\` (item — a tombstone). Exactly one writer per log file, which is what makes this conflict-free with no coordinator.
\`append\` must be crash-safe and must never interleave a partial line — open append-only, write one complete line, and take an advisory lock so two kith processes on one Device cannot tear each other's writes. \`read_all\` reads every \`*.jsonl\` in the collection directory and must **skip a damaged line and keep going** — one bad line costs one record, never the log.
\`derive_items\` is the union merge that produces the Sidecar view: newest \`bind\` wins for an Item's bytes; a \`remove\` tombstone wins **regardless of who wrote it**, because honouring tombstones conditionally on Role would break convergence; \`added_by\`/\`added_at\` come from the \`add\` record. Resolve the effective binding as ADR-0004 specifies. Test convergence hard: records read in any order derive the same Items; a tombstone from a non-adder still removes; a corrupt line is skipped.`),

  mk('config', 'Foundations', `## Your file: src/config.rs
The single TOML config per ROADMAP (apply backend and custom command, monitor names, daemon address/API-key override — **nothing more**; no themes, no keybindings) and docs/spec/cli-tui.md §8. \`$XDG_CONFIG_HOME/kith/config.toml\`. A missing file is not an error and yields defaults; unknown keys warn to stderr and are ignored (forward compatibility); a wrong *type* is a hard error the caller turns into exit 78. A named-but-undetected apply backend must NOT silently fall back — expose that so the caller can say so. Test: missing file yields defaults, unknown key survives with a warning, wrong type errors.`),

  mk('invite', 'Foundations', `## Your file: src/invite.rs
The v0.1 Invite artifact per docs/spec/circles-members-invites.md: a time-bounded printed code, no QR and no links. \`KITH1\` prefix, Crockford base32 payload, CRC-32 integrity check. The payload carries the Circle id, the Steward's Device Identity and the expiry. **The checksum is a checksum, not a signature** — say so in a doc comment; ROADMAP §5 forbids home-grown cryptography, so this catches typos and nothing else. Decoding must distinguish \`Malformed\`, \`Checksum\`, \`WrongVersion\` and \`Expired\` so the CLI can say something useful about each. Test round-trip, a single-character corruption caught by the CRC, a wrong prefix, and an expired ticket.`),

  mk('engine:syncthing', 'Foundations', `## Your file: src/engine/syncthing.rs (fill in the 13 \`todo!()\` methods)
Implement the rest of the adapter against the daemon's REST API + event stream, per ADR-0002. Syncthing v2.1.2 is installed on this machine — read its API docs knowledge carefully and prefer endpoints stable across v1.23+..v2.x.
- \`create_circle\`: add a folder (id, label, path), mark this Device its introducer, seed \`.stignore\`. Never mutate unrelated config.
- \`begin_join\`/\`complete_join\`: register the Steward's Device, knock; then accept the offered folder at \`root\`. Never enable autoAcceptFolders.
- \`pending_joins\`/\`admit\`: read pending devices; admit by adding the Device and sharing the Circle's folder. Deliberate, never automatic.
- \`expel\`, \`leave\`, \`set_introducer\`: per the ADR; \`leave\` stops replicating and never deletes local bytes.
- \`devices\`/\`status\`: peers with connection state and per-peer completion. \`devices()\` never returns self.
- \`observe\`: the long-poll event stream from a cursor, mapped to \`Change\`. A gap or a lost cursor surfaces as \`Change::Desynced\` rather than a silent hole — callers re-scan.
- \`versions\`/\`restore\`: the file-versioning endpoints; this is the real mitigation behind Roles-as-policy.
Map every HTTP failure into the closed \`SyncError\` enum — 401/403 is \`Unauthorized\` and is **never** auto-repaired. Keep the existing \`get\` helper and add \`post\`/\`put\` in the same shape. Unit-test the JSON→domain mapping with sample payloads as \`&str\`; do not require a live daemon in tests.`),
]

const w1r = await parallel(w1.map(p => () => p))

phase('Commands')
log('Wave 2: four agents — the CLI verbs that make the walkthrough run')

const w2 = [
  mk('cmd:create', 'Commands', `## Your file: src/cmd/create.rs
\`kith create <name> [--path <p>] [--adopt]\` — walkthrough step 4. Per docs/spec/circles-members-invites.md §3.1 and docs/spec/collections.md.
Sequence: require an Identity (exit 78 if absent); resolve the root (\`--path\`, else a default under the Person's home); \`SyncEngine::create_circle\`; seed \`.stignore\` from \`reserved_paths()\`; write \`.kith/circle.toml\` with \`founder_person\`/\`founder_device\` (the Steward's Device is read from here forever after, never from \`PeerDevice.introducer\`); write \`.kith/collections/main.toml\`; publish this Device's Membership claim. \`--adopt\` runs the content-adoption pass over an existing wallpaper directory instead of creating an empty one — the wp-sync migration path, adopting the tree rather than recreating it. Engine unreachable is exit 69 with a message naming what to start. Every step must be idempotent enough that a half-finished create can be re-run.`),

  mk('cmd:add', 'Commands', `## Your file: src/cmd/add.rs
\`kith add <paths>...\` — walkthrough steps 5 and 12. Per docs/spec/collections.md §4.
Copy by default (the source is the Person's own library; copy is reversible); register in place when the path is already inside the Circle root. Gate every candidate through the Collection's Provider \`claims()\` — content the Provider does not claim is refused with a message, never silently accepted. For each accepted candidate: hash it, extract Provider facts, mint an Item ULID, append an \`add\` record. Dedup on content hash against live Items only, so re-adding bytes that match a tombstoned Item revives it. Directory arguments recurse and preserve internal structure; the Collection root and any other Circle root are excluded from recursion. Pre-flight the free space and refuse rather than fill the disk; a partial run must be resumable. Batch the fsync (once per 64 records or 250 ms) rather than per record. Report a per-run summary: added, skipped-duplicate, refused-unclaimed.`),

  mk('cmd:membership', 'Commands', `## Your file: src/cmd/membership.rs
The four social verbs — walkthrough steps 6, 7, 8. Per docs/spec/circles-members-invites.md §3.
- \`invite(new)\`: mint or reprint the Circle's single open Invite window and print the code from src/invite.rs. Default TTL 24h. Works even when the engine is unreachable, since it needs only local state.
- \`join(code)\`: decode (distinguishing malformed / bad checksum / expired), \`begin_join\`, then record a pending join locally and tell the Person plainly that they now wait for the Steward to admit them.
- \`approve(device)\`/\`reject(device)\`: list knocking Devices, show each one's fingerprint (first 8 characters of its Device Identity, grouped 4-4) so the Steward can tell one knock from another out of band. Approve calls \`admit\`. **Reject is purely local state** — a dismissal file, not an engine call. With no argument and exactly one knock, act on it; with several, list them and exit 64.
Only the Steward's Device sees pending joins; say so honestly when a non-Steward runs approve.`),

  mk('cmd:report', 'Commands', `## Your file: src/cmd/report.rs
\`kith list [items|circles|members]\` (items default) and \`kith status\`, both with \`--json\`. Per docs/spec/cli-tui.md §§3-4.
The JSON envelope is exactly \`{schema, command, ok, exit, data, error, notes[]}\`, where \`notes[]\` carries the same honesty caveats a Person sees in prose — a script must not get a cleaner story than a human. Members show **Presence** (\`connected\`/\`not_connected\`/\`unknown\`), never "online", and the Role caveat is a note, not a footnote nobody reads. Items come from \`store::records::derive_items\`; People from \`store::claims::derive_people\`. \`status\` reports the Circle's sync state and per-peer completion, and when the engine is unreachable it prints local facts, says they are last-known, and exits 69. Never report what other Members hold — kith knows only local facts plus what the engine tells it.`),
]

const w2r = await parallel(w2.map(p => () => p))

phase('TUI')
log('Wave 3: three agents — the screens the wedge actually lives in')

const w3 = [
  mk('tui:gallery', 'TUI', `## Your file: src/tui/gallery.rs
The Gallery grid — walkthrough steps 9 and 12, and the screen the whole product exists for. Per docs/spec/gallery-preview-actions.md.
Expose \`pub struct Gallery\` with \`new(items) / render(frame, area) / handle_key(key) -> Option<GalleryAction>\` and \`pub enum GalleryAction { Open(ItemId), Apply(ItemId), Favourite(ItemId), Reveal(ItemId), Delete(ItemId), Quit }\`. Grid of thumbnails sorted by date added, a favourite marker and an unseen dot; the favourites toggle is the only filter. Thumbnails come from ratatui-image's \`StatefulImage\`; the cache is keyed on content hash at two canonical sizes so a resize never invalidates it, and it is **rebuildable — never authoritative**. Render the three honest states distinctly: normal, record-without-bytes (placeholder tile), and bytes-without-record (arriving tile). Selection is drawn *outside* the image — reverse-video caption plus a gutter bar — because pixel protocols forbid overpainting image cells. **Arrival must never move the selection**: that invariant is what makes it impossible for a Member's incoming content to be substituted under a Person's Apply keystroke mid-gesture. Widen the tile on the halfblocks rung — fewer, more legible pictures beat more mush.`),

  mk('tui:preview', 'TUI', `## Your file: src/tui/preview.rs
The fullscreen Preview — walkthrough step 10. Per docs/spec/gallery-preview-actions.md §5.
Expose \`pub struct Preview\` with \`new(item) / render(frame, area) / handle_key(key) -> Option<PreviewAction>\`. One Item large, with its Sidecar facts: title, who added it, when, resolution, byte size. Attribution renders the Person's display name, falling back to \`unknown Person (p-01k1yf)\` — a Person short form, **never** a device id standing in for a Person. Show which preview rung is in use without nagging about it. Degrade down the ladder to halfblocks and, when the bytes are absent or undecodable, to the text card, which is the tier that must never fail. Reserve a blank content row at the bottom on the sixel rung.`),

  mk('tui:members', 'TUI', `## Your file: src/tui/members.rs
The Members screen and the pending-join prompt — walkthrough step 8. Per docs/spec/circles-members-invites.md §§3.5-3.7.
Expose \`pub struct Members\` with \`new(people, pending) / render(frame, area) / handle_key(key) -> Option<MembersAction>\`. List Members with display name, Role and **Presence** — \`connected\` / \`not connected\` / \`unknown\`, never "online", and never a claim about the Person rather than the socket. The Role line must carry the policy-not-enforcement caveat in the UI copy itself, not a help page. The pending-join prompt is raised automatically on the Steward's Device and also pinned here, showing the knocking Device's fingerprint grouped 4-4 for out-of-band confirmation, with approve/reject keys and the consequence stated inline. An unclaimed Device — present in the Circle but matching no Membership claim — renders honestly by name-or-id rather than being hidden.`),
]

const w3r = await parallel(w3.map(p => () => p))

phase('Integrate')
log('Wave 4: wire main.rs and the app loop, then compile and test to green')

const integration = await agent(`${CTX}

## You are the integrator. You own src/main.rs and src/tui/mod.rs, and you may edit ANY file to fix a compile error.

Sixteen agents just wrote the modules in parallel against the fixed contracts. Your job is to make the whole thing build, pass its tests, and actually run.

What the other agents reported:
- Foundations: ${JSON.stringify(w1r.filter(Boolean).map(r => ({ s: r.summary, c: r.contract_changes, d: r.depends_on })))}
- Commands: ${JSON.stringify(w2r.filter(Boolean).map(r => ({ s: r.summary, c: r.contract_changes, d: r.depends_on })))}
- TUI: ${JSON.stringify(w3r.filter(Boolean).map(r => ({ s: r.summary, c: r.contract_changes, d: r.depends_on })))}

### 1. Write src/tui/mod.rs — the app loop
\`pub async fn run() -> i32\`. Terminal setup/teardown that restores the terminal on **every** exit path including panic (install a hook — a TUI that leaves a wrecked terminal behind is the first thing a Person will hate). Screen routing across Gallery / Preview / Members / pending-join prompt, plus a plain Circle switcher when a Person has more than one Circle. That is the entire v0.1 screen budget — add nothing. Event routing is overlay → screen → global. Subscribe to \`SyncEngine::observe\` and refresh on arrival, but **arrival must never move the Gallery selection**. Keep the consent invariant structural and greppable: construct an Apply command only inside a key handler, never inside a sync or tick handler.

### 2. Wire src/main.rs
Dispatch every verb to its \`cmd::\` function, bare \`kith\` to \`tui::run\`. Keep \`doctor\` and \`init\` working exactly as they do now. Honour \`--json\` globally. Map errors to the sysexits codes.

### 3. Compile to green — this is the bulk of the work
Run \`cargo check --all-targets\`, then fix. Iterate until clean. Where two modules disagree, the **fixed contract in this prompt wins**; adapt the caller, not the contract, unless the contract is genuinely impossible — say so if you change it. Then \`cargo test\` until green: if a test encodes a real spec requirement and the code is wrong, fix the code; if the test itself is wrong about the spec, fix the test and say which. Then \`cargo clippy --all-targets\` and clear what is worth clearing; do not churn style.

### 4. Prove it runs
Exercise what you can without a second machine, using a scratch \`XDG_DATA_HOME\`/\`XDG_CONFIG_HOME\` under /tmp — **never the user's real home**. At minimum: \`kith --help\`, \`kith doctor\`, \`kith init\`, \`kith create\` against the live Syncthing daemon on this machine, \`kith add\` with a generated test image, \`kith list --json\`, \`kith invite\`. Report exactly which walkthrough steps you saw work and which you could not reach, and why. **Do not claim a step passes that you did not run.**

Do NOT git commit. Report honestly — a truthful "steps 4-7 run, 8-12 blocked on a second Device" is worth far more than an optimistic summary.`, {
  label: 'integrate + compile to green',
  phase: 'Integrate',
  effort: 'high',
  schema: {
    type: 'object',
    properties: {
      builds: { type: 'boolean', description: 'Does cargo check --all-targets pass?' },
      tests: { type: 'string', description: 'cargo test result: counts passed/failed, and which failed' },
      steps_verified: { type: 'string', description: 'Which of walkthrough steps 4-12 you actually ran and saw work' },
      steps_blocked: { type: 'string', description: 'Which you could not reach, and the specific reason' },
      contract_changes: { type: 'string', description: 'Contracts you had to change to make modules agree, and why' },
      known_broken: { type: 'string', description: 'Anything that compiles but you believe is wrong or unfinished' },
    },
    required: ['builds', 'tests', 'steps_verified', 'steps_blocked', 'contract_changes', 'known_broken'],
  },
})

return { foundations: w1r, commands: w2r, tui: w3r, integration }
