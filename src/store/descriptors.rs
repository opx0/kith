//! The Circle and Collection descriptors, and the protocol that writes them.
//!
//! `circle.toml` says which Circle this tree is and who founded it;
//! `collections/<id>.toml` says which Provider a Collection's Items belong to.
//! Descriptors are the one rewritable thing in an otherwise append-only tree, and
//! what makes the rewrite safe with no coordinator is the protocol below — write
//! beside the target, flush, rename — under a temp name the Sync Engine ignores.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The descriptor schema this build writes and understands.
///
/// A descriptor carrying a higher one is still returned as it was found: refusing
/// to read it would break the Circle for the older Device.
pub const SCHEMA: u32 = 1;

/// Every byte of a Circle's shared state lives under this one hidden directory.
const WALLSYNC_DIR: &str = ".wallsync";
const CIRCLE_FILE: &str = "circle.toml";
const COLLECTIONS_DIR: &str = "collections";

/// The suffix that makes a half-written descriptor invisible to the Sync Engine.
///
/// This constant and the line `seed_stignore` writes are one decision, so they
/// are spelled once, here.
const TMP_SUFFIX: &str = ".wallsync-tmp";

/// The name of the engine's per-Circle ignore file.
const IGNORE_FILE: &str = ".stignore";

/// The engine's prefix for "you may delete this to unblock a directory removal".
///
/// wallsync applies it to its own two paths and to nothing else, which is why nothing
/// authoritative is ever stored under either.
const DELETE_OK: &str = "(?d)";

/// Which Circle this tree is, who founded it, and whose Device is its Steward's.
///
/// Every surface naming the Steward reads `founder_device` from here rather than
/// from the transport, because it is the one fact that reads the same from every
/// Device. A Device that has never run wallsync has published no Membership claim, so
/// a Circle can know its Steward's Device and still be unable to name the Person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CircleDescriptor {
    pub schema: u32,
    /// The Circle's immutable id, spelled `circle` on disk because the file is
    /// read by other wallsync builds and by People with an editor.
    #[serde(rename = "circle")]
    pub id: String,
    /// The Circle's name — mutable in a later milestone, write-once in v0.1.
    pub name: String,
    /// RFC 3339, and the tie-break when two Devices each claimed a Circle that had
    /// no descriptor yet: earliest `created` wins.
    pub created: String,
    pub founder_person: String,
    pub founder_device: String,
}

/// Which Provider a Collection's Items belong to.
///
/// The id is opaque in the format, so additional Collections need no format
/// change; a later wallsync's extra fields are read here without complaint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionDescriptor {
    pub schema: u32,
    /// The Collection's id, and also its filename.
    pub collection: String,
    /// The Provider that claims this Collection's Items — `wallpaper` in v0.1.
    ///
    /// A Provider this build lacks is read faithfully rather than refused, so the
    /// layer above can say which one is missing.
    pub provider: String,
}

/// `<root>/.wallsync` — a Circle's shared state.
pub fn wallsync_dir(root: &Path) -> PathBuf {
    root.join(WALLSYNC_DIR)
}

/// `<root>/.wallsync/circle.toml`.
pub fn circle_path(root: &Path) -> PathBuf {
    wallsync_dir(root).join(CIRCLE_FILE)
}

/// `<root>/.wallsync/collections` — a directory, because Circle→Collection is
/// one-to-many.
pub fn collections_dir(root: &Path) -> PathBuf {
    wallsync_dir(root).join(COLLECTIONS_DIR)
}

/// Serialise `value` as TOML and put it at `path` without ever letting a partial
/// document exist under that name.
///
/// Write beside the target, flush, then `rename(2)` over it: a reader sees either
/// the old descriptor or the new one, and the temp name never replicates. Missing
/// parent directories are created.
pub fn write_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = toml::to_string_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    write_bytes_atomic(path, text.as_bytes())
}

/// Read this Circle's descriptor; `Ok(None)` is "not here yet", malformed is an error.
///
/// A Member who adopts before the Steward's Device has upgraded holds a working
/// Circle whose descriptor arrives later, so absence is a state and not a fault.
pub fn read_circle(root: &Path) -> io::Result<Option<CircleDescriptor>> {
    read_toml(&circle_path(root))
}

/// Write this Circle's descriptor.
///
/// Write-once is a milestone rule enforced by `wallsync create`, not here; what is
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
/// Only files named `<id>.toml` with no dot inside the id are read, so a copy the
/// engine leaves beside a descriptor never becomes a phantom second Collection.
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

/// Seed this Circle's ignore file so wallsync's own scratch and its half-written
/// descriptors never leave this Device.
///
/// `reserved` is written verbatim, in the order given, and existing lines are
/// preserved in place — so adoption may call this unconditionally. Note this is
/// *not* `SyncEngine::reserved_paths()`, which includes conflict copies and must
/// keep replicating; pass the globs whose replication this Circle means to stop.
pub fn seed_stignore(root: &Path, reserved: &[&str]) -> io::Result<()> {
    let path = root.join(IGNORE_FILE);

    let existing = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    let mut wanted = vec![
        format!("{DELETE_OK}{WALLSYNC_DIR}/local"),
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

fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // Unknown keys are ignored rather than refused, so a descriptor written by a
    // newer wallsync stays readable. Nothing is dropped: v0.1 never rewrites one it
    // did not just create.
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
        // Flush before the rename publishes them, so a crash cannot leave a
        // descriptor whose name is new and whose content is not.
        .and_then(|()| file.sync_all());
    drop(file);

    if let Err(e) = staged.and_then(|()| fs::rename(&tmp, path)) {
        // Harmless if it survived, but a Circle a Person cannot read with `ls` is
        // half the point of this format gone.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Best effort: makes the rename itself durable. Not every filesystem permits
    // it, and the descriptor is whole either way.
    if let Some(dir) = path.parent() {
        let _ = fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// `<target>.wallsync-tmp`, beside the target so the rename stays within one filesystem.
///
/// Exclusive creation with a pid-qualified fallback, because two wallsync processes
/// sharing one temp file would interleave into a torn document the rename then
/// publishes. Both names end in the suffix the engine ignores.
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
/// Minted ids carry no dot, so refusing separators and dots costs nothing and
/// keeps the reading rule in [`read_collections`] honest.
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
            "wallsync-descriptors-{}-{n}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("scratch root");
        root
    }

    fn a_circle() -> CircleDescriptor {
        CircleDescriptor {
            schema: SCHEMA,
            id: "wallsync-4tj2q9xa".into(),
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
        let root = scratch("circle-keys");
        write_circle(&root, &a_circle()).unwrap();

        let text = fs::read_to_string(circle_path(&root)).unwrap();
        assert!(text.contains("circle = \"wallsync-4tj2q9xa\""), "{text}");
        assert!(!text.contains("id ="), "the id is spelled `circle` on disk: {text}");
        assert!(text.contains("founder_device = "), "{text}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_absent_descriptor_is_not_a_fault() {
        let root = scratch("absent");
        assert!(read_circle(&root).unwrap().is_none());
        assert!(read_collection(&root, "main").unwrap().is_none());
        assert!(read_collections(&root).unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_malformed_descriptor_is_an_error() {
        let root = scratch("malformed");
        fs::create_dir_all(wallsync_dir(&root)).unwrap();
        fs::write(circle_path(&root), "this is not = = toml").unwrap();

        let err = read_circle(&root).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("circle.toml"), "name the file: {err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_descriptor_missing_a_required_fact_is_an_error_not_a_default() {
        let root = scratch("incomplete");
        fs::create_dir_all(wallsync_dir(&root)).unwrap();
        fs::write(circle_path(&root), "schema = 1\ncircle = \"wallsync-4tj2q9xa\"\n").unwrap();

        assert_eq!(
            read_circle(&root).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unknown_keys_are_ignored_rather_than_refused() {
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

        let names: Vec<String> = fs::read_dir(wallsync_dir(&root))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![CIRCLE_FILE.to_string()], "{names:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_write_leaves_no_temp_file_either() {
        // A directory where the descriptor should be: the rename cannot land.
        let root = scratch("failed-write");
        let target = wallsync_dir(&root).join("circle.toml");
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
        assert_eq!(name, "circle.toml.wallsync-tmp");

        seed_stignore(&root, &[]).unwrap();
        let ignores = fs::read_to_string(root.join(IGNORE_FILE)).unwrap();
        let pattern = format!("{DELETE_OK}*{TMP_SUFFIX}");
        assert!(ignores.lines().any(|l| l == pattern), "{ignores}");
        assert!(name.ends_with(TMP_SUFFIX));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_rewrite_replaces_the_descriptor_whole() {
        let root = scratch("rewrite");
        write_circle(&root, &a_circle()).unwrap();

        let mut renamed = a_circle();
        renamed.name = "wallpapers".into();
        write_circle(&root, &renamed).unwrap();

        assert_eq!(read_circle(&root).unwrap().unwrap(), renamed);
        let names: Vec<String> = fs::read_dir(wallsync_dir(&root))
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
    fn the_seed_carries_wallsyncs_own_lines_and_the_globs_it_was_given() {
        // The fixture uses names no engine has ever used, so this module cannot
        // be quietly rewritten to recognise one.
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
                "(?d).wallsync/local",
                "(?d)*.wallsync-tmp",
                ".engine-bookkeeping",
                "archive/**",
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seeding_twice_adds_nothing_and_keeps_what_was_already_there() {
        let root = scratch("seed-twice");
        fs::write(root.join(IGNORE_FILE), "*.tmp\n// hand-written\n").unwrap();

        seed_stignore(&root, &[".stfolder"]).unwrap();
        let once = fs::read_to_string(root.join(IGNORE_FILE)).unwrap();
        seed_stignore(&root, &[".stfolder"]).unwrap();
        let twice = fs::read_to_string(root.join(IGNORE_FILE)).unwrap();

        assert_eq!(once, twice, "seeding is idempotent");
        assert!(once.starts_with("*.tmp\n// hand-written\n"), "{once}");
        assert_eq!(once.matches("(?d).wallsync/local").count(), 1, "{once}");
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
