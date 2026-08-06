# Syncthing REST + Events API as a Sync-Engine Abstraction Seam

Research for the wp-sync product spec. Current Syncthing stable at time of writing: **v2.1.3 (released 2026-08-05)** [14]. All claims below verified against docs.syncthing.net (which documents v2.1.x) and the official GitHub releases, August 2026.

## Question

> What can Syncthing's REST + events API express for a sync-engine abstraction seam whose operations are create/join/leave/sync/status/share/permissions? Specifically: folder types (send-only / receive-only / send-receive) and what they enforce; device management (add/remove, pending devices, auto-accept); the introducer mechanism and its propagation semantics; pending folders and folder sharing/acceptance flow; ignore patterns; conflict handling (sync-conflict files); the events API (which events exist for detecting new items, device connect/disconnect, sync completion); folder/device level encryption (untrusted devices); and crucially what Syncthing CANNOT enforce (relevant to how honest a P2P app can be about roles/permissions — e.g. can a peer be prevented from deleting or modifying files?). Primary source: docs.syncthing.net (REST API, config, events, advanced features). Also read the local file /home/opx/Projects/wp-sync/wp-sync-setup.sh to see which endpoints the existing script already uses.

## TL;DR

- **Every seam operation maps cleanly to REST calls.** Create = `POST /rest/config/folders`; join = add introducer device + folder; leave = `DELETE /rest/config/folders/{id}`; sync/status = `/rest/db/completion`, `/rest/db/status`; share = PATCH the folder's `devices` list; invitations = `/rest/cluster/pending/{devices,folders}` + events. Config changes apply immediately without restart in the modern (v1.12+) config API [2].
- **The events API (`GET /rest/events`, long-polling with `since`/`events` filters) covers all the reactive needs**: `ItemFinished`/`FolderSummary` for new items, `DeviceConnected`/`DeviceDisconnected` for presence, `FolderCompletion` for per-peer sync progress, `PendingDevicesChanged`/`PendingFoldersChanged` for invitation flow [9][10].
- **Folder types are local self-discipline, not remote enforcement.** Send-only ignores incoming changes; receive-only refuses to propagate local ones — but each device enforces only its *own* configured type; the cluster cannot compel compliance [3]. **No mechanism exists to prevent an accepted peer from modifying or deleting files** in a send-receive folder; mitigations are versioning, receive-only + revert, and `ignoreDelete` (all local) [3][4][12].
- **The introducer mechanism is real trust delegation**: an introducer's device list per mutually-shared folder is auto-copied, transitively ("an introducer's introducer will become your introducer"), and de-introduction cascades removals — a natural "circle admin" primitive, with sharp edges (mutual introducers re-introduce removed devices) [5].
- **Untrusted-device encryption exists** (per-folder-per-device `encryptionPassword`, `receiveencrypted` folder type) and hides content, names and directory structure — but it is officially "beta / testing only" and is for relay/backup nodes, not a permissions system [11].
- The existing wp-sync script already exercises the core seam: `/rest/system/{ping,status,restart}`, `/rest/config/{folders,devices}`, `/rest/config/defaults/device`, plus introducer + autoAcceptFolders — but does **not** yet use `/rest/events` or `/rest/cluster/pending/*` (acceptance is manual via the Web UI) [16].

## Findings

### 1. What the existing script already uses

`/home/opx/Projects/wp-sync/wp-sync-setup.sh` [16] authenticates with `X-API-Key` (scraped from `config.xml`, `~/.local/state/syncthing` on v2, `~/.config/syncthing` on v1) and calls:

| Endpoint | Use in script |
|---|---|
| `GET /rest/system/ping` | liveness wait loop (any HTTP status counts as "up") |
| `GET /rest/system/status` | read own device ID (`.myID`) |
| `PATCH /rest/config/defaults/device` | set `autoAcceptFolders: true` for future devices |
| `GET/POST /rest/config/folders` | check for / create the `wallpapers` folder (`type: "sendreceive"`, `fsWatcherEnabled`) |
| `GET /rest/config/folders/{id}`, `PATCH` | add the introducer to the folder's `devices` array |
| `GET/POST /rest/config/devices` | add introducer device (`introducer: true, autoAcceptFolders: true, addresses: ["dynamic"]`) |
| `POST /rest/system/restart` | apply config (actually unnecessary for these changes — see §2) |

Gap: acceptance of a joining device on the introducer side is manual ("open http://localhost:8384 and click 'Add Device'"). The pending-device REST endpoints + events (§5, §7) can fully automate this.

### 2. REST config API: shape and semantics

The modern config API (added v1.12.0, documented for v2.1.0) is a full CRUD tree [2]:

- `GET/PUT /rest/config` — whole config.
- `GET/PUT/POST /rest/config/folders` and `/rest/config/devices` — list-level (PUT replaces array, POST adds one).
- `GET/PUT/PATCH/DELETE /rest/config/folders/{id}` and `/rest/config/devices/{id}` — per-object.
- `GET/PUT/PATCH /rest/config/defaults/folder`, `/defaults/device`; `GET/PUT /rest/config/defaults/ignores` — templates applied to newly added / auto-accepted objects [2][4].
- `GET /rest/config/restart-required` — check whether a restart is pending.
- Key semantics: "When posting the configuration succeeds, the posted configuration is immediately applied, except for changes that require a restart" — folder/device changes take effect live. PATCH unmarshals JSON on top of the existing object, but "all child objects will replace the existing objects, not extend them" (so patching `devices` means sending the complete new array) [2].

Auth: `X-API-Key` header or `Authorization: Bearer` token; only `/rest/noauth/*` (e.g. `/rest/noauth/health`) is unauthenticated [1].

Status/ops endpoints relevant to the seam [1][13]:

- `GET /rest/db/status?folder=` — folder sync state; `GET /rest/db/browse` — remote/global tree listing; `POST /rest/db/scan` — force rescan; `POST /rest/db/override` (send-only: push local state) and `POST /rest/db/revert` (receive-only: discard local changes).
- `GET /rest/db/completion?folder=&device=` — both params optional; "an empty or absent folder parameter means all folders as an aggregate", "an empty or absent device parameter means the local device", and "if a device is specified but no folder, completion is calculated for all folders shared with that device". Returns `completion` (0–100), `globalBytes/needBytes`, `globalItems/needItems`, `needDeletes`, `sequence`, and `remoteState` (`valid`/`paused`/`notSharing`/`unknown` when disconnected) — `remoteState` also tells you "whether the remote device has accepted the folder (shares it with us)" [13].
- `GET /rest/system/connections` — live connection info per device; `POST /rest/system/pause|resume` — pause sync globally or per device [1].

### 3. Folder types and what they enforce

Four types via the folder `type` field: `sendreceive` (default), `sendonly`, `receiveonly` (since v0.14.50), `receiveencrypted` [3][4].

- **Send & Receive** — full bidirectional sync.
- **Send Only** — for a "reference copy". Incoming "changes are still *received* so the folder may become 'out of sync', but no changes will be applied." An **Override Changes** action (`POST /rest/db/override`) forces the local state onto the cluster, overwriting remote versions and deleting files absent locally [3][1].
- **Receive Only** — accepts and redistributes cluster changes but never propagates local edits: "any local modifications are preserved and do not cause the folder to become 'out of sync'" locally, though the device appears out of sync to others. **Revert Local Changes** (`POST /rest/db/revert`) undoes local edits and re-syncs from the cluster [3][1].
- **Receive Encrypted** — stores only ciphertext; used on untrusted devices (§9).

**The critical caveat (verbatim gist from the docs): enforcement operates locally only. Each device independently enforces its configured folder type; the cluster cannot compel compliance** [3]. Folder type is a *local* filter on what a device applies/announces, not a permission granted or denied to remote peers.

### 4. Device management

Device config object fields (relevant subset) [4]: `deviceID` (mandatory; cryptographic ID derived from the device's TLS certificate), `name`, `addresses` (incl. `dynamic` for discovery), `compression`, `paused`, `maxSendKbps`/`maxRecvKbps`, `allowedNetworks` (CIDR restriction), plus the trust-relevant flags:

- `introducer` — "set to true if this device should be trusted as an introducer, i.e. we should copy their list of devices per folder when connecting" [4].
- `introducedBy` — records who introduced this device, used for de-introduction [4].
- `autoAcceptFolders` — "if true, folders shared from this remote device are automatically added and synced locally under the default path" (default path + default ignore patterns come from `defaults/folder` and `defaults/ignores`) [4][2].
- `untrusted` — "marks a particular device as untrusted, which disallows ever sharing any unencrypted data with it" [4].

Add/remove = `POST /rest/config/devices` / `DELETE /rest/config/devices/{id}`. **Pending devices**: when an unknown device dials you, it appears at `GET /rest/cluster/pending/devices` (list of device IDs with name/address/time); `DELETE /rest/cluster/pending/devices` dismisses an offer [1]. Accepting = simply adding the device via the config API. The `PendingDevicesChanged` event fires when the pending set changes (replacing the deprecated `DeviceRejected` event) [9].

### 5. Introducer mechanism and propagation semantics

From the dedicated docs page [5]:

- When you connect to a device flagged `introducer`, you auto-adopt its per-folder device lists — but **only for folders you mutually share** with the introducer. Devices the introducer knows that don't share a mutual folder with you are not propagated.
- What's copied: "the autoconfiguration of device IDs, labels and configured address settings, but no other device-specific settings." And it's one-shot: "Once autoconfigured, device-specific settings will currently not receive any updates from an introducer."
- **Transitive**: "An introducers' introducer will become your introducer as well."
- **De-introduction cascades**: "If an introduced device is no longer present on an introducer, or no longer shares any mutual folders with the device, it will be automatically removed when devices in the cluster next connect to the introducer." So an introducer removing a peer removes it across the circle (for devices that got it via introduction, tracked by `introducedBy`).
- **Footgun**: two devices set as introducers *to each other* will "constantly 're-introduc[e]' the removed device to each other" — mutual introduction breaks removal [5].

### 6. Pending folders and the share/accept flow

Sharing a folder with a peer = adding `{deviceID: X}` to the folder's `devices` array (what the wp-sync script does via PATCH). On the receiving side, an unreciprocated share shows up at `GET /rest/cluster/pending/folders` (since v1.13.0): a map of folder ID → `offeredBy` → per-device `{time, label, receiveEncrypted, remoteEncrypted}` [6]. Optional `?device=` filters by offerer. `DELETE /rest/cluster/pending/folders` dismisses an offer [1].

Acceptance is not a dedicated endpoint: you accept by creating the folder locally via the config API with the same folder ID and sharing it back with the offering device [6][2]. `PendingFoldersChanged` fires on changes to this set [9]. With `autoAcceptFolders: true` on the offering device's entry, acceptance is automatic under `defaults/folder.path` [4].

Full "join a circle" flow expressible today (and mostly implemented in wp-sync): joiner adds introducer device + shares the folder to it → introducer sees `PendingDevicesChanged` → introducer accepts via `POST /rest/config/devices` and adds the device to the folder → joiner (with `autoAcceptFolders`) picks up the share; introducer propagates all other circle members automatically [4][5][16].

### 7. Ignore patterns

`.stignore` in the folder root; "The `.stignore` file itself will never be synced to other devices" — ignores are strictly per-device [7]. REST: `GET/POST /rest/db/ignores?folder=` reads/writes them at runtime; `defaults/ignores` seeds auto-accepted folders [1][2]. Syntax: `*` (no separator crossing), `**` (crosses separators), `?`, `[a-z]`, `{alt1,alt2}`, `!` negation, `(?i)` case-insensitive, `(?d)` "should be used by any OS generated files which you are happy to be removed" (deletable when blocking a directory removal), `#include` of shared pattern files [7]. Caveat: "ignored files can block removal of an otherwise empty directory" unless `(?d)`-prefixed [7]. Ignored files are invisible to the cluster — useful for local-only content inside a collection directory (e.g. thumbnails, app metadata you don't want replicated).

### 8. Conflict handling

- Concurrent modifications produce a conflict copy on the *losing* side named `<filename>.sync-conflict-<date>-<time>-<modifiedBy>.<ext>` where `modifiedBy` is the device ID that created the losing version [8].
- Loser selection: "The file with the older modification time will be marked as the conflicting file and thus be renamed"; tie → "the file originating from the device which has the larger value of the first 63 bits for its device ID" loses [8].
- Modify-vs-delete conflicts are resolved the same way; since v2.0, "a delete can now be the winning outcome of conflict resolution, resulting in the deleted file being moved to a conflict copy" [8][15].
- Conflict copies are ordinary files afterward: "the `sync-conflict` files are treated as normal files after they are created, so they are propagated between devices" [8].
- `maxConflicts` per folder caps retained conflict copies (default 10; `-1` unlimited, `0` disables conflict copies) [4].

For an append-mostly image collection, conflicts are rare (distinct filenames), but the app should still filter/handle `*.sync-conflict-*` names in the gallery.

### 9. Untrusted devices / folder encryption

Per-folder-per-device: set `encryptionPassword` on the device entry inside a folder's `devices` list; "Data sent will be encrypted by this password, and data received will be decrypted by the same password" — the folder ID is also key material [11][4]. The untrusted side runs the folder as `receiveencrypted` and stores opaque blocks. Hidden from the untrusted device: file data, metadata (names, timestamps, hashes) and directory structure ("Your directory structure is not replicated, even in encrypted-name form"). Still visible: approximate file sizes ("it's still easy to derive the original size"), folder ID and label [11]. The device-level `untrusted: true` flag is a safety interlock that "disallows ever sharing any unencrypted data with it" [4]. Multiple trusted devices can sync *through* one untrusted relay using the same password [11]. Status: "This feature should still be considered beta / testing only"; known inefficiency: no block reuse across files on the encrypted side, so renames re-transfer data [11].

Relevance: this is an *always-on relay / backup node* story (e.g. a friend circle's VPS holding ciphertext), **not** a per-member permission mechanism — every trusted member holding the password sees everything.

### 10. Events API

`GET /rest/events` is "a simple long polling interface": blocks until events newer than `?since=<lastSeenID>` exist, "times out after 60 seconds returning an empty array" (tunable via `?timeout=`); `?limit=n` returns only the last n (catch-up after disconnect); `?events=A,B` filters types. Events carry incrementing `id` (gaps ⇒ buffer overflow, resync your state), `globalID`, `time`, `type`, `data`. Default mask excludes the noisy `LocalChangeDetected`/`RemoteChangeDetected`; the convenience endpoint `/rest/events/disk` serves exactly those two [10][9].

33 event types exist [9]. The ones that matter for the seam:

| Seam need | Events |
|---|---|
| New/changed items in a collection | `ItemStarted`, `ItemFinished` (per file), `RemoteChangeDetected` / `LocalChangeDetected` (via `/rest/events/disk`), `LocalIndexUpdated` / `RemoteIndexUpdated`, `FolderSummary` |
| Peer presence | `DeviceConnected`, `DeviceDisconnected`, `DeviceDiscovered`, `DevicePaused` / `DeviceResumed` |
| Sync completion / progress | `FolderCompletion` (per remote device per folder), `StateChanged` (folder idle/scanning/syncing), `DownloadProgress`, `FolderScanProgress` |
| Invitation flow | `PendingDevicesChanged`, `PendingFoldersChanged` (successors to deprecated `DeviceRejected`/`FolderRejected`) |
| Config/infra | `ConfigSaved`, `FolderErrors`, `Failure`, `StartupComplete`, `ClusterConfigReceived`, `FolderWatchStateChanged` |

This is sufficient for a fully event-driven TUI: no polling needed beyond the long-poll loop itself.

### 11. What Syncthing CANNOT enforce

This is the honesty boundary for the product's "roles/permissions" story:

1. **No remote write protection.** Any device you share a send-receive folder with can create, modify and delete any file, and those changes propagate to everyone. There is no ACL, no per-file ownership, no read-only *grant*. Folder types are self-imposed: "Each device independently enforces its configured folder type; the cluster cannot compel compliance" [3]. You cannot make a *peer's* copy read-only; you can only make *your own* device ignore their writes (send-only) or keep your writes local (receive-only).
2. **Deletion cannot be prevented, only mitigated locally**: file versioning archives remote-originated changes/deletes ("Versioning applies to changes received from other devices… Local file changes are never versioned") [12]; `ignoreDelete` makes a device "pretend not to see instructions to delete files from other devices" (documented, but a known consistency footgun) [4]; send-only + Override can forcibly restore state cluster-wide [3].
3. **No content authorship guarantees.** Conflict metadata records `modifiedBy` device ID [8], and device IDs are strong (certificate-derived), but file contents are not signed per author — any member can overwrite a file and the cluster treats it as the new truth.
4. **Introducer trust is coarse.** An introducer can add devices to your world for mutually-shared folders, transitively, and its removals cascade [5]. There is no finer-grained "can invite but not remove" or per-folder admin role.
5. **Encryption is not permissioning.** `receiveencrypted` protects against an *untrusted host*, not against other members; anyone with the folder password has full read/write [11].
6. **Config API is per-device and all-powerful.** The REST API controls only the local instance; holding the API key means full control of that node (no scoped tokens) [1]. A wrapper app cannot stop the user (or another local process) from reconfiguring Syncthing out from under it — detectable via `ConfigSaved` events [9], but not preventable.
7. **Membership state is eventually consistent, not authoritative.** There is no cluster-wide membership registry; each device has its own device/folder lists, reconciled via introducers. Two-sided acceptance (pending devices/folders) is the only gate, and it gates *connection*, not capabilities.

## Implications for the spec

- **The seam fits.** Map: circle ≈ introducer-rooted device graph; collection ≈ folder; create = `POST /rest/config/folders`; join = pending-device/-folder handshake (+ `autoAcceptFolders`); leave = `DELETE /rest/config/folders/{id}` (collection) or `DELETE /rest/config/devices/{id}` (circle); sync/status = `/rest/db/completion` + `FolderCompletion`/`StateChanged` events; share = folder `devices` PATCH + pending-folder acceptance. All operations are plain JSON over localhost with one API key, and take effect without restart — a thin adapter, easily hidden behind an interface for a future non-Syncthing engine.
- **Automate the current manual step.** The script's "accept in the Web UI" gap closes with `GET /rest/cluster/pending/devices` + `PendingDevicesChanged` → confirm in the TUI → `POST /rest/config/devices`. This becomes the app's invitation inbox.
- **Be honest about permissions: model them as *policies*, not *guarantees*.** The spec should say "roles" are cooperative conventions enforced by each member's own node (folder types, versioning) plus social recovery (versioning restore, send-only override on a curator node), never "X cannot delete your files." A trustworthy default stack: send-receive + trash-can or simple versioning (`.stversions`) on every member, `maxConflicts` default, surfaced "restore" UX. An optional "curator" pattern — one send-only reference node — is the closest Syncthing gets to a moderated collection.
- **Introducer = circle admin, with documented sharp edges.** Exactly one introducer per circle (or a clear hierarchy) — never mutual introducers, or removals resurrect [5]. The spec's "remove member" story can lean on cascade de-introduction, but must note it only affects devices added via introduction and only takes effect on next connection.
- **Event-driven TUI is fully supported.** One long-poll loop on `/rest/events?since=&events=...` covers gallery refresh (`ItemFinished`, `FolderSummary`), presence badges (`DeviceConnected/Disconnected`), progress bars (`FolderCompletion`), and the invite inbox (`Pending*Changed`). Design for event-ID gaps (buffer overflow ⇒ full re-read of `/rest/db/*` state).
- **Use per-device ignores for app-local files** (thumbnails, provider state) inside collection dirs, seeded via `defaults/ignores` for auto-accepted folders; filter `*.sync-conflict-*` in the gallery and offer a "resolve" affordance since conflict copies replicate to everyone.
- **Untrusted-device encryption is a future "always-on ciphertext relay" feature, not a v1 permission tool** — it is officially beta, and within a circle it grants nothing granular.
- **Version floor:** target the v1.12+ config API / v1.13+ pending endpoints; realistically spec against the 2.x series (SQLite backend, ~6-month deleted-item retention default, delete-can-win conflict change, multi-connection default) [15][14]. Note v2's config path move (`~/.local/state/syncthing`) which the script already handles [16].

## Sources

1. https://docs.syncthing.net/dev/rest.html — REST API index (auth, system/db/cluster/events endpoint groups)
2. https://docs.syncthing.net/rest/config.html — config endpoint tree, methods, immediate-apply and PATCH semantics
3. https://docs.syncthing.net/users/foldertypes.html — folder types, override/revert, local-only enforcement caveat
4. https://docs.syncthing.net/users/config.html — folder/device config options (introducer, autoAcceptFolders, untrusted, maxConflicts, ignoreDelete, defaults)
5. https://docs.syncthing.net/users/introducer.html — introducer propagation, transitivity, de-introduction, mutual-introducer caveat
6. https://docs.syncthing.net/rest/cluster-pending-folders-get.html — pending folders response shape (since v1.13.0)
7. https://docs.syncthing.net/users/ignoring.html — .stignore syntax, per-device semantics, (?d)/(?i) prefixes
8. https://docs.syncthing.net/users/syncing.html — conflict file naming, loser selection, delete-vs-modify, propagation of conflict copies
9. https://docs.syncthing.net/dev/events.html — full event type list and payload semantics
10. https://docs.syncthing.net/rest/events-get.html — /rest/events long-polling parameters, default mask, /rest/events/disk
11. https://docs.syncthing.net/users/untrusted.html — untrusted (encrypted) devices, what is/isn't hidden, beta status
12. https://docs.syncthing.net/users/versioning.html — versioning modes; applies to remote changes only
13. https://docs.syncthing.net/rest/db-completion-get.html — completion/needBytes/remoteState semantics
14. https://github.com/syncthing/syncthing/releases — current release v2.1.3 (2026-08-05) and 2.1.x changes
15. https://github.com/syncthing/syncthing/releases/tag/v2.0.0 — v2.0.0 (August 2025): SQLite backend, delete-retention default, delete-can-win conflicts, multi-connection default
16. Local file: /home/opx/Projects/wp-sync/wp-sync-setup.sh — endpoints used by the existing script
