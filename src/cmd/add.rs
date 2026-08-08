//! `kith add <PATH>…` — the only way bytes become Items in v0.1.
//!
//! The module owns one question: *what should enter this Collection, and what
//! should be refused.* Everything it writes is one `add` record per accepted
//! candidate. Four decisions hold the flow up:
//!
//! * **Copy, never move.** A bad `kith add` is undone by removing Items; a bad
//!   move is undone by nothing.
//! * **Register in place** when the path is already inside the Circle root — no
//!   copy, no rename, no byte movement.
//! * **The Provider is the gate.** Content it does not `claim` is refused with a
//!   message, never silently accepted.
//! * **Bytes before record.** Staged, verified, published, then recorded: a crash
//!   in the window leaves an orphan the next reconcile adopts, never a record
//!   advertising bytes that were never staged.
//!
//! Import never talks to the Sync Engine. The engine is consulted once, before
//! any of this, for where this Device's Circles are and what its identity is.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::domain::{ItemId, PersonId};
use crate::engine::SyncEngine;
use crate::engine::syncthing::SyncthingEngine;
use crate::provider::wallpaper::WallpaperProvider;
use crate::provider::{ImportCandidate, Provider};
use crate::store::records::{self, Record};
use crate::store::descriptors;
use crate::{config, hash, identity};

// Sysexits, so the whole binary speaks one dialect.
const EX_OK: i32 = 0;
const EX_FAIL: i32 = 1;
const EX_USAGE: i32 = 64;
const EX_DATA: i32 = 65;
const EX_UNAVAILABLE: i32 = 69;
const EX_INTERNAL: i32 = 70;

/// v0.1's sole Collection id; everywhere else the id is an opaque string.
const MAIN: &str = "main";

/// The one Provider this build registers.
const WALLPAPER: &str = "wallpaper";

/// How much of a candidate is read to guess its type. 8 KiB rather than 512 B
/// because a text-shaped format's prologue can push its root element past a
/// smaller window.
const SNIFF_LEN: usize = 8 * 1024;

/// Copy buffer, matching the hasher's.
const COPY_BUF: usize = 1024 * 1024;

/// Above either of these a plan is confirmed rather than run.
const CONFIRM_ITEMS: usize = 500;
const CONFIRM_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Headroom left on the filesystem: a Collection that fills its own disk takes
/// the Person's session down with it.
const SPACE_MARGIN: u64 = 64 * 1024 * 1024;

/// Staged bytes older than this are litter from a crashed run, not state.
const STAGING_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How often the run narrates itself on stderr.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// ENOSPC. Named by number because the portable spelling of "the disk is full"
/// has moved between Rust releases and this check must not.
const ENOSPC: i32 = 28;

/// Import the named paths into the active Circle's Collection.
///
/// Returns the worst outcome of the run: 65 for a candidate refused or unread, 1
/// for an I/O failure mid-copy, 0 otherwise. `--dry-run`, `--move`, `--circle`
/// and `--yes` are not in this build's signature.
pub async fn run(paths: &[String]) -> i32 {
    if paths.is_empty() {
        eprintln!("kith add <PATH>… — the wallpapers to bring into this Circle");
        return EX_USAGE;
    }

    let job = match resolve().await {
        Ok(job) => job,
        Err(code) => return code,
    };
    let sources: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();

    // The Provider seam is synchronous and the whole import is blocking work, so
    // the import runs on `spawn_blocking` in one place rather than per call site.
    match tokio::task::spawn_blocking(move || import(&job, &sources)).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("kith add: the import task did not finish: {e}");
            EX_INTERNAL
        }
    }
}

// ── resolving what to import into ────────────────────────────────────

/// Everything the import needs, resolved once and never re-consulted.
struct Job {
    /// The Circle root, which in v0.1 is also the Collection root.
    root: PathBuf,
    circle_name: String,
    collection: String,
    provider: Box<dyn Provider>,
    /// The writing Device — the log's filename.
    device: String,
    /// The Person every record is attributed to. Asserted, never proven.
    person: PersonId,
    /// Every *other* Circle root this Device holds, excluded from recursion so
    /// `kith add ~/Pictures` cannot swallow a Circle that lives inside it.
    other_roots: Vec<PathBuf>,
    /// The globs the Sync Engine owns inside a Circle root, consumed as data.
    reserved: Vec<&'static str>,
    /// Whether this run may ask a Person a question.
    interactive: bool,
}

/// Resolve the Identity, the Device, the Circle and its Collection.
///
/// The import itself needs no engine, but *finding* the tree does: this Device's
/// Circle roots and its own Device id are engine facts and nothing caches them
/// yet, so an unreachable engine is reported rather than papered over.
async fn resolve() -> Result<Job, i32> {
    let identity = match identity::load() {
        Ok(Some(id)) => id,
        Ok(None) => {
            eprintln!("kith add: this Device has no Identity yet");
            eprintln!("  → Run: kith init <your name>");
            return Err(EX_USAGE);
        }
        Err(e) => {
            eprintln!("kith add: {e}");
            return Err(EX_USAGE);
        }
    };

    let creds = SyncthingEngine::discover().map_err(|e| {
        eprintln!("kith add: no Sync Engine configuration found ({e})");
        eprintln!("  → Run: kith doctor");
        EX_UNAVAILABLE
    })?;
    let engine = SyncthingEngine::new(creds);

    let circles = engine.circles().await.map_err(|e| {
        eprintln!("kith add: the Sync Engine did not answer ({e})");
        eprintln!("  → kith adapts a daemon you run; start it, then run: kith doctor");
        EX_UNAVAILABLE
    })?;
    let device = engine.local_device().await.map_err(|e| {
        eprintln!("kith add: the Sync Engine could not name this Device ({e})");
        EX_UNAVAILABLE
    })?;

    // The sole Circle, else refuse: the CLI never guesses from history, and
    // `--circle` is not in this build's signature.
    let chosen = match circles.len() {
        1 => circles[0].clone(),
        0 => {
            eprintln!("kith add: you are in no Circles yet");
            eprintln!("  → Run: kith create <name>, or join one with an Invite");
            return Err(EX_USAGE);
        }
        n => {
            eprintln!("kith add: you are in {n} Circles; this build cannot be told which one");
            for c in &circles {
                eprintln!("    {}", label(&c.name, &c.id.0));
            }
            return Err(EX_USAGE);
        }
    };
    let root = chosen.root.clone();

    // An adopted Circle whose Steward's Device has not run kith yet has no
    // descriptor: the v0.1 literals stand in and the gap is said out loud.
    let descriptor = match descriptors::read_collections(&root) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("kith add: {e}");
            return Err(EX_DATA);
        }
    };
    let collection = match descriptor.len() {
        0 => {
            eprintln!(
                "! this Circle has no Collection descriptor yet — using \"{MAIN}\" with the \
                 {WALLPAPER} Provider until it arrives"
            );
            None
        }
        1 => Some(descriptor.into_iter().next().expect("exactly one")),
        n => {
            // Forward compatibility: degraded, never broken.
            let pick = descriptor
                .iter()
                .find(|d| d.collection == MAIN)
                .or_else(|| descriptor.first())
                .cloned();
            eprintln!(
                "! {} has {n} Collections; this version uses 1 ({}) — upgrade to see the others",
                label(&chosen.name, &chosen.id.0),
                pick.as_ref().map(|d| d.collection.as_str()).unwrap_or(MAIN)
            );
            pick
        }
    };

    let (collection_id, provider_id) = match &collection {
        Some(d) => (d.collection.clone(), d.provider.clone()),
        None => (MAIN.to_string(), WALLPAPER.to_string()),
    };
    if provider_id != WALLPAPER {
        // No file is imported, so the Collection stays empty rather than wrong.
        eprintln!(
            "kith add: this Collection uses the \"{provider_id}\" Provider, which this version \
             does not have"
        );
        return Err(EX_DATA);
    }

    let circle_name = descriptors::read_circle(&root)
        .ok()
        .flatten()
        .map(|d| d.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| label(&chosen.name, &chosen.id.0));

    let other_roots = circles
        .iter()
        .filter(|c| c.id != chosen.id)
        .map(|c| canonical(&c.root))
        .collect();

    Ok(Job {
        root,
        circle_name,
        collection: collection_id,
        provider: Box::new(WallpaperProvider::new(config::load().apply_command)),
        device: device.0,
        person: identity.person,
        other_roots,
        reserved: engine.reserved_paths().to_vec(),
        interactive: io::stderr().is_terminal() && io::stdin().is_terminal(),
    })
}

fn label(name: &str, id: &str) -> String {
    if name.is_empty() { id.to_string() } else { name.to_string() }
}

// ── the plan ─────────────────────────────────────────────────────────

/// What a candidate turns out to be. The plan writes nothing, so every verdict
/// is reached before a single byte moves.
enum Verdict {
    /// Copy into `dest`, then record.
    Import { item: ItemId, dest: String, revives: bool },
    /// Already inside the Collection root, or byte-identical to a file that is:
    /// record only, no copy, no rename.
    Register { item: ItemId, dest: String, revives: bool },
    /// A live Item already has these bytes. Skipped, and never an error.
    Duplicate,
    /// The Provider does not claim it. Refused with a reason.
    Unclaimed { reason: String },
    /// Vanished mid-walk, unreadable, or not nameable.
    Unreadable { error: String },
}

struct Entry {
    source: PathBuf,
    size: u64,
    hash: String,
    verdict: Verdict,
}

struct Plan {
    entries: Vec<Entry>,
    /// Bytes an `Import` would copy. `Register` entries copy nothing.
    import_bytes: u64,
    import_count: usize,
}

/// One candidate found by the walk, with the destination its argument implies.
struct Found {
    path: PathBuf,
    /// Directory segments below a directory argument, preserved as given. A file
    /// argument lands at the Collection root and has none.
    dirs: Vec<String>,
    name: String,
}

fn import(job: &Job, sources: &[PathBuf]) -> i32 {
    // Litter from a crashed run, swept before anything is staged beside it.
    sweep_staging(&staging_dir(&job.root));

    let plan = plan(job, sources);

    // `--yes` is not in this build's signature, so a large plan on a non-terminal
    // is refused with its numbers rather than imported unasked.
    let big = plan.import_count > CONFIRM_ITEMS || plan.import_bytes > CONFIRM_BYTES;
    if big {
        let question = format!(
            "This would import {} Items ({}) into {}.",
            plan.import_count,
            bytes(plan.import_bytes),
            job.circle_name
        );
        if !job.interactive {
            eprintln!("kith add: {question}");
            eprintln!("  → Run it from a terminal so it can be confirmed. Nothing was copied.");
            return EX_USAGE;
        }
        if !confirm(&question) {
            eprintln!("Nothing was copied.");
            return EX_OK;
        }
    }

    // Refused before the first copy rather than discovered halfway through it.
    let free = free_bytes(&job.root);
    if !fits(plan.import_bytes, free) {
        eprintln!(
            "kith add: importing {} Items needs {}; {} has {} free. Nothing was copied.",
            plan.import_count,
            bytes(plan.import_bytes),
            job.root.display(),
            free.map(bytes).unwrap_or_else(|| "no".into())
        );
        return EX_FAIL;
    }

    let report = execute(job, &plan);
    render(job, &report)
}

/// Whether a plan fits, given what the filesystem says it has. A margin is always
/// left, and a filesystem this build cannot ask (`None`) fits: a failed copy is
/// cleaned up, a refusal nobody can explain is not.
fn fits(need: u64, free: Option<u64>) -> bool {
    match free {
        Some(free) => need.saturating_add(SPACE_MARGIN) <= free,
        None => true,
    }
}

fn plan(job: &Job, sources: &[PathBuf]) -> Plan {
    let mut found = Vec::new();
    let mut entries = Vec::new();
    collect(job, sources, &mut found, &mut entries);

    // Dedup is against **live** Items only, so bytes matching a tombstoned Item
    // are imported and the revival rule brings the original Item back.
    let records = records::read_all(&job.root, &job.collection).unwrap_or_default();
    let live: BTreeSet<String> = records::derive_items(&records, &job.root)
        .into_iter()
        .filter_map(|i| i.hash)
        .collect();
    let mut retired: BTreeSet<&str> = BTreeSet::new();
    for rec in &records {
        if let Record::Add { hash, .. } = rec
            && !hash.is_empty()
            && !live.contains(hash)
        {
            retired.insert(hash.as_str());
        }
    }

    // Two arguments naming one image is one Item, not two.
    let mut claimed_here: BTreeSet<String> = BTreeSet::new();
    // Destinations already spoken for, and what the root already holds.
    let mut placer = Placer::default();

    let mut import_bytes = 0u64;
    let mut import_count = 0usize;

    for candidate in found {
        let entry = judge(
            job,
            &candidate,
            &live,
            &retired,
            &mut claimed_here,
            &mut placer,
        );
        if let Verdict::Import { .. } = entry.verdict {
            import_bytes += entry.size;
            import_count += 1;
        }
        entries.push(entry);
    }

    Plan { entries, import_bytes, import_count }
}

/// One candidate, from bytes on disk to a verdict.
fn judge(
    job: &Job,
    candidate: &Found,
    live: &BTreeSet<String>,
    retired: &BTreeSet<&str>,
    claimed_here: &mut BTreeSet<String>,
    placer: &mut Placer,
) -> Entry {
    let source = candidate.path.clone();

    let size = match fs::metadata(&source) {
        Ok(m) => m.len(),
        Err(e) => return unreadable(source, e.to_string()),
    };

    // The Provider gate, first and cheaply: a bounded prefix, a MIME guess, and
    // the Provider's own answer.
    let mime = sniff(&prefix(&source));
    let claims = job.provider.claims(&ImportCandidate {
        path: &source,
        mime: mime.clone(),
    });
    if !claims {
        let reason = match &mime {
            Some(m) => format!("not claimed by the {} Provider ({m})", job.provider.id()),
            None => format!("not claimed by the {} Provider", job.provider.id()),
        };
        return Entry {
            source,
            size,
            hash: String::new(),
            verdict: Verdict::Unclaimed { reason },
        };
    }

    // The only full read of the source; the copy phase verifies against this.
    let digest = match hash::hash_file(&source) {
        Ok(h) => h,
        Err(e) => return unreadable(source, e.to_string()),
    };

    if live.contains(&digest) || claimed_here.contains(&digest) {
        return Entry { source, size, hash: digest, verdict: Verdict::Duplicate };
    }
    let revives = retired.contains(digest.as_str());
    claimed_here.insert(digest.clone());

    // Already inside the Collection root: register in place, no byte movement.
    if let Some(rel) = relative_to(&job.root, &source) {
        placer.reserve(&rel);
        return Entry {
            source,
            size,
            hash: digest,
            verdict: Verdict::Register { item: ItemId::generate(), dest: rel, revives },
        };
    }

    let item = ItemId::generate();
    let name = sanitise(&candidate.name, item.as_str());
    let dir = candidate.dirs.join("/");
    match placer.claim(&job.root, &dir, &name, &digest) {
        Placement::Free(dest) => {
            Entry { source, size, hash: digest, verdict: Verdict::Import { item, dest, revives } }
        }
        // An orphan the reconcile would have adopted anyway: recording it beats
        // copying it beside itself.
        Placement::SameBytes(dest) => {
            Entry { source, size, hash: digest, verdict: Verdict::Register { item, dest, revives } }
        }
        Placement::Impossible(error) => unreadable(source, error),
    }
}

fn unreadable(source: PathBuf, error: String) -> Entry {
    Entry { source, size: 0, hash: String::new(), verdict: Verdict::Unreadable { error } }
}

// ── the walk ─────────────────────────────────────────────────────────

/// Expand the arguments into candidates, depth-first and in a stable order.
fn collect(job: &Job, sources: &[PathBuf], out: &mut Vec<Found>, refused: &mut Vec<Entry>) {
    for source in sources {
        // A symlink *argument* imports its target's bytes and leaves the link
        // alone; one found during recursion is skipped. kith does not sync links.
        let meta = match fs::metadata(source) {
            Ok(m) => m,
            Err(e) => {
                refused.push(unreadable(source.clone(), e.to_string()));
                continue;
            }
        };

        if meta.is_dir() {
            // A directory named as an argument is walked even when it is the
            // Collection root: that argument *is* the register-in-place request.
            // The exclusion below applies only to what recursion finds.
            walk(job, source, &mut Vec::new(), out, refused);
        } else if meta.is_file() {
            match source.file_name().and_then(|n| n.to_str()) {
                Some(name) => out.push(Found {
                    path: source.clone(),
                    dirs: Vec::new(),
                    name: name.to_string(),
                }),
                None => refused.push(unreadable(
                    source.clone(),
                    "this name is not valid text".into(),
                )),
            }
        }
    }
}

fn walk(job: &Job, dir: &Path, prefix: &mut Vec<String>, out: &mut Vec<Found>, refused: &mut Vec<Entry>) {
    let mut names: Vec<String> = Vec::new();
    let listing = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            refused.push(unreadable(dir.to_path_buf(), e.to_string()));
            return;
        }
    };
    for entry in listing.flatten() {
        match entry.file_name().to_str() {
            Some(name) => names.push(name.to_string()),
            None => refused.push(unreadable(entry.path(), "this name is not valid text".into())),
        }
    }
    // Sorted, so two runs over one tree produce the same order and therefore the
    // same collision suffixes.
    names.sort();

    for name in names {
        // Dot-entries at any depth: `.kith/` and every dot-named engine artefact.
        if name.starts_with('.') {
            continue;
        }
        if is_reserved(job, &name) {
            continue;
        }

        let path = dir.join(&name);
        // Not followed and not imported: a symlink is not an Item.
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }

        if meta.is_dir() {
            // Never eat your own tail.
            let here = canonical(&path);
            if here == canonical(&job.root) || job.other_roots.contains(&here) {
                continue;
            }
            prefix.push(name);
            walk(job, &path, prefix, out, refused);
            prefix.pop();
        } else if meta.is_file() {
            out.push(Found { path, dirs: prefix.clone(), name });
        }
    }
}

/// Whether an entry name is one the Sync Engine owns. A glob naming a directory's
/// contents (`x/**`) is matched against the directory itself, which is what stops
/// the walk one level earlier.
fn is_reserved(job: &Job, name: &str) -> bool {
    job.reserved.iter().any(|glob| {
        let head = glob.split('/').next().unwrap_or(glob);
        glob_match(head, name)
    })
}

/// `*`-only glob matching, which is all the reserved globs use.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = name.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);

    while t < text.len() {
        if p < pat.len() && (pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

// ── destinations ─────────────────────────────────────────────────────

enum Placement {
    Free(String),
    /// A file already there holds exactly these bytes.
    SameBytes(String),
    Impossible(String),
}

/// Chooses destinations, and remembers what the Collection root already holds.
///
/// The index makes the comparison **case-insensitive** rather than merely
/// `exists()`-shaped: `sunset.png` beside `Sunset.png` is a pair no Member on a
/// case-insensitive filesystem can hold. Listing each directory once also keeps a
/// large import linear.
#[derive(Default)]
struct Placer {
    /// Lowercased relative path → the relative path as it is actually spelled.
    seen: BTreeMap<String, String>,
    /// Directories already listed.
    listed: BTreeSet<String>,
    /// Lowercased relative paths this run has already spoken for.
    taken: BTreeSet<String>,
}

impl Placer {
    fn ensure(&mut self, root: &Path, dir: &str) {
        if !self.listed.insert(dir.to_string()) {
            return;
        }
        let full = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir.replace('/', std::path::MAIN_SEPARATOR_STR))
        };
        let Ok(entries) = fs::read_dir(full) else { return };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            self.seen.insert(rel.to_lowercase(), rel);
        }
    }

    /// Reserve a relative destination, suffixing on collision. Identical bytes are
    /// reported rather than duplicated, and the incumbent keeps its own name.
    fn claim(&mut self, root: &Path, dir: &str, name: &str, digest: &str) -> Placement {
        self.ensure(root, dir);
        let (stem, ext) = split_extension(name);

        for n in 1..=1000u32 {
            let candidate = if n == 1 {
                name.to_string()
            } else {
                match ext {
                    Some(e) => format!("{stem}-{n}.{e}"),
                    None => format!("{stem}-{n}"),
                }
            };
            let rel = if dir.is_empty() { candidate } else { format!("{dir}/{candidate}") };
            let key = rel.to_lowercase();
            if self.taken.contains(&key) {
                continue;
            }

            match self.seen.get(&key) {
                None => {
                    self.taken.insert(key.clone());
                    self.seen.insert(key, rel.clone());
                    return Placement::Free(rel);
                }
                Some(actual) => {
                    let actual = actual.clone();
                    let full = root.join(actual.replace('/', std::path::MAIN_SEPARATOR_STR));
                    match fs::symlink_metadata(&full) {
                        Ok(meta) if meta.is_file() => match hash::hash_file(&full) {
                            Ok(existing) if existing == digest => {
                                self.taken.insert(key);
                                return Placement::SameBytes(actual);
                            }
                            _ => continue,
                        },
                        _ => continue,
                    }
                }
            }
        }
        Placement::Impossible(format!("no free name for {name:?} after 1000 tries"))
    }

    /// Mark a path that is already in the Collection root as spoken for.
    fn reserve(&mut self, rel: &str) {
        self.taken.insert(rel.to_lowercase());
    }
}

/// Make a filename kith is willing to write.
///
/// No NFC normalisation: no crate for it is in this build, so two names differing
/// only by composition land as two files rather than as one collision.
fn sanitise(name: &str, fallback_stem: &str) -> String {
    let cleaned: String = name
        .chars()
        // Separators and NUL cannot appear in one path component, and a control
        // character in a filename is a display attack.
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | '\0'))
        .collect();
    // A sanitised name must not *become* hidden: dot-entries are skipped.
    let cleaned = cleaned.trim_start_matches('.').trim().to_string();

    let cleaned = if cleaned.is_empty() {
        fallback_stem.to_string()
    } else {
        cleaned
    };
    truncate_name(&cleaned, 255)
}

/// 255 bytes is the filename limit on every filesystem kith targets; the
/// extension is preserved.
fn truncate_name(name: &str, limit: usize) -> String {
    if name.len() <= limit {
        return name.to_string();
    }
    let (stem, ext) = split_extension(name);
    let suffix = ext.map(|e| format!(".{e}")).unwrap_or_default();
    if suffix.len() >= limit {
        return floor_chars(name, limit);
    }
    format!("{}{suffix}", floor_chars(stem, limit - suffix.len()))
}

/// Truncate to at most `limit` bytes without splitting a character.
fn floor_chars(s: &str, limit: usize) -> String {
    let mut end = limit.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(0) | None => (name, None),
        Some(i) if i + 1 == name.len() => (name, None),
        Some(i) => (&name[..i], Some(&name[i + 1..])),
    }
}

/// `path` relative to `root`, `/`-separated, if it is genuinely inside it.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let (root, path) = (canonical(root), canonical(path));
    let rel = path.strip_prefix(&root).ok()?;
    let mut segments = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(s) => segments.push(s.to_str()?.to_string()),
            _ => return None,
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

fn canonical(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── sniffing ─────────────────────────────────────────────────────────

fn prefix(path: &Path) -> Vec<u8> {
    let mut buf = vec![0u8; SNIFF_LEN];
    let read = fs::File::open(path)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap_or(0);
    buf.truncate(read);
    buf
}

/// A MIME guess from magic bytes, handed to the Provider as a hint.
///
/// Magic numbers only: guessing at a text-shaped format would be the core
/// deciding what a Provider claims. `claims` is the gate either way.
fn sniff(prefix: &[u8]) -> Option<String> {
    const MAGIC: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"II*\x00", "image/tiff"),
        (b"MM\x00*", "image/tiff"),
        (b"qoif", "image/qoi"),
    ];
    for (magic, mime) in MAGIC {
        if prefix.starts_with(magic) {
            return Some((*mime).to_string());
        }
    }
    if prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP" {
        return Some("image/webp".to_string());
    }
    None
}

// ── the write phase ──────────────────────────────────────────────────

#[derive(Default)]
struct Report {
    added: usize,
    registered: usize,
    revived: usize,
    bytes: u64,
    duplicates: usize,
    unclaimed: Vec<(PathBuf, String)>,
    unreadable: Vec<(PathBuf, String)>,
    renamed: Vec<(PathBuf, String)>,
    facts: Vec<String>,
    /// Set when the run stopped early. Everything already imported stands.
    halted: Option<String>,
}

fn execute(job: &Job, plan: &Plan) -> Report {
    let mut report = Report::default();
    let total = plan
        .entries
        .iter()
        .filter(|e| matches!(e.verdict, Verdict::Import { .. } | Verdict::Register { .. }))
        .count();
    let mut done = 0usize;
    let mut narrated = Instant::now();
    let narrate = io::stderr().is_terminal();

    for entry in &plan.entries {
        match &entry.verdict {
            Verdict::Duplicate => report.duplicates += 1,
            Verdict::Unclaimed { reason } => {
                report.unclaimed.push((entry.source.clone(), reason.clone()));
            }
            Verdict::Unreadable { error } => {
                report.unreadable.push((entry.source.clone(), error.clone()));
            }
            Verdict::Register { item, dest, revives } => {
                done += 1;
                progress(narrate, &mut narrated, done, total, &entry.source);
                match record_in_place(job, entry, item, dest, *revives, &mut report) {
                    Ok(()) => {}
                    Err(e) => {
                        report.unreadable.push((entry.source.clone(), e.to_string()));
                    }
                }
            }
            Verdict::Import { item, dest, revives } => {
                done += 1;
                progress(narrate, &mut narrated, done, total, &entry.source);
                match copy_then_record(job, entry, item, dest, *revives, &mut report) {
                    Ok(()) => {}
                    Err(Halt::Entry(message)) => {
                        report.unreadable.push((entry.source.clone(), message));
                    }
                    Err(Halt::Run(message)) => {
                        report.halted = Some(message);
                        break;
                    }
                }
            }
        }
    }

    if narrate && total > 0 {
        eprint!("\r\u{1b}[2K");
        let _ = io::stderr().flush();
    }
    report
}

/// A failure that costs one entry, or one that ends the run.
enum Halt {
    Entry(String),
    Run(String),
}

/// Register bytes that are already in the Collection root. Nothing is copied.
///
/// `at` is the file's own mtime rather than now, and that is load-bearing: the
/// engine preserves mtimes, so two Devices registering the same pre-existing tree
/// write identical `at` and converge on one Item without talking.
fn record_in_place(
    job: &Job,
    entry: &Entry,
    item: &ItemId,
    dest: &str,
    revives: bool,
    report: &mut Report,
) -> io::Result<()> {
    let full = job.root.join(dest.replace('/', std::path::MAIN_SEPARATOR_STR));
    let size = fs::metadata(&full).map(|m| m.len()).unwrap_or(entry.size);
    let at = mtime_of(&full).unwrap_or_else(now);

    facts(job, &full, dest, report);
    append(job, item, dest, &entry.hash, size, &at)?;

    report.registered += 1;
    if revives {
        report.revived += 1;
    }
    Ok(())
}

/// Stage, verify, publish, record — in that order, always.
fn copy_then_record(
    job: &Job,
    entry: &Entry,
    item: &ItemId,
    dest: &str,
    revives: bool,
    report: &mut Report,
) -> Result<(), Halt> {
    let staging = staging_dir(&job.root);
    fs::create_dir_all(&staging).map_err(|e| fatal_or_entry(&e, &staging, e.to_string()))?;

    let ext = split_extension(dest).1.unwrap_or("bin");
    let staged = staging.join(format!("{}.{ext}", item.as_str()));
    let full = job.root.join(dest.replace('/', std::path::MAIN_SEPARATOR_STR));

    // 1. Stage, hashing the copy as it lands.
    let digest = match stage(&entry.source, &staged) {
        Ok(d) => d,
        Err(e) => {
            let _ = fs::remove_file(&staged);
            return Err(fatal_or_entry(&e, &job.root, e.to_string()));
        }
    };

    // 2. Verify. A digest that moved means the source changed underneath us, and
    //    the record must never describe bytes nobody has.
    if digest != entry.hash {
        let _ = fs::remove_file(&staged);
        return Err(Halt::Entry("source changed during import".into()));
    }

    // 3. Publish. Staging is on the same filesystem as the root by construction,
    //    so this is one atomic rename, never a half-copy that replicates.
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| fatal_or_entry(&e, &job.root, e.to_string()))?;
    }
    if let Err(e) = fs::rename(&staged, &full) {
        let _ = fs::remove_file(&staged);
        return Err(fatal_or_entry(&e, &job.root, e.to_string()));
    }
    if let Some(parent) = full.parent() {
        let _ = fs::File::open(parent).and_then(|d| d.sync_all());
    }

    // 4. Record. Bytes are in the tree first, so a crash here leaves an orphan
    //    the next reconcile adopts, never a record for bytes nobody has.
    facts(job, &full, dest, report);
    append(job, item, dest, &entry.hash, entry.size, &now())
        .map_err(|e| Halt::Entry(e.to_string()))?;

    report.added += 1;
    report.bytes += entry.size;
    if revives {
        report.revived += 1;
    }
    if !dest.ends_with(&file_name_of(&entry.source)) {
        report.renamed.push((entry.source.clone(), dest.to_string()));
    }
    Ok(())
}

/// Copy `source` to `staged`, hashing the bytes as they are written.
fn stage(source: &Path, staged: &Path) -> io::Result<String> {
    let mut input = fs::File::open(source)?;
    let mut output = fs::File::create(staged)?;
    // The engine replicates the executable bit, and a wallpaper is not a program.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(staged, fs::Permissions::from_mode(0o644))?;
    }

    let mut hasher = hash::Hasher::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.write_all(&buf[..n])?;
                hasher.update(&buf[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    output.sync_all()?;
    Ok(hasher.finish())
}

/// Ask the Provider what it can read out of the content.
///
/// Extraction failure is **not** an import failure: a wallpaper whose dimensions
/// could not be read still belongs in the Collection. The facts are narrated and
/// not persisted — the record's `facts` field is reserved and unwritten here.
fn facts(job: &Job, bytes_at: &Path, dest: &str, report: &mut Report) {
    let candidate = ImportCandidate { path: bytes_at, mime: None };
    match job.provider.extract_metadata(&candidate) {
        Ok(f) => {
            if let (Some(w), Some(h)) = (f.width, f.height) {
                report.facts.push(format!("{dest} {w}×{h}"));
            }
        }
        Err(e) => eprintln!("! {dest}: the {} Provider read no facts from it ({e})", job.provider.id()),
    }
}

/// Append one `add` record to this Device's log.
///
/// `store::records::append` flushes per record, so a 5 000-file import pays 5 000
/// flushes. Batching belongs behind a run-scoped writer in `store::records`, never
/// behind a second writer of the log here.
fn append(
    job: &Job,
    item: &ItemId,
    dest: &str,
    digest: &str,
    size: u64,
    at: &str,
) -> io::Result<()> {
    // The destination filename's stem, verbatim: prettifying is a guess.
    let name = dest.rsplit('/').next().unwrap_or(dest);
    let title = split_extension(name).0.to_string();

    records::append(
        &job.root,
        &job.collection,
        &job.device,
        &Record::Add {
            item: item.clone(),
            by: job.person.clone(),
            at: at.to_string(),
            title,
            path: dest.to_string(),
            hash: digest.to_string(),
            size,
        },
    )
}

/// A full filesystem ends the run: every subsequent copy would fail the same way.
/// Items already imported keep their records, and re-running resumes.
fn fatal_or_entry(e: &io::Error, root: &Path, message: String) -> Halt {
    if e.raw_os_error() == Some(ENOSPC) {
        let free = free_bytes(root).map(bytes).unwrap_or_else(|| "no".into());
        return Halt::Run(format!(
            "{} ran out of space ({free} free). Nothing partial was left in the Collection; \
             re-run this command to carry on.",
            root.display()
        ));
    }
    Halt::Entry(message)
}

// ── narration ────────────────────────────────────────────────────────

fn progress(narrate: bool, last: &mut Instant, done: usize, total: usize, current: &Path) {
    if !narrate || last.elapsed() < PROGRESS_EVERY {
        return;
    }
    *last = Instant::now();
    eprint!("\r\u{1b}[2K  {done}/{total}  {}", file_name_of(current));
    let _ = io::stderr().flush();
}

/// stdout carries the result; stderr carries everything that explains it.
fn render(job: &Job, report: &Report) -> i32 {
    let imported = report.added + report.registered;
    let skipped = report.duplicates + report.unclaimed.len() + report.unreadable.len();

    println!(
        "Added {imported} Item{} to {} ({}). Skipped {skipped}.",
        if imported == 1 { "" } else { "s" },
        job.circle_name,
        bytes(report.bytes)
    );

    if report.registered > 0 {
        eprintln!(
            "  info     {} Item{} already sat in this Circle — recorded in place, no bytes copied",
            report.registered,
            if report.registered == 1 { "" } else { "s" }
        );
    }
    if report.revived > 0 {
        eprintln!(
            "  info     {} Item{} had been removed from {} earlier — adding {} back",
            report.revived,
            if report.revived == 1 { "" } else { "s" },
            job.circle_name,
            if report.revived == 1 { "it" } else { "them" }
        );
    }
    for (source, dest) in &report.renamed {
        eprintln!(
            "  info     {} imported as {dest} — a different image already had that name",
            source.display()
        );
    }
    for fact in &report.facts {
        eprintln!("  info     {fact}");
    }
    if report.duplicates > 0 {
        eprintln!(
            "  skipped  {} already in {} (duplicate)",
            report.duplicates, job.circle_name
        );
    }
    for (source, reason) in &report.unclaimed {
        eprintln!("  skipped  {} — {reason}", source.display());
    }
    for (source, error) in &report.unreadable {
        eprintln!("  failed   {} — {error}", source.display());
    }

    if imported > 0 {
        eprintln!(
            "They sync to every Member of {}; nobody's screen changes until they choose Apply.",
            job.circle_name
        );
    }

    if let Some(halted) = &report.halted {
        eprintln!("kith add: {halted}");
        return EX_FAIL;
    }
    if !report.unreadable.is_empty() {
        return EX_FAIL;
    }
    if !report.unclaimed.is_empty() {
        return EX_DATA;
    }
    EX_OK
}

fn confirm(question: &str) -> bool {
    eprintln!("{question}");
    eprint!("Go ahead? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// ── small facts about the filesystem ─────────────────────────────────

fn staging_dir(root: &Path) -> PathBuf {
    // `.kith/local` never syncs, which is exactly why staging lives in it.
    descriptors::kith_dir(root).join("local").join("incoming")
}

/// Unlink staged bytes left behind by a crashed run.
fn sweep_staging(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|age| age > STAGING_TTL).unwrap_or(false))
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn mtime_of(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let millis = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    let millis = i64::try_from(millis).ok()?;
    Some(jiff::Timestamp::from_millisecond(millis).ok()?.to_string())
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Decimal, because that is what a Person reads on a disk label.
fn bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("GB", 1_000_000_000), ("MB", 1_000_000), ("kB", 1_000), ("B", 1)];
    for (unit, scale) in UNITS {
        if n >= scale && scale > 1 {
            return format!("{:.1} {unit}", n as f64 / scale as f64);
        }
    }
    format!("{n} B")
}

/// Free space on the filesystem holding `path`, or `None` when this build cannot
/// ask.
///
/// One libc symbol, declared here rather than pulled in as a dependency. Off
/// 64-bit Linux the struct layout is not known for certain, so the answer is
/// `None` and the pre-flight is skipped rather than guessed at.
#[cfg(all(unix, target_os = "linux", target_pointer_width = "64"))]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::{CString, c_char};
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    #[derive(Default)]
    struct StatVfs {
        f_bsize: u64,
        f_frsize: u64,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_favail: u64,
        f_fsid: u64,
        f_flag: u64,
        f_namemax: u64,
        spare: [u32; 6],
    }

    unsafe extern "C" {
        fn statvfs(path: *const c_char, buf: *mut StatVfs) -> i32;
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut out = StatVfs::default();
    if unsafe { statvfs(c_path.as_ptr(), &mut out) } != 0 {
        return None;
    }
    // Blocks available to an unprivileged process, times the fragment size —
    // never the total, which includes the root reserve kith cannot use.
    let unit = if out.f_frsize > 0 { out.f_frsize } else { out.f_bsize };
    out.f_bavail.checked_mul(unit)
}

#[cfg(not(all(unix, target_os = "linux", target_pointer_width = "64")))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::SystemTime;

    const ANA_DEVICE: &str = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2";
    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A scratch tree under the system temp directory, never the Person's home.
    fn scratch(label: &str) -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kith-add-{}-{n}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn job(root: &Path) -> Job {
        Job {
            root: root.to_path_buf(),
            circle_name: "walls".into(),
            collection: MAIN.into(),
            provider: Box::new(WallpaperProvider::new(None)),
            device: ANA_DEVICE.into(),
            person: PersonId::generate(),
            other_roots: Vec::new(),
            reserved: vec![
                ".stfolder",
                ".stversions/**",
                ".stignore",
                "~engine~*.tmp",
                "*.sync-conflict-*",
            ],
            interactive: false,
        }
    }

    /// A real image, so the Provider's own claim and metadata paths run.
    fn wallpaper(path: &Path, w: u32, h: u32) {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).unwrap();
        }
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 0])
        });
        img.save(path).unwrap();
    }

    fn items(job: &Job) -> Vec<crate::domain::Item> {
        let recs = records::read_all(&job.root, &job.collection).unwrap();
        records::derive_items(&recs, &job.root)
    }

    // ── the happy path ───────────────────────────────────────────────

    #[test]
    fn a_wallpaper_becomes_an_item_with_bytes_in_the_collection_and_a_record() {
        let home = scratch("import-home");
        let root = scratch("import-root");
        let job = job(&root);
        wallpaper(&home.join("sunset.png"), 8, 4);

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);

        assert!(root.join("sunset.png").is_file(), "the bytes are in the Collection");
        assert!(home.join("sunset.png").is_file(), "copy, never move: the library is untouched");

        let items = items(&job);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "sunset", "the stem, verbatim");
        assert_eq!(items[0].added_by, job.person);
        assert_eq!(items[0].path, Some(root.join("sunset.png")));
        assert_eq!(items[0].hash, Some(hash::hash_file(&root.join("sunset.png")).unwrap()));
    }

    #[test]
    fn a_directory_argument_recurses_and_preserves_its_internal_structure() {
        let home = scratch("tree-home");
        let root = scratch("tree-root");
        let job = job(&root);
        wallpaper(&home.join("top.png"), 4, 4);
        wallpaper(&home.join("nature/forest.png"), 5, 5);
        wallpaper(&home.join("nature/deep/ridge.png"), 6, 6);

        assert_eq!(import(&job, &[home.clone()]), EX_OK);

        assert!(root.join("top.png").is_file());
        assert!(root.join("nature/forest.png").is_file());
        assert!(root.join("nature/deep/ridge.png").is_file());
        assert_eq!(items(&job).len(), 3, "structure is storage; the Collection is flat");
    }

    #[test]
    fn a_file_argument_lands_at_the_collection_root_however_deep_it_was() {
        let home = scratch("flat-home");
        let root = scratch("flat-root");
        let job = job(&root);
        wallpaper(&home.join("nature/forest.png"), 4, 4);

        assert_eq!(import(&job, &[home.join("nature/forest.png")]), EX_OK);
        assert!(root.join("forest.png").is_file());
        assert!(!root.join("nature").exists());
    }

    // ── the Provider gate ────────────────────────────────────────────

    #[test]
    fn content_the_provider_does_not_claim_is_refused_with_a_reason_never_accepted() {
        let home = scratch("unclaimed-home");
        let root = scratch("unclaimed-root");
        let job = job(&root);
        fs::write(home.join("notes.txt"), b"not a wallpaper").unwrap();
        wallpaper(&home.join("sunset.png"), 4, 4);

        // Exit 65: input we cannot use. The wallpaper still landed.
        assert_eq!(import(&job, &[home.clone()]), EX_DATA);
        assert!(root.join("sunset.png").is_file());
        assert!(!root.join("notes.txt").exists(), "refused, not silently accepted");
        assert_eq!(items(&job).len(), 1);
    }

    // ── dedup, revival, resumability ─────────────────────────────────

    #[test]
    fn adding_the_same_bytes_twice_is_a_duplicate_and_writes_no_second_record() {
        let home = scratch("dup-home");
        let root = scratch("dup-root");
        let job = job(&root);
        wallpaper(&home.join("sunset.png"), 4, 4);

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);
        // A duplicate does not affect the exit code.
        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);

        assert_eq!(items(&job).len(), 1);
        let recs = records::read_all(&root, MAIN).unwrap();
        assert_eq!(recs.len(), 1, "no second record for bytes already here");
    }

    #[test]
    fn one_run_naming_the_same_bytes_twice_mints_one_item() {
        let home = scratch("dup-run-home");
        let root = scratch("dup-run-root");
        let job = job(&root);
        wallpaper(&home.join("sunset.png"), 4, 4);
        fs::copy(home.join("sunset.png"), home.join("copy.png")).unwrap();

        assert_eq!(import(&job, &[home.join("sunset.png"), home.join("copy.png")]), EX_OK);
        assert_eq!(items(&job).len(), 1);
    }

    #[test]
    fn re_adding_bytes_whose_item_was_removed_revives_the_original_item() {
        let home = scratch("revive-home");
        let root = scratch("revive-root");
        let job = job(&root);
        wallpaper(&home.join("sunset.png"), 4, 4);

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);
        let original = items(&job).into_iter().next().unwrap();

        // The Delete Action's half: a tombstone, then the bytes.
        records::append(
            &root,
            MAIN,
            ANA_DEVICE,
            &Record::Remove {
                item: original.id.clone(),
                by: job.person.clone(),
                at: now(),
            },
        )
        .unwrap();
        fs::remove_file(root.join("sunset.png")).unwrap();
        assert!(items(&job).is_empty(), "the tombstone holds");

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);
        let back = items(&job);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, original.id, "the original Item, revived");
        assert_eq!(back[0].added_by, original.added_by, "and its original adder");
        assert_eq!(back[0].added_at, original.added_at);
    }

    #[test]
    fn a_partial_run_resumes_by_skipping_everything_it_already_imported() {
        let home = scratch("resume-home");
        let root = scratch("resume-root");
        let job = job(&root);
        wallpaper(&home.join("a.png"), 4, 4);
        assert_eq!(import(&job, &[home.clone()]), EX_OK);

        // The run "continues" with more content in the same directory.
        wallpaper(&home.join("b.png"), 5, 5);
        assert_eq!(import(&job, &[home.clone()]), EX_OK);

        assert_eq!(items(&job).len(), 2);
        assert_eq!(records::read_all(&root, MAIN).unwrap().len(), 2, "no record was written twice");
    }

    // ── register in place ────────────────────────────────────────────

    #[test]
    fn a_path_already_inside_the_circle_is_recorded_in_place_and_copies_nothing() {
        let root = scratch("in-place-root");
        let job = job(&root);
        wallpaper(&root.join("nature/forest.png"), 4, 4);

        assert_eq!(import(&job, &[root.clone()]), EX_OK);

        let items = items(&job);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, Some(root.join("nature/forest.png")), "not moved, not copied");
        assert_eq!(
            fs::read_dir(root.join("nature")).unwrap().count(),
            1,
            "no second copy beside the original"
        );

        // An adopted Item is dated from its bytes, not from discovery.
        let recorded = &records::read_all(&root, MAIN).unwrap()[0];
        assert_eq!(recorded.at(), mtime_of(&root.join("nature/forest.png")).unwrap());
    }

    #[test]
    fn bytes_already_in_the_collection_under_the_wanted_name_are_recorded_not_duplicated() {
        let home = scratch("same-bytes-home");
        let root = scratch("same-bytes-root");
        let job = job(&root);
        wallpaper(&home.join("sunset.png"), 4, 4);
        fs::copy(home.join("sunset.png"), root.join("sunset.png")).unwrap();

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);

        assert!(!root.join("sunset-2.png").exists(), "the same bytes are not copied beside themselves");
        assert_eq!(items(&job).len(), 1);
    }

    // ── what is never a candidate ────────────────────────────────────

    #[test]
    fn the_collection_root_and_every_other_circle_root_are_excluded_from_recursion() {
        let home = scratch("tail-home");
        let root = home.join("Wallpapers");
        let other = home.join("Photos");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();

        let mut job = job(&root);
        job.other_roots = vec![canonical(&other)];

        wallpaper(&home.join("loose.png"), 4, 4);
        wallpaper(&root.join("already-mine.png"), 5, 5);
        wallpaper(&other.join("someone-elses.png"), 6, 6);

        assert_eq!(import(&job, &[home.clone()]), EX_OK);

        let titles: BTreeSet<String> = items(&job).into_iter().map(|i| i.title).collect();
        assert_eq!(titles, BTreeSet::from(["loose".to_string()]));
        assert!(!root.join("someone-elses.png").exists());
    }

    #[test]
    fn dot_entries_symlinks_and_engine_artefacts_are_never_candidates() {
        let home = scratch("skip-home");
        let root = scratch("skip-root");
        let job = job(&root);

        wallpaper(&home.join("keep.png"), 4, 4);
        wallpaper(&home.join(".hidden.png"), 4, 4);
        wallpaper(&home.join(".kith/inside.png"), 4, 4);
        wallpaper(&home.join(".stversions/old.png"), 4, 4);
        wallpaper(&home.join("keep.sync-conflict-20260807-091402-K5J2FVL.png"), 4, 4);
        #[cfg(unix)]
        std::os::unix::fs::symlink(home.join("keep.png"), home.join("link.png")).unwrap();

        assert_eq!(import(&job, &[home.clone()]), EX_OK);

        let titles: BTreeSet<String> = items(&job).into_iter().map(|i| i.title).collect();
        assert_eq!(titles, BTreeSet::from(["keep".to_string()]), "{titles:?}");
    }

    /// A symlink named as an argument is a Person pointing at content, so its
    /// target's bytes are imported and the link is left alone.
    #[cfg(unix)]
    #[test]
    fn a_symlink_passed_as_an_argument_imports_its_targets_bytes() {
        let home = scratch("symlink-home");
        let root = scratch("symlink-root");
        let job = job(&root);
        wallpaper(&home.join("real.png"), 4, 4);
        std::os::unix::fs::symlink(home.join("real.png"), home.join("pointer.png")).unwrap();

        assert_eq!(import(&job, &[home.join("pointer.png")]), EX_OK);

        let landed = root.join("pointer.png");
        assert!(
            fs::symlink_metadata(&landed).unwrap().is_file(),
            "bytes, never a link — kith does not sync links"
        );
        assert!(fs::symlink_metadata(home.join("pointer.png")).unwrap().file_type().is_symlink());
    }

    // ── collisions ───────────────────────────────────────────────────

    #[test]
    fn a_name_collision_with_different_bytes_lands_under_a_new_name() {
        let home = scratch("collide-home");
        let root = scratch("collide-root");
        let job = job(&root);
        wallpaper(&root.join("sunset.png"), 4, 4);
        wallpaper(&home.join("sunset.png"), 9, 9);

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);

        assert!(root.join("sunset-2.png").is_file(), "the incumbent keeps its name");
        assert_eq!(
            hash::hash_file(&root.join("sunset-2.png")).unwrap(),
            hash::hash_file(&home.join("sunset.png")).unwrap()
        );
    }

    #[test]
    fn a_collision_is_compared_case_insensitively() {
        let root = scratch("case-root");
        let home = scratch("case-home");
        let job = job(&root);
        wallpaper(&root.join("Sunset.png"), 4, 4);
        wallpaper(&home.join("sunset.png"), 9, 9);

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);
        // `Sunset.png` beside `sunset.png` is a permanent conflict on a Device
        // with a case-insensitive filesystem, so kith never creates the pair.
        assert!(!root.join("sunset.png").exists());
        assert!(root.join("sunset-2.png").is_file());
    }

    // ── refusing rather than filling the disk ────────────────────────

    #[test]
    fn an_import_that_would_not_fit_is_refused_and_a_margin_is_always_left() {
        // 5.1 GB wanted, 2.3 GB free, nothing copied.
        assert!(!fits(5_100_000_000, Some(2_300_000_000)));
        assert!(fits(1_000, Some(1_000_000_000)));
        assert!(
            !fits(1_000, Some(SPACE_MARGIN)),
            "filling the disk to the last megabyte takes the session down with it"
        );
        assert!(
            fits(u64::MAX, None),
            "a filesystem this build cannot ask is not a refusal nobody can explain"
        );
        // Overflow is a refusal, never a wrap-around into "it fits".
        assert!(!fits(u64::MAX, Some(1_000)));
    }

    #[test]
    fn the_plan_knows_what_it_would_copy_before_a_byte_moves() {
        let home = scratch("space-home");
        let root = scratch("space-root");
        let job = job(&root);
        wallpaper(&home.join("sunset.png"), 4, 4);

        let plan = plan(&job, &[home.join("sunset.png")]);
        assert_eq!(plan.import_count, 1);
        assert!(plan.import_bytes > 0);
        assert!(!root.join("sunset.png").exists(), "planning writes nothing");
        assert!(records::read_all(&root, MAIN).unwrap().is_empty());
    }

    // ── the staging area ─────────────────────────────────────────────

    #[test]
    fn the_staging_area_is_swept_and_never_left_behind() {
        let home = scratch("staging-home");
        let root = scratch("staging-root");
        let job = job(&root);
        wallpaper(&home.join("sunset.png"), 4, 4);

        let staging = staging_dir(&root);
        fs::create_dir_all(&staging).unwrap();
        let litter = staging.join("01ABCDEF.png");
        fs::write(&litter, b"a crashed run's leftovers").unwrap();
        let old = SystemTime::now() - STAGING_TTL - Duration::from_secs(60);
        fs::File::open(&litter).unwrap().set_modified(old).unwrap();

        assert_eq!(import(&job, &[home.join("sunset.png")]), EX_OK);

        assert!(!litter.exists(), "stale staged bytes are swept");
        assert_eq!(
            fs::read_dir(&staging).unwrap().count(),
            0,
            "and a finished import leaves nothing behind"
        );
    }

    // ── unit-level rules ─────────────────────────────────────────────

    #[test]
    fn sanitising_strips_controls_and_leading_dots_and_keeps_the_extension() {
        assert_eq!(sanitise("sunset.png", "X"), "sunset.png");
        assert_eq!(sanitise("sun\u{7}set.png", "X"), "sunset.png");
        assert_eq!(sanitise("..hidden.png", "X"), "hidden.png");
        assert_eq!(sanitise("a/b.png", "X"), "ab.png");
        // A name that sanitises to nothing gets the Item id as its stem.
        assert_eq!(sanitise("...", "01ABCDEF"), "01ABCDEF");
        assert_eq!(sanitise("", "01ABCDEF"), "01ABCDEF");
    }

    #[test]
    fn a_very_long_name_is_truncated_with_its_extension_intact() {
        let long = format!("{}.png", "a".repeat(400));
        let cut = truncate_name(&long, 255);
        assert_eq!(cut.len(), 255);
        assert!(cut.ends_with(".png"), "{cut}");
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let name = format!("{}.png", "é".repeat(200));
        let cut = truncate_name(&name, 255);
        assert!(cut.len() <= 255);
        assert!(cut.ends_with(".png"));
        assert!(cut.is_char_boundary(cut.len()));
    }

    #[test]
    fn the_engines_own_globs_are_matched_as_data() {
        assert!(glob_match("*.sync-conflict-*", "sunset.sync-conflict-20260807-091402-K5J.png"));
        assert!(!glob_match("*.sync-conflict-*", "sunset.png"));
        assert!(glob_match("~engine~*.tmp", "~engine~abc.tmp"));
        assert!(glob_match(".stfolder", ".stfolder"));
        assert!(!glob_match(".stfolder", ".stfolders"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn sniffing_reads_magic_bytes_and_guesses_nothing_else() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest").as_deref(), Some("image/png"));
        assert_eq!(sniff(b"\xFF\xD8\xFFrest").as_deref(), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ").as_deref(), Some("image/webp"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), None, "a RIFF is not an image");
        // Text-shaped formats have no magic number, and kith invents none.
        assert_eq!(sniff(b"<?xml version=\"1.0\"?><svg"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn relative_to_only_answers_for_paths_genuinely_inside_the_root() {
        let root = scratch("relative");
        fs::create_dir_all(root.join("nature")).unwrap();
        fs::write(root.join("nature/forest.png"), b"x").unwrap();

        assert_eq!(
            relative_to(&root, &root.join("nature/forest.png")).as_deref(),
            Some("nature/forest.png")
        );
        assert_eq!(relative_to(&root, &root), None, "the root is not inside itself");
        assert_eq!(relative_to(&root, Path::new("/etc/passwd")), None);
    }

    #[test]
    fn byte_counts_read_the_way_a_person_reads_a_disk_label() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(48_300_000), "48.3 MB");
        assert_eq!(bytes(5_100_000_000), "5.1 GB");
    }
}
