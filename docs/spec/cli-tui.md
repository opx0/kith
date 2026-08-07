# SPEC — CLI & TUI surface

- **Status:** Accepted
- **Date:** 2026-08-07
- **Resolves:** [#16 Spec: CLI & TUI surface](https://github.com/opx0/wp-sync/issues/16)
- **Informed by:** `ROADMAP.md` §§2–3, `CONTEXT.md`, ADR-0001, ADR-0002, ADR-0003

## Purpose

One binary, `kith`, is both a scriptable CLI and a keyboard-first TUI. This spec fixes
that surface for v0.1: every subcommand, every flag, every exit code, the JSON envelope
scripts parse, the TUI screen stack and its keymap, how status and failure are worded,
the config file, and `kith doctor`'s checks.

The surface is closed. ROADMAP §2 fixes the CLI at eleven verbs plus bare `kith`, and the
TUI at Gallery, Preview, Members, a pending-join prompt and a Circle switcher. **Nothing
in this document adds a verb or a screen.** Where an ADR names a verb that ROADMAP does
not (`kith apply`, `kith rotate`, `kith restore`), it is placed in "Out of scope for v0.1"
with its milestone, or folded into an existing verb as a flag — as adoption is, on
`create` (§4.2) — never smuggled in.

Gallery and Preview are described here only as far as navigation requires. Their tiles,
sorting, unseen/favourite markers, Sidecar fact layout and the Action set belong to
`docs/spec/gallery-preview-actions.md` (#15) and are referenced, not restated.

## Domain objects involved

| Object | How this surface touches it |
|---|---|
| **Person** | Created by `kith init`; named in attribution, presence and the Members screen. Never called a user. |
| **Device** | Bound at `init`; shown as a 7-character short id in join prompts and `kith status`. |
| **Identity** | A precondition, never a thing this surface edits. Absent → every Circle verb refuses with one fix line. |
| **Circle** | The scope argument of almost every command (`--circle`), the TUI's top-level context, the Circle switcher's list. |
| **Member / Role** | Listed by `kith list members` and the Members screen, always with the Role caveat (§7.2). |
| **Invite** | Produced by `kith invite` as one paste-able code; consumed by `kith join`; gated by `kith approve` / `kith reject` and the join prompt. |
| **Collection** | Implicit in v0.1 — one per Circle. `kith add` imports into it; `kith list items` and the Gallery read it. |
| **Item** | Addressed by the Item ref grammar (§1.4); rendered as rows in `kith list items`, tiles in the Gallery. |
| **Sidecar** | Read for the facts shown in `list items`, Preview and Members; written by `kith add`. Format is ADR-0004's. |
| **Favourite** | Toggled by `f`; private, never printed to anyone else's surface, never sent across the seam. |
| **Provider / Action / Apply** | The action menu, the `a` key, the monitor picker, and `doctor`'s apply section. Availability and reasons per ADR-0003 §3. |
| **Sync Engine** | Surfaced by `kith status`, `kith doctor` and the TUI status bar. Named "Sync Engine" everywhere except actionable setup instructions (§7.8). |

---

## Behaviour

### 1. One binary, two surfaces

#### 1.1 Invocation

| Invocation | Result |
|---|---|
| `kith` | Opens the TUI on the active Circle. |
| `kith <verb> …` | Runs the CLI. Never enters the alternate screen, never takes raw mode, never draws a frame. |
| `kith --circle photos` | TUI, opened on `photos`. |
| `kith --json` (no verb) | Refused: exit 64, `usage.no_tui_json`. |
| `kith` with stdout **or** stdin not a terminal | Refused: exit 64, `usage.not_a_tty`, message `kith: no subcommand and this is not a terminal — try 'kith list' or 'kith --help'`. |

The TUI is the only surface that reads keys. The CLI prompts only for the two things a
Person must supply interactively — the Person's name at `kith init` and `y`/`n`
confirmations — and only when stderr is a terminal; otherwise it takes the flag or exits
64. `--quiet` never turns a prompt into a silent default.

#### 1.2 Global flags

| Flag | Meaning |
|---|---|
| `--circle <CIRCLE>` | Circle to act on: exact id, exact name, or a unique case-insensitive name prefix. |
| `--json` | Emit exactly one JSON envelope on stdout (§3.2). Implies machine framing: no colour, no progress, no prompts. |
| `--config <PATH>` | Config file path override; also `$KITH_CONFIG`. |
| `--color auto\|always\|never` | Default `auto` (colour only when the stream is a terminal). `NO_COLOR` in the environment forces `never`. |
| `-q, --quiet` | Suppress narration on stderr. Data on stdout is unaffected. |
| `-v, --verbose` | Repeatable. `-v` adds seam-level diagnostics to stderr, `-vv` adds every Sync Engine request/response line. |
| `-h, --help` | clap-generated, exit 0. |
| `-V, --version` | One line, exit 0. The richer report is `kith version` (§4.10). |

All are `global = true`, so they parse before or after the verb.

#### 1.3 Active Circle resolution

For `invite`, `approve`, `reject`, `add`, `list items`, `list members` and `status`:

1. `--circle` if given. No match → exit 64, `circle.unknown`, listing known Circle names.
2. Exactly one Circle exists → that one.
3. Otherwise → exit 64, `circle.ambiguous`, message
   `kith: you are in 3 Circles; say which one with --circle <name>` followed by the names.

**The CLI never guesses from history.** A command's meaning must be readable from its own
text. The TUI does the opposite — it opens on `last_circle` from `state.toml` — because a
TUI has a visible, switchable context and a CLI invocation in a script does not.

#### 1.4 Item refs

Item addressing is fixed now even though no v0.1 verb takes an Item argument, because
`kith list items --json` must emit stable handles and v0.2's `kith apply <item>` must not
redesign them.

```
<item-ref> := "@" <id-prefix>        # prefix of the Item id (ADR-0004), min 4 chars
            | <path>                 # absolute, or relative to the Collection root,
                                     # or relative to cwd when cwd is inside the Circle root
```

Resolution: a leading `@` means id, always. Anything else is a path, resolved against cwd
then against the Collection root. Ambiguous id prefix → exit 65 `item.ambiguous` listing
the candidates; no match → exit 65 `item.unknown`. Titles are never matched — two Items
may share one.

#### 1.5 Streams

**stdout carries data; stderr carries narration.** `kith invite | wl-copy` must put a code
on the clipboard and a human sentence on the terminal, and it does. Progress indicators,
spinners, warnings, caveats, prompts and `-v` diagnostics are stderr. With `--json`,
stdout holds one envelope and nothing else, ever — including on failure.

When stdout is **not** a terminal, human table output drops padding, borders and the
header row and separates columns with a single tab, so `cut -f2` works. Colour follows
`--color`.

### 2. Command anatomy in code

One dispatch path, so no command can invent its own output rules:

```rust
#[derive(Parser)]
#[command(name = "kith", version, about = "Local-first collections shared with the people you trust")]
pub struct Cli {
    #[command(subcommand)] pub command: Option<Command>,
    #[command(flatten)]    pub global:  Global,
}

#[derive(Subcommand)]
pub enum Command {
    Init(InitArgs), Create(CreateArgs), Join(JoinArgs), Invite(InviteArgs),
    Approve(DecideArgs), Reject(DecideArgs), Add(AddArgs), List(ListArgs),
    Status, Doctor, Version,
}

/// Every verb implements this once. `run` may not print; `render` may not act.
pub trait Verb {
    type Data: serde::Serialize;
    const NAME: &'static str;                    // "list.items", "approve", …
    fn run(self, cx: &mut Cx) -> Result<Self::Data, Failure>;
    fn render(data: &Self::Data, w: &mut dyn Write, cx: &Cx) -> io::Result<()>;
}

pub struct Failure {
    pub code: &'static str,        // "engine.unreachable"
    pub exit: Exit,
    pub message: String,           // one sentence, no trailing period-plus-detail
    pub detail: Option<String>,    // stderr tail from a backend, engine text, …
    pub fix:    Option<String>,    // an imperative the Person can actually run
}

#[repr(u8)]
pub enum Exit {
    Ok = 0, Failed = 1, Usage = 64, Data = 65, Unavailable = 69,
    Internal = 70, Pending = 75, Refused = 77, Config = 78,
}
```

`Cx` carries the parsed config, the `SyncEngine` handle, the note sink and the colour
choice. The dispatcher calls `run`, then either `render` (human) or serialises the
envelope (`--json`); notes accumulated during `run` are flushed to stderr or into the
envelope by the dispatcher, never by the verb.

clap errors are mapped by hand — `Cli::try_parse()`, then `DisplayHelp`/`DisplayVersion`
→ exit 0, everything else → exit 64 — because clap's default of 2 collides with nothing
useful and 64 is `EX_USAGE`, matching ADR-0003 §2's exit table.

### 3. The output contract

#### 3.1 Exit codes

Sysexits-flavoured, and deliberately the same numbers ADR-0003 §2 gives external
Providers, so an Action failure and a CLI failure never disagree about what 69 means.

| Code | `Exit` | Meaning | Examples |
|---|---|---|---|
| 0 | `Ok` | It happened. | anything successful; `doctor` with warnings; `status` while syncing |
| 1 | `Failed` | It ran and failed for an ordinary reason. | copying bytes failed mid-import; `doctor` with a failed check |
| 64 | `Usage` | The invocation cannot proceed as given — bad arguments, ambiguity, or an unmet precondition. | no `--circle` with three Circles; `kith` without a terminal; `kith join` before `kith init` |
| 65 | `Data` | Input we cannot use. | malformed or expired Invite code; bytes the Provider does not claim; unknown Item ref |
| 69 | `Unavailable` | Something this Device needs is not here. | Sync Engine unreachable or below the version floor; no apply backend |
| 70 | `Internal` | A bug. Unexpected engine response, cache we could not rebuild. | never expected; always worth an issue |
| 75 | `Pending` | Correct, but not finished, and finishing is not ours to do. | `kith join` knocked; admission is the admin's move |
| 77 | `Refused` | Role policy on this Device says no. | a non-admin running `kith approve` |
| 78 | `Config` | Configuration is wrong. | unparseable TOML; discovered credentials rejected (`SyncError::Unauthorized`) |

`SyncError` (ADR-0002 §1) maps: `Unreachable` → 69, `Incompatible` → 69, `Unauthorized` →
78, `NotFound` → 65, `Engine(_)` → 1.

#### 3.2 The JSON envelope

`--json` prints one compact object, newline-terminated, on stdout — one invocation, one
object, success or failure.

```json
{"schema":1,"command":"list.members","ok":true,"exit":0,
 "data":{"circle":{"id":"kith-4npq7x2b","name":"walls"},
         "members":[{"person":"Ana","role":"admin","presence":"online","device":"WXYZ123","introducer":true},
                    {"person":"Ben","role":"member","presence":"offline","device":"KX7QF2A","introducer":false,"last_seen":"2026-08-07T09:12:44Z"}]},
 "notes":[{"level":"caveat","code":"role.advisory","message":"Roles are agreements, not enforcement…"}]}
```

```json
{"schema":1,"command":"invite","ok":false,"exit":69,
 "data":null,
 "error":{"code":"engine.unreachable","message":"The Sync Engine is not answering at http://127.0.0.1:8384.",
          "detail":"connection refused","fix":"Start the Sync Engine daemon (Syncthing), then run: kith doctor"},
 "notes":[]}
```

| Field | Type | Notes |
|---|---|---|
| `schema` | integer | Envelope version, currently `1`. Bumped only on a breaking shape change; new optional fields do not bump it. Independent of the product version. |
| `command` | string | Dotted verb path: `init`, `list.items`, `list.circles`, `list.members`, `doctor`, … |
| `ok` | bool | `exit == 0`. |
| `exit` | integer | The process's own exit code, duplicated so a captured log is self-contained. |
| `data` | object \| null | Per-verb, specified with each verb below. Present even on partial failure (`kith add` reports what it did import). |
| `error` | object \| null | Present iff `!ok`: `code`, `message`, optional `detail`, optional `fix`. |
| `notes` | array | `{level, code, message}` with `level ∈ info \| warn \| caveat`. |

**Notes are the honesty channel.** Everything the human surface would have said in grey —
"the Sync Engine is offline, this is last-known state", "Roles are not enforced", "your
terminal has no pixel protocol" — travels as a note, so a script is told exactly what a
Person is told. Standing codes: `role.advisory`, `preview.degraded`, `apply.unavailable`,
`engine.unreachable`, `engine.remote`, `presence.stale`, `invite.no_revoke`,
`join.reknock`, `config.unknown_key`, `circle.conflicts`.

Timestamps are RFC 3339 UTC. Byte counts are integers, never pre-formatted strings.
Person names are strings as typed. Device ids are the 7-character short form; the full id
appears only as `device_full` in `status` and `doctor`.

### 4. The verbs

Eleven, in ROADMAP order.

#### 4.1 `kith init`

```
kith init [--name <NAME>]
```

Creates the Person on this Device and binds the Device to them.

- `--name` is 1–64 characters after trimming; must contain a non-space character. Absent
  and stderr is a terminal → prompt `Your name (shown to the People you share with):`.
  Absent and not a terminal → exit 64.
- Calls `SyncEngine::local_device()` for the Device id. The engine is required: the
  binding is meaningless without it. Unreachable → exit 69 and **nothing is written**.
- Writes `$XDG_STATE_HOME/kith/person.toml` (§8.3). The Identity itself — the daemon's
  certificate — is neither read nor copied nor backed up (ADR-0002 §2).
- Already initialised → exit 64, `identity.exists`, message
  `kith: this Device already speaks for Ana. v0.1 has no rename.` Nothing is changed.

`data`: `{"person":{"name":"Ana","device":"WXYZ123","device_full":"WXYZ123-…","created":"…"}}`

Human output:

```
You are Ana on this Device (WXYZ123).
Next: kith create <name> to start a Circle, or kith join <code> if someone invited you.
```

#### 4.2 `kith create`

```
kith create <NAME> [--path <DIR>] [--adopt [<DIR>]]
```

Creates a Circle, its sole Collection, and makes this Device the Circle's admin and its
single introducer (ADR-0002 §3).

- `<NAME>`: 1–64 printable characters, unique among this Device's Circles (case-insensitive).
  Duplicate → exit 64 `circle.duplicate_name`.
- `--path` defaults to `~/kith/<slug>` where slug lowercases the name and replaces runs of
  non-alphanumerics with `-`. An existing non-empty directory → exit 64
  `circle.path_not_empty`, unless `--adopt` names it.
- `--adopt` adopts an existing wp-sync tree in place per ADR-0002 §7 — same synced space,
  same peers, no bytes moved. With no argument it auto-detects; zero candidates → exit 65
  `adopt.not_found`; several → exit 64 listing them, `--adopt <DIR>` disambiguates.
  Adoption prompts once, on stderr, for the `autoAcceptFolders` cleanup ADR-0002 §7
  describes, defaulting to **no**; `--quiet` or a non-terminal stderr skips the prompt and
  leaves the setting alone with a `warn` note.
  ROADMAP's CLI surface has no `adopt` verb, and adoption is a way of creating a Circle,
  not a capability of its own — so it is a flag on `create`, and ADR-0002 §7 spells it
  `kith create --adopt` too.
- Requires Identity and the engine: missing Identity → 64, unreachable → 69, nothing written.

`data`: `{"circle":{"id":"kith-4npq7x2b","name":"walls","root":"/home/ana/kith/walls","role":"admin","introducer":true,"adopted":false}}`

```
Created walls (kith-4npq7x2b) at ~/kith/walls.
You are this Circle's admin: invites and joins run on this Device.
Next: kith add <paths…>, then kith invite.
```

#### 4.3 `kith invite`

```
kith invite [--circle <CIRCLE>] [--expires <DURATION>]
```

Prints one time-bounded Invite code.

- Admin only. A member Role → exit 77 `role.refused` with the short caveat (§7.2) and the
  admin's name.
- `--expires` accepts `30m`, `12h`, `7d`; default `24h`; hard cap `7d` (beyond → exit 64).
- The ticket is ADR-0002 §2's `InviteTicket` — Circle id and name, this Device as
  introducer, address hints, issue and expiry times, nonce — serialised with `postcard`,
  Crockford-base32 encoded, CRC32 suffixed, prefixed `kith1`. One line, typically 110–160
  characters, case-insensitive on input with hyphens and whitespace ignored so a code
  survives being wrapped by a chat client.
- The Invite is recorded in `$XDG_STATE_HOME/kith/invites.toml` so admission can check
  expiry on the gatekeeper's own Device (§4.5).
- Engine unreachable → exit 69: the ticket needs the live Device id and address hints.

stdout is the code alone. stderr:

```
Invite to walls, valid until 2026-08-08 14:02 (24h).
Send it over a channel you already trust — kith has no messaging.
Anyone holding this code can knock; nobody enters without your approval.
Invites cannot be revoked in v0.1. Let it expire, or issue a shorter one with --expires 2h.
```

`data`: `{"invite":{"code":"kith1…","circle":"walls","issued_at":"…","expires_at":"…"}}`
plus a `caveat` note `invite.no_revoke`.

#### 4.4 `kith join`

```
kith join <CODE> [--path <DIR>] [--wait <DURATION> | --no-wait]
```

Consumes an Invite. Joining is genuinely two-phase and two-sided (ADR-0002 §1), and this
verb says so rather than pretending.

1. Decode and validate the code. Malformed or bad CRC → 65 `invite.malformed`; past its
   expiry → 65 `invite.expired`.
2. Identity required → else 64 with `fix: run kith init first`.
3. Already a Member of that Circle → exit 0, no engine calls, note `join.already_member`.
4. `SyncEngine::begin_join(&ticket)`, then record the consumed Invite in
   `$XDG_STATE_HOME/kith/pending-joins.toml` with the ticket's Circle id, the chosen root
   (`--path`, default `~/kith/<slug(circle_name)>`) and the ticket's own expiry as its
   deadline.
5. Wait for a `Change::CircleOffered` whose Circle id matches (default `--wait 10m`,
   spinner on stderr when it is a terminal). On match → `complete_join(offer, root)`,
   exit 0. On timeout → exit 75 `join.pending`.
6. `--no-wait` skips step 5 and exits 75 immediately.

**Automatic completion, and why it is not auto-accept.** Any later `kith` process — CLI or
TUI — completes an offer whose Circle id matches an unexpired record in
`pending-joins.toml`, at the root already chosen. Offers matching no record are never
accepted; they are counted by `kith status` and named by `kith doctor`. This keeps
ADR-0002's rule intact — the joiner chose the path and consented by running `kith join` —
without inventing a joiner-side screen the ROADMAP does not list, and without ever
touching the engine's global auto-accept setting.

`Ctrl-C` while waiting cancels the wait, not the knock: `still pending — Ana can still
admit you; run kith or kith status later to finish.`

`data`: `{"join":{"circle":"walls","state":"joined"|"pending","root":"/home/ben/kith/walls","introducer":"WXYZ123"}}`

#### 4.5 `kith approve` / `kith reject`

```
kith approve [<REQUEST>] [--all] [--circle <CIRCLE>]
kith reject  [<REQUEST>] [--all] [--circle <CIRCLE>]
```

`<REQUEST>` is a Device short id or the Device's advertised name; a unique prefix of
either is accepted. With no argument: exactly one pending → that one; several → exit 64
listing them; none → exit 0 with `data.pending: []` and `no pending join requests`.

- Admin only. Only the introducer's Device ever sees knocks, so a member Role invoking
  either verb gets exit 77 and
  `Only Ana can admit Members — membership changes run on the admin's Device.`
- **Expiry is checked here**, on the gatekeeper's own hardware (ADR-0002 §4). If no
  unexpired Invite for this Circle is recorded in `invites.toml`, approval is refused:
  exit 65 `invite.expired`, `fix: issue a fresh Invite with kith invite, then ask them to
  run kith join again.`
  *Call recorded here:* v0.1 correlates a knock to an Invite by **Circle plus time
  window**, not per ticket. The nonce cannot ride along — the transport's pending-device
  record carries only a Device id and an advertised name, and writing the daemon's own
  device name is forbidden by ADR-0002 §6. Per-ticket correlation is a v0.2 candidate; a
  human reading a name before pressing approve is v0.1's second factor.
- `approve` → `SyncEngine::admit`. `reject` dismisses the pending record; it is not a ban,
  and the caveat says so (`join.reknock`).
- Engine unreachable → 69, nothing attempted.

`data`: `{"decided":[{"device":"KX7QF2A","name":"ben-thinkpad","result":"approved"}],"pending":[…]}`

```
Admitted ben-thinkpad (KX7QF2A) to walls.
Sync begins when both Devices are online.
```

```
Rejected ben-thinkpad (KX7QF2A).
This is not a ban: the same Device can knock again. kith has no blocklist in v0.1.
```

#### 4.6 `kith add`

```
kith add [--circle <CIRCLE>] [--move] [--dry-run] <PATH>...
```

Imports bytes into the Circle's Collection as Items.

- Directories recurse. Skipped without comment: dot-entries, `.kith/`, `.stversions/`,
  `.stfolder/`, and `*.sync-conflict-*` copies (ADR-0002 §2).
- Each candidate goes through the Provider's `claims()` (ADR-0003 §1). Unclaimed → skipped
  with reason `not a wallpaper (image/svg+xml)`.
- Default copies bytes into the Collection root. `--move` renames within a filesystem,
  copy-then-unlink across one. A path already inside the Circle root is **registered in
  place** — no copy, no move — which is how an adopted tree gets its Sidecars.
- Name collision in the Collection: identical content hash → skipped as `duplicate`
  (`info`, does not affect the exit code); different content → imported as `sunset-2.png`
  with an `info` note.
- Every imported Item gets a Sidecar attributing it to this Person with the import time
  and the Provider's extracted facts (ADR-0003 §1, format per ADR-0004).
- `--dry-run` reports the same shape and writes nothing.
- **Works with the engine down.** Bytes land in the tree and sync when the daemon returns;
  a `warn` note says so and the exit code stays 0. This is ADR-0002 §6's promise made
  literal.

Exit is the worst outcome: any unclaimed or unreadable candidate → 65; an I/O failure
mid-copy → 1; otherwise 0. `data` always enumerates both lists, so a partial run is fully
inspectable:

```json
{"imported":[{"id":"a3f19c02","path":"walls/sunset.png","bytes":1993421,"width":3840,"height":2160}],
 "skipped":[{"path":"/home/ana/notes.txt","reason":"not claimed by the wallpaper Provider"}]}
```

```
Added 12 Items to walls (48.3 MB). Skipped 1.
  skipped  ~/Pictures/walls/notes.txt — not claimed by the wallpaper Provider
```

#### 4.7 `kith list`

```
kith list [items|circles|members] [--circle <CIRCLE>]
```

Subject defaults to `items`. Three subjects, one per ROADMAP listing capability; no
filters, no sort flags (the Gallery owns filtering, and v0.1's only filter is favourites).

**`items`** — the Collection, newest first, matching the Gallery's order:

```
   ID        TITLE              ADDED BY   ADDED        SIZE     DIMENSIONS
 ● a3f19c02  sunset             Ana        2h ago       1.9 MB   3840×2160
 ★ 7b2e5510  ridge-fog          Ben        yesterday    4.2 MB   5120×2880
   0c9a1d84  neon-alley         Ana        3 Aug        2.7 MB   3840×2160
```

`●` unseen, `★` favourite (private to this Person), both defined in #15.

**`circles`** — `NAME  ID  ROLE  MEMBERS  ITEMS  STATE  ROOT`.

**`members`** — `PERSON  ROLE  PRESENCE  DEVICE`, with the full Role caveat printed
below the table on stderr, and as a `caveat` note under `--json`.

Listing never needs the engine. With it down, `items` and `circles` read the synced tree
and cache and exit **0** — the list is real. `members` prints `presence: unknown` or
`last seen 2h ago`, adds `presence.stale`, and still exits 0. Only `status` treats
unreachability as its own result (§4.8).

#### 4.8 `kith status`

```
kith status [--circle <CIRCLE>]
```

Without `--circle`, every Circle. What the wedge needs when two non-expert friends wonder
whether anything is happening.

```
Sync Engine   reachable · v2.0.4 · http://127.0.0.1:8384
You           Ana (WXYZ123)

walls  kith-4npq7x2b  ~/kith/walls
  state     syncing · 62% · 118 MB to receive
  items     42
  members   2 — Ben online (91%), Ana this Device
  admin     you (this Device is the Circle's introducer)
  changed   4 minutes ago
```

`data` mirrors it: `{"engine":{"reachable":true,"version":"2.0.4","address":"…","credentials":"~/.local/state/syncthing/config.xml"},"person":{…},"circles":[{"id":…,"state":"syncing","percent":62,"bytes_needed":123456789,"items":42,"conflicts":0,"peers":[{"person":"Ben","device":"KX7QF2A","connected":true,"percent":91}],"introducer":"WXYZ123","last_change":"…"}]}`

Exit 69 when the engine is unreachable — local facts still print, prefixed by the offline
line (§7.1) — so `kith status` is usable as a health probe. Otherwise 0, including while
syncing or behind. `conflicts > 0` adds the `circle.conflicts` note (§9).

#### 4.9 `kith doctor`

The wedge dies silently if sync breaks between two people who cannot debug it, so ROADMAP
ships diagnosis in v0.1 even though the Health screen waits. Full specification in §5.

#### 4.10 `kith version`

```
kith version
```

```
kith 0.1.0 (a1b2c3d, 2026-08-07, x86_64-unknown-linux-gnu)
  preview     kitty, iTerm2, sixel, halfblocks compiled in
  sync engine client floor v1.13 · daemon v2.0.4
  provider    protocol 1
  json        schema 1
```

`data`: `{"version":"0.1.0","commit":"a1b2c3d","built":"2026-08-07","target":"…","preview_rungs":["kitty","iterm2","sixel","halfblocks"],"engine":{"floor":"1.13","daemon":"2.0.4"},"provider_protocol":1,"json_schema":1}`

Always exits 0. Engine unreachable → `daemon: null` and a `warn` note; a version command
that fails because a daemon is down would be absurd. `-V` prints the first line only.

#### 4.11 Bare `kith`

Opens the TUI (§6). Preconditions, checked before the alternate screen is entered so no
message is ever painted over:

| Condition | Behaviour |
|---|---|
| No Identity | exit 64, `Run kith init first — kith needs to know your name before it can show you a Circle.` |
| No Circles | The TUI **does** open, on an empty Gallery: `No Circles yet. Run kith create <name>, or kith join <code> if someone invited you.` |
| Engine unreachable | The TUI opens. Browsing, Preview, Favourites and Apply all work off the tree and cache (ADR-0002 §6); the status bar carries §7.1's line. |
| Terminal smaller than 60×18 | exit 64, `kith needs at least 60×18; this terminal is 52×14.` |

---

### 5. `kith doctor` in full

Sixteen checks in seven sections, run top to bottom, each independent — a failed check
never cancels a later one, because the second question a stuck Person asks is always "what
else is broken".

| Id | Title | Asks | `warn` when | `fail` when |
|---|---|---|---|---|
| `config.file` | config file | Parse the TOML at the resolved path | unknown keys present | unparseable, or a value has the wrong type |
| `engine.credentials` | credentials | Where address + API key came from (ADR-0002 §6 order) | found only in the legacy `~/.config/wp-sync/identity` | not found anywhere |
| `engine.reachable` | reachable | `SyncEngine::health()` | address is non-loopback (note `engine.remote`) | `Unreachable`, or `Unauthorized` |
| `engine.version` | version floor | daemon ≥ v1.13 (pending endpoints) | ≥ floor but untested major | below floor |
| `identity.person` | Person | `person.toml` exists and names a Person | — | missing |
| `identity.device` | Device binding | recorded Device id == `local_device()` | engine unreachable, so unverifiable | mismatch — this daemon is not the one you initialised against |
| `circle.<id>.root` | *name* · root | root exists, is a directory, is writable | — | missing or read-only |
| `circle.<id>.sync` | *name* · sync | engine state, Item count, bytes behind | state is `error`, or conflict copies exist | the engine does not know this Circle |
| `circle.<id>.peers` | *name* · peers | Member count and how many are connected | Members exist but none connected | — |
| `circle.<id>.recovery` | *name* · recovery | versioning configured per ADR-0002 §2 | versioning absent or non-conforming | — |
| `preview.protocol` | protocol | Rung chosen by `ratatui-image`'s query | `halfblocks` — degraded, never broken | — |
| `preview.cell_size` | cell size | Terminal cell pixel size for budgets (ADR-0003 §5) | not reported; kith assumes 8×16 | — |
| `apply.session` | session | `XDG_CURRENT_DESKTOP`, session type | neither Wayland nor X11 detected | — |
| `apply.backend` | backend | Backend chosen by ADR-0003 §4's ladder | none found — Apply is unavailable, browsing is not | — |
| `apply.monitors` | monitors | Targets from `apply_targets()`, with configured labels | backend cannot enumerate; only "all monitors" | — |
| `cache.writable` | cache | `$XDG_CACHE_HOME/kith` writable, current size | — | not writable |

Circle checks are `skip` when there are no Circles (`no Circles yet — nothing to check`).
Preview checks are `skip` when stdout is not a terminal. Skips never affect the exit code.

**Output.** Two columns, symbol plus title plus one line of detail; failures and warnings
add an indented `→` imperative.

```
kith doctor

Configuration
  ✓ config file       ~/.config/kith/config.toml (parsed)
Sync Engine
  ✓ credentials       ~/.local/state/syncthing/config.xml
  ✓ reachable         http://127.0.0.1:8384
  ✓ version floor     v2.0.4 (needs ≥ 1.13)
Identity
  ✓ Person            Ana
  ✓ Device binding    WXYZ123 matches this daemon
Circles
  ✓ walls · root      ~/kith/walls (writable)
  ✓ walls · sync      idle · 42 Items · nothing to receive
  ! walls · peers     1 Member, 0 online
    → Ben's Device has not connected. Discovery is the Sync Engine's job; check that
      Syncthing is running on both machines and on the same network.
  ✓ walls · recovery  versioning on (keep 5, 30 days)
Preview
  ! protocol          halfblocks — your terminal has no pixel protocol
    → Images render as coloured blocks. Degraded, never broken. kitty, WezTerm, foot
      and Ghostty give full-resolution previews.
  ✓ cell size         8×17 px (queried)
Apply
  ✓ session           wayland · Hyprland
  ✓ backend           swww
  ✓ monitors          DP-1 "Desk left", HDMI-A-1 "TV"
Storage
  ✓ cache             ~/.cache/kith (writable, 12 MB)

16 checks · 14 ok · 2 warnings · 0 failed
```

Symbols are `✓ ! ✗` for ok/warn/fail and `·` for skip, with colour as reinforcement only —
never the sole signal (§7.6).

**Exit:** 0 when no check failed, 1 when any did. Warnings never fail the run. Halfblocks
is the shipped fallback, not a defect; an apply backend is optional to the wedge; a
Circle with nobody online yet is Tuesday.

`--json` gives `data.checks` as an array of `{"id","section","title","status","detail","fix"}`
plus `data.summary` `{"ok":14,"warn":2,"fail":0,"skip":0}`. This is the machine-readable
form of every honesty rule in §7, which is the point: a script that wraps kith gets the
caveats too.

---

### 6. The TUI

#### 6.1 Frame

Three fixed rows around one content area, every screen:

```
┌ kith · walls ───────────────────────────────── 42 Items · 3 unseen ┐   title
│                                                                    │
│  content — Gallery grid, Preview pane, or Members list             │
│                                                                    │
├────────────────────────────────────────────────────────────────────┤
│ ● syncing 62% · 2 Members, 1 online              kitty · swww      │   status
│ j k h l move · enter preview · a apply · f fav · m members · ? keys│   hints
└────────────────────────────────────────────────────────────────────┘
```

- **Title:** binary, active Circle, and a right-aligned count. A pending join adds
  `· 1 wants to join` here until it is dealt with.
- **Status:** left is the Circle's live state from `Change::CircleState` /
  `PeerProgress`; right is the preview rung and the apply backend, so both degradations
  are permanently visible rather than discovered on failure. Transient results (an Action
  failed, a Favourite toggled) take this row for 4 seconds, then it reverts.
- **Hints:** the 5–7 keys that matter on this screen, always ending `? keys`. This row is
  never hidden and never scrolls away; it is the whole discoverability budget.

#### 6.2 Screen stack and event routing

```rust
pub enum Screen  { Gallery, Preview { item: ItemId }, Members }
pub enum Overlay { Help, Circles, Actions { item: ItemId },
                   Monitors { item: ItemId, targets: Vec<ApplyTarget> },
                   Join(JoinRequest), Confirm(Confirm), Detail(String) }

pub struct App { stack: Vec<Screen>, overlay: Option<Overlay>, circle: CircleId, /* … */ }
pub enum Event { Key(KeyEvent), Resize(u16, u16), Sync(Change), Tick }

impl App { fn on_key(&mut self, k: KeyEvent) -> Option<Cmd>; }
```

`stack` is rooted at `Gallery` and never empties. Keys route **overlay → screen →
global**; the first handler that claims a key wins, so an overlay can shadow a global key
and nothing else can. At most one overlay is open at a time; a second request queues
(join prompts) or is dropped (help).

The UI redraws on `Key`, `Resize` and `Sync`; `Tick` fires at 250 ms only while a spinner
or progress figure is on screen. `Change::Desynced` (ADR-0002 §5) shows
`resynchronising…` on the status row and rebuilds; the Person is told, never left
looking at stale tiles.

Terminal discipline: alternate screen plus raw mode, restored by a panic hook and by a
`Drop` guard, so a crash never leaves a wrecked terminal. `Ctrl-Z` restores the terminal,
raises `SIGSTOP`, and redraws on resume. `SIGWINCH` re-lays out; the Gallery reflows and
`ratatui-image` re-encodes at the new cell budget.

#### 6.3 Keybinding philosophy

1. **Arrows and Enter are always sufficient.** The walkthrough has Ben moving with arrow
   keys; vim keys are an accelerator, never a requirement.
2. **Same letter, same meaning, everywhere an Item is focused.** `f a d y r` mean
   favourite, apply, delete, copy path, reveal in the Gallery and in Preview — ADR-0003
   §3's "common five".
3. **Lowercase is safe or reversible; uppercase widens scope.** `f` favourite, `F` filter
   to favourites; `g` moves, `G` jumps to the end; `L` leaves a Circle.
4. **Every destructive key confirms, and the confirmation defaults to the safe answer.**
   Enter on a confirm means "no" when "yes" deletes.
5. **One chord, `gg`.** No leader keys, no modes, no command line. A Person who knows one
   screen knows the others.
6. **Nothing is hidden behind memory.** The hint row plus `?` covers 100% of the keymap;
   an unbound key flashes `no binding for 'z' — press ? for keys` instead of doing nothing.
7. **No remapping in v0.1.** ROADMAP's Configuration row is three settings, and keybinding
   config is v1.0. Fixed keys mean documentation and hints cannot lie.

#### 6.4 The v0.1 keymap

**Global** (any screen, unless an overlay shadows it):

| Key | Does |
|---|---|
| `?` | Help overlay — the full keymap for this screen |
| `q` | Pop one level; at the Gallery, quit |
| `Esc` | Close the overlay, else pop one level; never quits |
| `Ctrl-C` | Quit immediately, restoring the terminal |
| `Ctrl-Z` | Suspend |
| `c` | Circle switcher |
| `m` | Members |
| `!` | Detail of the last failure (ADR-0003 §3's expandable detail) |

**Movement** (Gallery grid, Members list, and every list overlay):

| Key | Does |
|---|---|
| `j` `↓` / `k` `↑` | Next / previous |
| `h` `←` / `l` `→` | Left / right in the grid; previous / next in a list |
| `gg` / `G` | First / last |
| `Ctrl-d` / `Ctrl-u`, `PgDn` / `PgUp` | Half page, page |
| `Home` / `End` | First / last |
| `Enter` | Primary action for the focused row (see per-screen) |

**Item focused** (Gallery and Preview):

| Key | Does |
|---|---|
| `Enter` | Open Preview (Gallery); return to Gallery (Preview) |
| `a` | Apply — raises the monitor picker when more than one target exists |
| `f` | Toggle Favourite (private; nothing is announced) |
| `d` | Delete, after a confirm that names the consequence (§9) |
| `y` | Copy the Item's path to the clipboard |
| `r` | Reveal on disk |
| `Space` | Action menu — every core and Provider Action, unavailable ones greyed **with their reason** |
| `F` | Toggle "favourites only" — v0.1's only filter (ROADMAP Gallery row) |

**Preview only:** `j` `k` `←` `→` move to the adjacent Item in the Gallery's order without
leaving Preview; `q` / `Esc` / `Enter` return.

**Members only:** `Enter` on a pending-join row opens the join prompt; `Enter` on a Member
row does nothing (there is no Member detail screen in v0.1). `L` leaves the Circle, after
a confirm.

**Overlays:**

| Overlay | Keys |
|---|---|
| Help | `?` `q` `Esc` close; `j`/`k` scroll |
| Circle switcher | `j`/`k` move, `Enter` switch, `Esc` cancel |
| Action menu | `j`/`k` move, `Enter` perform, `Esc` cancel; unavailable entries refuse with their reason on the status row |
| Monitor picker | `j`/`k` move, `Enter` apply, `Esc` cancel |
| Join prompt | `a` approve, `x` reject, `Esc` decide later |
| Confirm | `y` yes, `n` no, `Enter` = the safe default, `Esc` cancel |
| Detail | `j`/`k` scroll, `y` copy, `q`/`Esc` close |

`a` and `x` in the join prompt are the one place a letter means something new; the overlay
prints its own keys in its body, and no Item is focused behind it, so nothing is shadowed
that a Person could be reaching for.

#### 6.5 Screens

**Gallery** — the root screen and the wedge. A grid of tiles rendered through
`ratatui-image` on the ADR-0001 ladder, newest first, favourite marker and unseen dot.
Tile geometry, thumbnail budgets and cache behaviour, sorting, the favourites filter and
the full Action set are `docs/spec/gallery-preview-actions.md` (#15). Here it is only the
stack root: it owns movement, the item-focused keys, and never scrolls the hint row away.
Empty states: no Circles (§4.11); a Circle with no Items yet
(`Nothing here yet. kith add <paths…>, or wait — Ben's Items appear as they arrive.`).

**Preview** — fullscreen single Item with its Sidecar facts, reached with `Enter`. Layout
and the exact fact line (*added by Ana · today · 3840×2160 · 1.9 MB*) are #15's. Entering
Preview is what marks an Item seen.

**Members** — the Circle's People, name, Role, presence, plus any pending joins pinned
above them:

```
  Pending
  → ben-thinkpad   KX7QF2A   knocked 2 minutes ago        enter to decide

  Members
    Ana            admin     this Device · introducer
    Ben            member    online · 91%

  Roles are agreements, not enforcement — admission is the only gate.
  L leave circle · enter decide · esc back
```

*Call recorded here:* pending joins live on the Members screen as well as in the prompt.
The prompt is a moment; a Circle admin who pressed `Esc` needs somewhere to go back to,
and "the screen that lists who is in this Circle" is that place. No new screen is added.

**Join prompt** — raised automatically on `Change::JoinRequested` when this Device is the
introducer and the Gallery or Members is focused; elsewhere it queues and the title row
shows `1 wants to join` until Members is visited. Never raised over a confirm, and never
over Preview — an interruption that steals a keystroke during Apply is how consent rules
get broken by accident.

```
  ┌ A Device wants to join walls ──────────────────────────────┐
  │  Name          ben-thinkpad                                │
  │  Device        KX7QF2A                                     │
  │  First seen    2 minutes ago                               │
  │                                                            │
  │  Admitting adds this Device to walls. It receives every     │
  │  Item, and can add, change or delete Items — kith cannot    │
  │  prevent that, only restore. Approve People, not Devices.  │
  │                                                            │
  │  a approve   x reject   esc decide later                   │
  └────────────────────────────────────────────────────────────┘
```

**Circle switcher** — `c`, a plain list, exactly what ROADMAP asks for:

```
  ┌ Switch Circle ─────────────────────────┐
  │ > walls     42 Items   1 Member online │
  │   photos    11 Items   nobody online   │
  │ j k move · enter switch · esc cancel   │
  └────────────────────────────────────────┘
```

With one Circle, `c` does not open: the status row says `walls is your only Circle.`
Switching resets the stack to the new Circle's Gallery — a Preview of an Item in `walls`
must never survive a switch to `photos` — and the choice is persisted as `last_circle` in
`state.toml`, which is where the next bare `kith` opens.

---

### 7. Status and error surfacing

Three severities, one vocabulary, both surfaces:

| Severity | Means | CLI | TUI | JSON |
|---|---|---|---|---|
| **caveat** | A standing truth about how kith works | stderr line under the output | footer line on the screen that shows it | `notes[] level:"caveat"` |
| **warn** | Degraded, still working | `!` line on stderr; exit unchanged | status row, `!` glyph | `notes[] level:"warn"` |
| **fail** | It did not happen | `✗` line on stderr with `→ fix`; non-zero exit | status row for 4 s, detail under `!` | `error` object |

#### 7.1 Sync Engine unreachable

The status row's right side reads `sync engine offline`, and the reconnect backoff is
shown, not hidden: `reconnecting in 8s` (ADR-0002 §5's jittered 1s → 60s). On the first
transition, one banner takes the status row:

> `! Sync Engine offline — showing local content. Nothing is lost; changes sync when it returns.`

CLI commands that can work offline (`add`, `list`, `version`) do, and say so:

> `! Sync Engine offline (http://127.0.0.1:8384). Working from local content.`

Commands that cannot (`init`, `create`, `invite`, `join`, `approve`, `reject`) refuse
**before touching anything**, exit 69:

> `✗ The Sync Engine is not answering at http://127.0.0.1:8384.`
> `  → Start it (Syncthing: systemctl --user start syncthing), then run kith doctor.`

Never a modal. Never a silent retry. Never a spinner without a stated deadline.

#### 7.2 Role caveats

Wherever a Role is shown, one of two strings appears verbatim. Long form — Members screen
footer, `kith list members`, `kith doctor`'s Circles section header:

> **Roles are agreements, not enforcement.** kith has no server: any Member's Device can
> add, change or delete Items in this Circle. What kith does guarantee is admission — only
> Devices you approve get in — and recovery: every other Device keeps the last 5 versions
> of every Item for 30 days.

Short form — one-line contexts (`invite` refusal, join prompt, status):

> Roles are agreements, not enforcement — admission is the only gate.

Under `--json` both arrive as `notes[] {level:"caveat", code:"role.advisory"}`. A surface
that shows a Role without one of these strings is a bug, not a style choice.

#### 7.3 Degraded preview rung

The chosen rung is permanently on the status row (`kitty`, `iTerm2`, `sixel`,
`halfblocks`). On halfblocks it reads `halfblocks (degraded)` and `doctor` warns with
§5's text. The words *unsupported*, *error* and *failed* are banned here: halfblocks is
the shipped fallback and ADR-0001 promises kith is never unusable because of a terminal.

#### 7.4 Apply unavailable

Per ADR-0003 §4, the Action is declared, greyed and explained — never omitted:

> `Apply — unavailable: no wallpaper backend found (probed gsettings, plasma-apply-wallpaperimage,
> swww, hyprpaper, swaybg, xwallpaper, feh). Set [provider.wallpaper.custom] in
> ~/.config/kith/config.toml.`

The status row's right side shows `no apply backend`. Everything else — browsing, Preview,
Favourites, sync — is untouched, and the wording says so rather than implying kith is
broken.

#### 7.5 Presence and progress honesty

Presence is `online` / `offline` only from live engine state. Otherwise it is
`last seen 2h ago`, or `unknown` when there is nothing to base it on — never inferred.
Percentages come only from `Change::PeerProgress` / `FolderCompletion`; with no figure,
the surface says `syncing…` and shows an indeterminate spinner rather than a fabricated
bar. A count kith cannot verify is printed as `—`.

#### 7.6 Colour, symbols, width

Colour is never the only signal: `✓ ! ✗ ● ★` carry the meaning and colour reinforces it.
`--color never` and `NO_COLOR` produce output that reads identically. Human tables
truncate to terminal width with `…`; piped output does not truncate at all.

#### 7.7 Failure is never silent

Every failed Action leaves a status-row line plus a `!` detail overlay carrying the
backend's stderr tail (ADR-0003 §3). Every failed command prints `✗ message` on stderr,
plus `→ fix` when an imperative exists, and exits non-zero. A `fix` line must be something
a Person can literally run or open; if there is none, the field is omitted rather than
padded with advice.

#### 7.8 Wording rules

Surface strings use the glossary: Person, Device, Circle, Member, Collection, Item, Apply,
Sync Engine. Never "user", "account", "folder", "file" (in a domain position), "friend",
"permission", "server", "cloud".

The one licensed exception: **an instruction the Person must type may name Syncthing.**
`systemctl --user start syncthing` cannot be written any other way, and an honest product
does not hide the name of the program its owner has to start. The rule is narrow — the
concept is always "the Sync Engine", and the program name appears only inside a `fix` line
or a `doctor` detail, in parentheses or in a command.

---

### 8. Configuration

#### 8.1 Where

`--config <PATH>` → `$KITH_CONFIG` → `$XDG_CONFIG_HOME/kith/config.toml` →
`~/.config/kith/config.toml`. **A missing file is not an error** — every key has a default
and kith runs with no config at all. Unknown keys are a `warn` note plus a `doctor`
warning, never fatal; invalid TOML or a wrong-typed value is fatal, exit 78, naming the
line.

#### 8.2 The whole file

ROADMAP's Configuration row is three things — apply backend and custom command, monitor
names, daemon address and API-key override. This is all of them, and nothing else:

```toml
# ~/.config/kith/config.toml
# Every key is optional. This file does not need to exist.

[sync_engine]
# Override credential discovery (ADR-0002 §6). Omit both to auto-discover.
address = "http://127.0.0.1:8384"
api_key = "…"

[provider.wallpaper]
# auto | gnome | kde | swww | hyprpaper | swaybg | xwallpaper | feh | custom
backend = "auto"

# Friendly names for outputs, keyed by the name the backend reports.
# Used by the monitor picker and `kith doctor`; unlisted outputs show their raw name.
[provider.wallpaper.monitors]
"DP-1"     = "Desk left"
"HDMI-A-1" = "TV"

# The escape hatch (ADR-0003 §4). {item} = path to the Item's bytes, {target} = monitor.
# Setting this implies backend = "custom".
[provider.wallpaper.custom]
apply   = "xfconf-query -c xfce4-desktop -p /backdrop/screen0/monitor{target}/workspace0/last-image -s {item}"
targets = "xrandr --listmonitors | awk '/[0-9]+:/ {print $4}'"
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `sync_engine.address` | string | discovered | A non-loopback address is refused unless set here explicitly (ADR-0002 §6), and still earns the `engine.remote` warning. |
| `sync_engine.api_key` | string | discovered | Read, never written. kith never rotates, regenerates or guesses a key. |
| `provider.wallpaper.backend` | enum | `"auto"` | `auto` runs ADR-0003 §4's detection ladder. A named backend that is not detected does **not** silently fall back: Apply is `Unavailable` with `configured backend "swww" not detected`. |
| `provider.wallpaper.monitors` | table | `{}` | Labels only. It cannot create, reorder or hide outputs; the backend enumerates at Action time because monitors hotplug. |
| `provider.wallpaper.custom.apply` | string | — | Required if `backend = "custom"`. Run through the shell; `{item}` is always quoted by kith. |
| `provider.wallpaper.custom.targets` | string | — | Optional; absent means `AllMonitors` only. One output name per line on stdout. |

Not in this file, deliberately: keybindings and themes (v1.0), Circle roots (they live in
the engine's own record, reachable via `CircleRef.root`), rotation (v0.2, ADR-0003 §7),
cache sizes and TUI layout (not settings — behaviour).

#### 8.3 Everything else on disk

| Path | Holds | Authority |
|---|---|---|
| `~/.config/kith/config.toml` | §8.2 | Person-owned; kith reads it and never writes it |
| `$XDG_STATE_HOME/kith/person.toml` | `name`, bound `device`, `created` | Local, durable, never synced, never escrowed |
| `$XDG_STATE_HOME/kith/state.toml` | `last_circle`, TUI leftovers | Rebuildable; deleting it costs a switcher press |
| `$XDG_STATE_HOME/kith/invites.toml` | Invites this Device issued, with expiry | Local; the admit-time expiry check reads it (§4.5) |
| `$XDG_STATE_HOME/kith/pending-joins.toml` | Invites consumed, awaiting an offer | Local; consumed by the auto-completion rule (§4.4) |
| `$XDG_CACHE_HOME/kith/cache.sqlite3` | SQLite cache, event cursor | Rebuildable by ADR-0001's authority rule; deletable at any time |
| `$XDG_CACHE_HOME/kith/thumbs/` | `<content-hash>-<class>.png` (ADR-0003 §5) | Rebuildable |
| `<circle root>/` | Items | Source of truth |
| `<circle root>/.kith/` | Sidecars, Membership claims (`members/<device-id>.toml`), Roles (ADR-0004) | Synced source of truth |
| `<circle root>/.kith/local/` | Per-Device scratch, never synced | Rebuildable |

Favourites are per-Person and never cross the seam (ADR-0002 §2); where their bytes live
is ADR-0004's call, and this surface only toggles them through the Collection API.

---

## Edge cases & failure honesty

| Situation | What happens |
|---|---|
| `kith create` / `join` / `invite` before `kith init` | Exit 64, `identity.missing`, `→ run kith init first`. Nothing is written or knocked. |
| Three Circles, no `--circle` | Exit 64 listing the names. No default, no last-used, no guessing (§1.3). |
| `kith join` with a code for a Circle you are already in | Exit 0, note `join.already_member`. No second knock. |
| `kith join`, then the admin never approves | Exit 75 after `--wait`. The knock stands; the message says the offer completes automatically next time kith runs (§4.4). |
| `kith approve` when the Invite has expired | Exit 65 at admit time — the one gate that runs on the gatekeeper's own hardware (ADR-0002 §4). `→ kith invite` for a fresh one. |
| A rejected Device knocks again | It reappears as pending. Stated plainly: `this is not a ban — kith has no blocklist in v0.1`. |
| `kith add` of a path already inside the Circle root | Registered in place: a Sidecar is written, no bytes are copied or moved. |
| `kith add` while the Sync Engine is down | Items land locally, exit 0, `warn` note. They sync when the daemon returns. |
| `kith add` of something not a wallpaper | Skipped with the Provider's reason; run exits 65 but every claimed Item still imported. |
| Delete confirmation | `Delete sunset from walls? This deletes it for every Member. Other Devices keep the last 5 versions for 30 days; v0.1 has no restore. [y/N]` — the honesty is in the confirm, not in a footnote. |
| Leave confirmation | `Leave walls? Your Device stops syncing it. The Items already on disk stay at ~/kith/walls; kith deletes nothing. Rejoining needs a fresh Invite. [y/N]` |
| Conflict copies exist (`*.sync-conflict-*`) | Hidden from the Gallery (ADR-0002 §2), counted by `status` and `doctor` with the `circle.conflicts` note: `2 conflicting copies are on disk next to their Items. v0.1 does not resolve these.` Resolution is the v0.2 Health screen. *Call recorded here:* count and name them, never pretend they do not exist, never build a resolver ROADMAP has cut. |
| Credentials rejected | Exit 78 naming **where the key was read from**, never regenerating or rotating one (ADR-0002 §6). |
| Daemon below the version floor | Membership verbs exit 69 and explain; browsing, Preview, Favourites and Apply keep working. |
| Terminal too small, or not a terminal | Exit 64 with the actual measured size, before the alternate screen. |
| Panic in the TUI | The panic hook restores the terminal first, then prints the message and the backtrace hint on stderr. There is no log file in v0.1 and no `kith logs` verb — nothing is written that a Person is not shown. |
| Event stream gap (`Change::Desynced`) | Status row says `resynchronising…`, kith rescans the tree and rebuilds the cache (ADR-0002 §5). No data is at risk and the surface says which. |
| An unbound key in the TUI | `no binding for 'z' — press ? for keys`. Never a silent no-op. |

---

## Coverage — ROADMAP v0.1 → CLI verb → TUI screen

Every row of ROADMAP §2's in-scope table, and nothing that is not a row of it.

| ROADMAP v0.1 capability | CLI | TUI |
|---|---|---|
| Identity — create a Person, bind this Device | `kith init` | — (a precondition for opening it) |
| Circles — create | `kith create <name>` | — |
| Circles — list | `kith list circles` | Circle switcher (`c`) |
| Circles — join via Invite | `kith join <code>` | automatic completion of the matching offer at startup (§4.4) |
| Circles — founder is admin and sole introducer | shown by `create`, `status`, `list circles` | Members screen (`introducer`) |
| Collections — import a directory as Items | `kith add <path>…` | — (v0.2) |
| Collections — adopt an existing wp-sync tree | `kith create <name> --adopt` | — |
| Collections — list Items | `kith list items` | **Gallery** |
| Members — list with name and presence | `kith list members` | **Members** (`m`) |
| Members — two Roles, honestly stated | Role column + `role.advisory` caveat | Members footer caveat (§7.2) |
| Members — leave a Circle | — (v0.2 `kith leave`) | Members → `L`, confirmed |
| Invites — time-bounded code | `kith invite [--expires]` | — |
| Invites — approve / reject a pending join | `kith approve` / `kith reject` | **join prompt** (auto) and Members → `Enter` |
| Invites — codes expire, no revoke | `invite.no_revoke` caveat | prompt copy |
| Gallery — grid, thumbnails, favourite marker, unseen dot | `kith list items` (`★`, `●` columns) | **Gallery** — see #15 |
| Gallery — favourites toggle | — | `F` |
| Preview — fullscreen with Sidecar facts | — | **Preview** (`Enter`) — see #15 |
| Actions — Apply with a monitor picker | — (v0.2 `kith apply`) | `a` → monitor picker; `Space` menu |
| Actions — Favourite (private) | `★` column in `list items` | `f` |
| Actions — Reveal | — | `r` |
| Actions — Delete | — | `d`, confirmed |
| Actions — copy path | — | `y` |
| Actions — unavailable Actions explain themselves | `kith doctor` apply section | `Space` menu, greyed with reason (§7.4) |
| Providers — backend matrix + custom command | `kith doctor` (`apply.*`), config §8.2 | status row (`swww` / `no apply backend`) |
| Sync Engine — state and per-Member progress | `kith status` | status row |
| Sync Engine — diagnosis | `kith doctor` | — (diagnosis is CLI-only by design: it must work when the TUI cannot open) |
| Configuration — one TOML | the file; validated by `kith doctor` (`config.file`) | — (no settings screen in v0.1) |
| — | `kith version`, `-V` | — |
| — | bare `kith` | every screen |

### Against the §3 walkthrough

| Step | Surface |
|---|---|
| 1 install, daemon running | none — kith never owns the daemon |
| 2 both run `kith doctor` | §5: `engine.reachable`, `preview.protocol` (kitty for Ana, halfblocks + warn for Ben) |
| 3 Ana `kith init` | §4.1, prompts for her name |
| 4 `kith create walls` | §4.2 — Circle, Collection, admin, introducer |
| 5 `kith add ~/Pictures/walls/*` | §4.6 — Items with Sidecars attributing Ana |
| 6 `kith invite` | §4.3 — one code on stdout, framing on stderr |
| 7 Ben `kith init`, `kith join <code>` | §4.1, §4.4 — knock, then wait |
| 8 Ana approves | join prompt `a` (§6.5) or `kith approve` (§4.5) |
| 9 Ben runs `kith` | Gallery, tiles arriving via `observe()`, unseen dots |
| 10 arrows, `Enter`, `f` | §6.4 movement, Preview, Favourite — private, nothing announced |
| 11 `a`, two monitors | monitor picker (§6.4) → `wallpaper.apply` |
| 12 `kith add ~/Downloads/sunset.png` | §4.6; Ana's Gallery marks it unseen and her screen does not change — there is no code path from sync to Apply (ADR-0003 §6) |

---

## Out of scope for v0.1

Named, with the milestone that gets them, so each can be refused by pointing at a line.

| Deferred | Why | Returns |
|---|---|---|
| `kith apply`, `kith fav`, `kith browse` (ADR-0003 §3's CLI Action parity) | Actions are TUI-only in v0.1; ROADMAP puts CLI parity behind rotation needing it | v0.2 |
| `kith rotate` (ADR-0003 §7) | Automation is cut from v0.1 | v0.2 |
| `kith leave` as a verb | Leaving exists in v0.1 — on the Members screen | v0.2 with CLI parity |
| `kith restore`, `kith versions` (ADR-0002 §4) | History is v0.3 and must be designed with Role honesty | v0.3 |
| `kith circle adopt-steward` (ADR-0002 §3 succession — a surviving Member becomes the Steward, and their Device the Circle's introducer) | Members module ships without role editing or removal | v0.2 |
| Conflict resolution UI | ADR-0002's resolve affordance needs the Health screen | v0.2 |
| `kith status --watch`, `kith logs` | The TUI is the live view; nothing writes a log a Person is not shown | v0.2 |
| Per-ticket Invite correlation at admit time | The pending-device record cannot carry the nonce (§4.5) | v0.2 |
| `kith completions`, a man page | Packaging polish | v1.0 |
| Item rename / `--title` on `add` | ROADMAP: no rename in v0.1 | v0.3 with tags |
| Search, sort flags, grouping | ROADMAP Search row | v0.3 |
| Dashboard, Activity and Health screens, notifications | ROADMAP's cut table | v0.2 |
| Keybinding remap, themes, a settings screen | Configuration growth | v1.0 |
| Second Device per Person in the switcher and Members | One Device per Person in v0.1 | v0.3 |
| A `kith config` verb | Edit the TOML; `kith doctor` validates it | not planned |
