//! The one production implementor of the Sync Engine seam.
//!
//! This is the only module in kith allowed to say "Syncthing". Everything it
//! learns — folder ids, device ids, introducer flags, event types — stops here.
//!
//! kith never launches, embeds, configures or supervises the daemon. Every
//! surviving Syncthing wrapper adapts a separately-running daemon; the one that
//! owned the process aged worst and died. That rule has no exceptions, including
//! "just for onboarding".
//!
//! ## What this module writes, and what it refuses to
//!
//! Every configuration write is a read-modify-write of the daemon's own JSON:
//! the object comes back from the daemon, kith changes the keys it owns, and the
//! whole object goes back. Fields kith has never heard of survive untouched,
//! which is the point — the daemon's config belongs to the Person, and a wrapper
//! that rewrites it wholesale is a wrapper that eventually eats someone's setup.
//!
//! kith's writes are scoped to: the folders it created or adopted, the `devices`
//! array of those folders, device entries for Devices a Person admitted, and the
//! introducer flag on those entries. It never touches the GUI block, global
//! options, `defaults/*`, or a folder it does not own — and it never calls
//! `/rest/system/restart` or `/rest/system/shutdown`, because config writes have
//! applied live since the config API was rebuilt in v1.12.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use serde_json::{Value, json};

use super::{
    Change, CircleId, CircleOffer, CircleRef, CircleStatus, Cursor, DeviceId, EngineHealth,
    Envelope, InviteTicket, JoinRequest, PeerCompletion, PeerDevice, RelPath, SyncEngine,
    SyncError, Version,
};

/// The lowest daemon version whose REST semantics we rely on.
const VERSION_FLOOR: &str = "1.23.0";

/// Globs the daemon owns inside a Circle root. Answered through
/// `SyncEngine::reserved_paths` so these spellings never climb above the seam.
const RESERVED: &[&str] = &[
    ".stfolder",
    ".stversions/**",
    ".stignore",
    ".stglobalignore",
    ".syncthing.*.tmp",
    "~syncthing~*.tmp",
    "*.sync-conflict-*",
];

/// Circle ids are `kith-` plus eight random base32 characters — never derived
/// from the Circle's name, because the name is mutable and the id is not.
const CIRCLE_ID_PREFIX: &str = "kith-";
const CIRCLE_ID_ENTROPY: usize = 8;

/// The per-Device scratch space, kept out of replication. `(?d)` so it cannot
/// block a directory delete on a Device that still holds thumbnails.
const LOCAL_SCRATCH_IGNORE: &str = "(?d).kith/local";

/// How long one long poll waits before the daemon answers with an empty batch.
const EVENT_TIMEOUT_SECS: u64 = 60;

/// The change feed's subscription. Naming the types is what keeps the daemon's
/// ring buffer full of events kith cares about instead of events it discards:
/// an unfiltered subscription overflows sooner, and an overflow costs a rescan.
///
/// Some of these have no home in the seam's four-variant `Change` yet
/// (`StateChanged`, `FolderCompletion`, `PendingFoldersChanged`, `ConfigSaved`,
/// `FolderErrors`). They stay subscribed because they are the subscription
/// ADR-0002 §5 fixed, and because `ConfigSaved` carries the folder list this
/// module needs to attribute presence to Circles.
const EVENT_FILTER: &str = "ItemFinished,LocalIndexUpdated,RemoteIndexUpdated,StateChanged,\
FolderSummary,FolderCompletion,DeviceConnected,DeviceDisconnected,PendingDevicesChanged,\
PendingFoldersChanged,ConfigSaved,FolderErrors";

/// Reconnection backoff while the engine is unreachable (ADR-0002 §5).
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How many Envelopes may queue before the feed applies backpressure to itself.
const FEED_BUFFER: usize = 256;

pub struct SyncthingEngine {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

/// Credentials discovered from the daemon's own configuration.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub base_url: String,
    pub api_key: String,
    pub source: PathBuf,
}

impl SyncthingEngine {
    pub fn new(creds: Credentials) -> Self {
        Self {
            base_url: creds.base_url,
            api_key: creds.api_key,
            http: reqwest::Client::new(),
        }
    }

    /// Find the running daemon's address and API key without ever writing to its
    /// configuration.
    ///
    /// kith reads what the daemon already has; if credentials are absent or
    /// rejected, that is reported to the Person rather than repaired, because
    /// silently rewriting another program's config is how wrappers earn their
    /// reputation.
    pub fn discover() -> Result<Credentials, SyncError> {
        for path in Self::config_candidates() {
            let Ok(xml) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(api_key) = extract_tag(&xml, "apikey") else {
                continue;
            };
            let address = extract_gui_address(&xml).unwrap_or_else(|| "127.0.0.1:8384".into());
            let scheme = if xml.contains("tls=\"true\"") { "https" } else { "http" };
            return Ok(Credentials {
                base_url: format!("{scheme}://{address}"),
                api_key,
                source: path,
            });
        }
        Err(SyncError::Unreachable)
    }

    fn config_candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(explicit) = std::env::var("KITH_ENGINE_CONFIG") {
            out.push(PathBuf::from(explicit));
        }
        if let Some(base) = directories::BaseDirs::new() {
            // Syncthing v2 keeps config in the state dir; v1 kept it in the config dir.
            out.push(base.state_dir().unwrap_or(base.config_dir()).join("syncthing/config.xml"));
            out.push(base.config_dir().join("syncthing/config.xml"));
            out.push(base.home_dir().join(".local/state/syncthing/config.xml"));
        }
        out
    }

    // ── HTTP ─────────────────────────────────────────────────────────────
    //
    // One place maps transport and status codes into the closed `SyncError`, so
    // there is exactly one answer to "what does a 403 mean" and it is the
    // answer §6 requires: `Unauthorized`, reported, never repaired.

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, SyncError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .header("X-API-Key", &self.api_key);
        if let Some(body) = body {
            request = request.json(body);
        }

        let resp = request.send().await.map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                SyncError::Unreachable
            } else {
                SyncError::Engine(e.to_string())
            }
        })?;

        let status = resp.status().as_u16();
        // Read the body whatever the status: the daemon puts its reason in
        // there as plain text, and a reason is the difference between "the
        // engine said no" and a message a Person can act on.
        let text = resp
            .text()
            .await
            .map_err(|e| SyncError::Engine(e.to_string()))?;

        match status {
            // Config writes answer 200 with an empty body. That is a success,
            // not a parse failure.
            200..=299 if text.trim().is_empty() => Ok(Value::Null),
            200..=299 => serde_json::from_str(&text).map_err(|e| SyncError::Engine(e.to_string())),
            // Never auto-repaired. kith does not rewrite credentials it did not
            // issue, so this travels up as-is and the Person is told where the
            // key was read from.
            401 | 403 => Err(SyncError::Unauthorized),
            404 => Err(SyncError::NotFound),
            other => Err(SyncError::Engine(format!("HTTP {other}: {}", text.trim()))),
        }
    }

    async fn get(&self, path: &str) -> Result<serde_json::Value, SyncError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<serde_json::Value, SyncError> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn put(&self, path: &str, body: &Value) -> Result<serde_json::Value, SyncError> {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }

    async fn delete(&self, path: &str) -> Result<serde_json::Value, SyncError> {
        self.send(reqwest::Method::DELETE, path, None).await
    }

    // ── config read-modify-write ─────────────────────────────────────────

    /// One Circle's folder configuration as the daemon holds it, or `NotFound`.
    async fn folder(&self, circle: &CircleId) -> Result<Value, SyncError> {
        self.get(&format!("/rest/config/folders/{}", escape(&circle.0)))
            .await
    }

    /// Write a folder back after changing the keys kith owns. Everything else in
    /// the object — including keys this kith has never heard of — rides along
    /// untouched, which is the whole discipline.
    async fn put_folder(&self, circle: &CircleId, folder: &Value) -> Result<(), SyncError> {
        self.put(&format!("/rest/config/folders/{}", escape(&circle.0)), folder)
            .await
            .map(|_| ())
    }

    /// Give a peer Device an entry in the daemon's device list if it has none.
    ///
    /// An entry that already exists is left exactly as it is: it may predate
    /// kith, and its addresses, compression and rate limits are the Person's
    /// settings, not ours. The one thing kith insists on for an entry it
    /// creates is `autoAcceptFolders: false` — the daemon's own default may be
    /// `true` (wp-sync set it globally, ADR-0002 §7) and kith accepts Circles
    /// explicitly or not at all.
    async fn ensure_device(&self, device: &DeviceId, name: &str) -> Result<(), SyncError> {
        let path = format!("/rest/config/devices/{}", escape(&device.0));
        match self.get(&path).await {
            Ok(_) => Ok(()),
            Err(SyncError::NotFound) => self
                .post("/rest/config/devices", &new_device_entry(device, name))
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// Pin an address the Invite carried, so a joiner can reach the Steward on a
    /// network where discovery does not.
    ///
    /// Added alongside `dynamic` rather than replacing it: the hint is what the
    /// Steward's Device looked like when the code was printed, and discovery has
    /// to keep working when that stops being true.
    async fn set_device_address(&self, device: &DeviceId, address: &str) -> Result<(), SyncError> {
        let path = format!("/rest/config/devices/{}", escape(&device.0));
        let mut entry = self.get(&path).await?;
        let Some(addresses) = entry.get_mut("addresses").and_then(Value::as_array_mut) else {
            return Ok(());
        };
        if addresses.iter().any(|a| a.as_str() == Some(address)) {
            return Ok(());
        }
        addresses.push(Value::String(address.to_string()));
        self.put(&path, &entry).await.map(|_| ())
    }

    /// Seed a Circle's ignore patterns so per-Device scratch never replicates.
    async fn seed_ignores(&self, circle: &CircleId) -> Result<(), SyncError> {
        self.post(
            &format!("/rest/db/ignores?folder={}", escape(&circle.0)),
            &json!({ "ignore": [LOCAL_SCRATCH_IGNORE] }),
        )
        .await
        .map(|_| ())
    }

    /// Build a Circle's folder object from the daemon's own folder defaults and
    /// then impose ADR-0002 §2's recipe on top.
    ///
    /// Starting from the defaults rather than from `{}` is deliberate: the
    /// folder schema gains fields with every release, and a hand-built object
    /// would hand the daemon a zero for every field kith has not heard of.
    async fn folder_from_recipe(
        &self,
        id: &CircleId,
        name: &str,
        root: &Path,
        devices: &[DeviceId],
    ) -> Result<Value, SyncError> {
        let mut folder = self.get("/rest/config/defaults/folder").await?;
        if !folder.is_object() {
            folder = json!({});
        }
        apply_recipe(&mut folder, id, name, root);
        folder["devices"] = Value::Array(devices.iter().map(shared_with).collect());
        Ok(folder)
    }
}

impl SyncEngine for SyncthingEngine {
    type Changes = Pin<Box<dyn Stream<Item = Envelope> + Send>>;

    async fn health(&self) -> Result<EngineHealth, SyncError> {
        let v = self.get("/rest/system/version").await?;
        let version = v
            .get("version")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(EngineHealth {
            reachable: true,
            version,
        })
    }

    async fn local_device(&self) -> Result<DeviceId, SyncError> {
        let v = self.get("/rest/system/status").await?;
        v.get("myID")
            .and_then(|s| s.as_str())
            .map(|s| DeviceId(s.to_string()))
            .ok_or_else(|| SyncError::Engine("status carried no myID".into()))
    }

    fn reserved_paths(&self) -> &[&'static str] {
        RESERVED
    }

    /// Create a Circle: one folder, at `root`, shared with nobody yet.
    ///
    /// **On being the Circle's introducer.** The daemon has no self-introducer
    /// flag and no folder-scoped one: the flag lives on *peer* device entries
    /// and means "copy this peer's device lists to me". The creating Device is
    /// the introducer precisely by flagging nobody (ADR-0002 §3), so there is
    /// nothing to write here — which is also why the Circle's Steward Device is
    /// recorded above the seam in `circle.toml`'s `founder_device` and never
    /// read back out of the daemon's config.
    ///
    /// Nothing else in the daemon's configuration is touched.
    async fn create_circle(&self, name: &str, root: &Path) -> Result<CircleRef, SyncError> {
        let me = self.local_device().await?;
        let id = CircleId(mint_circle_id());

        let folder = self
            .folder_from_recipe(&id, name, root, std::slice::from_ref(&me))
            .await?;
        self.post("/rest/config/folders", &folder).await?;
        self.seed_ignores(&id).await?;

        Ok(CircleRef {
            id,
            name: name.to_string(),
            root: root.to_path_buf(),
        })
    }

    async fn circles(&self) -> Result<Vec<CircleRef>, SyncError> {
        let v = self.get("/rest/config/folders").await?;
        Ok(map_folders(&v))
    }

    /// Joiner, phase 1: register the Steward's Device and knock.
    ///
    /// Adding the entry *is* the knock — the two daemons connect, the Steward's
    /// daemon does not know this Device, and it appears in their pending list.
    /// There is no separate "request" call.
    ///
    /// `autoAcceptFolders` is never enabled. Acceptance is phase 2 and it is
    /// explicit, so the joiner chooses the root and no global default is touched.
    async fn begin_join(&self, invite: &InviteTicket) -> Result<(), SyncError> {
        self.ensure_device(&invite.steward_device, "").await?;
        if let Some(address) = &invite.address {
            self.set_device_address(&invite.steward_device, address).await?;
        }
        // The Steward's Device is this Device's one introducer: it is where the
        // rest of the Circle's Devices will be learned from.
        self.set_introducer(&invite.steward_device, true).await
    }

    /// Joiner, phase 2: the Circle was offered back; place it at `root`.
    ///
    /// Accepting an offered folder is adding it to this daemon's config with the
    /// offered id — there is no "accept" endpoint, and that is the reason kith
    /// can put the bytes wherever the Person said instead of wherever the daemon
    /// would have guessed.
    async fn complete_join(&self, offer: &CircleOffer, root: &Path) -> Result<CircleRef, SyncError> {
        let me = self.local_device().await?;
        self.ensure_device(&offer.from, "").await?;

        // The offer may have been reconstructed from the Invite rather than read
        // off the daemon, in which case it carries no label. Prefer what the
        // offering Device actually said; fall back to the Circle id, never to a
        // guess at what the Circle might be called.
        let name = if offer.label.is_empty() {
            self.get("/rest/cluster/pending/folders")
                .await
                .ok()
                .and_then(|v| offered_label(&v, &offer.circle, &offer.from))
                .unwrap_or_else(|| offer.circle.0.clone())
        } else {
            offer.label.clone()
        };

        let mut devices = vec![me.clone()];
        if offer.from != me {
            devices.push(offer.from.clone());
        }
        let folder = self
            .folder_from_recipe(&offer.circle, &name, root, &devices)
            .await?;
        self.post("/rest/config/folders", &folder).await?;
        self.seed_ignores(&offer.circle).await?;

        Ok(CircleRef {
            id: offer.circle.clone(),
            name,
            root: root.to_path_buf(),
        })
    }

    async fn pending_joins(&self) -> Result<Vec<JoinRequest>, SyncError> {
        let v = self.get("/rest/cluster/pending/devices").await?;
        Ok(map_pending_devices(&v))
    }

    async fn pending_circles(&self) -> Result<Vec<CircleOffer>, SyncError> {
        let v = self.get("/rest/cluster/pending/folders").await?;
        Ok(map_pending_folders(&v))
    }

    /// Admit a knocking Device into a Circle.
    ///
    /// Two writes, both scoped: an entry for the Device if it has none, and the
    /// Device appended to this one folder's `devices` array. Deliberate, never
    /// automatic — this is the gate that runs on the gatekeeper's own hardware
    /// and it is the only Role promise kith can actually keep (ADR-0002 §4).
    async fn admit(&self, circle: &CircleId, request: &JoinRequest) -> Result<(), SyncError> {
        let mut folder = self.folder(circle).await?;
        self.ensure_device(&request.device, &request.name).await?;

        if shared_devices(&folder).iter().any(|d| *d == request.device) {
            return Ok(());
        }
        let Some(devices) = folder.get_mut("devices").and_then(Value::as_array_mut) else {
            return Err(SyncError::Engine("folder carried no devices array".into()));
        };
        devices.push(shared_with(&request.device));
        self.put_folder(circle, &folder).await
    }

    /// Remove a Device from a Circle.
    ///
    /// Only the Circle's `devices` array is touched. That is enough for the
    /// de-introduction cascade — a Member that learned of this Device by
    /// introduction drops it once the introducer stops offering it any shared
    /// folder — and it leaves the Device's own entry alone, because the same
    /// Device may be in another Circle and its entry may predate kith.
    ///
    /// Forward-looking only: bytes already on that Device stay there, and the
    /// cascade lands as each Member next connects to the introducer.
    async fn expel(&self, circle: &CircleId, device: &DeviceId) -> Result<(), SyncError> {
        let mut folder = self.folder(circle).await?;
        let Some(devices) = folder.get_mut("devices").and_then(Value::as_array_mut) else {
            return Err(SyncError::Engine("folder carried no devices array".into()));
        };
        let before = devices.len();
        devices.retain(|d| d.get("deviceID").and_then(Value::as_str) != Some(device.0.as_str()));
        if devices.len() == before {
            return Err(SyncError::NotFound);
        }
        self.put_folder(circle, &folder).await
    }

    /// Stop replicating a Circle.
    ///
    /// **Local bytes are kept.** Dropping the folder from the daemon's config
    /// removes it from the cluster and touches not one file on disk — the
    /// Circle's contents, its `.stfolder` marker and its archived versions are
    /// all still there afterwards. Deleting content is a decision for a Person
    /// and a different surface, never a side effect of leaving.
    async fn leave(&self, circle: &CircleId) -> Result<(), SyncError> {
        // The daemon answers a delete of an unknown folder with 200. Ask first,
        // so leaving a Circle this engine never replicated says so.
        self.folder(circle).await?;
        self.delete(&format!("/rest/config/folders/{}", escape(&circle.0)))
            .await
            .map(|_| ())
    }

    /// Flag or unflag a peer Device as *this* Device's introducer.
    ///
    /// Device-scoped, not Circle-scoped: the daemon has no way to say "introduce
    /// me for this folder only", so two People sharing two Circles get device
    /// lists propagated for both. Additive and harmless, and the place the
    /// mapping leaks (ADR-0002 §3).
    ///
    /// Never two introducers and never mutual — that rule is enforced above the
    /// seam, where the Circle's Steward Device is known. This method flags the
    /// one Device it is given.
    async fn set_introducer(&self, device: &DeviceId, flag: bool) -> Result<(), SyncError> {
        let path = format!("/rest/config/devices/{}", escape(&device.0));
        let mut entry = self.get(&path).await?;
        if !entry.is_object() {
            return Err(SyncError::NotFound);
        }
        entry["introducer"] = json!(flag);
        // An entry kith flags is an entry kith is about to be introduced by, and
        // a daemon whose global default is `autoAcceptFolders: true` would start
        // taking folders from it unasked.
        entry["autoAcceptFolders"] = json!(false);
        self.put(&path, &entry).await.map(|_| ())
    }

    /// Peer Devices sharing a Circle, with presence.
    ///
    /// Never returns this Device. That is why `PeerDevice::introducer` is a
    /// cross-check and not an answer: the introducer flags nobody, so on the
    /// Steward's own Device the flag sits on no peer at all, and the one Device
    /// that could have carried it is the one this method omits.
    async fn devices(&self, circle: &CircleId) -> Result<Vec<PeerDevice>, SyncError> {
        let folder = self.folder(circle).await?;
        let me = self.local_device().await?;
        let entries = self.get("/rest/config/devices").await?;
        let connections = self.get("/rest/system/connections").await?;
        Ok(map_peer_devices(&folder, &entries, &connections, &me))
    }

    async fn status(&self, circle: &CircleId) -> Result<CircleStatus, SyncError> {
        let folder = self.folder(circle).await?;
        let me = self.local_device().await?;
        let db = self
            .get(&format!("/rest/db/status?folder={}", escape(&circle.0)))
            .await?;

        let mut peers = Vec::new();
        for device in shared_devices(&folder) {
            if device == me {
                continue;
            }
            let completion = self
                .get(&format!(
                    "/rest/db/completion?folder={}&device={}",
                    escape(&circle.0),
                    escape(&device.0)
                ))
                .await?;
            peers.push(PeerCompletion {
                percent: completion
                    .get("completion")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                device,
            });
        }

        Ok(map_status(&db, peers))
    }

    /// Subscribe to the change feed from `resume`, or from now.
    ///
    /// The long poll, the cursor bookkeeping and the reconnection backoff all
    /// live inside the returned stream: a caller holds a `Stream<Item =
    /// Envelope>` and never learns that a daemon went away and came back.
    ///
    /// A gap in the event ids, a cursor from a previous daemon run, or a lost
    /// connection surfaces as `Change::Desynced` rather than as a silent hole.
    /// Callers re-scan the tree instead of trusting deltas they did not receive.
    async fn observe(&self, resume: Option<Cursor>) -> Result<Self::Changes, SyncError> {
        let (tx, rx) = tokio::sync::mpsc::channel(FEED_BUFFER);

        // The feed gets its own client: a long poll outlives any sane timeout on
        // the request/response client, and a hung daemon must still time out
        // rather than wedge the feed forever.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(EVENT_TIMEOUT_SECS + 30))
            .build()
            .unwrap_or_else(|_| self.http.clone());

        let feed = Feed {
            engine: SyncthingEngine {
                base_url: self.base_url.clone(),
                api_key: self.api_key.clone(),
                http,
            },
            tx,
        };
        let resume = resume.and_then(|c| c.0.parse::<u64>().ok());
        tokio::spawn(feed.run(resume));

        Ok(Box::pin(ChangeFeed { rx }))
    }

    /// Archived versions the daemon holds for one path.
    ///
    /// This and [`restore`](Self::restore) are the real mitigation behind
    /// Roles-as-policy: nothing stops a Member deleting bytes their Device
    /// already holds, so the answer is recovery rather than permission. Every
    /// *other* Device kept the previous versions, because versioning archives
    /// remote-originated changes and deletes.
    async fn versions(&self, circle: &CircleId, path: &RelPath) -> Result<Vec<Version>, SyncError> {
        match self
            .get(&format!("/rest/folder/versions?folder={}", escape(&circle.0)))
            .await
        {
            Ok(v) => Ok(map_versions(&v, path)),
            // A Circle whose folder carries no versioner holds no archived
            // versions — a true answer to the question asked. That the recipe is
            // missing is a `kith doctor` finding, not a failure of this call.
            Err(SyncError::Engine(msg)) if msg.contains("no versioner") => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Put one archived version back.
    ///
    /// The restored file replicates to the whole Circle as an ordinary new
    /// version, including onto the Device that did the damage. That is the
    /// honest answer to "what if someone deletes everything": not prevention —
    /// restoration, on any surviving Member's say-so.
    async fn restore(
        &self,
        circle: &CircleId,
        path: &RelPath,
        version: &Version,
    ) -> Result<(), SyncError> {
        // The archived timestamp must go back exactly as it was listed: the
        // daemon matches it against the timestamp it wrote into the archived
        // file's name, and a timestamp rendered in another zone matches nothing.
        let mut body = serde_json::Map::new();
        body.insert(path.0.clone(), Value::String(version.archived_at.clone()));

        let outcome = self
            .post(
                &format!("/rest/folder/versions?folder={}", escape(&circle.0)),
                &Value::Object(body),
            )
            .await?;

        // The daemon answers 200 whatever happened: an empty object means the
        // bytes are back, anything else is a per-path reason. Reporting it is
        // the difference between a recovery and a Person believing in one.
        if let Some(reason) = restore_failure(&outcome) {
            return Err(SyncError::Engine(reason));
        }
        Ok(())
    }
}

// ── the change feed ──────────────────────────────────────────────────────

/// The stream handed back by [`SyncEngine::observe`]. Nothing but a receiver:
/// all the work happens in [`Feed::run`] on its own task, so a caller that stops
/// polling applies backpressure instead of losing events.
struct ChangeFeed {
    rx: tokio::sync::mpsc::Receiver<Envelope>,
}

impl Stream for ChangeFeed {
    type Item = Envelope;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Envelope>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

/// Cursor bookkeeping for the change feed, kept out of the polling loop so that
/// losing continuity can be tested without a daemon anywhere near it.
#[derive(Debug, Default)]
struct Cursors {
    last: Option<u64>,
}

impl Cursors {
    fn new(resume: Option<u64>) -> Self {
        Self { last: resume }
    }

    /// Record an event id, answering whether continuity was lost before it.
    ///
    /// Event ids are contiguous within a subscription, so a jump means the
    /// daemon's ring buffer overflowed while kith was away and the events in
    /// between are gone for good.
    fn advance(&mut self, id: u64) -> bool {
        let gap = matches!(self.last, Some(last) if id > last + 1);
        self.last = Some(id);
        gap
    }

    /// Whether the daemon's own newest event id is behind our cursor.
    ///
    /// Event ids restart with the daemon, so a tip behind the cursor means the
    /// daemon was restarted under us and our cursor names an event this run will
    /// never emit. Left unnoticed it is the worst failure the feed has: the long
    /// poll would answer with an empty batch forever and the UI would look calm.
    fn stale_against(&self, tip: u64) -> bool {
        matches!(self.last, Some(last) if tip < last)
    }

    fn resume_from(&self) -> u64 {
        self.last.unwrap_or(0)
    }
}

struct Feed {
    engine: SyncthingEngine,
    tx: tokio::sync::mpsc::Sender<Envelope>,
}

impl Feed {
    async fn run(self, resume: Option<u64>) {
        let mut cursors = Cursors::new(resume);
        // A resumed cursor has to be proved to belong to this daemon run before
        // it is trusted; a fresh subscription just needs a place to start.
        let mut verify = true;
        let mut reload_circles = true;
        let mut circles_of: HashMap<String, Vec<CircleId>> = HashMap::new();
        let mut backoff = BACKOFF_MIN;

        loop {
            // Which Circles a Device belongs to. Presence is per Circle in the
            // seam and per Device on the wire, so the feed has to hold the
            // mapping itself.
            if reload_circles {
                match self.engine.get("/rest/config/folders").await {
                    Ok(v) => {
                        circles_of = circle_membership(&v);
                        reload_circles = false;
                    }
                    Err(SyncError::Unauthorized) => return,
                    Err(_) => {
                        if !self.wait(&mut backoff).await {
                            return;
                        }
                        continue;
                    }
                }
            }

            if verify {
                match self.tip().await {
                    Ok(tip) => {
                        if cursors.stale_against(tip) {
                            if !self.emit(tip, Change::Desynced).await {
                                return;
                            }
                            cursors = Cursors::new(Some(tip));
                        } else if cursors.last.is_none() {
                            // `resume: None` means "from now", not "replay the
                            // daemon's whole ring buffer".
                            cursors = Cursors::new(Some(tip));
                        }
                        verify = false;
                    }
                    Err(SyncError::Unauthorized) => return,
                    Err(_) => {
                        if !self.wait(&mut backoff).await {
                            return;
                        }
                        continue;
                    }
                }
            }

            let batch = match self.poll(cursors.resume_from()).await {
                Ok(batch) => batch,
                // Credentials are never repaired here and never guessed. The
                // feed ends; `health()` is where the Person is told why.
                Err(SyncError::Unauthorized) => return,
                Err(_) => {
                    // A dropped long poll is also how a daemon restart looks, so
                    // the cursor is re-proved before it is used again. The retry
                    // is against the events endpoint itself rather than the
                    // unauthenticated health probe ADR-0002 §5 describes: it
                    // fails identically when the daemon is absent, and it costs
                    // one request instead of two.
                    verify = true;
                    reload_circles = true;
                    if !self.wait(&mut backoff).await {
                        return;
                    }
                    continue;
                }
            };
            backoff = BACKOFF_MIN;

            for event in batch {
                let Some(id) = event.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if cursors.advance(id) && !self.emit(id, Change::Desynced).await {
                    return;
                }
                // The daemon hands its whole folder list to every ConfigSaved,
                // so somebody changing config under us costs no extra request.
                if event.get("type").and_then(Value::as_str) == Some("ConfigSaved") {
                    if let Some(folders) = event.pointer("/data/folders") {
                        circles_of = circle_membership(folders);
                    } else {
                        reload_circles = true;
                    }
                }
                for change in map_event(&event, &circles_of) {
                    if !self.emit(id, change).await {
                        return;
                    }
                }
            }
        }
    }

    async fn emit(&self, cursor: u64, change: Change) -> bool {
        self.tx
            .send(Envelope {
                cursor: Cursor(cursor.to_string()),
                change,
            })
            .await
            .is_ok()
    }

    /// The newest event id the daemon holds for this subscription.
    ///
    /// A subscription the daemon has not seen before starts empty and answers
    /// zero — which is exactly right, because a cursor from before a restart is
    /// ahead of it and gets caught.
    async fn tip(&self) -> Result<u64, SyncError> {
        let v = self
            .engine
            .get(&format!(
                "/rest/events?since=0&limit=1&timeout=0&events={EVENT_FILTER}"
            ))
            .await?;
        Ok(v.as_array()
            .and_then(|events| events.last())
            .and_then(|e| e.get("id"))
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }

    async fn poll(&self, since: u64) -> Result<Vec<Value>, SyncError> {
        let v = self
            .engine
            .get(&format!(
                "/rest/events?since={since}&timeout={EVENT_TIMEOUT_SECS}&events={EVENT_FILTER}"
            ))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// Sleep out one backoff step. Answers `false` once nobody is listening.
    async fn wait(&self, backoff: &mut Duration) -> bool {
        tokio::time::sleep(jitter(*backoff)).await;
        *backoff = (*backoff * 2).min(BACKOFF_MAX);
        !self.tx.is_closed()
    }
}

/// Half the delay plus a random half, so two Devices that lose the same daemon
/// do not spend the next minute knocking in lockstep.
fn jitter(base: Duration) -> Duration {
    let half = base.as_millis() as u64 / 2;
    if half == 0 {
        return base;
    }
    let noise = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    Duration::from_millis(half + noise % half)
}

// ── the mapping layer ────────────────────────────────────────────────────
//
// Free functions over `serde_json::Value` on purpose: every one of them is the
// place a Syncthing payload becomes a kith value, and every one of them is
// testable from a sample payload with no daemon in the room.

/// `/rest/config/folders` → the Circles this engine replicates.
///
/// **Not every replicated space is a Circle.** A Person runs one daemon for
/// everything they sync, and their photo archive is not a Circle just because
/// kith can see it: offering it as one would put an unrelated directory in the
/// switcher, make `kith add` ambiguous, and — worst — let `kith invite` hand a
/// stranger a code to a folder nobody meant to share. ADR-0002 §2 fixes the two
/// marks that make a folder kith's: the `kith-` id kith mints at creation, and,
/// for a space adopted in place under §7 (which keeps its original id), the
/// `.kith/` directory kith wrote into the tree. Anything carrying neither belongs
/// to some other program and is left alone.
fn map_folders(v: &Value) -> Vec<CircleRef> {
    v.as_array()
        .map(|folders| {
            folders
                .iter()
                .filter_map(|f| {
                    let id = f.get("id")?.as_str()?.to_string();
                    let root = PathBuf::from(f.get("path")?.as_str()?);
                    if !id.starts_with(CIRCLE_ID_PREFIX) && !root.join(".kith").is_dir() {
                        return None;
                    }
                    Some(CircleRef {
                        id: CircleId(id),
                        name: f
                            .get("label")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        root,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `/rest/cluster/pending/devices` → Devices currently knocking.
///
/// The field names are read in both spellings the daemon has used: they were
/// lower-case, went upper-case for part of the 1.29 series, and came back. This
/// is the churn the seam exists to absorb, and absorbing it costs three lines.
fn map_pending_devices(v: &Value) -> Vec<JoinRequest> {
    v.as_object()
        .map(|pending| {
            pending
                .iter()
                .map(|(device, entry)| JoinRequest {
                    device: DeviceId(device.clone()),
                    name: either(entry, "name", "Name").unwrap_or_default(),
                    seen_at: either(entry, "time", "Time").unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn map_pending_folders(v: &Value) -> Vec<CircleOffer> {
    let Some(pending) = v.as_object() else {
        return Vec::new();
    };
    pending
        .iter()
        .flat_map(|(circle, entry)| {
            entry
                .get("offeredBy")
                .and_then(Value::as_object)
                .map(|offers| {
                    offers
                        .iter()
                        .map(|(device, offer)| CircleOffer {
                            circle: CircleId(circle.clone()),
                            from: DeviceId(device.clone()),
                            label: either(offer, "label", "Label").unwrap_or_default(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// The label the offering Device gave a Circle it is offering back.
fn offered_label(pending: &Value, circle: &CircleId, from: &DeviceId) -> Option<String> {
    pending
        .pointer(&format!("/{}/offeredBy/{}", pointer(&circle.0), pointer(&from.0)))
        .and_then(|o| either(o, "label", "Label"))
        .filter(|label| !label.is_empty())
}

/// A Circle's folder plus the daemon's device list and connection table → the
/// peers sharing it. This Device is never among them.
fn map_peer_devices(
    folder: &Value,
    entries: &Value,
    connections: &Value,
    me: &DeviceId,
) -> Vec<PeerDevice> {
    let by_id: HashMap<&str, &Value> = entries
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|d| Some((d.get("deviceID")?.as_str()?, d)))
                .collect()
        })
        .unwrap_or_default();

    shared_devices(folder)
        .into_iter()
        .filter(|device| device != me)
        .map(|device| {
            let entry = by_id.get(device.0.as_str());
            PeerDevice {
                name: entry
                    .and_then(|e| e.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                introducer: entry
                    .and_then(|e| e.get("introducer"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                connected: connections
                    .pointer(&format!("/connections/{}/connected", pointer(&device.0)))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                device,
            }
        })
        .collect()
}

/// `/rest/db/status` plus per-peer completion → one Circle's sync state.
fn map_status(db: &Value, peers: Vec<PeerCompletion>) -> CircleStatus {
    let number = |key: &str| db.get(key).and_then(Value::as_u64).unwrap_or(0);
    CircleStatus {
        state: db
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        // The Circle's Items as the whole Circle sees them, not just the ones
        // that have landed here — a Member who has synced nothing is still
        // looking at a Circle with things in it.
        items: number("globalFiles"),
        bytes_needed: number("needBytes"),
        peers,
    }
}

/// `/rest/folder/versions` → the archived versions of one path.
///
/// `archived_at` carries the daemon's own rendering of the timestamp verbatim,
/// because that exact string is what [`SyncEngine::restore`] has to hand back.
fn map_versions(v: &Value, path: &RelPath) -> Vec<Version> {
    v.get(&path.0)
        .and_then(Value::as_array)
        .map(|versions| {
            versions
                .iter()
                .filter_map(|entry| {
                    Some(Version {
                        archived_at: either(entry, "versionTime", "VersionTime")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The reason a restore did not happen, if it did not happen.
fn restore_failure(outcome: &Value) -> Option<String> {
    outcome
        .as_object()?
        .values()
        .filter_map(Value::as_str)
        .find(|reason| !reason.is_empty())
        .map(str::to_string)
}

/// One engine event, in whatever `Change`s the seam can carry.
///
/// The seam's `Change` has four variants and the daemon has dozens of event
/// types, so this is a narrowing and says so out loud. `StateChanged`,
/// `FolderCompletion`, `FolderErrors` and `PendingFoldersChanged` have no
/// variant to land in: scanning state and per-peer progress are read from
/// [`SyncEngine::status`] instead, and a Circle offered back to a joiner is read
/// from the daemon's pending folders when phase 2 runs.
fn map_event(event: &Value, circles_of: &HashMap<String, Vec<CircleId>>) -> Vec<Change> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
    let data = event.get("data").unwrap_or(&Value::Null);
    let circle = || {
        data.get("folder")
            .and_then(Value::as_str)
            .map(|f| CircleId(f.to_string()))
    };

    match kind {
        // Bytes landed at a path, and the daemon named the path.
        "ItemFinished" => match (circle(), data.get("item").and_then(Value::as_str)) {
            (Some(circle), Some(item)) => vec![Change::Path {
                circle,
                path: RelPath(item.to_string()),
            }],
            _ => Vec::new(),
        },
        "LocalIndexUpdated" => {
            let Some(circle) = circle() else {
                return Vec::new();
            };
            match data.get("filenames").and_then(Value::as_array) {
                Some(names) if !names.is_empty() => names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|name| Change::Path {
                        circle: circle.clone(),
                        path: RelPath(name.to_string()),
                    })
                    .collect(),
                _ => vec![circle_wide(circle)],
            }
        }
        // Something in the Circle changed and the daemon did not say what.
        "RemoteIndexUpdated" | "FolderSummary" => circle().map(circle_wide).into_iter().collect(),
        "DeviceConnected" | "DeviceDisconnected" => {
            let Some(device) = data.get("id").and_then(Value::as_str) else {
                return Vec::new();
            };
            let connected = kind == "DeviceConnected";
            // Presence is per Circle in the seam and per Device on the wire. A
            // Device in no Circle of ours is not a peer and says nothing.
            circles_of
                .get(device)
                .map(|circles| {
                    circles
                        .iter()
                        .map(|circle| Change::Presence {
                            circle: circle.clone(),
                            device: DeviceId(device.to_string()),
                            connected,
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        "PendingDevicesChanged" => data
            .get("added")
            .and_then(Value::as_array)
            .map(|added| {
                added
                    .iter()
                    .filter_map(|d| either(d, "deviceID", "DeviceID"))
                    .map(|device| Change::Knock {
                        device: DeviceId(device),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A change the engine reported without naming a path. The Circle root stands
/// for "something in here moved; re-read it" — the seam has no other carrier,
/// and a caller that re-reads is never wrong, only occasionally early.
fn circle_wide(circle: CircleId) -> Change {
    Change::Path {
        circle,
        path: RelPath(String::new()),
    }
}

/// Device id → the Circles it shares, for attributing presence.
fn circle_membership(folders: &Value) -> HashMap<String, Vec<CircleId>> {
    let mut out: HashMap<String, Vec<CircleId>> = HashMap::new();
    for folder in folders.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(id) = folder.get("id").and_then(Value::as_str) else {
            continue;
        };
        for device in shared_devices(folder) {
            out.entry(device.0).or_default().push(CircleId(id.to_string()));
        }
    }
    out
}

/// The Devices a folder is shared with, in config order.
fn shared_devices(folder: &Value) -> Vec<DeviceId> {
    folder
        .get("devices")
        .and_then(Value::as_array)
        .map(|devices| {
            devices
                .iter()
                .filter_map(|d| Some(DeviceId(d.get("deviceID")?.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

// ── writing the daemon's own shapes ──────────────────────────────────────

/// ADR-0002 §2's folder recipe, imposed on a folder object without disturbing
/// anything else in it.
fn apply_recipe(folder: &mut Value, id: &CircleId, name: &str, root: &Path) {
    folder["id"] = json!(id.0);
    // The label is the Circle's name and may change; the id never does and is
    // never derived from it.
    folder["label"] = json!(name);
    folder["path"] = json!(root.to_string_lossy());
    // Every Member contributes — that is the wedge. Curator topologies deferred.
    folder["type"] = json!("sendreceive");
    // Imports and Actions show up without anybody asking for a rescan.
    folder["fsWatcherEnabled"] = json!(true);
    folder["maxConflicts"] = json!(10);
    // The recovery net, and the only enforcement behind Roles-as-policy: five
    // versions, thirty days, on every Device but the one that made the change.
    folder["versioning"] = json!({
        "type": "simple",
        "params": { "keep": "5", "cleanoutDays": "30" },
        "cleanupIntervalS": 3600,
        "fsPath": "",
        "fsType": "basic",
    });
    folder["paused"] = json!(false);
}

/// A folder's share entry for one Device.
fn shared_with(device: &DeviceId) -> Value {
    json!({ "deviceID": device.0, "introducedBy": "", "encryptionPassword": "" })
}

/// A device entry for a Device kith is admitting or knocking at.
///
/// `autoAcceptFolders` is written explicitly rather than left to the daemon's
/// default, which may well be `true`: wp-sync set that default globally, and
/// inheriting it would silently accept folders from this Device forever.
fn new_device_entry(device: &DeviceId, name: &str) -> Value {
    json!({
        "deviceID": device.0,
        "name": name,
        // Discovery handles addressing; introduction does not propagate address
        // changes anyway.
        "addresses": ["dynamic"],
        "introducer": false,
        "skipIntroductionRemovals": false,
        "autoAcceptFolders": false,
        "paused": false,
    })
}

/// `kith-` plus eight random base32 characters.
///
/// Taken from the random tail of a ULID, which is Crockford base32 already —
/// the id is a handle, never a name, and nothing may be read back out of it.
fn mint_circle_id() -> String {
    let ulid = ulid::Ulid::generate().to_string();
    let tail = &ulid[ulid.len().saturating_sub(CIRCLE_ID_ENTROPY)..];
    format!("{CIRCLE_ID_PREFIX}{tail}")
}

// ── small helpers ────────────────────────────────────────────────────────

/// A field under either of two spellings. The daemon has renamed fields between
/// releases; the seam is where that stops mattering.
fn either(value: &Value, first: &str, second: &str) -> Option<String> {
    value
        .get(first)
        .or_else(|| value.get(second))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Percent-encode a path segment or query value. Folder ids on adopted installs
/// are whatever the Person's previous client chose, so they are escaped rather
/// than assumed to be URL-safe.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Escape a key for a JSON pointer, where `~` and `/` are the two reserved
/// characters. Device ids contain neither; folder ids on adopted installs might.
fn pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Pull a single-line XML element's text without taking an XML dependency.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let value = xml[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The GUI listen address, which is also the REST address.
fn extract_gui_address(xml: &str) -> Option<String> {
    let gui = xml.find("<gui")?;
    extract_tag(&xml[gui..], "address")
}

/// The version floor, exposed for `kith doctor`.
pub fn version_floor() -> &'static str {
    VERSION_FLOOR
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        <configuration version="37">
          <gui enabled="true" tls="false">
            <address>127.0.0.1:8384</address>
            <apikey>abc123XYZ</apikey>
          </gui>
        </configuration>"#;

    /// Sample payloads are the daemon's own, captured from Syncthing v2.1.2 and
    /// trimmed to the fields the seam reads. Nothing here needs a daemon: the
    /// mapping is the part that breaks when the REST surface churns, so the
    /// mapping is the part that is pinned down.
    const ME: &str = "M7KZXLB-HFVBLRE-JB7FCKY-RYD72WT-QEQ4347-Y44X3BX-OQZVGRM-LDNS2QO";
    const PEER: &str = "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2";
    const STRANGER: &str = "LIZXMUU-KBDARMK-77NWE7L-6GO6LRW-CKU2VOE-Q6CVPAW-I4JOBXH-B5AAAQU";

    const FOLDERS: &str = r#"[
      { "id": "kith-7QM4XKC2", "label": "Wallpapers", "path": "/home/ana/Pictures/Circle",
        "type": "sendreceive", "maxConflicts": 10,
        "devices": [
          { "deviceID": "M7KZXLB-HFVBLRE-JB7FCKY-RYD72WT-QEQ4347-Y44X3BX-OQZVGRM-LDNS2QO",
            "introducedBy": "", "encryptionPassword": "" },
          { "deviceID": "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2",
            "introducedBy": "", "encryptionPassword": "" }
        ] },
      { "id": "wallpapers", "label": "", "path": "/home/ana/Wallpapers",
        "devices": [
          { "deviceID": "M7KZXLB-HFVBLRE-JB7FCKY-RYD72WT-QEQ4347-Y44X3BX-OQZVGRM-LDNS2QO",
            "introducedBy": "", "encryptionPassword": "" }
        ] }
    ]"#;

    const DEVICE_ENTRIES: &str = r#"[
      { "deviceID": "M7KZXLB-HFVBLRE-JB7FCKY-RYD72WT-QEQ4347-Y44X3BX-OQZVGRM-LDNS2QO",
        "name": "zero", "introducer": false, "autoAcceptFolders": false },
      { "deviceID": "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2",
        "name": "Bo's phone", "introducer": true, "autoAcceptFolders": false }
    ]"#;

    const CONNECTIONS: &str = r#"{
      "connections": {
        "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2": {
          "connected": true, "paused": false, "clientVersion": "v2.0.11",
          "address": "192.168.1.40:22000", "type": "tcp-client" }
      },
      "total": { "at": "2026-08-07T21:42:25+05:30" }
    }"#;

    const PENDING_DEVICES: &str = r#"{
      "LIZXMUU-KBDARMK-77NWE7L-6GO6LRW-CKU2VOE-Q6CVPAW-I4JOBXH-B5AAAQU": {
        "time": "2026-08-07T16:18:57Z", "name": "Bo's laptop", "address": "127.0.0.1:22102" }
    }"#;

    const PENDING_FOLDERS: &str = r#"{
      "kith-OFFERED1": {
        "offeredBy": {
          "LIZXMUU-KBDARMK-77NWE7L-6GO6LRW-CKU2VOE-Q6CVPAW-I4JOBXH-B5AAAQU": {
            "time": "2026-08-07T16:19:38Z", "label": "Wallpapers",
            "receiveEncrypted": false, "remoteEncrypted": false }
        } }
    }"#;

    const DB_STATUS: &str = r#"{
      "globalFiles": 148, "globalBytes": 505656060, "globalTotalItems": 214,
      "localFiles": 148, "needFiles": 0, "needBytes": 0, "needTotalItems": 0,
      "state": "idle", "stateChanged": "2026-08-07T21:41:40+05:30", "error": ""
    }"#;

    const VERSIONS: &str = r#"{
      "hello.txt": [
        { "versionTime": "2025-01-01T12:00:00+05:30", "modTime": "2026-08-07T21:47:37+05:30", "size": 6 },
        { "versionTime": "2026-01-01T12:00:00+05:30", "modTime": "2026-08-07T21:47:37+05:30", "size": 10 }
      ],
      "sub/pic.png": [
        { "versionTime": "2026-01-02T13:00:00+05:30", "modTime": "2026-08-07T21:47:37+05:30", "size": 7 }
      ]
    }"#;

    /// One long poll's worth of events, in the order the daemon emitted them.
    const EVENT_BATCH: &str = r#"[
      { "id": 66, "globalID": 67, "type": "ItemFinished",
        "data": { "action": "update", "error": null, "folder": "kith-7QM4XKC2",
                  "item": "sunset.png", "type": "file" } },
      { "id": 67, "globalID": 68, "type": "LocalIndexUpdated",
        "data": { "filenames": ["sunset.png", "dawn.png"], "folder": "kith-7QM4XKC2",
                  "items": 2, "sequence": 2, "version": 2 } },
      { "id": 68, "globalID": 69, "type": "RemoteIndexUpdated",
        "data": { "device": "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2",
                  "folder": "kith-7QM4XKC2", "items": 1, "sequence": 1, "version": 1 } },
      { "id": 69, "globalID": 70, "type": "DeviceConnected",
        "data": { "addr": "192.168.1.40:22000", "clientName": "syncthing",
                  "clientVersion": "v2.1.2", "deviceName": "Bo's phone",
                  "id": "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2",
                  "type": "tcp-server" } },
      { "id": 70, "globalID": 71, "type": "DeviceDisconnected",
        "data": { "error": "read timeout",
                  "id": "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2" } },
      { "id": 71, "globalID": 72, "type": "PendingDevicesChanged",
        "data": { "added": [ { "address": "127.0.0.1:22102",
                               "deviceID": "LIZXMUU-KBDARMK-77NWE7L-6GO6LRW-CKU2VOE-Q6CVPAW-I4JOBXH-B5AAAQU",
                               "name": "Bo's laptop" } ] } },
      { "id": 72, "globalID": 73, "type": "StateChanged",
        "data": { "duration": 0.9, "folder": "kith-7QM4XKC2", "from": "scanning", "to": "idle" } }
    ]"#;

    fn json_of(text: &str) -> Value {
        serde_json::from_str(text).expect("the sample payload should be JSON")
    }

    #[test]
    fn discovers_address_and_key_from_daemon_config() {
        assert_eq!(extract_tag(SAMPLE, "apikey").as_deref(), Some("abc123XYZ"));
        assert_eq!(extract_gui_address(SAMPLE).as_deref(), Some("127.0.0.1:8384"));
    }

    #[test]
    fn missing_key_is_absent_rather_than_empty() {
        assert_eq!(extract_tag("<gui></gui>", "apikey"), None);
    }

    #[test]
    fn reserved_paths_cover_the_daemons_own_artefacts() {
        // These spellings must never be duplicated above the seam.
        assert!(RESERVED.contains(&".stfolder"));
        assert!(RESERVED.contains(&"*.sync-conflict-*"));
    }

    #[test]
    fn a_folder_list_becomes_circles() {
        // The adopted wp-sync folder keeps its original id (ADR-0002 §7), so the
        // only thing that marks it as kith's is the `.kith/` directory adoption
        // wrote into its tree. That has to be a real directory for the mapping to
        // find it, which is what this fixture builds.
        let adopted = std::env::temp_dir().join(format!("kith-adopted-{}", std::process::id()));
        std::fs::create_dir_all(adopted.join(".kith")).unwrap();
        let folders = FOLDERS.replace("/home/ana/Wallpapers", &adopted.display().to_string());

        let circles = map_folders(&json_of(&folders));
        assert_eq!(circles.len(), 2);
        assert_eq!(circles[0].id, CircleId("kith-7QM4XKC2".into()));
        assert_eq!(circles[0].name, "Wallpapers");
        assert_eq!(circles[0].root, PathBuf::from("/home/ana/Pictures/Circle"));
        // An adopted wp-sync folder has no label, and an unnamed Circle is a
        // real state rather than a reason to skip it.
        assert_eq!(circles[1].id, CircleId("wallpapers".into()));
        assert_eq!(circles[1].name, "");

        let _ = std::fs::remove_dir_all(&adopted);
    }

    /// A Person runs one daemon for everything they sync. A folder kith neither
    /// created nor adopted is somebody else's, and calling it a Circle would put
    /// it in the switcher, make `kith add` ambiguous and — worst — let
    /// `kith invite` hand out a code to a directory nobody meant to share.
    #[test]
    fn a_folder_kith_never_touched_is_not_a_circle() {
        let circles = map_folders(&json_of(FOLDERS));
        assert_eq!(circles.len(), 1, "only the kith- folder; the adopted one has no .kith here");
        assert_eq!(circles[0].id, CircleId("kith-7QM4XKC2".into()));
    }

    #[test]
    fn pending_devices_become_join_requests() {
        let knocking = map_pending_devices(&json_of(PENDING_DEVICES));
        assert_eq!(knocking.len(), 1);
        assert_eq!(knocking[0].device, DeviceId(STRANGER.into()));
        assert_eq!(knocking[0].name, "Bo's laptop");
        assert_eq!(knocking[0].seen_at, "2026-08-07T16:18:57Z");
    }

    #[test]
    fn pending_devices_survive_the_daemon_renaming_its_own_fields() {
        // The 1.29 series shipped these keys capitalised. A Person on that build
        // still gets a name and a time, not two empty strings.
        let capitalised = json_of(
            r#"{ "LIZXMUU-KBDARMK-77NWE7L-6GO6LRW-CKU2VOE-Q6CVPAW-I4JOBXH-B5AAAQU":
                 { "Time": "2026-08-07T16:18:57Z", "Name": "Bo's laptop" } }"#,
        );
        let knocking = map_pending_devices(&capitalised);
        assert_eq!(knocking[0].name, "Bo's laptop");
        assert_eq!(knocking[0].seen_at, "2026-08-07T16:18:57Z");
    }

    #[test]
    fn an_offered_circle_carries_the_label_the_offering_device_gave_it() {
        let label = offered_label(
            &json_of(PENDING_FOLDERS),
            &CircleId("kith-OFFERED1".into()),
            &DeviceId(STRANGER.into()),
        );
        assert_eq!(label.as_deref(), Some("Wallpapers"));
        // An offer from a Device that never made one is absent, not empty.
        assert_eq!(
            offered_label(
                &json_of(PENDING_FOLDERS),
                &CircleId("kith-OFFERED1".into()),
                &DeviceId(PEER.into())
            ),
            None
        );
    }

    #[test]
    fn peers_carry_presence_and_this_device_is_never_among_them() {
        let folders = json_of(FOLDERS);
        let circle = &folders[0];
        let peers = map_peer_devices(
            circle,
            &json_of(DEVICE_ENTRIES),
            &json_of(CONNECTIONS),
            &DeviceId(ME.into()),
        );

        assert_eq!(peers.len(), 1, "the Circle has two Devices and one is ours");
        assert_eq!(peers[0].device, DeviceId(PEER.into()));
        assert_eq!(peers[0].name, "Bo's phone");
        assert!(peers[0].connected);
        assert!(peers[0].introducer, "a cross-check, never the answer");
        assert!(
            !peers.iter().any(|p| p.device == DeviceId(ME.into())),
            "devices() never returns self — which is why the introducer flag \
             cannot name the Steward's Device from the Steward's own Device"
        );
    }

    #[test]
    fn a_peer_the_daemon_has_no_connection_for_is_simply_not_present() {
        let folders = json_of(FOLDERS);
        let peers = map_peer_devices(
            &folders[0],
            &json_of(DEVICE_ENTRIES),
            &json_of(r#"{ "connections": {} }"#),
            &DeviceId(ME.into()),
        );
        assert!(!peers[0].connected);
    }

    #[test]
    fn a_circle_status_reads_the_whole_circle_not_just_what_landed_here() {
        let status = map_status(
            &json_of(DB_STATUS),
            vec![PeerCompletion {
                device: DeviceId(PEER.into()),
                percent: 42.5,
            }],
        );
        assert_eq!(status.state, "idle");
        assert_eq!(status.items, 148);
        assert_eq!(status.bytes_needed, 0);
        assert_eq!(status.peers[0].percent, 42.5);
    }

    #[test]
    fn an_event_batch_becomes_changes() {
        let circles_of = circle_membership(&json_of(FOLDERS));
        let batch = json_of(EVENT_BATCH);
        let changes: Vec<Change> = batch
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|event| map_event(event, &circles_of))
            .collect();

        let circle = CircleId("kith-7QM4XKC2".into());
        let peer = DeviceId(PEER.into());

        assert!(matches!(
            &changes[0],
            Change::Path { circle: c, path } if *c == circle && path.0 == "sunset.png"
        ));
        // One local index update, two named files, two changes.
        assert!(matches!(&changes[1], Change::Path { path, .. } if path.0 == "sunset.png"));
        assert!(matches!(&changes[2], Change::Path { path, .. } if path.0 == "dawn.png"));
        // A remote index update names no file: the Circle root stands for
        // "something moved in here, re-read it".
        assert!(matches!(
            &changes[3],
            Change::Path { circle: c, path } if *c == circle && path.0.is_empty()
        ));
        assert!(matches!(
            &changes[4],
            Change::Presence { circle: c, device, connected: true } if *c == circle && *device == peer
        ));
        assert!(matches!(
            &changes[5],
            Change::Presence { device, connected: false, .. } if *device == peer
        ));
        assert!(matches!(
            &changes[6],
            Change::Knock { device } if *device == DeviceId(STRANGER.into())
        ));
        assert_eq!(
            changes.len(),
            7,
            "StateChanged has no home in the seam's Change and is dropped, not invented"
        );
    }

    #[test]
    fn presence_is_reported_once_per_circle_the_device_shares() {
        // The same Device in two Circles is two Presence facts, because Presence
        // is per Circle above the seam and per Device on the wire.
        let two = json_of(&FOLDERS.replace("\"wallpapers\"", "\"kith-SECOND01\"").replace(
            r#"{ "deviceID": "M7KZXLB-HFVBLRE-JB7FCKY-RYD72WT-QEQ4347-Y44X3BX-OQZVGRM-LDNS2QO",
            "introducedBy": "", "encryptionPassword": "" }
        ] }
    ]"#,
            r#"{ "deviceID": "M7KZXLB-HFVBLRE-JB7FCKY-RYD72WT-QEQ4347-Y44X3BX-OQZVGRM-LDNS2QO",
            "introducedBy": "", "encryptionPassword": "" },
          { "deviceID": "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2",
            "introducedBy": "", "encryptionPassword": "" }
        ] }
    ]"#,
        ));
        let circles_of = circle_membership(&two);
        let connected = json_of(
            r#"{ "type": "DeviceConnected",
                 "data": { "id": "MKNSEL2-Z7BKMYM-EYGID6P-5HU44J5-AN5TPZ6-WNKU3JA-24PUCIK-632SIQ2" } }"#,
        );
        assert_eq!(map_event(&connected, &circles_of).len(), 2);
    }

    #[test]
    fn a_device_in_no_circle_of_ours_says_nothing() {
        let circles_of = circle_membership(&json_of(FOLDERS));
        let connected = json_of(
            r#"{ "type": "DeviceConnected",
                 "data": { "id": "LIZXMUU-KBDARMK-77NWE7L-6GO6LRW-CKU2VOE-Q6CVPAW-I4JOBXH-B5AAAQU" } }"#,
        );
        assert!(map_event(&connected, &circles_of).is_empty());
    }

    #[test]
    fn a_gap_in_the_event_ids_surfaces_as_desynced() {
        let mut cursors = Cursors::new(Some(66));
        assert!(!cursors.advance(67), "the next id is continuity");
        assert!(!cursors.advance(68));
        assert!(
            cursors.advance(412),
            "the daemon's buffer overflowed while kith was away — the events in \
             between are gone, and a caller must re-scan rather than trust deltas"
        );
        assert!(!cursors.advance(413), "and continuity resumes from there");
    }

    #[test]
    fn a_fresh_subscription_has_nothing_to_lose_continuity_with() {
        let mut cursors = Cursors::new(None);
        assert!(!cursors.advance(9_001));
        assert_eq!(cursors.resume_from(), 9_001);
    }

    #[test]
    fn a_cursor_from_a_previous_daemon_run_surfaces_as_desynced() {
        // Event ids restart with the daemon, so a tip behind our cursor means
        // our cursor names an event this run will never emit. Unnoticed, the
        // long poll would answer empty forever and the UI would look calm.
        let cursors = Cursors::new(Some(9_001));
        assert!(cursors.stale_against(12));
        assert!(!cursors.stale_against(9_001));
        assert!(!cursors.stale_against(9_002));
        // Nothing to be stale against before the first event.
        assert!(!Cursors::new(None).stale_against(0));
    }

    #[test]
    fn versions_are_listed_for_one_path_and_carry_the_daemons_own_timestamp() {
        let versions = map_versions(&json_of(VERSIONS), &RelPath("hello.txt".into()));
        assert_eq!(versions.len(), 2);
        // Verbatim: restore matches this string against the timestamp the daemon
        // wrote into the archived file's name, and the same instant rendered in
        // another zone matches nothing.
        assert_eq!(versions[0].archived_at, "2025-01-01T12:00:00+05:30");
        assert_eq!(
            map_versions(&json_of(VERSIONS), &RelPath("sub/pic.png".into())).len(),
            1
        );
        // A path the engine holds nothing for is an empty answer, not a failure.
        assert!(map_versions(&json_of(VERSIONS), &RelPath("gone.png".into())).is_empty());
    }

    #[test]
    fn a_restore_that_did_not_happen_is_never_reported_as_one() {
        assert_eq!(restore_failure(&json_of("{}")), None);
        assert_eq!(
            restore_failure(&json_of(
                r#"{ "hello.txt": "simple versioner: restore: version not found" }"#
            ))
            .as_deref(),
            Some("simple versioner: restore: version not found")
        );
    }

    #[test]
    fn the_folder_recipe_is_the_one_the_adr_fixed() {
        let mut folder = json_of(r#"{ "rescanIntervalS": 3600, "somethingKithNeverHeardOf": 7 }"#);
        apply_recipe(
            &mut folder,
            &CircleId("kith-7QM4XKC2".into()),
            "Wallpapers",
            Path::new("/home/ana/Pictures/Circle"),
        );

        assert_eq!(folder["id"], json!("kith-7QM4XKC2"));
        assert_eq!(folder["label"], json!("Wallpapers"));
        assert_eq!(folder["path"], json!("/home/ana/Pictures/Circle"));
        assert_eq!(folder["type"], json!("sendreceive"));
        assert_eq!(folder["fsWatcherEnabled"], json!(true));
        assert_eq!(folder["maxConflicts"], json!(10));
        assert_eq!(folder["versioning"]["type"], json!("simple"));
        assert_eq!(folder["versioning"]["params"]["keep"], json!("5"));
        assert_eq!(folder["versioning"]["params"]["cleanoutDays"], json!("30"));
        // Read-modify-write: what kith does not own survives untouched.
        assert_eq!(folder["somethingKithNeverHeardOf"], json!(7));
        assert_eq!(folder["rescanIntervalS"], json!(3600));
    }

    #[test]
    fn a_device_entry_kith_writes_never_accepts_circles_on_its_own() {
        // The daemon's own default for this may be `true` — wp-sync set it
        // globally — so kith writes the answer instead of inheriting it.
        let entry = new_device_entry(&DeviceId(PEER.into()), "Bo's phone");
        assert_eq!(entry["autoAcceptFolders"], json!(false));
        assert_eq!(entry["introducer"], json!(false));
        assert_eq!(entry["addresses"], json!(["dynamic"]));
    }

    #[test]
    fn a_circle_id_is_a_handle_and_never_a_name() {
        let a = mint_circle_id();
        let b = mint_circle_id();
        assert!(a.starts_with(CIRCLE_ID_PREFIX));
        assert_eq!(a.len(), CIRCLE_ID_PREFIX.len() + CIRCLE_ID_ENTROPY);
        assert_ne!(a, b, "nothing about a Circle's id is derived from anything");
        assert!(a[CIRCLE_ID_PREFIX.len()..].chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn ids_are_escaped_before_they_reach_a_url() {
        // An adopted folder's id is whatever the Person's previous client chose.
        assert_eq!(escape("kith-7QM4XKC2"), "kith-7QM4XKC2");
        assert_eq!(escape("holiday photos/2026"), "holiday%20photos%2F2026");
        assert_eq!(pointer("a/b~c"), "a~1b~0c");
    }
}
