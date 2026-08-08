//! The wallpaper Provider — v0.1's only Provider, compiled into the binary.
//!
//! A private trait carries one frontend over several interchangeable
//! apply-backends. The matrix is deliberately small.

use std::path::Path;
use std::process::Command;

use super::{
    ActionDecl, ActionError, ActionOutcome, ApplyTarget, Availability, ImportCandidate,
    PixelBudget, Preview, Provider, ProviderError, ProviderFacts,
};
use crate::domain::Item;

const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif", "tiff", "tif"];

pub struct WallpaperProvider {
    backends: Vec<Box<dyn ApplyBackend>>,
    custom_command: Option<String>,
}

/// One way to set a wallpaper. Private to this Provider — not part of the seam.
trait ApplyBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn detect(&self, env: &SessionEnv) -> bool;
    fn targets(&self) -> Result<Vec<ApplyTarget>, ActionError>;
    fn apply(&self, bytes: &Path, target: &ApplyTarget) -> Result<(), ActionError>;
}

pub struct SessionEnv {
    pub wayland: bool,
    pub x11: bool,
}

impl SessionEnv {
    pub fn detect() -> Self {
        Self {
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            x11: std::env::var_os("DISPLAY").is_some(),
        }
    }
}

fn on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file())
        })
        .unwrap_or(false)
}

/// Caelestia owns the wallpaper on a shell that already manages it: setting one
/// also regenerates the system colour scheme, so wallsync asks it rather than
/// painting over it.
struct Caelestia;
impl ApplyBackend for Caelestia {
    fn id(&self) -> &'static str {
        "caelestia"
    }
    fn detect(&self, env: &SessionEnv) -> bool {
        env.wayland && on_path("caelestia")
    }
    fn targets(&self) -> Result<Vec<ApplyTarget>, ActionError> {
        Ok(vec![ApplyTarget::AllMonitors])
    }
    fn apply(&self, bytes: &Path, _target: &ApplyTarget) -> Result<(), ActionError> {
        run(Command::new("caelestia").arg("wallpaper").arg("-f").arg(bytes))
    }
}

struct Swww;
impl ApplyBackend for Swww {
    fn id(&self) -> &'static str {
        "swww"
    }
    fn detect(&self, env: &SessionEnv) -> bool {
        env.wayland && on_path("swww")
    }
    fn targets(&self) -> Result<Vec<ApplyTarget>, ActionError> {
        Ok(vec![ApplyTarget::AllMonitors])
    }
    fn apply(&self, bytes: &Path, target: &ApplyTarget) -> Result<(), ActionError> {
        let mut cmd = Command::new("swww");
        cmd.arg("img");
        if let ApplyTarget::Monitor(name) = target {
            cmd.arg("--outputs").arg(name);
        }
        run(cmd.arg(bytes))
    }
}

struct Hyprpaper;
impl ApplyBackend for Hyprpaper {
    fn id(&self) -> &'static str {
        "hyprpaper"
    }
    fn detect(&self, env: &SessionEnv) -> bool {
        env.wayland && on_path("hyprpaper") && on_path("hyprctl")
    }
    fn targets(&self) -> Result<Vec<ApplyTarget>, ActionError> {
        Ok(vec![ApplyTarget::AllMonitors])
    }
    fn apply(&self, bytes: &Path, target: &ApplyTarget) -> Result<(), ActionError> {
        let path = bytes.display();
        run(Command::new("hyprctl").args(["hyprpaper", "preload", &path.to_string()]))?;
        let monitor = match target {
            ApplyTarget::Monitor(name) => name.as_str(),
            ApplyTarget::AllMonitors => "",
        };
        run(Command::new("hyprctl").args([
            "hyprpaper",
            "wallpaper",
            &format!("{monitor},{path}"),
        ]))
    }
}

struct Feh;
impl ApplyBackend for Feh {
    fn id(&self) -> &'static str {
        "feh"
    }
    fn detect(&self, env: &SessionEnv) -> bool {
        env.x11 && on_path("feh")
    }
    fn targets(&self) -> Result<Vec<ApplyTarget>, ActionError> {
        Ok(vec![ApplyTarget::AllMonitors])
    }
    fn apply(&self, bytes: &Path, _target: &ApplyTarget) -> Result<(), ActionError> {
        run(Command::new("feh").arg("--bg-fill").arg(bytes))
    }
}

/// Single-quote for `sh`, the only form with no escapes inside it: a literal
/// quote is closed, escaped and reopened.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn run(cmd: &mut Command) -> Result<(), ActionError> {
    match cmd.output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(ActionError::Failed(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        )),
        Err(e) => Err(ActionError::Failed(e.to_string())),
    }
}

impl Default for WallpaperProvider {
    fn default() -> Self {
        Self::new(None)
    }
}

impl WallpaperProvider {
    pub fn new(custom_command: Option<String>) -> Self {
        Self {
            backends: vec![
                Box::new(Caelestia),
                Box::new(Swww),
                Box::new(Hyprpaper),
                Box::new(Feh),
            ],
            custom_command,
        }
    }

    /// The backend that will actually run here, or nothing. A configured custom
    /// command always wins.
    fn active(&self) -> Option<&dyn ApplyBackend> {
        if self.custom_command.is_some() {
            return None;
        }
        let env = SessionEnv::detect();
        self.backends
            .iter()
            .find(|b| b.detect(&env))
            .map(|b| b.as_ref())
    }

    /// Which backends are present, for `wallsync doctor`.
    pub fn detected(&self) -> Vec<&'static str> {
        let env = SessionEnv::detect();
        self.backends
            .iter()
            .filter(|b| b.detect(&env))
            .map(|b| b.id())
            .collect()
    }
}

impl Provider for WallpaperProvider {
    fn id(&self) -> &'static str {
        "wallpaper"
    }

    fn claims(&self, candidate: &ImportCandidate<'_>) -> bool {
        if let Some(mime) = &candidate.mime
            && mime.starts_with("image/")
        {
            return true;
        }
        candidate
            .path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
    }

    fn extract_metadata(&self, candidate: &ImportCandidate<'_>) -> Result<ProviderFacts, ProviderError> {
        let dims = image::image_dimensions(candidate.path)
            .map_err(|e| ProviderError::Unreadable(e.to_string()))?;
        Ok(ProviderFacts {
            width: Some(dims.0),
            height: Some(dims.1),
            format: candidate
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase()),
        })
    }

    fn preview(&self, item: &Item, budget: PixelBudget) -> Result<Preview, ProviderError> {
        let Some(path) = &item.path else {
            return Ok(Preview::Text(format!("{} — not yet arrived", item.title)));
        };
        let img = image::open(path).map_err(|e| ProviderError::Unreadable(e.to_string()))?;
        Ok(Preview::Image(Box::new(img.thumbnail(budget.w_px, budget.h_px))))
    }

    fn actions(&self, _item: &Item) -> Vec<ActionDecl> {
        let availability = if self.custom_command.is_some() || self.active().is_some() {
            Availability::Available
        } else {
            Availability::Unavailable {
                reason: "no wallpaper backend detected (looked for caelestia, swww, hyprpaper, feh)".into(),
            }
        };
        vec![ActionDecl {
            id: "wallpaper.apply".into(),
            availability,
        }]
    }

    fn apply_targets(&self) -> Result<Vec<ApplyTarget>, ProviderError> {
        match self.active() {
            Some(b) => b
                .targets()
                .map_err(|e| ProviderError::Unsupported(e.to_string())),
            None if self.custom_command.is_some() => Ok(vec![ApplyTarget::AllMonitors]),
            None => Ok(Vec::new()),
        }
    }

    fn perform(
        &self,
        action: &str,
        item: &Item,
        target: Option<&ApplyTarget>,
    ) -> Result<ActionOutcome, ActionError> {
        if action != "wallpaper.apply" {
            return Err(ActionError::Failed(format!("unknown Action {action}")));
        }
        let path = item
            .path
            .as_ref()
            .ok_or_else(|| ActionError::Failed("this Item's bytes have not arrived".into()))?;
        let target = target.cloned().unwrap_or(ApplyTarget::AllMonitors);

        if let Some(template) = &self.custom_command {
            // Both placeholders are quoted: the template runs through a shell and
            // the Item's name came from a peer.
            let filled = template
                .replace("{item}", &shell_quote(&path.display().to_string()))
                .replace(
                    "{target}",
                    &shell_quote(match &target {
                        ApplyTarget::Monitor(name) => name.as_str(),
                        ApplyTarget::AllMonitors => "",
                    }),
                );
            run(Command::new("sh").arg("-c").arg(&filled))?;
            return Ok(ActionOutcome {
                message: "applied via the configured command".into(),
            });
        }

        let backend = self.active().ok_or_else(|| {
            ActionError::NoBackend("looked for caelestia, swww, hyprpaper, feh".into())
        })?;
        backend.apply(path, &target)?;
        Ok(ActionOutcome {
            message: format!("applied with {}", backend.id()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn candidate(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn claims_images_by_extension_case_insensitively() {
        let p = WallpaperProvider::default();
        for name in ["a.png", "b.JPG", "c.jpeg", "d.WebP"] {
            let path = candidate(name);
            assert!(p.claims(&ImportCandidate { path: &path, mime: None }), "{name}");
        }
    }

    #[test]
    fn refuses_content_it_does_not_understand() {
        let p = WallpaperProvider::default();
        let path = candidate("notes.txt");
        assert!(!p.claims(&ImportCandidate { path: &path, mime: None }));
    }

    #[test]
    fn a_custom_command_gets_the_two_placeholders_the_config_documents() {
        let p = WallpaperProvider::new(Some("set {item} on {target}".into()));
        assert!(p.custom_command.is_some());
        let filled = "set {item} on {target}"
            .replace("{item}", &shell_quote("/tmp/a b.png"))
            .replace("{target}", &shell_quote("DP-1"));
        assert_eq!(filled, "set '/tmp/a b.png' on 'DP-1'");
    }

    #[test]
    fn a_name_a_peer_chose_cannot_become_a_second_shell_command() {
        assert_eq!(shell_quote("a; rm -rf ~"), "'a; rm -rf ~'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn apply_is_declared_unavailable_with_a_reason_never_omitted() {
        let p = WallpaperProvider::new(None);
        let item = Item {
            id: crate::domain::ItemId::generate(),
            title: "x".into(),
            added_by: crate::domain::PersonId::generate(),
            added_at: String::new(),
            path: None,
            hash: None,
            bytes: None,
        };
        let actions = p.actions(&item);
        assert_eq!(actions.len(), 1, "Apply is always declared");
        assert_eq!(actions[0].id, "wallpaper.apply");
    }
}
