//! The Circle and Collection descriptors, and the protocol that writes them.
//!
//! A Circle's shared state has exactly two singletons (ADR-0004 §5): `circle.toml`
//! says which Circle this tree is and who founded it, and `collections/<id>.toml`
//! says which Provider a Collection's Items belong to. Everything else under
//! `.kith/` is an append-only log or a per-Device claim.
//!
//! Descriptors are the one rewritable thing in a tree whose spine is "append,
//! never rewrite" (ADR-0004 §1, W2). That is not a loophole: W1 still holds, so a
//! descriptor has exactly one writing Device and rewriting it races with nobody.
//! What makes the rewrite safe on a transport with no coordinator is the protocol
//! below — write beside the target, flush, rename — with a temp name the Sync
//! Engine has been taught to ignore, so a half-written descriptor never replicates.
//!
//! v0.1 writes each descriptor once and never rewrites it (ROADMAP §2: no rename,
//! no delete, no Circle settings). The protocol is here in full anyway, because the
//! milestone that gains rename must not have to invent one.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The descriptor schema this build writes and understands.
///
/// Bumped only by a breaking change: additive fields do not move it, and a
/// breaking change is a new path rather than a rewrite (ADR-0004 §11). Reading is
/// not applying, so a descriptor carrying a higher `schema` is returned here as it
/// was found and reported by `kith doctor` — refusing to read it would break a
/// Circle for the older Device, which is the opposite of degrading gracefully.
pub const SCHEMA: u32 = 1;

/// Every byte of a Circle's shared state lives under this one directory, and it is
/// hidden from the Gallery (ADR-0004 §2).
const KITH_DIR: &str = ".kith";
const CIRCLE_FILE: &str = "circle.toml";
const COLLECTIONS_DIR: &str = "collections";

/// The suffix that makes a half-written descriptor invisible to the Sync Engine.
///
/// This constant and the line `seed_stignore` writes are one decision, so they are
/// spelled once, here: change this and the engine stops ignoring our temp files.
const TMP_SUFFIX: &str = ".kith-tmp";

/// The name of the engine's per-Circle ignore file.
///
/// Along with [`DELETE_OK`] this is one of exactly two engine spellings in this
/// module, and both are here only because the module's own contract names the
/// file it seeds. Everything that is a *policy* — which globs a Circle stops
/// replicating — arrives as an argument (see [`seed_stignore`]). When the seam
/// grows an accessor for the ignore file's name, these two constants go with it.
const IGNORE_FILE: &str = ".stignore";

/// The engine's prefix for "you may delete this to unblock a directory removal".
///
/// kith applies it to its own two paths and to nothing else: it is a statement
/// about paths kith owns, and it is exactly why nothing authoritative is ever
/// stored under `.kith/local` (ADR-0004 §7).
const DELETE_OK: &str = "(?d)";

/// Which Circle this tree is, who founded it, and — in v0.1 — whose Device is its
/// Steward's.
///
/// Written once by the founding Device and never rewritten (ADR-0004 §5). Every
/// surface that names the Circle's Steward reads `founder_device` from here rather
/// than from the transport, because this is the one fact that reads the same from
/// every Device, including the Steward's own (ADR-0002 §3).
///
/// `founder_person` names a **Person** and `founder_device` names a **Device**, and
/// the gap between them is real: a Device that has never run kith has published no
/// Membership claim, so a Circle can know its Steward's Device and still be unable
/// to name the Steward. kith says so rather than inventing a placeholder Person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CircleDescriptor {
    pub schema: u32,
    /// The Circle's immutable id. On disk the key is `circle`, as ADR-0004 §5
    /// fixed it — the file is read by other kith builds and by People with an
    /// editor, so its spelling is part of the format and not of this struct.
    #[serde(rename = "circle")]
    pub id: String,
    /// The Circle's name. Mutable in a later milestone; write-once in v0.1.
    pub name: String,
    /// RFC 3339, and the tie-break when two Devices each claimed a Circle that had
    /// no descriptor yet: earliest `created` wins (ADR-0004 §8).
    pub created: String,
    pub founder_person: String,
    pub founder_device: String,
}

/// Which Provider a Collection's Items belong to.
///
/// v0.1 creates exactly one Collection per Circle, with the literal id `main` and
/// the `wallpaper` Provider. The id is opaque in the format, so v0.3's additional
/// Collections need no format change (docs/spec/collections.md §8).
///
/// Two fields ADR-0004 §5 sketches are deliberately absent from this build's
/// struct: `name`, which v0.1 has no surface to show or edit, and `root`, which is
/// always `"."` while the sole Collection *is* the Circle root. Both are additive
/// when they land, and a descriptor written by a later kith that carries them is
/// read here without complaint — v0.1 simply keeps its own answer for them, and
/// `kith doctor` is where that is surfaced rather than guessed at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionDescriptor {
    pub schema: u32,
    /// The Collection's id. Also its filename, which is why [`read_collection`]
    /// refuses an id that could name a path.
    pub collection: String,
    /// The Provider that claims this Collection's Items — `wallpaper` in v0.1. A
    /// descriptor naming a Provider this build lacks is not an error here: it is
    /// read faithfully so the layer above can say which Provider is missing.
    pub provider: String,
}

/// `<root>/.kith` — a Circle's shared state.
pub fn kith_dir(root: &Path) -> PathBuf {
    root.join(KITH_DIR)
}

/// `<root>/.kith/circle.toml`.
pub fn circle_path(root: &Path) -> PathBuf {
    kith_dir(root).join(CIRCLE_FILE)
}

/// `<root>/.kith/collections` — a directory rather than a file, because the
/// one-to-many Circle→Collection shape is modelled from day one.
pub fn collections_dir(root: &Path) -> PathBuf {
    kith_dir(root).join(COLLECTIONS_DIR)
}

/// Serialise `value` as TOML and put it at `path` without ever letting a partial
/// document exist under that name.
///
/// Write `<target>.kith-tmp` beside the target, flush it to the platter, then
/// `rename(2)` over the target (ADR-0004 §3). The rename is atomic, so a reader —
/// or the Sync Engine's scanner — sees either the old descriptor or the new one and
/// never half of either, and the temp name is ignored from sync, so the partial
/// document never replicates.
///
/// Missing parent directories are created: a descriptor is often the first thing
/// written into a brand-new `.kith/`.
pub fn write_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = toml::to_string_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write_bytes_atomic(path, text.as_bytes())
}

/// Read this Circle's descriptor.
///
/// `Ok(None)` means the tree has no descriptor yet, which is an ordinary state and
/// not a fault: a Member who adopts a wp-sync Circle before the Steward's Device
/// has upgraded gets a working Collection and a descriptor that arrives later
/// (docs/spec/collections.md §4.3). A descriptor that exists but does not parse is
/// an error — that one someone has to look at.
///
/// Conflict copies are not consulted here. Absorbing one means matching the
/// engine's own name for it (ADR-0004 §8), and those globs live behind the seam;
/// the layer that already holds them does that work.
pub fn read_circle(root: &Path) -> io::Result<Option<CircleDescriptor>> {
    read_toml(&circle_path(root))
}

/// Write this Circle's descriptor.
///
/// Write-once is a *milestone* rule, not a format rule (ADR-0004 §1), so it is
/// enforced by the caller that reads first — `kith create` — and not here. What is
/// enforced here is that the bytes land whole.
pub fn write_circle(root: &Path, d: &CircleDescriptor) -> io::Result<()> {
    write_atomic(&circle_path(root), d)
}

/// Read one Collection's descriptor. Absent is `Ok(None)`; malformed is an error.
pub fn read_collection(root: &Path, id: &str) -> io::Result<Option<CollectionDescriptor>> {
    read_toml(&collection_file(root, id)?)
}

/// Write one Collection's descriptor, at the path its own id names.
pub fn write_collection(root: &Path, d: &CollectionDescriptor) -> io::Result<()> {
    let path = collection_file(root, &d.collection)?;
    write_atomic(&path, d)
}

/// Every Collection descriptor in this Circle, ordered by id.
///
/// An absent `collections/` directory yields an empty list — the same "not yet"
/// that [`read_circle`] reports as `None`. Only files named `<id>.toml` with no dot
/// inside the id are read: the segment before the first dot is the id (ADR-0004
/// §4.3), so a copy the engine may leave beside a descriptor — a conflict copy, or
/// a later milestone's generation file — never becomes a phantom second Collection.
pub fn read_collections(root: &Path) -> io::Result<Vec<CollectionDescriptor>> {
    let dir = collections_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".toml") else { continue };
        if stem.is_empty() || stem.contains('.') {
            continue;
        }
        if let Some(d) = read_toml::<CollectionDescriptor>(&entry.path())? {
            out.push(d);
        }
    }
    out.sort_by(|a, b| a.collection.cmp(&b.collection));
    Ok(out)
}

/// Seed this Circle's ignore file so kith's own scratch and its half-written
/// descriptors never leave this Device.
///
/// Two lines are kith's own and are always present (ADR-0002 §2, ADR-0004 §2):
/// `.kith/local`, the per-Device staging area, and `*.kith-tmp`, this module's temp
/// files. Both carry [`DELETE_OK`], because kith is content for the engine to
/// remove them when they block a directory removal — and that permission is exactly
/// why nothing authoritative is ever stored under either.
///
/// `reserved` is written verbatim, in the order given. The globs belong to the Sync
/// Engine implementation and arrive as an argument so that no engine spelling has
/// to be repeated here (ADR-0002 §1).
///
/// One caveat for callers, because the two lists are easy to confuse: this is *not*
/// `SyncEngine::reserved_paths()`. That list exists to hide engine artefacts from
/// the Gallery, and it includes conflict copies — which must keep replicating, so
/// that the Circle can handle them rather than hide them (ADR-0002 §2). Pass the
/// globs whose replication this Circle actually means to stop.
///
/// Existing lines are preserved in place and nothing is reordered: adopting a
/// wp-sync Circle adds to that Circle's ignores and never replaces them
/// (docs/spec/collections.md §4.2). Seeding twice writes nothing the second time,
/// so adoption may call it unconditionally.
pub fn seed_stignore(root: &Path, reserved: &[&str]) -> io::Result<()> {
    let path = root.join(IGNORE_FILE);

    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    let mut wanted = vec![
        format!("{DELETE_OK}{KITH_DIR}/local"),
        format!("{DELETE_OK}*{TMP_SUFFIX}"),
    ];
    wanted.extend(reserved.iter().map(|g| (*g).to_string()));

    let mut added = false;
    for line in wanted {
        if !lines.iter().any(|have| have.trim() == line) {
            lines.push(line);
            added = true;
        }
    }
    if !added {
        return Ok(());
    }

    let mut body = lines.join("\n");
    body.push('\n');
    write_bytes_atomic(&path, body.as_bytes())
}

// ── the protocol itself ──────────────────────────────────────────────

fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // Unknown keys are ignored rather than refused: a descriptor written by a newer
    // kith stays readable, which is the whole of ADR-0004 §11's forward-compat rule
    // for this file. Nothing is dropped by ignoring them, because v0.1 never
    // rewrites a descriptor it did not just create.
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    let (tmp, mut file) = create_temp(path)?;
    let staged = file
        .write_all(bytes)
        // Flush our own bytes to the platter before the rename publishes them, so a
        // crash cannot leave a descriptor whose name is new and whose content is not.
        .and_then(|()| file.sync_all());
    drop(file);

    if let Err(e) = staged.and_then(|()| fs::rename(&tmp, path)) {
        // Leave no stray temp file behind on the way out. It would be ignored from
        // sync and therefore harmless, but a Circle that accumulates litter is one a
        // Person cannot read with `ls`, and that readability is half the point of
        // this format.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Best effort: makes the rename itself durable. Not every filesystem permits it,
    // and a refusal here does not make the write wrong — the descriptor is whole
    // either way, and the tree, not this call, is the authority (ADR-0001).
    if let Some(dir) = path.parent() {
        let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// `<target>.kith-tmp`, beside the target so the rename stays within one filesystem.
///
/// Exclusive creation, with a pid-qualified fallback: two kith processes on one
/// Device are the only race W1 does not already rule out, and two writers sharing
/// one temp file would interleave into a torn document that the rename then
/// publishes. Both names end in the suffix the engine ignores, which is the
/// property that actually matters.
fn create_temp(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    let tmp = temp_path(path, None);
    match fs::File::create_new(&tmp) {
        Ok(f) => Ok((tmp, f)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let tmp = temp_path(path, Some(std::process::id()));
            let f = fs::File::create(&tmp)?;
            Ok((tmp, f))
        }
        Err(e) => Err(e),
    }
}

fn temp_path(path: &Path, qualifier: Option<u32>) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    if let Some(q) = qualifier {
        name.push(format!(".{q}"));
    }
    name.push(TMP_SUFFIX);
    PathBuf::from(name)
}

/// A Collection id is also a filename, so it may never name a path.
///
/// v0.1's id is the literal `main` and v0.3 mints opaque ids from an alphabet with
/// no dot in it, so refusing separators and dots costs nothing and buys the
/// guarantee that no descriptor can be read from — or written to — outside the
/// Circle. It also keeps the reading rule in [`read_collections`] honest.
fn collection_file(root: &Path, id: &str) -> io::Result<PathBuf> {
    let usable = !id.is_empty()
        && !id.contains('.')
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0');
    if !usable {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{id:?} is not a usable Collection id"),
        ));
    }
    Ok(collections_dir(root).join(format!("{id}.toml")))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A scratch Circle root, never in the Person's home.
    fn scratch(label: &str) -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "kith-descriptors-{}-{n}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("scratch root");
        root
    }

    fn a_circle() -> CircleDescriptor {
        CircleDescriptor {
            schema: SCHEMA,
            id: "kith-4tj2q9xa".into(),
            name: "walls".into(),
            created: "2026-08-07T09:02:11.004Z".into(),
            founder_person: "p-01k1yfq2m7vj3w8t0pz4rxab6c".into(),
            founder_device: "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2"
                .into(),
        }
    }

    fn a_collection() -> CollectionDescriptor {
        CollectionDescriptor {
            schema: SCHEMA,
            collection: "main".into(),
            provider: "wallpaper".into(),
        }
    }

    #[test]
    fn circle_descriptor_round_trips_through_the_tree() {
        let root = scratch("circle-round-trip");
        let d = a_circle();
        write_circle(&root, &d).unwrap();

        assert_eq!(read_circle(&root).unwrap().unwrap(), d);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn on_disk_keys_are_the_ones_the_format_fixed() {
        // The file is read by other kith builds and by People with an editor, so
        // its key names are part of the format even where this struct's field is
        // spelled differently.
        let root = scratch("circle-keys");
        write_circle(&root, &a_circle()).unwrap();

        let text = fs::read_to_string(circle_path(&root)).unwrap();
        assert!(text.contains("circle = \"kith-4tj2q9xa\""), "{text}");
        assert!(!text.contains("id ="), "the id is spelled `circle` on disk: {text}");
        assert!(text.contains("founder_device = "), "{text}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_absent_descriptor_is_not_a_fault() {
        // A Member who adopts before the Steward's Device has upgraded holds a
        // working Circle with no descriptor in it. That is a state, not an error.
        let root = scratch("absent");
        assert!(read_circle(&root).unwrap().is_none());
        assert!(read_collection(&root, "main").unwrap().is_none());
        assert!(read_collections(&root).unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_malformed_descriptor_is_an_error() {
        let root = scratch("malformed");
        fs::create_dir_all(kith_dir(&root)).unwrap();
        fs::write(circle_path(&root), "this is not = = toml").unwrap();

        let err = read_circle(&root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("circle.toml"), "name the file: {err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_descriptor_missing_a_required_fact_is_an_error_not_a_default() {
        let root = scratch("incomplete");
        fs::create_dir_all(kith_dir(&root)).unwrap();
        fs::write(circle_path(&root), "schema = 1\ncircle = \"kith-4tj2q9xa\"\n").unwrap();

        assert_eq!(
            read_circle(&root).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_refused() {
        // ADR-0004 §11: a descriptor written by a newer kith stays readable.
        let root = scratch("forward-compat");
        fs::create_dir_all(collections_dir(&root)).unwrap();
        fs::write(
            collections_dir(&root).join("main.toml"),
            "schema = 1\ncollection = \"main\"\nprovider = \"wallpaper\"\n\
             name = \"walls\"\nroot = \".\"\ncreated = \"2026-08-07T09:02:11.004Z\"\n",
        )
        .unwrap();

        let d = read_collection(&root, "main").unwrap().unwrap();
        assert_eq!(d, a_collection());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_write_leaves_no_temp_file_behind() {
        let root = scratch("no-litter");
        write_circle(&root, &a_circle()).unwrap();

        let names: Vec<String> = fs::read_dir(kith_dir(&root))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![CIRCLE_FILE.to_string()], "{names:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_write_leaves_no_temp_file_either() {
        // A directory where the descriptor should be: the rename cannot land, and
        // the half-written document must not survive the failure.
        let root = scratch("failed-write");
        let target = kith_dir(&root).join("circle.toml");
        fs::create_dir_all(&target).unwrap();

        assert!(write_circle(&root, &a_circle()).is_err());
        assert!(!temp_path(&target, None).exists());
        assert!(!temp_path(&target, Some(std::process::id())).exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_temp_name_is_the_one_the_seed_teaches_the_engine_to_ignore() {
        let root = scratch("temp-name");
        let tmp = temp_path(&circle_path(&root), None);
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "circle.toml.kith-tmp");

        seed_stignore(&root, &[]).unwrap();
        let ignores = fs::read_to_string(root.join(IGNORE_FILE)).unwrap();
        let pattern = format!("{DELETE_OK}*{TMP_SUFFIX}");
        assert!(ignores.lines().any(|l| l == pattern), "{ignores}");
        assert!(name.ends_with(TMP_SUFFIX));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_rewrite_replaces_the_descriptor_whole() {
        // Not a v0.1 path — nothing rewrites a descriptor before rename ships —
        // but the protocol is the one that milestone will use.
        let root = scratch("rewrite");
        write_circle(&root, &a_circle()).unwrap();

        let mut renamed = a_circle();
        renamed.name = "wallpapers".into();
        write_circle(&root, &renamed).unwrap();

        assert_eq!(read_circle(&root).unwrap().unwrap(), renamed);
        let names: Vec<String> = fs::read_dir(kith_dir(&root))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn collection_descriptor_round_trips_and_lands_where_its_id_says() {
        let root = scratch("collection-round-trip");
        let d = a_collection();
        write_collection(&root, &d).unwrap();

        assert!(collections_dir(&root).join("main.toml").is_file());
        assert_eq!(read_collection(&root, "main").unwrap().unwrap(), d);
        assert!(read_collection(&root, "holiday").unwrap().is_none());
        assert_eq!(read_collections(&root).unwrap(), vec![d]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_collection_id_can_never_name_a_path_outside_the_circle() {
        let root = scratch("traversal");
        for id in ["../../etc/passwd", "..", "", "a/b", "main.2"] {
            assert_eq!(
                read_collection(&root, id).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{id:?} must not resolve to a descriptor path"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_copy_beside_a_descriptor_is_not_a_second_collection() {
        // The id is the segment before the first dot (ADR-0004 §4.3), so a
        // conflict copy or a later generation of `main.toml` never doubles the
        // Circle's Collections.
        let root = scratch("copies");
        write_collection(&root, &a_collection()).unwrap();
        fs::write(
            collections_dir(&root).join("main.2.toml"),
            "schema = 1\ncollection = \"main\"\nprovider = \"wallpaper\"\n",
        )
        .unwrap();
        fs::write(collections_dir(&root).join("notes.txt"), "hello").unwrap();

        let found = read_collections(&root).unwrap();
        assert_eq!(found, vec![a_collection()]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_seed_carries_kiths_own_lines_and_the_globs_it_was_given() {
        // The globs are the caller's, verbatim and in order. The fixture uses
        // names no engine has ever used, which is the point: this module cannot
        // recognise them, so it cannot be quietly rewritten to know one.
        let root = scratch("seed");
        seed_stignore(&root, &[".engine-bookkeeping", "archive/**"]).unwrap();

        let lines: Vec<String> = fs::read_to_string(root.join(IGNORE_FILE))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            lines,
            vec![
                "(?d).kith/local",
                "(?d)*.kith-tmp",
                ".engine-bookkeeping",
                "archive/**",
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seeding_twice_adds_nothing_and_keeps_what_was_already_there() {
        // Adoption runs against a Circle whose ignores are somebody else's work.
        let root = scratch("seed-twice");
        fs::write(root.join(IGNORE_FILE), "*.tmp\n// hand-written\n").unwrap();

        seed_stignore(&root, &[".stfolder"]).unwrap();
        let once = fs::read_to_string(root.join(IGNORE_FILE)).unwrap();
        seed_stignore(&root, &[".stfolder"]).unwrap();
        let twice = fs::read_to_string(root.join(IGNORE_FILE)).unwrap();

        assert_eq!(once, twice, "seeding is idempotent");
        assert!(once.starts_with("*.tmp\n// hand-written\n"), "{once}");
        assert_eq!(once.matches("(?d).kith/local").count(), 1, "{once}");
        assert!(once.ends_with('\n'));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_seed_leaves_no_temp_file_behind() {
        let root = scratch("seed-litter");
        seed_stignore(&root, &[]).unwrap();

        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![IGNORE_FILE.to_string()], "{names:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}
