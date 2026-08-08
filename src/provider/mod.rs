//! The Provider seam — the content-type-aware layer the core knows nothing
//! about. v0.1 registers exactly one Provider, compiled in.
//!
//! Synchronous on purpose: the core runs every call on `spawn_blocking`, which
//! keeps runtime types out of the seam.

use std::path::Path;

use crate::domain::Item;

pub mod wallpaper;

/// Everything kith knows about one kind of content. Object-safe.
pub trait Provider: Send + Sync {
    /// Stable identifier ("wallpaper"). Recorded in Collection metadata.
    fn id(&self) -> &'static str;

    /// Does this Provider claim these bytes? Must be cheap and pure — never a
    /// full read.
    fn claims(&self, candidate: &ImportCandidate<'_>) -> bool;

    /// Facts read from the content itself at import time. Pure: no network, no
    /// mutation.
    fn extract_metadata(&self, candidate: &ImportCandidate<'_>) -> Result<ProviderFacts, ProviderError>;

    /// Produce a preview within `budget` pixels — pixels or text, never escape
    /// sequences. The core owns all terminal encoding.
    fn preview(&self, item: &Item, budget: PixelBudget) -> Result<Preview, ProviderError>;

    /// Actions this Provider offers on a claimed Item *on this Device*.
    ///
    /// An unavailable Action is declared with a reason, never omitted.
    fn actions(&self, item: &Item) -> Vec<ActionDecl>;

    /// Targets Apply can address here, enumerated at call time — monitors
    /// hotplug, so nothing is cached.
    fn apply_targets(&self) -> Result<Vec<ApplyTarget>, ProviderError>;

    /// Execute a declared Action, leaving no half-state on failure.
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
