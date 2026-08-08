//! The one production implementor of the Sync Engine seam, and the only module in
//! kith allowed to say "Syncthing".
//!
//! kith never launches, embeds or supervises the daemon. Every configuration write
//! is a read-modify-write of the daemon's own JSON, scoped to the folders kith
//! created or adopted and the device entries for Devices a Person admitted, so
//! fields kith has never heard of survive untouched.

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

/// Globs the daemon owns inside a Circle root.
const RESERVED: &[&str] = &[
    ".stfolder",
    ".stversions/**",
    ".stignore",
    ".stglobalignore",
    ".syncthing.*.tmp",
    "~syncthing~*.tmp",
    "*.sync-conflict-*",
];

const CIRCLE_ID_PREFIX: &str = "kith-";
const CIRCLE_ID_ENTROPY: usize = 8;

/// The per-Device scratch space, kept out of replication. `(?d)` so it cannot
/// block a directory delete on a Device that still holds thumbnails.
const LOCAL_SCRATCH_IGNORE: &str = "(?d).kith/local";

/// How long one long poll waits before the daemon answers with an empty batch.
const EVENT_TIMEOUT_SECS: u64 = 60;

/// The change feed's subscription. Filtered so the daemon's ring buffer fills with
/// events kith cares about; an overflow costs a rescan.
const EVENT_FILTER: &str = "ItemFinished,LocalIndexUpdated,RemoteIndexUpdated,StateChanged,\
FolderSummary,FolderCompletion,DeviceConnected,DeviceDisconnected,PendingDevicesChanged,\
PendingFoldersChanged,ConfigSaved,FolderErrors";

/// Reconnection backoff while the engine is unreachable.
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

    /// Find the running daemon's address and API key, never writing to its config.
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
        // Read the body whatever the status: the daemon puts its reason in there
        // as plain text.
        let text = resp
            .text()
            .await
            .map_err(|e| SyncError::Engine(e.to_string()))?;

        match status {
            // Config writes answer 200 with an empty body: a success, not a parse failure.
            200..=299 if text.trim().is_empty() => Ok(Value::Null),
            200..=299 => serde_json::from_str(&text).map_err(|e| SyncError::Engine(e.to_string())),
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

    /// One Circle's folder configuration as the daemon holds it, or `NotFound`.
    async fn folder(&self, circle: &CircleId) -> Result<Value, SyncError> {
        self.get(&format!("/rest/config/folders/{}", escape(&circle.0)))
            .await
    }

    /// Write a folder back after changing the keys kith owns; everything else in
    /// the object rides along untouched.
    async fn put_folder(&self, circle: &CircleId, folder: &Value) -> Result<(), SyncError> {
        self.put(&format!("/rest/config/folders/{}", escape(&circle.0)), folder)
            .await
            .map(|_| ())
    }

    /// Give a peer Device an entry in the daemon's device list if it has none.
    ///
    /// An existing entry is left exactly as it is: it may predate kith and its
    /// settings are the Person's.
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
    /// network where discovery does not. Added alongside `dynamic`, never replacing it.
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

    /// Build a Circle's folder object from the daemon's own folder defaults, then
    /// impose kith's recipe on top — the schema gains fields every release, and a
    /// hand-built object would zero every one kith has not heard of.
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
        Ok(EngineHealth { version })
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
    /// The creating Device becomes the introducer precisely by flagging nobody, so
    /// there is nothing to write for it here.
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
    /// Adding the entry *is* the knock: the two daemons connect and this Device
    /// appears in the Steward's pending list. There is no separate "request" call.
    async fn begin_join(&self, invite: &InviteTicket) -> Result<(), SyncError> {
        self.ensure_device(&invite.steward_device, "").await?;
        if let Some(address) = &invite.address {
            self.set_device_address(&invite.steward_device, address).await?;
        }
        // The Steward's Device is where the rest of the Circle's Devices are learned from.
        self.set_introducer(&invite.steward_device, true).await
    }

    /// Joiner, phase 2: the Circle was offered back; place it at `root`.
    ///
    /// There is no "accept" endpoint — accepting is adding the folder to this
    /// daemon's config with the offered id, which is why kith picks the root.
    async fn complete_join(&self, offer: &CircleOffer, root: &Path) -> Result<CircleRef, SyncError> {
        let me = self.local_device().await?;
        self.ensure_device(&offer.from, "").await?;

        // An offer reconstructed from the Invite carries no label; fall back to the
        // Circle id rather than to a guess at what the Circle might be called.
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

    /// Admit a knocking Device into a Circle: an entry for the Device if it has
    /// none, and the Device appended to this one folder's `devices` array.
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

    /// Remove a Device from a Circle: only the Circle's `devices` array is touched,
    /// which is enough for the de-introduction cascade.
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

    /// Stop replicating a Circle. Local bytes are kept: dropping the folder from
    /// the daemon's config touches not one file on disk.
    async fn leave(&self, circle: &CircleId) -> Result<(), SyncError> {
        // The daemon answers a delete of an unknown folder with 200. Ask first,
        // so leaving a Circle this engine never replicated says so.
        self.folder(circle).await?;
        self.delete(&format!("/rest/config/folders/{}", escape(&circle.0)))
            .await
            .map(|_| ())
    }

    /// Flag or unflag a peer Device as *this* Device's introducer. Device-scoped,
    /// not Circle-scoped: the daemon offers no folder-scoped introduction.
    async fn set_introducer(&self, device: &DeviceId, flag: bool) -> Result<(), SyncError> {
        let path = format!("/rest/config/devices/{}", escape(&device.0));
        let mut entry = self.get(&path).await?;
        if !entry.is_object() {
            return Err(SyncError::NotFound);
        }
        entry["introducer"] = json!(flag);
        // A daemon whose global default is `autoAcceptFolders: true` would start
        // taking folders from an introducer unasked.
        entry["autoAcceptFolders"] = json!(false);
        self.put(&path, &entry).await.map(|_| ())
    }

    /// Peer Devices sharing a Circle, with presence. Never returns this Device,
    /// which is why `PeerDevice::introducer` is a cross-check and not an answer.
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

    /// Subscribe to the change feed from `resume`, or from now; lost continuity
    /// surfaces as `Change::Desynced` rather than as a silent hole.
    async fn observe(&self, resume: Option<Cursor>) -> Result<Self::Changes, SyncError> {
        let (tx, rx) = tokio::sync::mpsc::channel(FEED_BUFFER);

        // The feed gets its own client: a long poll outlives any sane timeout on
        // the request/response client, but a hung daemon must still time out.
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
    async fn versions(&self, circle: &CircleId, path: &RelPath) -> Result<Vec<Version>, SyncError> {
        match self
            .get(&format!("/rest/folder/versions?folder={}", escape(&circle.0)))
            .await
        {
            Ok(v) => Ok(map_versions(&v, path)),
            // A folder with no versioner holds no archived versions — a true
            // answer, and a `kith doctor` finding rather than a failure here.
            Err(SyncError::Engine(msg)) if msg.contains("no versioner") => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Put one archived version back; it replicates to the whole Circle as an
    /// ordinary new version.
    async fn restore(
        &self,
        circle: &CircleId,
        path: &RelPath,
        version: &Version,
    ) -> Result<(), SyncError> {
        // The archived timestamp goes back exactly as listed: the daemon matches
        // it against the archived file's name, and another zone matches nothing.
        let mut body = serde_json::Map::new();
        body.insert(path.0.clone(), Value::String(version.archived_at.clone()));

        let outcome = self
            .post(
                &format!("/rest/folder/versions?folder={}", escape(&circle.0)),
                &Value::Object(body),
            )
            .await?;

        // The daemon answers 200 whatever happened: an empty object means the
        // bytes are back, anything else is a per-path reason.
        if let Some(reason) = restore_failure(&outcome) {
            return Err(SyncError::Engine(reason));
        }
        Ok(())
    }
}

/// The stream handed back by [`SyncEngine::observe`]. Nothing but a receiver, so a
/// caller that stops polling applies backpressure instead of losing events.
struct ChangeFeed {
    rx: tokio::sync::mpsc::Receiver<Envelope>,
}

impl Stream for ChangeFeed {
    type Item = Envelope;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Envelope>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

/// Cursor bookkeeping for the change feed, kept out of the polling loop so losing
/// continuity is testable without a daemon.
#[derive(Debug, Default)]
struct Cursors {
    last: Option<u64>,
}

impl Cursors {
    fn new(resume: Option<u64>) -> Self {
        Self { last: resume }
    }

    /// Record an event id, answering whether continuity was lost before it: ids are
    /// contiguous, so a jump means the daemon's ring buffer overflowed.
    fn advance(&mut self, id: u64) -> bool {
        let gap = matches!(self.last, Some(last) if id > last + 1);
        self.last = Some(id);
        gap
    }

    /// Whether the daemon's own newest event id is behind our cursor. Ids restart
    /// with the daemon, and unnoticed the long poll would answer `200 []` forever
    /// while the UI looked calm — which is why the tip is probed at all.
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
        // A resumed cursor must be proved to belong to this daemon run.
        let mut verify = true;
        let mut reload_circles = true;
        let mut circles_of: HashMap<String, Vec<CircleId>> = HashMap::new();
        let mut backoff = BACKOFF_MIN;

        loop {
            // Presence is per Circle in the seam and per Device on the wire, so the
            // feed holds the mapping itself.
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
                            // `resume: None` means "from now", not "replay everything".
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
                Err(SyncError::Unauthorized) => return,
                Err(_) => {
                    // A dropped long poll is also how a daemon restart looks, so
                    // the cursor is re-proved before it is used again.
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
                // ConfigSaved carries the whole folder list, so config changing
                // under us costs no extra request.
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

    /// The newest event id the daemon holds for this subscription; an unseen
    /// subscription answers zero, so a pre-restart cursor is caught.
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

/// Half the delay plus a random half, so two Devices that lose the same daemon do
/// not retry in lockstep.
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

/// `/rest/config/folders` → the Circles this engine replicates.
///
/// Not every replicated space is a Circle: only a `kith-` id or a `.kith/`
/// directory in the tree marks a folder as kith's. Anything else is left alone.
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

/// `/rest/cluster/pending/devices` → Devices currently knocking. Field names are
/// read in both spellings: part of the 1.29 series capitalised them.
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

/// A Circle's folder plus the daemon's device list and connection table → the peers
/// sharing it. This Device is never among them.
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
        // The whole Circle's Items, not just the ones that landed here.
        items: number("globalFiles"),
        bytes_needed: number("needBytes"),
        peers,
    }
}

/// `/rest/folder/versions` → the archived versions of one path. `archived_at` is the
/// daemon's own rendering verbatim, because that exact string goes back to restore.
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

/// One engine event, in whatever `Change`s the seam can carry. A deliberate
/// narrowing: event types with no variant to land in are dropped, not invented.
fn map_event(event: &Value, circles_of: &HashMap<String, Vec<CircleId>>) -> Vec<Change> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
    let data = event.get("data").unwrap_or(&Value::Null);
    let circle = || {
        data.get("folder")
            .and_then(Value::as_str)
            .map(|f| CircleId(f.to_string()))
    };

    match kind {
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
            // A Device in no Circle of ours is not a peer and says nothing.
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

/// A change the engine reported without naming a path: the Circle root stands for
/// "something in here moved; re-read it".
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

/// kith's folder recipe, imposed on a folder object without disturbing anything
/// else in it.
fn apply_recipe(folder: &mut Value, id: &CircleId, name: &str, root: &Path) {
    folder["id"] = json!(id.0);
    folder["label"] = json!(name);
    folder["path"] = json!(root.to_string_lossy());
    folder["type"] = json!("sendreceive");
    // Imports and Actions show up without anybody asking for a rescan.
    folder["fsWatcherEnabled"] = json!(true);
    folder["maxConflicts"] = json!(10);
    // The recovery net behind Roles-as-policy.
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

/// A device entry for a Device kith is admitting or knocking at. `autoAcceptFolders`
/// is written explicitly because the daemon's own default may well be `true`.
fn new_device_entry(device: &DeviceId, name: &str) -> Value {
    json!({
        "deviceID": device.0,
        "name": name,
        "addresses": ["dynamic"],
        "introducer": false,
        "skipIntroductionRemovals": false,
        "autoAcceptFolders": false,
        "paused": false,
    })
}

/// `kith-` plus eight random base32 characters, never derived from the Circle's
/// name: the id is a handle and nothing may be read back out of it.
fn mint_circle_id() -> String {
    let ulid = ulid::Ulid::generate().to_string();
    let tail = &ulid[ulid.len().saturating_sub(CIRCLE_ID_ENTROPY)..];
    format!("{CIRCLE_ID_PREFIX}{tail}")
}

/// A field under either of two spellings, because the daemon has renamed fields
/// between releases.
fn either(value: &Value, first: &str, second: &str) -> Option<String> {
    value
        .get(first)
        .or_else(|| value.get(second))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Percent-encode a path segment or query value: an adopted folder's id is whatever
/// the Person's previous client chose, never assumed URL-safe.
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

/// Escape a key for a JSON pointer, where `~` and `/` are reserved.
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

    // Sample payloads are the daemon's own, captured from Syncthing v2.1.2.
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
        assert!(RESERVED.contains(&".stfolder"));
        assert!(RESERVED.contains(&"*.sync-conflict-*"));
    }

    #[test]
    fn a_folder_list_becomes_circles() {
        // An adopted folder keeps its original id, so only a real `.kith/`
        // directory in its tree marks it as kith's.
        let adopted = std::env::temp_dir().join(format!("kith-adopted-{}", std::process::id()));
        std::fs::create_dir_all(adopted.join(".kith")).unwrap();
        let folders = FOLDERS.replace("/home/ana/Wallpapers", &adopted.display().to_string());

        let circles = map_folders(&json_of(&folders));
        assert_eq!(circles.len(), 2);
        assert_eq!(circles[0].id, CircleId("kith-7QM4XKC2".into()));
        assert_eq!(circles[0].name, "Wallpapers");
        assert_eq!(circles[0].root, PathBuf::from("/home/ana/Pictures/Circle"));
        // An unnamed Circle is a real state, not a reason to skip it.
        assert_eq!(circles[1].id, CircleId("wallpapers".into()));
        assert_eq!(circles[1].name, "");

        let _ = std::fs::remove_dir_all(&adopted);
    }

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
        // The 1.29 series shipped these keys capitalised.
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
        // A remote index update names no file, so the Circle root stands in.
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
        // Verbatim: restore matches this string against the archived file's name.
        assert_eq!(versions[0].archived_at, "2025-01-01T12:00:00+05:30");
        assert_eq!(
            map_versions(&json_of(VERSIONS), &RelPath("sub/pic.png".into())).len(),
            1
        );
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
        assert_eq!(escape("kith-7QM4XKC2"), "kith-7QM4XKC2");
        assert_eq!(escape("holiday photos/2026"), "holiday%20photos%2F2026");
        assert_eq!(pointer("a/b~c"), "a~1b~0c");
    }
}
