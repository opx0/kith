//! The Provider seam (ADR-0003) — the content-type-aware layer.
//!
//! The core knows People, Circles, Collections and Items. It knows nothing about
//! wallpapers; that knowledge lives entirely behind this trait. v0.1 registers
//! exactly one Provider, compiled in.
//!
//! The seam is synchronous on purpose: Providers do plain I/O and CPU work, and
//! the core runs every call on `spawn_blocking`. Concurrency stays the core's
//! problem, which keeps runtime types out of the seam.

use std::path::Path;

use crate::domain::Item;

pub mod wallpaper;

/// Everything kith knows about one kind of content. Object-safe.
pub trait Provider: Send + Sync {
    /// Stable identifier ("wallpaper"). Recorded in Collection metadata.
    fn id(&self) -> &'static str;

    /// Does this Provider claim these bytes?
    ///
    /// Must be cheap and pure — extension match plus the bounded magic-byte
    /// prefix the core already sniffed, never a full read. This is also the
    /// import gate: content a Collection's Provider does not claim is refused
    /// with a message, not silently accepted.
    fn claims(&self, candidate: &ImportCandidate<'_>) -> bool;

    /// Facts read from the content itself at import time. Pure: no network, no
    /// mutation. Producing them is this seam's job; where they are stored is
    /// ADR-0004's.
    fn extract_metadata(&self, candidate: &ImportCandidate<'_>) -> Result<ProviderFacts, ProviderError>;

    /// Produce a preview within `budget` pixels. Returns pixels or text — never
    /// escape sequences. The core owns all terminal encoding.
    fn preview(&self, item: &Item, budget: PixelBudget) -> Result<Preview, ProviderError>;

    /// Actions this Provider offers on a claimed Item *on this Device*.
    ///
    /// Availability is per-Device: no detected backend means Apply is declared
    /// `Unavailable` with a reason, never omitted — a missing Action is a bug
    /// report, a declared-unavailable one is an explanation.
    fn actions(&self, item: &Item) -> Vec<ActionDecl>;

    /// Targets Apply can address here, enumerated at call time. Monitors
    /// hotplug, so nothing is cached.
    fn apply_targets(&self) -> Result<Vec<ApplyTarget>, ProviderError>;

    /// Execute a declared Action.
    ///
    /// Must leave no half-state on failure: a failed Apply changes nothing on
    /// screen and records nothing.
    fn perform(
        &self,
        action: &str,
        item: &Item,
        target: Option<&ApplyTarget>,
    ) -> Result<ActionOutcome, ActionError>;
}

pub struct ImportCandidate<'a> {
    /// Where the bytes currently sit.
    pub path: &'a Path,
    /// Core-sniffed from a bounded prefix.
    pub mime: Option<String>,
}

/// Facts a Provider read out of the content, stored under its own namespace.
#[derive(Clone, Debug, Default)]
pub struct ProviderFacts {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
}

pub enum Preview {
    /// Decoded pixels, already scaled to fit the budget.
    Image(Box<image::DynamicImage>),
    /// The text card — the tier that must never fail.
    Text(String),
}

/// Computed by the core from the terminal's cell size.
#[derive(Clone, Copy, Debug)]
pub struct PixelBudget {
    pub w_px: u32,
    pub h_px: u32,
}

#[derive(Clone, Debug)]
pub struct ActionDecl {
    /// Namespaced: `wallpaper.apply`.
    pub id: String,
    pub label: String,
    /// True → the TUI and CLI offer target selection.
    pub needs_target: bool,
    pub availability: Availability,
}

#[derive(Clone, Debug)]
pub enum Availability {
    Available,
    Unavailable { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyTarget {
    AllMonitors,
    Monitor(String),
}

#[derive(Clone, Debug)]
pub struct ActionOutcome {
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("unreadable content: {0}")]
    Unreadable(String),
    #[error("unsupported content: {0}")]
    Unsupported(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("no backend available: {0}")]
    NoBackend(String),
    #[error("action failed: {0}")]
    Failed(String),
}
