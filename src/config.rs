//! The one configuration file — `config.toml`, and nothing else.
//!
//! ROADMAP fixes this file at three things: the apply backend and its custom
//! command, monitor names, and an override for the Sync Engine's address and
//! API key. No themes, no keybindings, no Circle roots. Everything else kith
//! knows is either derived from the synced tree or is behaviour rather than a
//! setting, and a config file that grows keys nobody can explain is how a small
//! tool stops being small.
//!
//! Three rules the whole surface leans on (`docs/spec/cli-tui.md` §8.1):
//!
//! * **A missing file is not an error.** Every key has a default and kith runs
//!   with no config at all — [`load`] on a Device with no file returns exactly
//!   what an empty file returns.
//! * **An unknown key is a warning, never fatal.** A file written for a later
//!   kith still works here. kith names the keys it ignored rather than letting a
//!   Person believe they were honoured.
//! * **A wrong type is fatal.** Guessing what a Person meant is how a config
//!   quietly stops meaning what it says; the caller turns [`ConfigError`] into
//!   exit 78 and the message names the line.
//!
//! And one rule this module exists to make possible: a **named** apply backend
//! that this Device does not have is refused out loud, never quietly replaced by
//! whatever else happened to be installed — see [`Config::backend_refusal`].
//!
//! kith reads this file and never writes it. It is the Person's, not ours: there
//! is no `kith config` verb, no migration pass, and nothing here is rewritten on
//! upgrade.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Every configuration failure is the same kind of failure, and it is this one
/// (sysexits `EX_CONFIG`). Exposed so a caller does not have to spell 78.
pub const EXIT_CONFIG: i32 = 78;

/// The standing note code an ignored key travels under, so `--json` tells a
/// script exactly what the human surface said in grey (spec §3.2).
pub const UNKNOWN_KEY_NOTE: &str = "config.unknown_key";

/// Accepted values of `provider.wallpaper.backend` (ADR-0003 §4's ladder, plus
/// `auto` for "let kith choose" and `custom` for the escape hatch).
pub const APPLY_BACKENDS: &[&str] = &[
    "auto",
    "gnome",
    "kde",
    "swww",
    "hyprpaper",
    "swaybg",
    "xwallpaper",
    "feh",
    "custom",
];

/// The settings a Person can hold, flattened to the five things v0.1 acts on.
///
/// Deliberately not derived `Serialize`: kith never writes this file back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// The apply backend the Person named, or `None` when they left the choice
    /// to kith.
    ///
    /// `backend = "auto"` normalises to `None` — auto *is* the absence of a
    /// choice, and a surface that has to special-case the string will forget to.
    /// A value here is a demand, not a preference: see
    /// [`backend_refusal`](Config::backend_refusal).
    pub apply_backend: Option<String>,

    /// `provider.wallpaper.custom.apply` — the shell template Apply runs when no
    /// built-in backend fits (ADR-0003 §4). `{item}` is the path to the Item's
    /// bytes and is always quoted by kith; `{target}` is the chosen monitor.
    ///
    /// Setting it implies `apply_backend == Some("custom")`, per spec §8.2.
    pub apply_command: Option<String>,

    /// Output names the Person has given labels to in
    /// `[provider.wallpaper.monitors]`, in the file's own (alphabetical) order.
    ///
    /// Labels only. This list cannot create, reorder or hide an output — the
    /// backend enumerates monitors at Action time because monitors hotplug, and
    /// an unlisted output shows its raw name. The label each one was given rides
    /// on [`Loaded::monitor_labels`], which is where the monitor picker and
    /// `kith doctor` should read it from.
    pub monitors: Vec<String>,

    /// `sync_engine.address` — overrides credential discovery (ADR-0002 §6).
    /// Whether a non-loopback address is allowed, and the `engine.remote`
    /// warning it earns, are the engine seam's call; this module only carries
    /// what the Person wrote.
    pub engine_address: Option<String>,

    /// `sync_engine.api_key` — read, never written. kith does not rotate,
    /// regenerate or guess a key, and says where it read one from when it is
    /// rejected.
    pub engine_api_key: Option<String>,
}

impl Config {
    /// The backend the Person named, or `None` when they left it to kith.
    ///
    /// `None` means "run ADR-0003 §4's detection ladder", and is the only case
    /// in which kith picks a backend on a Person's behalf.
    pub fn named_backend(&self) -> Option<&str> {
        self.apply_backend.as_deref()
    }

    /// The refusal a surface prints when the named backend is not among the
    /// backends this Device actually has, given the ids the Provider detected.
    ///
    /// `None` means nothing was named, or the named one is present — in both
    /// cases Apply proceeds. A `Some` is the whole point of this module: a
    /// Person who wrote `backend = "swww"` on a machine without swww gets Apply
    /// declared `Unavailable` with this reason, never a silent substitution by
    /// feh. Falling back would be the surface doing something the Person did not
    /// ask for, on the one Action that is meant to be deliberate.
    pub fn backend_refusal(&self, detected: &[&str]) -> Option<String> {
        let named = self.named_backend()?;
        let present = if named == "custom" {
            self.apply_command.is_some()
        } else {
            detected.contains(&named)
        };
        if present {
            None
        } else {
            Some(format!("configured backend \"{named}\" not detected"))
        }
    }
}

/// A parsed config plus everything the surfaces need to be honest about it:
/// where it came from, whether it was there at all, and which keys were ignored.
///
/// [`load`] hands back only the [`Config`]; callers that render notes, run
/// `kith doctor`'s `config.file` check, or want the two keys that do not fit the
/// flat shape ([`monitor_labels`](Self::monitor_labels),
/// [`apply_targets_command`](Self::apply_targets_command)) use [`inspect`].
#[derive(Clone, Debug, Default)]
pub struct Loaded {
    pub config: Config,
    /// Where kith looked. `None` only when this Device has no config directory.
    pub path: Option<PathBuf>,
    /// Whether a file was actually there. A missing one is not an error, but an
    /// explicit `--config` that resolves to nothing is worth a caller's word.
    pub present: bool,
    /// Dotted paths of keys kith does not know, in file order. Ignored, warned
    /// about, never fatal.
    pub unknown_keys: Vec<String>,
    /// `output name → friendly label`, aligned with [`Config::monitors`].
    pub monitor_labels: Vec<(String, String)>,
    /// `provider.wallpaper.custom.targets` — the optional command that lists one
    /// output name per line. Absent means the custom backend can address all
    /// monitors and nothing narrower (ADR-0003 §4).
    pub apply_targets_command: Option<String>,
}

impl Loaded {
    /// The friendly label a Person gave an output, if they gave it one. An
    /// output with no label keeps its raw name; labels rename nothing.
    pub fn label(&self, output: &str) -> Option<&str> {
        self.monitor_labels
            .iter()
            .find(|(name, _)| name == output)
            .map(|(_, label)| label.as_str())
    }

    /// One human sentence per ignored key, ready for a `!` line on stderr or a
    /// [`UNKNOWN_KEY_NOTE`] note in the JSON envelope.
    pub fn warnings(&self) -> Vec<String> {
        let where_ = self
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "config.toml".into());
        self.unknown_keys
            .iter()
            .map(|key| format!("unknown key {key} in {where_} — ignored"))
            .collect()
    }
}

/// Why kith will not run on this file. Every variant is exit [`EXIT_CONFIG`];
/// they differ only in what a Person has to go and fix.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path} could not be read: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Invalid TOML, or a value of the wrong type. The message is the parser's
    /// own, which names the line and points at the column.
    #[error("{path} is not valid configuration:\n{message}")]
    Parse { path: PathBuf, message: String },
    /// It parses, but kith cannot honour it — and will not guess.
    #[error("{path}: {key} {message}")]
    Invalid {
        path: PathBuf,
        key: String,
        message: String,
        fix: String,
    },
}

impl ConfigError {
    /// The stable code for the JSON envelope's `error.code`.
    pub fn code(&self) -> &'static str {
        match self {
            ConfigError::Unreadable { .. } => "config.unreadable",
            ConfigError::Parse { .. } => "config.parse",
            ConfigError::Invalid { .. } => "config.invalid",
        }
    }

    /// An imperative the Person can literally act on, or nothing. Never padded
    /// with advice — spec §7.7.
    pub fn fix(&self) -> Option<String> {
        match self {
            ConfigError::Unreadable { path, .. } => Some(format!(
                "Make {} readable, or remove it — every key is optional and kith runs with no config at all.",
                path.display()
            )),
            ConfigError::Parse { .. } => Some(
                "Fix that value, or delete the key — every key is optional and kith runs with no config at all."
                    .into(),
            ),
            ConfigError::Invalid { fix, .. } => Some(fix.clone()),
        }
    }
}

/// `$KITH_CONFIG`, else `$XDG_CONFIG_HOME/kith/config.toml` (spec §8.1).
///
/// `--config <PATH>` is the argument parser's business: it wins over both, and
/// reaches this module as [`inspect_at`]. `None` means this Device has no config
/// directory at all, which is not an error either — it just means defaults.
pub fn path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("KITH_CONFIG")
        && !explicit.is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    directories::BaseDirs::new().map(|b| b.config_dir().join("kith/config.toml"))
}

/// The settings, or defaults. A missing file is not an error.
///
/// Unknown keys are warned about on stderr — stdout stays clean, so
/// `kith invite | wl-copy` is unaffected — and ignored.
///
/// A file kith cannot understand ends the process with [`EXIT_CONFIG`] instead
/// of returning defaults, because running on defaults would point kith at a
/// different daemon, or a different apply backend, than the Person wrote down.
/// Callers that must render the failure themselves — the dispatcher, and
/// `kith doctor`, which has fifteen more checks to run and may not exit on the
/// first one — call [`inspect`] instead. So does anything already inside the
/// alternate screen: an exit from here would not restore the terminal.
pub fn load() -> Config {
    match inspect() {
        Ok(loaded) => {
            for warning in loaded.warnings() {
                eprintln!("! {warning}");
            }
            loaded.config
        }
        Err(e) => {
            eprintln!("✗ {e}");
            if let Some(fix) = e.fix() {
                eprintln!("  → {fix}");
            }
            std::process::exit(EXIT_CONFIG);
        }
    }
}

/// [`load`] with the failure and the ignored keys handed back rather than
/// printed, plus the two keys the flat [`Config`] has no room for.
pub fn inspect() -> Result<Loaded, ConfigError> {
    match path() {
        Some(p) => inspect_at(&p),
        None => Ok(Loaded::default()),
    }
}

/// [`inspect`] against an explicit path — `--config`, or a test's scratch file.
///
/// A path that does not exist yields defaults with `present: false` rather than
/// an error, so there is exactly one rule about missing files everywhere in
/// kith. A caller that wants to complain about an explicit `--config` naming
/// nothing has `present` to complain with.
pub fn inspect_at(path: &Path) -> Result<Loaded, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let mut loaded = parse(&text, path)?;
            loaded.path = Some(path.to_path_buf());
            loaded.present = true;
            Ok(loaded)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Loaded {
            path: Some(path.to_path_buf()),
            present: false,
            ..Loaded::default()
        }),
        Err(source) => Err(ConfigError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

// ── parsing ──────────────────────────────────────────────────────────
//
// Two passes over the same text, each doing what it is good at. The typed pass
// gives serde's own error for a wrong type, which names the line and points at
// the column — and it *ignores* keys it does not know, which is the forward
// compatibility we want. The untyped pass then walks the same table against the
// known key surface to find those ignored keys, because a key nobody mentions is
// a config that silently does not mean what it says.

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawFile {
    sync_engine: Option<RawEngine>,
    provider: Option<RawProvider>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawEngine {
    address: Option<String>,
    api_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawProvider {
    wallpaper: Option<RawWallpaper>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawWallpaper {
    backend: Option<String>,
    monitors: Option<BTreeMap<String, String>>,
    custom: Option<RawCustom>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawCustom {
    apply: Option<String>,
    targets: Option<String>,
}

fn parse(text: &str, path: &Path) -> Result<Loaded, ConfigError> {
    let raw: RawFile = toml::from_str(text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let table: toml::Table = toml::from_str(text).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut unknown_keys = Vec::new();
    collect_unknown(&table, KNOWN_KEYS, "", &mut unknown_keys);

    let engine = raw.sync_engine.unwrap_or_default();
    let wallpaper = raw.provider.unwrap_or_default().wallpaper.unwrap_or_default();
    let custom = wallpaper.custom.unwrap_or_default();

    let engine_address = non_empty(path, "sync_engine.address", engine.address)?;
    if let Some(address) = &engine_address
        && !address.starts_with("http://")
        && !address.starts_with("https://")
    {
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            key: "sync_engine.address".into(),
            message: format!("is {address:?}, which is not a URL"),
            fix: "Write it in full, scheme and all: address = \"http://127.0.0.1:8384\".".into(),
        });
    }
    let engine_api_key = non_empty(path, "sync_engine.api_key", engine.api_key)?;

    let apply_command = non_empty(path, "provider.wallpaper.custom.apply", custom.apply)?;
    let apply_targets_command =
        non_empty(path, "provider.wallpaper.custom.targets", custom.targets)?;

    // The enum is case-folded on the way in: a Person writing "SWWW" meant swww,
    // and there is nothing else it could have meant.
    let named = wallpaper
        .backend
        .map(|b| b.trim().to_ascii_lowercase())
        .filter(|b| !b.is_empty());
    if let Some(backend) = &named
        && !APPLY_BACKENDS.contains(&backend.as_str())
    {
        return Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            key: "provider.wallpaper.backend".into(),
            message: format!("is {backend:?}, which kith does not know"),
            fix: format!("Use one of: {}.", APPLY_BACKENDS.join(", ")),
        });
    }

    let apply_backend = match (named.as_deref(), apply_command.is_some()) {
        // Spec §8.2: setting the custom command implies the custom backend.
        (None | Some("auto") | Some("custom"), true) => Some("custom".to_string()),
        (Some("custom"), false) => {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                key: "provider.wallpaper.backend".into(),
                message: "is \"custom\" but [provider.wallpaper.custom] sets no apply command"
                    .into(),
                fix: "Add apply = \"…\" under [provider.wallpaper.custom], or set backend = \"auto\"."
                    .into(),
            });
        }
        // A named backend *and* a custom command is two answers to one question.
        // kith will not pick one: whichever it picked would be a wallpaper the
        // Person did not ask for, and Apply is the Action that must never
        // surprise anybody.
        (Some(other), true) => {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                key: "provider.wallpaper.backend".into(),
                message: format!(
                    "is {other:?} while [provider.wallpaper.custom] also sets an apply command"
                ),
                fix: "Keep one: set backend = \"custom\" to use the command, or delete [provider.wallpaper.custom]."
                    .into(),
            });
        }
        // No named backend: ADR-0003 §4's ladder decides, which is the only time
        // kith chooses a backend on a Person's behalf.
        (None | Some("auto"), false) => None,
        (Some(other), false) => Some(other.to_string()),
    };

    let labels = wallpaper.monitors.unwrap_or_default();
    for (output, label) in &labels {
        if label.trim().is_empty() {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                key: format!("provider.wallpaper.monitors.{}", quote(output)),
                message: "is empty".into(),
                fix: "Give the output a label, or delete the line — an unlabelled output keeps its own name."
                    .into(),
            });
        }
    }

    Ok(Loaded {
        config: Config {
            apply_backend,
            apply_command,
            monitors: labels.keys().cloned().collect(),
            engine_address,
            engine_api_key,
        },
        path: Some(path.to_path_buf()),
        present: true,
        unknown_keys,
        monitor_labels: labels.into_iter().collect(),
        apply_targets_command,
    })
}

/// A key written as an empty string is a mistake, not a setting: it would read
/// as "configured" everywhere downstream and behave as "absent".
fn non_empty(path: &Path, key: &str, value: Option<String>) -> Result<Option<String>, ConfigError> {
    match value {
        Some(v) if v.trim().is_empty() => Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            key: key.to_string(),
            message: "is empty".into(),
            fix: format!("Give {key} a value, or delete the key — every key is optional."),
        }),
        other => Ok(other),
    }
}

/// One known key. `open` marks a table whose keys are the Person's own names
/// (their monitor outputs), where kith has no list to check anything against.
struct Key {
    name: &'static str,
    children: &'static [Key],
    open: bool,
}

const LEAF: &[Key] = &[];

/// The whole known key surface, and the whole of ROADMAP's Configuration row.
/// Anything absent from this tree is unknown: named once, ignored, never fatal.
const KNOWN_KEYS: &[Key] = &[
    Key {
        name: "sync_engine",
        open: false,
        children: &[
            Key { name: "address", open: false, children: LEAF },
            Key { name: "api_key", open: false, children: LEAF },
        ],
    },
    Key {
        name: "provider",
        open: false,
        children: &[Key {
            name: "wallpaper",
            open: false,
            children: &[
                Key { name: "backend", open: false, children: LEAF },
                Key { name: "monitors", open: true, children: LEAF },
                Key {
                    name: "custom",
                    open: false,
                    children: &[
                        Key { name: "apply", open: false, children: LEAF },
                        Key { name: "targets", open: false, children: LEAF },
                    ],
                },
            ],
        }],
    },
];

fn collect_unknown(table: &toml::Table, known: &[Key], prefix: &str, out: &mut Vec<String>) {
    for (name, value) in table {
        let dotted = if prefix.is_empty() {
            quote(name)
        } else {
            format!("{prefix}.{}", quote(name))
        };
        match known.iter().find(|k| k.name == name.as_str()) {
            // Forward compatibility: a key this kith does not know is one a
            // later kith might. Say which, ignore it, keep running.
            None => out.push(dotted),
            Some(k) if k.open => {}
            Some(k) if !k.children.is_empty() => {
                if let Some(inner) = value.as_table() {
                    collect_unknown(inner, k.children, &dotted, out);
                }
            }
            // A leaf: its type is the typed pass's problem, not this one's.
            Some(_) => {}
        }
    }
}

/// Keys are printed back to the Person, so one containing a dot must not read as
/// two keys.
fn quote(key: &str) -> String {
    if key.contains('.') || key.chars().any(char::is_whitespace) || key.is_empty() {
        format!("{key:?}")
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Result<Loaded, ConfigError> {
        parse(text, Path::new("/nowhere/kith/config.toml"))
    }

    fn ok(text: &str) -> Loaded {
        at(text).expect("this config should load")
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let scratch = std::env::temp_dir().join("kith-config-tests");
        std::fs::create_dir_all(&scratch).unwrap();
        let missing = scratch.join("there-is-no-config-here.toml");
        let _ = std::fs::remove_file(&missing);

        let loaded = inspect_at(&missing).expect("a missing config is defaults, not a failure");
        assert_eq!(loaded.config, Config::default());
        assert!(!loaded.present, "the caller can still tell nothing was there");
        assert!(loaded.warnings().is_empty());
    }

    #[test]
    fn an_empty_file_says_exactly_what_no_file_says() {
        assert_eq!(ok("").config, Config::default());
    }

    #[test]
    fn an_unknown_key_survives_with_a_warning_and_the_rest_still_loads() {
        let loaded = ok(r#"
            theme = "dark"

            [sync_engine]
            address = "http://127.0.0.1:8384"
            timeout = 30

            [provider.wallpaper]
            backend = "swww"
            rotate = "hourly"
        "#);

        assert_eq!(loaded.config.named_backend(), Some("swww"));
        assert_eq!(
            loaded.config.engine_address.as_deref(),
            Some("http://127.0.0.1:8384")
        );
        assert_eq!(
            loaded.unknown_keys,
            vec![
                "provider.wallpaper.rotate".to_string(),
                "sync_engine.timeout".to_string(),
                "theme".to_string(),
            ]
        );
        assert!(loaded.warnings()[0].contains("unknown key"));
        assert!(loaded.warnings()[0].contains("ignored"));
    }

    #[test]
    fn a_wrong_type_is_fatal_and_names_the_line() {
        let err = at("[provider.wallpaper]\nbackend = 5\n").expect_err("a wrong type is fatal");
        assert_eq!(err.code(), "config.parse");
        let text = err.to_string();
        assert!(text.contains("line 2"), "the Person is told where: {text}");
        assert!(err.fix().is_some());
    }

    #[test]
    fn invalid_toml_is_fatal_too() {
        let err = at("[sync_engine\naddress = 1").expect_err("broken TOML is fatal");
        assert_eq!(err.code(), "config.parse");
    }

    #[test]
    fn auto_is_the_absence_of_a_choice() {
        let loaded = ok("[provider.wallpaper]\nbackend = \"AUTO\"\n");
        assert_eq!(loaded.config.named_backend(), None);
        assert_eq!(loaded.config.backend_refusal(&["feh"]), None);
    }

    #[test]
    fn a_named_backend_is_never_silently_replaced() {
        let cfg = ok("[provider.wallpaper]\nbackend = \"swww\"\n").config;
        assert_eq!(
            cfg.backend_refusal(&["feh", "hyprpaper"]).as_deref(),
            Some("configured backend \"swww\" not detected"),
            "a backend that is not here is refused, not swapped for one that is"
        );
        assert_eq!(cfg.backend_refusal(&["swww", "feh"]), None);
    }

    #[test]
    fn an_unknown_backend_name_is_refused_rather_than_ignored() {
        let err = at("[provider.wallpaper]\nbackend = \"wallutils\"\n")
            .expect_err("kith cannot honour a backend it does not have");
        assert_eq!(err.code(), "config.invalid");
        assert!(err.fix().unwrap().contains("swww"));
    }

    #[test]
    fn a_custom_command_implies_the_custom_backend() {
        let loaded = ok(r#"
            [provider.wallpaper.custom]
            apply   = "xfconf-query -s {item}"
            targets = "xrandr --listmonitors"
        "#);
        assert_eq!(loaded.config.named_backend(), Some("custom"));
        assert_eq!(
            loaded.config.apply_command.as_deref(),
            Some("xfconf-query -s {item}")
        );
        assert_eq!(
            loaded.apply_targets_command.as_deref(),
            Some("xrandr --listmonitors")
        );
        // The escape hatch is present, so nothing is refused.
        assert_eq!(loaded.config.backend_refusal(&[]), None);
    }

    #[test]
    fn the_custom_backend_without_a_command_is_refused() {
        let err = at("[provider.wallpaper]\nbackend = \"custom\"\n")
            .expect_err("custom with nothing to run cannot apply anything");
        assert_eq!(err.code(), "config.invalid");
    }

    #[test]
    fn a_named_backend_and_a_custom_command_are_two_answers_to_one_question() {
        let err = at(r#"
            [provider.wallpaper]
            backend = "swww"

            [provider.wallpaper.custom]
            apply = "echo {item}"
        "#)
        .expect_err("kith will not guess which one was meant");
        assert_eq!(err.code(), "config.invalid");
    }

    #[test]
    fn monitor_labels_are_read_and_rename_nothing() {
        let loaded = ok(r#"
            [provider.wallpaper.monitors]
            "DP-1"     = "Desk left"
            "HDMI-A-1" = "TV"
        "#);
        assert_eq!(loaded.config.monitors, vec!["DP-1", "HDMI-A-1"]);
        assert_eq!(loaded.label("DP-1"), Some("Desk left"));
        assert_eq!(
            loaded.label("eDP-1"),
            None,
            "an unlisted output keeps its own name"
        );
        assert!(
            loaded.unknown_keys.is_empty(),
            "output names are the Person's, not keys kith knows"
        );
    }

    #[test]
    fn engine_overrides_are_carried_verbatim() {
        let cfg = ok(r#"
            [sync_engine]
            address = "http://192.168.1.4:8384"
            api_key = "abc123"
        "#)
        .config;
        assert_eq!(cfg.engine_address.as_deref(), Some("http://192.168.1.4:8384"));
        assert_eq!(cfg.engine_api_key.as_deref(), Some("abc123"));
    }

    #[test]
    fn an_address_without_a_scheme_is_refused_before_the_engine_sees_it() {
        let err = at("[sync_engine]\naddress = \"127.0.0.1:8384\"\n")
            .expect_err("half a URL is not a URL");
        assert_eq!(err.code(), "config.invalid");
        assert!(err.fix().unwrap().contains("http://"));
    }

    #[test]
    fn an_empty_value_is_a_mistake_not_a_setting() {
        let err = at("[sync_engine]\napi_key = \"\"\n").expect_err("an empty key is not a key");
        assert_eq!(err.code(), "config.invalid");
    }

    #[test]
    fn a_dotted_unknown_key_is_reported_as_one_key() {
        let loaded = ok("\"my.setting\" = 1\n");
        assert_eq!(loaded.unknown_keys, vec!["\"my.setting\"".to_string()]);
    }
}
