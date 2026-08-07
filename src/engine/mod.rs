//! The Sync Engine seam (ADR-0002) — the churn firewall.
//!
//! Everything above this module speaks kith vocabulary; everything below it may
//! speak Syncthing. The REST surface underneath churns (the config API was rebuilt
//! in v1.12, pending endpoints arrived in v1.13, v2.0 changed conflict semantics),
//! so the seam is deliberately narrow: seventeen methods, each pinned to a domain
//! operation, and adding an eighteenth is a reviewable decision.
//!
//! The trait returns `impl Future + Send` rather than using `async fn` directly:
//! the core spawns engine work onto tokio tasks, which requires the futures to be
//! Send, and bare `async fn` in a trait does not promise that.

use std::future::Future;
use std::path::{Path, PathBuf};

use futures_core::Stream;

pub mod syncthing;

/// The transport seam. Makes a Circle's bytes present on every Member Device.
/// No opinions about Collections, Items or Providers.
pub trait SyncEngine: Send + Sync + 'static {
    /// Live change feed. Ends only on unrecoverable engine loss.
    type Changes: Stream<Item = Envelope> + Send + Unpin;

    // ── engine & self ────────────────────────────────────────────────
    /// Reachability plus version-floor check. Cheap; drives the status bar.
    fn health(&self) -> impl Future<Output = Result<EngineHealth, SyncError>> + Send;

    /// This Device's engine identity. kith mints no device id of its own.
    fn local_device(&self) -> impl Future<Output = Result<DeviceId, SyncError>> + Send;

    /// Globs the engine owns inside a Circle root — its bookkeeping, its temp
    /// files, its conflict copies. Not async: a constant of the implementation.
    ///
    /// This method exists so engine artefact *names* never climb above the seam.
    /// The Gallery, the Item scanner and the hasher must all skip these paths and
    /// none of them may contain a Syncthing spelling.
    fn reserved_paths(&self) -> &[&'static str];

    // ── circle lifecycle ─────────────────────────────────────────────
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

    // ── inspection ───────────────────────────────────────────────────
    /// Peer Devices sharing a Circle, with connection state.
    ///
    /// Never returns self — which is why the Circle's Steward is read from the
    /// Circle descriptor and `PeerDevice::introducer` is only ever a cross-check.
    fn devices(
        &self,
        circle: &CircleId,
    ) -> impl Future<Output = Result<Vec<PeerDevice>, SyncError>> + Send;

    /// Local sync state plus per-peer completion for one Circle.
    fn status(
        &self,
        circle: &CircleId,
    ) -> impl Future<Output = Result<CircleStatus, SyncError>> + Send;

    // ── change feed ──────────────────────────────────────────────────
    /// Subscribe from `resume` (None = now). Gaps surface as `Change::Desynced`.
    fn observe(
        &self,
        resume: Option<Cursor>,
    ) -> impl Future<Output = Result<Self::Changes, SyncError>> + Send;

    // ── damage control ───────────────────────────────────────────────
    /// Archived versions the engine holds for one path.
    fn versions(
        &self,
        circle: &CircleId,
        path: &RelPath,
    ) -> impl Future<Output = Result<Vec<Version>, SyncError>> + Send;

    /// Restore one archived version — the "a Member deleted everything" path.
    ///
    /// This is the real mitigation behind Roles-as-policy: nothing stops a Member
    /// from deleting bytes their Device already holds, so the answer is recovery,
    /// not permission.
    fn restore(
        &self,
        circle: &CircleId,
        path: &RelPath,
        version: &Version,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;
}

// ── supporting types: none of them Syncthing-shaped ──────────────────

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
    /// Where the Circle's bytes live, so the core reads Items straight off disk.
    pub root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PeerDevice {
    pub device: DeviceId,
    pub name: String,
    pub connected: bool,
    /// This peer is flagged as *this* Device's introducer.
    ///
    /// Not a way to identify the Circle's Steward: the introducer flags nobody,
    /// so on the Steward's own Device no peer carries the flag. Cross-check only —
    /// a mismatch against the Circle descriptor is a `doctor` warning.
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
    /// Bytes arrived, changed or vanished at a path inside a Circle.
    Path { circle: CircleId, path: RelPath },
    /// A peer connected or disconnected.
    Presence { circle: CircleId, device: DeviceId, connected: bool },
    /// A Device is knocking.
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
    pub reachable: bool,
    pub version: String,
}

/// One closed enum. Upper layers match on category, never on engine text.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("the Sync Engine is not reachable")]
    Unreachable,
    /// Never auto-repaired: kith does not rewrite credentials it did not issue.
    #[error("the Sync Engine rejected our credentials")]
    Unauthorized,
    #[error("the Sync Engine is below the supported version floor: {0}")]
    Incompatible(String),
    #[error("not known to the Sync Engine")]
    NotFound,
    #[error("Sync Engine error: {0}")]
    Engine(String),
}
