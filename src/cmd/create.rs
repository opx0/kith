//! `wallsync create <name> [--path <dir>] [--adopt]` — a Circle comes into existence.
//!
//! The order is fixed: Identity, root, `SyncEngine::create_circle`, ignore file,
//! `.wallsync/circle.toml`, `.wallsync/collections/main.toml`, this Device's claim. The
//! ignore file comes first because it teaches the engine to skip the staging
//! suffix the next write uses. Every step is idempotent, so a create that died
//! part-way is finished by running it again.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::cmd::membership::fingerprint;
use crate::config::Config;
use crate::domain::ItemId;
use crate::engine::syncthing::{Credentials, SyncthingEngine};
use crate::engine::{CircleId, CircleRef, SyncEngine, SyncError};
use crate::identity::Identity;
use crate::provider::wallpaper::WallpaperProvider;
use crate::provider::{ImportCandidate, Provider};
use crate::store::descriptors::{CircleDescriptor, CollectionDescriptor};
use crate::store::{claims, descriptors, records};

// sysexits, the dialect the whole binary speaks.
const EX_OK: i32 = 0;
const EX_FAIL: i32 = 1;
const EX_USAGE: i32 = 64;
const EX_DATA: i32 = 65;
const EX_UNAVAILABLE: i32 = 69;
const EX_CONFIG: i32 = 78;

/// v0.1's sole Collection id, a literal in the one module that creates it.
const COLLECTION: &str = "main";

/// The Provider this build binds a new Collection to.
const PROVIDER: &str = "wallpaper";

/// `wallsync create` — returns this process's exit code.
///
/// A missing Identity exits 78, matching `add` and the membership verbs rather
/// than cli-tui §4.2's local spelling of 64.
pub async fn run(name: &str, path: Option<&str>, adopt: bool) -> i32 {
    let identity = match crate::identity::load() {
        Ok(Some(id)) => id,
        Ok(None) => {
            eprintln!("✗ No Identity on this Device.");
            eprintln!(
                "  → Run 'wallsync init <name>' and give wallsync a name to attach to what you add."
            );
            return EX_CONFIG;
        }
        Err(e) => {
            eprintln!("✗ {e}");
            return EX_CONFIG;
        }
    };

    let config = crate::config::load();
    let credentials = match credentials(&config) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("✗ No Sync Engine configuration found on this Device.");
            eprintln!("  → Install and start the daemon (Syncthing), then run wallsync doctor.");
            return EX_UNAVAILABLE;
        }
    };

    let address = credentials.base_url.clone();
    let engine = SyncthingEngine::new(credentials);

    match create(&engine, &identity, name, path, adopt).await {
        Ok(made) => {
            made.report();
            EX_OK
        }
        Err(fault) => {
            fault.report(&address);
            fault.exit()
        }
    }
}

/// Where to reach the Sync Engine: the Person's override first, discovery second.
fn credentials(config: &Config) -> Result<Credentials, SyncError> {
    let discovered = SyncthingEngine::discover();
    match (&config.engine_address, &config.engine_api_key) {
        (None, None) => discovered,
        (address, api_key) => {
            let found = discovered.ok();
            let base_url = address
                .clone()
                .or_else(|| found.as_ref().map(|c| c.base_url.clone()))
                .ok_or(SyncError::Unreachable)?;
            let api_key = api_key
                .clone()
                .or_else(|| found.as_ref().map(|c| c.api_key.clone()))
                .ok_or(SyncError::Unauthorized)?;
            Ok(Credentials {
                base_url,
                api_key,
                // Where the Person told us to look, so a rejected key can name it.
                source: crate::config::path().unwrap_or_else(|| PathBuf::from("config.toml")),
            })
        }
    }
}

// ── the flow itself, engine-generic so it can be exercised without a daemon ──

/// What `wallsync create` did, in enough detail to say it out loud.
struct Made {
    circle: CircleRef,
    root: PathBuf,
    /// The engine was already replicating this space; nothing was created.
    kept_space: bool,
    stewardship: Stewardship,
    /// `Some` only when `--adopt` ran the content pass.
    content: Option<Adoption>,
}

/// Who this Circle's descriptor names as the Steward's Device, from here on.
enum Stewardship {
    /// We wrote `circle.toml`, or it was already ours.
    Ours,
    /// A descriptor already named another Device. Adopted into, left as found.
    Theirs(String),
    /// No descriptor yet, and the engine says another Device admits peers.
    Awaiting(String),
}

async fn create<E: SyncEngine>(
    engine: &E,
    identity: &Identity,
    name: &str,
    path: Option<&str>,
    adopt: bool,
) -> Result<Made, Fault> {
    let name = validate_name(name)?;

    // Both checks first: creating a Circle writes engine config, never queued.
    engine.health().await.map_err(Fault::Engine)?;
    let device = engine.local_device().await.map_err(Fault::Engine)?.0;
    let known = engine.circles().await.map_err(Fault::Engine)?;

    let (root, replicated) = resolve_root(engine, &known, &name, path, adopt).await?;

    // A duplicate name is a refusal, not a rename; the same root again is a resume.
    if let Some(clash) = known
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(&name) && !same_path(&c.root, &root))
    {
        return Err(Fault::DuplicateName(clash.root.clone()));
    }

    let created_root = prepare_root(&root, &device, adopt)?;

    let (circle, created_space) = match replicated {
        Some(c) => (c, false),
        None => (
            engine
                .create_circle(&name, &root)
                .await
                .map_err(Fault::Engine)?,
            true,
        ),
    };

    // From here on a failure has to leave nothing behind.
    let finished = finish(engine, identity, &device, &circle, &root, adopt).await;
    match finished {
        Ok((stewardship, content)) => Ok(Made {
            circle,
            root,
            kept_space: !created_space,
            stewardship,
            content,
        }),
        Err(fault) => {
            if created_space {
                // Never for an adopted space, which wallsync did not create.
                let _ = engine.leave(&circle.id).await;
            }
            if created_root {
                let _ = fs::remove_dir_all(&root);
            }
            Err(fault)
        }
    }
}

/// Everything that happens inside the Circle's own tree, in order.
async fn finish<E: SyncEngine>(
    engine: &E,
    identity: &Identity,
    device: &str,
    circle: &CircleRef,
    root: &Path,
    adopt: bool,
) -> Result<(Stewardship, Option<Adoption>), Fault> {
    // First, because it teaches the engine to ignore the `*.wallsync-tmp` staging
    // file the very next write uses. Seeding is additive and idempotent.
    //
    // The engine's reserved globs deliberately do *not* go in here: that would
    // stop `*.sync-conflict-*` replicating, and a conflict copy has to reach
    // every Member. They are passed to the scanner below instead.
    descriptors::seed_stignore(root, &[]).map_err(|e| Fault::Io(root.join(".stignore"), e))?;

    let existing = descriptors::read_circle(root)
        .map_err(|e| Fault::Io(descriptors::circle_path(root), e))?;

    let stewardship = match existing {
        // Write-once: a descriptor already in the tree is read, never rewritten.
        Some(d) if d.founder_device == device => {
            ensure_collection(root)?;
            Stewardship::Ours
        }
        Some(d) => Stewardship::Theirs(d.founder_device),
        None => match steward_peer(engine, &circle.id).await? {
            // Its descriptors will arrive; two writers would be two claimants.
            Some(peer) => Stewardship::Awaiting(peer),
            None => {
                write_descriptors(root, circle, identity, device)?;
                Stewardship::Ours
            }
        },
    };

    // Always, on every branch: a Device with no claim is nameable by nobody.
    claims::publish(root, device, identity, &now())
        .map_err(|e| Fault::Claim(root.join(".wallsync/members"), e))?;

    let content = if adopt {
        Some(adopt_content(root, device, identity, engine.reserved_paths())?)
    } else {
        None
    };

    Ok((stewardship, content))
}

/// Write the two singletons, staged and renamed so nothing partial replicates.
fn write_descriptors(
    root: &Path,
    circle: &CircleRef,
    identity: &Identity,
    device: &str,
) -> Result<(), Fault> {
    let descriptor = CircleDescriptor {
        schema: descriptors::SCHEMA,
        id: circle.id.0.clone(),
        name: circle.name.clone(),
        created: now(),
        founder_person: identity.person.to_string(),
        founder_device: device.to_string(),
    };
    descriptors::write_circle(root, &descriptor)
        .map_err(|e| Fault::Io(descriptors::circle_path(root), e))?;
    ensure_collection(root)
}

/// The Collection descriptor, written once. Its absence beside an existing
/// `circle.toml` is exactly the half-finished create this command re-runs into.
fn ensure_collection(root: &Path) -> Result<(), Fault> {
    let path = descriptors::collections_dir(root).join(format!("{COLLECTION}.toml"));
    if descriptors::read_collection(root, COLLECTION)
        .map_err(|e| Fault::Io(path.clone(), e))?
        .is_some()
    {
        return Ok(());
    }
    descriptors::write_collection(
        root,
        &CollectionDescriptor {
            schema: descriptors::SCHEMA,
            collection: COLLECTION.to_string(),
            provider: PROVIDER.to_string(),
        },
    )
    .map_err(|e| Fault::Io(path, e))
}

/// The one legitimate read of `PeerDevice.introducer`: a bootstrap for a tree
/// with no wallsync metadata. After that, `founder_device` is the answer everywhere.
async fn steward_peer<E: SyncEngine>(
    engine: &E,
    circle: &CircleId,
) -> Result<Option<String>, Fault> {
    let peers = engine.devices(circle).await.map_err(Fault::Engine)?;
    Ok(peers
        .into_iter()
        .find(|p| p.introducer)
        .map(|p| p.device.0))
}

// ── the root ─────────────────────────────────────────────────────────

/// Decide where the Circle lives, and whether the engine is already replicating
/// it. `--adopt` with no `--path` auto-detects the tree to take over.
async fn resolve_root<E: SyncEngine>(
    engine: &E,
    known: &[CircleRef],
    name: &str,
    path: Option<&str>,
    adopt: bool,
) -> Result<(PathBuf, Option<CircleRef>), Fault> {
    // `--adopt <DIR>` and `--path <DIR>` are the same argument here.
    let requested = match path {
        Some(p) => Some(std::path::absolute(p).map_err(|e| Fault::Io(PathBuf::from(p), e))?),
        None if adopt => None,
        None => Some(default_root(name)?),
    };

    match requested {
        Some(root) => {
            // A space the engine already replicates is taken over, never doubled.
            // `known` holds only Circles wallsync recognises, so a folder it has
            // never seen — the wp-sync tree this migration exists for — is found
            // by asking the engine directly.
            let replicated = match known.iter().find(|c| same_path(&c.root, &root)) {
                Some(c) => Some(c.clone()),
                None => engine.replicated_at(&root).await.map_err(Fault::Engine)?,
            };
            Ok((root, replicated))
        }
        None => {
            let candidate = detect_adoptable(known)?;
            Ok((candidate.root.clone(), Some(candidate)))
        }
    }
}

/// `~/wallsync/<slug>`. circles §3.1 puts the default under `$XDG_DATA_HOME`; this
/// build follows cli-tui, which `join`, `list circles` and `doctor` all echo.
fn default_root(name: &str) -> Result<PathBuf, Fault> {
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .ok_or(Fault::NoHome)?;
    Ok(home.join("wallsync").join(slug(name)))
}

/// The name, lowercased, with runs of anything else collapsed to `-`.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let slug = out.trim_matches('-').to_string();
    // A name of nothing but punctuation still needs somewhere to live.
    if slug.is_empty() {
        "circle".to_string()
    } else {
        slug
    }
}

/// Create the root if it is ours to create, and refuse to move into a directory
/// that already belongs to somebody else. Returns whether *this run* created it.
fn prepare_root(root: &Path, device: &str, adopt: bool) -> Result<bool, Fault> {
    if adopt {
        if !root.is_dir() {
            return Err(Fault::AdoptNotADirectory(root.to_path_buf()));
        }
        return Ok(false);
    }

    match fs::read_dir(root) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|e| Fault::Io(root.to_path_buf(), e))?;
            // 0700: a Circle's content is for its Members, not the next account.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
            }
            Ok(true)
        }
        Err(e) => Err(Fault::Io(root.to_path_buf(), e)),
        Ok(mut entries) => {
            if entries.next().is_none() {
                return Ok(false);
            }
            // Not empty — unless it is a Circle this command already started.
            match descriptors::read_circle(root) {
                Ok(Some(d)) if d.founder_device == device => Ok(false),
                Ok(Some(_)) | Ok(None) => Err(Fault::PathNotEmpty(root.to_path_buf())),
                Err(e) => Err(Fault::Io(descriptors::circle_path(root), e)),
            }
        }
    }
}

/// Spaces the engine replicates that wallsync has never recorded anything in.
fn detect_adoptable(known: &[CircleRef]) -> Result<CircleRef, Fault> {
    let legacy = std::env::var("WP_FOLDER_ID").unwrap_or_else(|_| "wallpapers".to_string());

    let candidates: Vec<CircleRef> = known
        .iter()
        .filter(|c| c.root.is_dir())
        .filter(|c| !matches!(descriptors::read_circle(&c.root), Ok(Some(_))))
        .cloned()
        .collect();

    // wp-sync's own space wins outright; it is the tree this flag exists for.
    if let Some(c) = candidates.iter().find(|c| c.id.0 == legacy) {
        return Ok(c.clone());
    }
    match candidates.len() {
        0 => Err(Fault::AdoptNotFound),
        1 => Ok(candidates[0].clone()),
        _ => Err(Fault::AdoptAmbiguous(candidates)),
    }
}

/// Two paths naming one directory. Canonicalised where the filesystem can say
/// so, literal where it cannot — a root not created yet still has to match.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// ── content adoption ─────────────────────────────────────────────────

/// What the adoption pass found. Bytes are counted, never moved.
struct Adoption {
    adopted: usize,
    /// Already carried a record — a re-run, or a peer's log that got here first.
    already_recorded: usize,
    unclaimed: usize,
    unreadable: usize,
    bytes: u64,
}

/// Record what is already in the tree, without touching a byte of it.
///
/// `at` is the file's modification time, **not now**: the engine preserves
/// mtimes, so two Devices adopting one tree write records that tie-break onto
/// one Item. `--adopt` is explicit, so the settle window does not apply.
fn adopt_content(
    root: &Path,
    device: &str,
    identity: &Identity,
    reserved: &[&'static str],
) -> Result<Adoption, Fault> {
    let provider = WallpaperProvider::default();

    // Every hash the Circle already has a record for, ours or a peer's.
    let mut recorded: std::collections::BTreeSet<String> = records::read_all(root, COLLECTION)
        .map_err(|e| Fault::Io(root.join(".wallsync/items"), e))?
        .iter()
        .filter_map(|r| match r {
            records::Record::Add { hash, .. } | records::Record::Bind { hash, .. } => {
                Some(hash.clone())
            }
            records::Record::Remove { .. } => None,
        })
        .collect();

    let mut found = Adoption {
        adopted: 0,
        already_recorded: 0,
        unclaimed: 0,
        unreadable: 0,
        bytes: 0,
    };

    for path in walk(root, reserved).map_err(|e| Fault::Io(root.to_path_buf(), e))? {
        let candidate = ImportCandidate {
            path: &path,
            mime: sniff(&path),
        };
        if !provider.claims(&candidate) {
            found.unclaimed += 1;
            continue;
        }

        let (size, at) = match fs::metadata(&path).map(|m| (m.len(), mtime(&m))) {
            Ok(facts) => facts,
            Err(_) => {
                found.unreadable += 1;
                continue;
            }
        };
        let Ok(hash) = crate::hash::hash_file(&path) else {
            found.unreadable += 1;
            continue;
        };

        if !recorded.insert(hash.clone()) {
            found.already_recorded += 1;
            continue;
        }

        let Some(rel) = relative(root, &path) else {
            found.unreadable += 1;
            continue;
        };
        let record = records::Record::Add {
            item: ItemId::generate(),
            by: identity.person.clone(),
            at,
            // The stem, verbatim. Prettifying is a guess; the name is theirs.
            title: title_of(&path),
            path: rel,
            hash,
            size,
        };
        records::append(root, COLLECTION, device, &record)
            .map_err(|e| Fault::Io(root.join(".wallsync/items"), e))?;
        found.adopted += 1;
        found.bytes += size;
    }

    Ok(found)
}

/// Every candidate file under the Collection root, depth-first, sorted. Never
/// walked: dot-entries at any depth, what the engine declares its own, symlinks.
fn walk(root: &Path, reserved: &[&'static str]) -> io::Result<Vec<PathBuf>> {
    let mut queue = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = queue.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory that vanished mid-walk is not a failed adoption.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
            Err(e) => return Err(e),
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // A glob may name an entry or a path inside the Circle, so both are
            // offered to it. Matching only the name would walk straight into a
            // reserved directory that is not a dot-entry.
            let rel = relative(root, &path);
            let engines = is_reserved(&name, reserved)
                || rel.as_deref().is_some_and(|r| is_reserved(r, reserved));
            if name.starts_with('.') || engines {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                queue.push(path);
            } else if kind.is_file() {
                files.push(path);
            }
        }
    }

    // Byte-wise, so two runs — and two Devices — see one tree in one order.
    files.sort();
    Ok(files)
}

/// Whether a name matches one of the Sync Engine's globs, never spelled here.
fn is_reserved(name: &str, reserved: &[&'static str]) -> bool {
    reserved.iter().any(|glob| glob_match(glob, name))
}

/// `*` matches within one path segment, `**` across them, `?` one character that
/// is not a separator. Deliberately not a full ignore-file implementation.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn matches(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                (0..=t.len()).any(|i| matches(&p[2..], &t[i..]))
            }
            Some(b'*') => {
                for i in 0..=t.len() {
                    if matches(&p[1..], &t[i..]) {
                        return true;
                    }
                    if t.get(i) == Some(&b'/') {
                        break;
                    }
                }
                false
            }
            Some(b'?') => matches!(t.first(), Some(c) if *c != b'/') && matches(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && matches(&p[1..], &t[1..]),
        }
    }
    matches(pattern.as_bytes(), text.as_bytes())
}

/// A bounded prefix, turned into a MIME guess for the Provider to judge. An
/// extension-less wallpaper is real in a tree wallsync adopts.
fn sniff(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = [0u8; 8192];
    let n = file.read(&mut buf).ok()?;
    let bytes = &buf[..n];

    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        "image/tiff"
    } else {
        return None;
    };
    Some(mime.to_string())
}

/// A record's path: relative to the Collection root, `/`-separated.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn title_of(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A file's modification time as RFC 3339 — the `at` an adopted record carries.
fn mtime(meta: &fs::Metadata) -> String {
    meta.modified()
        .ok()
        .and_then(|t| jiff::Timestamp::try_from(t).ok())
        .unwrap_or_else(jiff::Timestamp::now)
        .to_string()
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

// ── saying what happened ─────────────────────────────────────────────

impl Made {
    fn report(&self) {
        match &self.content {
            None => {
                println!(
                    "Created {} ({}) at {}.",
                    self.circle.name,
                    self.circle.id.0,
                    pretty(&self.root)
                );
                println!("  Collection  {} · {PROVIDER}", self.circle.name);
            }
            Some(found) => {
                println!("Adopted the existing wallpaper tree at {}.", pretty(&self.root));
                if self.kept_space {
                    println!(
                        "  circle      {} ({}) — wallsync kept the synced space it was already in; nothing moved",
                        self.circle.name, self.circle.id.0
                    );
                } else {
                    println!("  circle      {} ({})", self.circle.name, self.circle.id.0);
                }
                println!(
                    "  items       {} adopted (0 bytes copied, {} on disk)",
                    found.adopted,
                    bytes(found.bytes)
                );
                if found.already_recorded > 0 {
                    println!(
                        "  known       {} already had records — nothing was written twice",
                        found.already_recorded
                    );
                }
                if found.unclaimed > 0 {
                    println!(
                        "  ignored     {} files the wallpaper Provider does not claim — they still sync to every Member",
                        found.unclaimed
                    );
                }
                if found.unreadable > 0 {
                    println!("  unreadable  {} files wallsync could not read", found.unreadable);
                }
            }
        }

        match &self.stewardship {
            Stewardship::Ours => {
                println!(
                    "You are this Circle's Steward and its admin: invites and joins run on this Device."
                );
                // Verbatim, per cli-tui §7.2. stderr: narration, not data.
                eprintln!("Roles are agreements, not enforcement — admission is the only gate.");
                println!("Next: wallsync add <paths…>, then wallsync invite.");
            }
            Stewardship::Theirs(device) => {
                println!(
                    "This Circle's Steward Device is {} — invites and joins run there, not here.",
                    fingerprint(device)
                );
                eprintln!("Roles are agreements, not enforcement — admission is the only gate.");
            }
            Stewardship::Awaiting(device) => {
                println!(
                    "This Circle has no wallsync record yet; wallsync cannot name its Steward until {} runs wallsync.",
                    fingerprint(device)
                );
                println!("Everything else works now.");
            }
        }

        // Content arriving from a Circle must never change a screen.
        if self.content.is_some() && wp_sync_auto_apply_present() {
            eprintln!(
                "! wp-sync's automatic wallpaper apply is still enabled — new wallpapers would change your screen without you asking."
            );
            eprintln!("  → Stop it with: systemctl --user disable --now wp-sync-apply.path");
        }
    }
}

fn wp_sync_auto_apply_present() -> bool {
    directories::BaseDirs::new().is_some_and(|b| {
        b.config_dir()
            .join("systemd/user/wp-sync-apply.path")
            .exists()
    })
}

/// A path with the Person's home written the way they would write it.
fn pretty(path: &Path) -> String {
    let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    match home.and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ── failure ──────────────────────────────────────────────────────────

/// Why the Circle was not made. Every variant fails before anything is written.
#[derive(Debug)]
enum Fault {
    BadName(&'static str),
    NoHome,
    Engine(SyncError),
    DuplicateName(PathBuf),
    PathNotEmpty(PathBuf),
    AdoptNotADirectory(PathBuf),
    AdoptNotFound,
    AdoptAmbiguous(Vec<CircleRef>),
    Io(PathBuf, io::Error),
    Claim(PathBuf, io::Error),
}

impl Fault {
    fn exit(&self) -> i32 {
        match self {
            Fault::BadName(_) | Fault::DuplicateName(_) | Fault::PathNotEmpty(_) => EX_USAGE,
            Fault::AdoptAmbiguous(_) => EX_USAGE,
            Fault::AdoptNotFound | Fault::AdoptNotADirectory(_) => EX_DATA,
            Fault::NoHome => EX_CONFIG,
            Fault::Engine(SyncError::Unreachable | SyncError::Incompatible(_)) => EX_UNAVAILABLE,
            Fault::Engine(SyncError::Unauthorized) => EX_CONFIG,
            Fault::Engine(SyncError::NotFound) => EX_DATA,
            Fault::Engine(SyncError::Engine(_)) => EX_FAIL,
            Fault::Io(..) | Fault::Claim(..) => EX_FAIL,
        }
    }

    fn report(&self, address: &str) {
        match self {
            Fault::BadName(why) => {
                eprintln!("✗ That Circle name {why}.");
                eprintln!("  → Give it 1–64 printable characters: wallsync create walls");
            }
            Fault::NoHome => {
                eprintln!("✗ wallsync cannot work out where this Person's home directory is.");
                eprintln!("  → Say where the Circle should live: wallsync create <name> --path <dir>");
            }
            Fault::Engine(SyncError::Unreachable) => {
                eprintln!("✗ The Sync Engine is not answering at {address}.");
                eprintln!(
                    "  → Start it (Syncthing: systemctl --user start syncthing), then run wallsync doctor."
                );
            }
            Fault::Engine(SyncError::Unauthorized) => {
                eprintln!("✗ The Sync Engine at {address} rejected our credentials.");
                eprintln!(
                    "  → Check its API key. wallsync never rewrites the daemon's configuration; run wallsync doctor to see where the key was read from."
                );
            }
            Fault::Engine(SyncError::Incompatible(version)) => {
                eprintln!("✗ The Sync Engine at {address} is below the version wallsync needs ({version}).");
                eprintln!("  → Upgrade the daemon, then run wallsync doctor.");
            }
            Fault::Engine(e) => {
                eprintln!("✗ The Sync Engine could not create this Circle: {e}");
                eprintln!("  → Run wallsync doctor.");
            }
            Fault::DuplicateName(root) => {
                eprintln!("✗ This Device is already in a Circle by that name, at {}.", pretty(root));
                eprintln!("  → Pick another name. Circle names are display names; the Circle's id is what is unique.");
            }
            Fault::PathNotEmpty(root) => {
                eprintln!("✗ {} is not empty, and wallsync did not put anything there.", pretty(root));
                eprintln!(
                    "  → Choose an empty directory with --path, or adopt what is there with --adopt --path {}",
                    pretty(root)
                );
            }
            Fault::AdoptNotADirectory(root) => {
                eprintln!("✗ There is no directory at {} to adopt.", pretty(root));
                eprintln!("  → Point --path at the wallpaper directory you want wallsync to take over.");
            }
            Fault::AdoptNotFound => {
                eprintln!("✗ wallsync found no existing synced wallpaper directory to adopt.");
                eprintln!("  → Name it: wallsync create <name> --adopt --path <dir>");
            }
            Fault::AdoptAmbiguous(candidates) => {
                eprintln!("✗ wallsync found more than one synced directory it could adopt:");
                for c in candidates {
                    eprintln!("    {}  {}", c.id.0, pretty(&c.root));
                }
                eprintln!("  → Say which: wallsync create <name> --adopt --path <dir>");
            }
            Fault::Io(path, e) => {
                eprintln!("✗ {}: {e}", path.display());
                eprintln!("  → Nothing was left half-made. Fix that path and run the same command again.");
            }
            Fault::Claim(path, e) => {
                eprintln!("✗ This Device could not publish its Membership claim in {}: {e}", path.display());
                eprintln!("  → Until it does, the Circle sees a Device it cannot name. Run the same command again.");
            }
        }
    }
}

/// 1–64 characters with something visible among them; the `CircleId` is unique.
fn validate_name(name: &str) -> Result<String, Fault> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Fault::BadName("is empty"));
    }
    if name.chars().count() > 64 {
        return Err(Fault::BadName("is longer than 64 characters"));
    }
    if name.chars().any(char::is_control) {
        return Err(Fault::BadName("contains control characters"));
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::domain::PersonId;
    use crate::engine::{
        CircleOffer, CircleStatus, Cursor, DeviceId, EngineHealth, Envelope, InviteTicket,
        JoinRequest, PeerDevice, RelPath, Version,
    };

    const ANA_DEVICE: &str = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2";
    const BEN_DEVICE: &str = "K5J2FVL-B3QTXAO-7SWNDUE-HMR4YZI-6CPGA2N-XQTLB5V-JW3EOHY-RD6MSAK";

    // ── a Sync Engine that is entirely ours ──────────────────────────

    /// A test double for the seam — this file is tested with no daemon near it.
    struct FakeEngine {
        device: String,
        circles: Mutex<Vec<CircleRef>>,
        peers: Vec<PeerDevice>,
        left: Mutex<Vec<String>>,
        health: Option<SyncError>,
        creates: Mutex<u32>,
    }

    impl FakeEngine {
        fn new() -> Self {
            Self {
                device: ANA_DEVICE.to_string(),
                circles: Mutex::new(Vec::new()),
                peers: Vec::new(),
                left: Mutex::new(Vec::new()),
                health: None,
                creates: Mutex::new(0),
            }
        }

        fn replicating(mut self, id: &str, name: &str, root: &Path) -> Self {
            self.circles.get_mut().unwrap().push(CircleRef {
                id: CircleId(id.to_string()),
                name: name.to_string(),
                root: root.to_path_buf(),
            });
            self
        }

        fn with_introducer(mut self, device: &str) -> Self {
            self.peers.push(PeerDevice {
                device: DeviceId(device.to_string()),
                name: "ben-thinkpad".into(),
                connected: true,
                introducer: true,
            });
            self
        }
    }

    impl SyncEngine for FakeEngine {
        type Changes = tokio_stream::Empty<Envelope>;

        async fn health(&self) -> Result<EngineHealth, SyncError> {
            match &self.health {
                Some(SyncError::Unreachable) => Err(SyncError::Unreachable),
                Some(e) => Err(SyncError::Engine(e.to_string())),
                None => Ok(EngineHealth {
                    version: "2.0.4".into(),
                }),
            }
        }

        async fn local_device(&self) -> Result<DeviceId, SyncError> {
            Ok(DeviceId(self.device.clone()))
        }

        fn reserved_paths(&self) -> &[&'static str] {
            // The *shapes* the real implementation answers with, spelled with
            // words no engine has ever used: this module cannot recognise them.
            &[
                ".enginedir",
                "archive/**",
                "*.engine-conflict-*",
                ".engine.*.tmp",
                "~engine~*.tmp",
            ]
        }

        async fn create_circle(&self, name: &str, root: &Path) -> Result<CircleRef, SyncError> {
            *self.creates.lock().unwrap() += 1;
            let made = CircleRef {
                id: CircleId("wallsync-4tj2q9xa".into()),
                name: name.to_string(),
                root: root.to_path_buf(),
            };
            self.circles.lock().unwrap().push(made.clone());
            Ok(made)
        }

        async fn replicated_at(&self, root: &Path) -> Result<Option<CircleRef>, SyncError> {
            Ok(self.circles.lock().unwrap().iter().find(|c| c.root == root).cloned())
        }

        async fn circles(&self) -> Result<Vec<CircleRef>, SyncError> {
            Ok(self.circles.lock().unwrap().clone())
        }

        async fn begin_join(&self, _invite: &InviteTicket) -> Result<(), SyncError> {
            Err(SyncError::NotFound)
        }

        async fn complete_join(
            &self,
            _offer: &CircleOffer,
            _root: &Path,
        ) -> Result<CircleRef, SyncError> {
            Err(SyncError::NotFound)
        }

        async fn pending_joins(&self) -> Result<Vec<JoinRequest>, SyncError> {
            Ok(Vec::new())
        }

        async fn pending_circles(&self) -> Result<Vec<CircleOffer>, SyncError> {
            Ok(Vec::new())
        }

        async fn admit(&self, _c: &CircleId, _r: &JoinRequest) -> Result<(), SyncError> {
            Err(SyncError::NotFound)
        }

        async fn expel(&self, _c: &CircleId, _d: &DeviceId) -> Result<(), SyncError> {
            Err(SyncError::NotFound)
        }

        async fn leave(&self, circle: &CircleId) -> Result<(), SyncError> {
            self.left.lock().unwrap().push(circle.0.clone());
            self.circles.lock().unwrap().retain(|c| c.id.0 != circle.0);
            Ok(())
        }

        async fn set_introducer(&self, _d: &DeviceId, _f: bool) -> Result<(), SyncError> {
            Err(SyncError::NotFound)
        }

        async fn devices(&self, _c: &CircleId) -> Result<Vec<PeerDevice>, SyncError> {
            Ok(self.peers.clone())
        }

        async fn status(&self, _c: &CircleId) -> Result<CircleStatus, SyncError> {
            Ok(CircleStatus {
                state: "idle".into(),
                items: 0,
                bytes_needed: 0,
                peers: Vec::new(),
            })
        }

        async fn observe(&self, _resume: Option<Cursor>) -> Result<Self::Changes, SyncError> {
            Ok(tokio_stream::empty())
        }

        async fn versions(&self, _c: &CircleId, _p: &RelPath) -> Result<Vec<Version>, SyncError> {
            Ok(Vec::new())
        }

        async fn restore(
            &self,
            _c: &CircleId,
            _p: &RelPath,
            _v: &Version,
        ) -> Result<(), SyncError> {
            Err(SyncError::NotFound)
        }
    }

    // ── fixtures ─────────────────────────────────────────────────────

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A scratch directory, never in the Person's home.
    fn scratch(label: &str) -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wallsync-create-{}-{n}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn ana() -> Identity {
        Identity {
            schema: 1,
            person: PersonId::generate(),
            display_name: "Ana".into(),
            created: "2026-08-07T09:00:00Z".into(),
        }
    }

    fn png(path: &Path) {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).unwrap();
        }
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(path.to_string_lossy().as_bytes());
        fs::write(path, bytes).unwrap();
    }

    fn item_records(root: &Path) -> Vec<records::Record> {
        records::read_all(root, COLLECTION).unwrap()
    }

    // ── creating ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_fresh_create_writes_the_circle_its_collection_and_this_devices_claim() {
        let base = scratch("fresh");
        let root = base.join("walls");
        let engine = FakeEngine::new();
        let id = ana();

        let made = create(&engine, &id, "walls", Some(root.to_str().unwrap()), false)
            .await
            .expect("a fresh Circle");

        let circle = descriptors::read_circle(&root).unwrap().unwrap();
        assert_eq!(circle.name, "walls");
        assert_eq!(circle.id, made.circle.id.0);
        assert_eq!(circle.founder_person, id.person.to_string());
        assert_eq!(
            circle.founder_device, ANA_DEVICE,
            "the Circle remembers its Steward's Device, and reads it from here forever after"
        );

        let collection = descriptors::read_collection(&root, "main").unwrap().unwrap();
        assert_eq!(collection.provider, "wallpaper");

        let people = claims::derive_people(&claims::read_all(&root).unwrap());
        assert_eq!(people.len(), 1, "the founder is nameable immediately");
        assert_eq!(people[0].display_name, "Ana");
        assert_eq!(people[0].devices, vec![ANA_DEVICE.to_string()]);

        // The staging suffix is ignored from sync before the first descriptor.
        let ignores = fs::read_to_string(root.join(".stignore")).unwrap();
        assert!(ignores.contains("(?d).wallsync/local"), "{ignores}");
        assert!(ignores.contains("(?d)*.wallsync-tmp"), "{ignores}");
        // Conflict copies must keep replicating, so nothing here stops them.
        assert!(!ignores.contains("sync-conflict"), "{ignores}");

        assert!(made.content.is_none(), "a fresh Circle adopts nothing");
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn re_running_finishes_a_half_made_circle_without_making_a_second_one() {
        let base = scratch("resume");
        let root = base.join("walls");
        fs::create_dir_all(&root).unwrap();
        let engine = FakeEngine::new().replicating("wallsync-4tj2q9xa", "walls", &root);
        let id = ana();

        create(&engine, &id, "walls", Some(root.to_str().unwrap()), false)
            .await
            .expect("adopting the space it already made");

        assert_eq!(
            *engine.creates.lock().unwrap(),
            0,
            "the space was already there; nothing is allocated twice"
        );
        assert!(descriptors::read_circle(&root).unwrap().is_some());

        let before = fs::read_to_string(descriptors::circle_path(&root)).unwrap();
        create(&engine, &id, "walls", Some(root.to_str().unwrap()), false)
            .await
            .expect("a second run changes nothing");
        let after = fs::read_to_string(descriptors::circle_path(&root)).unwrap();

        assert_eq!(before, after, "the descriptor is write-once, even by its own writer");
        assert_eq!(engine.circles.lock().unwrap().len(), 1);
        assert_eq!(claims::read_all(&root).unwrap().len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn a_directory_with_someone_elses_content_is_refused_rather_than_moved_into() {
        let base = scratch("occupied");
        let root = base.join("pictures");
        png(&root.join("sunset.png"));
        let engine = FakeEngine::new();

        let fault = create(&engine, &ana(), "walls", Some(root.to_str().unwrap()), false)
            .await
            .err()
            .expect("wallsync does not move into an occupied directory");

        assert!(matches!(fault, Fault::PathNotEmpty(_)));
        assert_eq!(fault.exit(), EX_USAGE);
        assert!(!root.join(".wallsync").exists(), "nothing was written");
        assert_eq!(*engine.creates.lock().unwrap(), 0);
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn a_circle_by_that_name_already_here_is_refused_and_not_renamed() {
        let base = scratch("duplicate");
        let engine = FakeEngine::new().replicating("wallsync-aaaaaaaa", "Walls", &base.join("other"));

        let fault = create(
            &engine,
            &ana(),
            "walls",
            Some(base.join("walls").to_str().unwrap()),
            false,
        )
        .await
        .err()
        .expect("one name, one Circle, on this Device");

        assert!(matches!(fault, Fault::DuplicateName(_)));
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_engine_refuses_before_anything_is_written() {
        let base = scratch("unreachable");
        let root = base.join("walls");
        let mut engine = FakeEngine::new();
        engine.health = Some(SyncError::Unreachable);

        let fault = create(&engine, &ana(), "walls", Some(root.to_str().unwrap()), false)
            .await
            .err()
            .expect("creating a Circle writes engine config; wallsync will not queue that");

        assert_eq!(fault.exit(), EX_UNAVAILABLE);
        assert!(!root.exists(), "not even the root was made");
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn a_failed_descriptor_write_undoes_the_space_it_just_created() {
        let base = scratch("rollback");
        let root = base.join("walls");
        // A directory where `circle.toml` belongs: the rename cannot land.
        fs::create_dir_all(descriptors::circle_path(&root)).unwrap();
        let engine = FakeEngine::new();

        let fault = create(&engine, &ana(), "walls", Some(root.to_str().unwrap()), true)
            .await
            .err()
            .expect("the descriptor cannot be written");

        assert!(matches!(fault, Fault::Io(..)));
        assert_eq!(
            engine.left.lock().unwrap().as_slice(),
            ["wallsync-4tj2q9xa"],
            "the space wallsync allocated is given back"
        );
        assert!(root.exists(), "an adopted root is never removed — wallsync did not create it");
        fs::remove_dir_all(&base).unwrap();
    }

    // ── adopting ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn adopting_records_every_wallpaper_it_finds_and_moves_no_bytes() {
        let base = scratch("adopt");
        let root = base.join("Wallpapers");
        png(&root.join("sunset.png"));
        png(&root.join("nature/forest-4k.png"));
        let engine = FakeEngine::new().replicating("wallpapers", "Wallpapers", &root);
        let id = ana();

        let made = create(&engine, &id, "walls", Some(root.to_str().unwrap()), true)
            .await
            .expect("adoption");

        let found = made.content.expect("the content pass ran");
        assert_eq!(found.adopted, 2);
        assert_eq!(*engine.creates.lock().unwrap(), 0, "same space, same peers");
        assert!(root.join("sunset.png").is_file(), "not one byte moved");
        assert!(root.join("nature/forest-4k.png").is_file());

        let records = item_records(&root);
        assert_eq!(records.len(), 2);
        for record in &records {
            let records::Record::Add {
                by, at, path, title, ..
            } = record
            else {
                panic!("adoption writes `add` records and nothing else");
            };
            assert_eq!(by, &id.person, "attributed to the Person, never the Device");

            // ADR-0004 §4.5: `at` is the file's own mtime, so two Devices
            // adopting one tree write records that tie-break to the same Item.
            let file = root.join(path);
            let expected = mtime(&fs::metadata(&file).unwrap());
            assert_eq!(at, &expected, "{path} was dated from the bytes, not from now");
            assert_eq!(title, &title_of(&file), "the stem, verbatim");
        }

        let paths: Vec<String> = records
            .iter()
            .map(|r| match r {
                records::Record::Add { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert!(
            paths.contains(&"nature/forest-4k.png".to_string()),
            "a nested wallpaper is an ordinary Item: {paths:?}"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn adopting_twice_writes_nothing_the_second_time() {
        let base = scratch("adopt-twice");
        let root = base.join("Wallpapers");
        png(&root.join("sunset.png"));
        let engine = FakeEngine::new().replicating("wallpapers", "Wallpapers", &root);
        let id = ana();

        create(&engine, &id, "walls", Some(root.to_str().unwrap()), true)
            .await
            .unwrap();
        let again = create(&engine, &id, "walls", Some(root.to_str().unwrap()), true)
            .await
            .unwrap();

        let found = again.content.unwrap();
        assert_eq!(found.adopted, 0);
        assert_eq!(found.already_recorded, 1);
        assert_eq!(item_records(&root).len(), 1, "one Item, one record");
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn adoption_never_touches_engine_artefacts_dot_entries_symlinks_or_unclaimed_content() {
        let base = scratch("adopt-skips");
        let root = base.join("Wallpapers");
        png(&root.join("sunset.png"));
        png(&root.join(".hidden/secret.png"));
        png(&root.join(".enginedir/sunset.png"));
        // A reserved subtree that is not a dot-entry: the walk has to read the
        // glob against the path, not only against the name.
        png(&root.join("archive/last-year.png"));
        png(&root.join("sunset.engine-conflict-20260807-091402-K5J2FVL.png"));
        png(&root.join(".engine.sunset.png.tmp"));
        fs::write(root.join("notes.txt"), "not a wallpaper").unwrap();
        // An extension-less wallpaper, found by its magic bytes.
        png(&root.join("bare-image"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("sunset.png"), root.join("link.png")).unwrap();

        let engine = FakeEngine::new().replicating("wallpapers", "Wallpapers", &root);
        let made = create(&engine, &ana(), "walls", Some(root.to_str().unwrap()), true)
            .await
            .unwrap();

        let found = made.content.unwrap();
        assert_eq!(found.adopted, 2, "sunset.png and bare-image, nothing else");
        assert_eq!(found.unclaimed, 1, "notes.txt is not claimed and is not deleted");

        let paths: Vec<String> = item_records(&root)
            .iter()
            .map(|r| match r {
                records::Record::Add { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert!(paths.contains(&"sunset.png".to_string()), "{paths:?}");
        assert!(paths.contains(&"bare-image".to_string()), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("conflict")), "{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with('.')), "{paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with("archive/")), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("link")), "{paths:?}");
        assert!(root.join("notes.txt").is_file(), "wallsync deletes nothing it did not create");
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn adopting_beside_a_peer_that_admits_joins_writes_no_descriptor() {
        let base = scratch("adopt-member");
        let root = base.join("Wallpapers");
        png(&root.join("sunset.png"));
        let engine = FakeEngine::new()
            .replicating("wallpapers", "Wallpapers", &root)
            .with_introducer(BEN_DEVICE);

        let made = create(&engine, &ana(), "walls", Some(root.to_str().unwrap()), true)
            .await
            .unwrap();

        assert!(matches!(made.stewardship, Stewardship::Awaiting(_)));
        assert!(
            descriptors::read_circle(&root).unwrap().is_none(),
            "the Steward's Device writes that, and its descriptor will arrive"
        );
        // The Collection still works: a claim, and records for what is here.
        assert_eq!(claims::read_all(&root).unwrap().len(), 1);
        assert_eq!(item_records(&root).len(), 1);
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn adopting_into_an_existing_descriptor_leaves_it_exactly_as_found() {
        let base = scratch("adopt-existing");
        let root = base.join("Wallpapers");
        png(&root.join("sunset.png"));
        let theirs = CircleDescriptor {
            schema: descriptors::SCHEMA,
            id: "wallsync-bbbbbbbb".into(),
            name: "walls".into(),
            created: "2026-08-01T09:00:00Z".into(),
            founder_person: "p-01k1yfq2m7vj3w8t0pz4rxab6c".into(),
            founder_device: BEN_DEVICE.into(),
        };
        descriptors::write_circle(&root, &theirs).unwrap();
        let engine = FakeEngine::new().replicating("wallsync-bbbbbbbb", "walls", &root);

        let made = create(&engine, &ana(), "walls", Some(root.to_str().unwrap()), true)
            .await
            .unwrap();

        assert!(matches!(made.stewardship, Stewardship::Theirs(_)));
        assert_eq!(
            descriptors::read_circle(&root).unwrap().unwrap(),
            theirs,
            "write-once means write-once, including for a Device adopting into it"
        );
        assert_eq!(claims::read_all(&root).unwrap().len(), 1, "Ana is nameable here");
        fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn adopt_with_nothing_to_adopt_says_so_rather_than_creating_something() {
        let engine = FakeEngine::new();
        let fault = create(&engine, &ana(), "walls", None, true)
            .await
            .err()
            .expect("there is no tree here");
        assert!(matches!(fault, Fault::AdoptNotFound));
        assert_eq!(fault.exit(), EX_DATA);
        assert_eq!(*engine.creates.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn adopt_with_several_candidates_asks_which_one() {
        let base = scratch("adopt-many");
        let a = base.join("one");
        let b = base.join("two");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let engine = FakeEngine::new()
            .replicating("legacy-one", "one", &a)
            .replicating("legacy-two", "two", &b);

        let fault = create(&engine, &ana(), "walls", None, true)
            .await
            .err()
            .expect("wallsync does not guess which tree a Person meant");
        assert!(matches!(fault, Fault::AdoptAmbiguous(c) if c.len() == 2));
        fs::remove_dir_all(&base).unwrap();
    }

    // ── the small rules ──────────────────────────────────────────────

    #[test]
    fn a_slug_is_lowercase_and_free_of_runs() {
        assert_eq!(slug("walls"), "walls");
        assert_eq!(slug("Ana & Ben's Walls"), "ana-ben-s-walls");
        assert_eq!(slug("  spaced  out  "), "spaced-out");
        assert_eq!(slug("🌅"), "circle", "a name still needs somewhere to live");
    }

    #[test]
    fn a_name_is_a_display_string_within_bounds() {
        assert_eq!(validate_name("  walls  ").unwrap(), "walls");
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"w".repeat(65)).is_err());
        assert!(validate_name("wa\nlls").is_err());
        assert!(validate_name("wällş 🌅").is_ok(), "names are the Person's own");
    }

    /// Every shape the seam's globs come in, spelled with words no engine uses.
    #[test]
    fn the_glob_matcher_reads_every_shape_a_reserved_path_comes_in() {
        assert!(glob_match("*.engine-conflict-*", "sunset.engine-conflict-2026-K5J.png"));
        assert!(!glob_match("*.engine-conflict-*", "sunset.png"));
        assert!(glob_match(".engine.*.tmp", ".engine.sunset.png.tmp"));
        assert!(glob_match("~engine~*.tmp", "~engine~sunset.tmp"));
        assert!(glob_match(".enginedir", ".enginedir"));
        assert!(!glob_match(".enginedir", ".enginedirs"));
        // `*` stops at a separator; `**` does not.
        assert!(!glob_match("*.png", "nature/forest.png"));
        assert!(glob_match("archive/**", "archive/deep/sunset.png"));
        assert!(glob_match("?.png", "a.png"));
        assert!(!glob_match("?.png", "/.png"));
    }

    #[test]
    fn magic_bytes_name_a_wallpaper_that_has_no_extension() {
        let dir = scratch("sniff");
        let image = dir.join("bare");
        png(&image);
        assert_eq!(sniff(&image).as_deref(), Some("image/png"));

        let text = dir.join("notes");
        fs::write(&text, "hello").unwrap();
        assert_eq!(sniff(&text), None, "the Provider's extension rule still applies");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_path_is_relative_and_slash_separated() {
        let root = Path::new("/tmp/walls");
        assert_eq!(
            relative(root, &root.join("nature/forest.png")).as_deref(),
            Some("nature/forest.png")
        );
        assert_eq!(relative(root, Path::new("/etc/passwd")), None);
        assert_eq!(relative(root, root), None);
    }

    #[test]
    fn a_fingerprint_is_eight_characters_grouped_four_and_four() {
        assert_eq!(fingerprint(ANA_DEVICE), "P56I-OI7M");
        assert_eq!(fingerprint("abc"), "ABC");
    }
}
