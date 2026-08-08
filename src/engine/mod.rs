//! The Sync Engine seam: everything above it speaks kith vocabulary, everything
//! below it may speak Syncthing.
//!
//! Methods return `impl Future + Send` rather than `async fn` because the core
//! spawns engine work onto tokio tasks and bare `async fn` promises no `Send`.

use std::future::Future;
use std::path::{Path, PathBuf};

use futures_core::Stream;

pub mod syncthing;

/// The transport seam. Makes a Circle's bytes present on every Member Device.
pub trait SyncEngine: Send + Sync + 'static {
    /// Live change feed. Ends only on unrecoverable engine loss.
    type Changes: Stream<Item = Envelope> + Send + Unpin;

    /// Reachability plus version-floor check. Cheap; drives the status bar.
    fn health(&self) -> impl Future<Output = Result<EngineHealth, SyncError>> + Send;

    /// This Device's engine identity. kith mints no device id of its own.
    fn local_device(&self) -> impl Future<Output = Result<DeviceId, SyncError>> + Send;

    /// Globs the engine owns inside a Circle root, so engine artefact names never
    /// climb above the seam.
    fn reserved_paths(&self) -> &[&'static str];

    /// Create a Circle: allocate its replicated space, become its introducer.
    fn create_circle(
        &self,
        name: &str,
        root: &Path,
    ) -> impl Future<Output = Result<CircleRef, SyncError>> + Send;

    /// Circles this engine replicates, whether kith created them or adopted them.
    fn circles(&self) -> impl Future<Output = Result<Vec<CircleRef>, SyncError>> + Send;

    /// Joiner, phase 1: consume an Invite — register the introducer and knock.
    fn begin_join(
        &self,
        invite: &InviteTicket,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;

    /// Joiner, phase 2: the Circle was offered back; place it at `root`.
    fn complete_join(
        &self,
        offer: &CircleOffer,
        root: &Path,
    ) -> impl Future<Output = Result<CircleRef, SyncError>> + Send;

    /// Steward side: Devices currently knocking.
    fn pending_joins(&self) -> impl Future<Output = Result<Vec<JoinRequest>, SyncError>> + Send;

    /// Joiner side: Circles a Steward has offered back but this Device has not placed.
    fn pending_circles(&self) -> impl Future<Output = Result<Vec<CircleOffer>, SyncError>> + Send;

    /// Steward side: admit a knocking Device. Deliberate, never automatic.
    fn admit(
        &self,
        circle: &CircleId,
        request: &JoinRequest,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;

    /// Steward side: remove a Device; de-introduction propagates the removal.
    fn expel(
        &self,
        circle: &CircleId,
        device: &DeviceId,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;

    /// Leave a Circle: stop replicating. Local bytes are kept, never deleted here.
    fn leave(&self, circle: &CircleId) -> impl Future<Output = Result<(), SyncError>> + Send;

    /// Succession: flag or unflag a peer Device as this Device's introducer.
    fn set_introducer(
        &self,
        device: &DeviceId,
        flag: bool,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;

    /// Peer Devices sharing a Circle, with connection state. Never returns self —
    /// which is why `PeerDevice::introducer` is only ever a cross-check.
    fn devices(
        &self,
        circle: &CircleId,
    ) -> impl Future<Output = Result<Vec<PeerDevice>, SyncError>> + Send;

    /// Local sync state plus per-peer completion for one Circle.
    fn status(
        &self,
        circle: &CircleId,
    ) -> impl Future<Output = Result<CircleStatus, SyncError>> + Send;

    /// Subscribe from `resume` (None = now). Gaps surface as `Change::Desynced`.
    fn observe(
        &self,
        resume: Option<Cursor>,
    ) -> impl Future<Output = Result<Self::Changes, SyncError>> + Send;

    /// Archived versions the engine holds for one path.
    fn versions(
        &self,
        circle: &CircleId,
        path: &RelPath,
    ) -> impl Future<Output = Result<Vec<Version>, SyncError>> + Send;

    /// Restore one archived version — the "a Member deleted everything" path.
    fn restore(
        &self,
        circle: &CircleId,
        path: &RelPath,
        version: &Version,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CircleId(pub String);

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DeviceId(pub String);

/// A path relative to a Circle root.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RelPath(pub String);

#[derive(Clone, Debug)]
pub struct CircleRef {
    pub id: CircleId,
    pub name: String,
    pub root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PeerDevice {
    pub device: DeviceId,
    pub name: String,
    pub connected: bool,
    /// Flagged as *this* Device's introducer. A cross-check only, never a way to
    /// identify the Circle's Steward: the introducer flags nobody.
    pub introducer: bool,
}

#[derive(Clone, Debug)]
pub struct PeerCompletion {
    pub device: DeviceId,
    pub percent: f64,
}

#[derive(Clone, Debug)]
pub struct CircleStatus {
    pub state: String,
    pub items: u64,
    pub bytes_needed: u64,
    pub peers: Vec<PeerCompletion>,
}

#[derive(Clone, Debug)]
pub struct JoinRequest {
    pub device: DeviceId,
    pub name: String,
    pub seen_at: String,
}

#[derive(Clone, Debug)]
pub struct CircleOffer {
    pub circle: CircleId,
    pub from: DeviceId,
    pub label: String,
}

/// The payload an Invite code carries.
#[derive(Clone, Debug)]
pub struct InviteTicket {
    pub circle: CircleId,
    pub steward_device: DeviceId,
    pub expires_at: u64,
    /// An address hint for the Steward's Device, for networks discovery cannot cross.
    pub address: Option<String>,
}

/// Opaque replay token for the change feed.
#[derive(Clone, Debug)]
pub struct Cursor(pub String);

#[derive(Clone, Debug)]
pub struct Envelope {
    pub cursor: Cursor,
    pub change: Change,
}

#[derive(Clone, Debug)]
pub enum Change {
    Path { circle: CircleId, path: RelPath },
    Presence { circle: CircleId, device: DeviceId, connected: bool },
    Knock { device: DeviceId },
    /// The feed lost continuity; callers must re-scan rather than trust deltas.
    Desynced,
}

#[derive(Clone, Debug)]
pub struct Version {
    pub archived_at: String,
}

#[derive(Clone, Debug)]
pub struct EngineHealth {
    pub version: String,
}

/// One closed enum. Upper layers match on category, never on engine text.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("the Sync Engine is not reachable")]
    Unreachable,
    #[error("the Sync Engine rejected our credentials")]
    Unauthorized,
    #[error("the Sync Engine is below the supported version floor: {0}")]
    Incompatible(String),
    #[error("not known to the Sync Engine")]
    NotFound,
    #[error("Sync Engine error: {0}")]
    Engine(String),
}
