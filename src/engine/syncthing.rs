//! The one production implementor of the Sync Engine seam.
//!
//! This is the only module in kith allowed to say "Syncthing". Everything it
//! learns — folder ids, device ids, introducer flags, event types — stops here.
//!
//! kith never launches, embeds, configures or supervises the daemon. Every
//! surviving Syncthing wrapper adapts a separately-running daemon; the one that
//! owned the process aged worst and died. That rule has no exceptions, including
//! "just for onboarding".

use std::path::{Path, PathBuf};
use std::pin::Pin;

use futures_core::Stream;

use super::{
    CircleId, CircleOffer, CircleRef, CircleStatus, Cursor, DeviceId, EngineHealth, Envelope,
    InviteTicket, JoinRequest, PeerDevice, RelPath, SyncEngine, SyncError, Version,
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

    async fn get(&self, path: &str) -> Result<serde_json::Value, SyncError> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base_url))
            .header("X-API-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() || e.is_timeout() {
                    SyncError::Unreachable
                } else {
                    SyncError::Engine(e.to_string())
                }
            })?;

        match resp.status().as_u16() {
            200 => resp
                .json()
                .await
                .map_err(|e| SyncError::Engine(e.to_string())),
            401 | 403 => Err(SyncError::Unauthorized),
            404 => Err(SyncError::NotFound),
            other => Err(SyncError::Engine(format!("HTTP {other}"))),
        }
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

    async fn create_circle(&self, _name: &str, _root: &Path) -> Result<CircleRef, SyncError> {
        todo!("kith create")
    }

    async fn circles(&self) -> Result<Vec<CircleRef>, SyncError> {
        let v = self.get("/rest/config/folders").await?;
        Ok(v.as_array()
            .map(|folders| {
                folders
                    .iter()
                    .filter_map(|f| {
                        Some(CircleRef {
                            id: CircleId(f.get("id")?.as_str()?.to_string()),
                            name: f
                                .get("label")
                                .and_then(|s| s.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            root: PathBuf::from(f.get("path")?.as_str()?),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn begin_join(&self, _invite: &InviteTicket) -> Result<(), SyncError> {
        todo!("kith join, phase 1")
    }

    async fn complete_join(
        &self,
        _offer: &CircleOffer,
        _root: &Path,
    ) -> Result<CircleRef, SyncError> {
        todo!("kith join, phase 2")
    }

    async fn pending_joins(&self) -> Result<Vec<JoinRequest>, SyncError> {
        todo!("pending devices")
    }

    async fn admit(&self, _circle: &CircleId, _request: &JoinRequest) -> Result<(), SyncError> {
        todo!("kith approve")
    }

    async fn expel(&self, _circle: &CircleId, _device: &DeviceId) -> Result<(), SyncError> {
        todo!("v0.2 member removal")
    }

    async fn leave(&self, _circle: &CircleId) -> Result<(), SyncError> {
        todo!("leave a Circle")
    }

    async fn set_introducer(&self, _device: &DeviceId, _flag: bool) -> Result<(), SyncError> {
        todo!("v0.2 succession")
    }

    async fn devices(&self, _circle: &CircleId) -> Result<Vec<PeerDevice>, SyncError> {
        todo!("cluster devices + connections")
    }

    async fn status(&self, _circle: &CircleId) -> Result<CircleStatus, SyncError> {
        todo!("folder status + completion")
    }

    async fn observe(&self, _resume: Option<Cursor>) -> Result<Self::Changes, SyncError> {
        todo!("long-poll event stream")
    }

    async fn versions(&self, _circle: &CircleId, _path: &RelPath) -> Result<Vec<Version>, SyncError> {
        todo!("v0.3 history")
    }

    async fn restore(
        &self,
        _circle: &CircleId,
        _path: &RelPath,
        _version: &Version,
    ) -> Result<(), SyncError> {
        todo!("v0.3 restore")
    }
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
}
