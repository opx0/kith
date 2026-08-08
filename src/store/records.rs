//! The record logs — one append-only log per Device, per Collection.
//!
//! A log names its writing Device in its path and only that Device ever writes
//! it, so two Members never write one file. Logs are appended to and never
//! rewritten, and [`derive_items`] is a pure function of the *union* of the
//! records — so read order and arrival order are irrelevant, and absorbing a
//! conflict copy is just reading one more log.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{Item, ItemId, PersonId};

/// The record schema this build writes and understands.
///
/// It rides on every line rather than on the file, so a log whose tail a newer
/// wallsync wrote is still readable up to that point.
const SCHEMA: u32 = 1;

/// How much of a log's tail is read to recover the next `seq`.
const TAIL_WINDOW: u64 = 64 * 1024;

/// One line of one log: a fact one Device asserted about one Item.
///
/// The Collection is the log's directory and the writing Device its filename, so
/// neither is a field; `by` is asserted, not proven, because nothing is signed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Record {
    /// This Item entered this Collection.
    Add {
        item: ItemId,
        by: PersonId,
        at: String,
        title: String,
        /// Relative to the Collection root, `/`-separated.
        path: String,
        hash: String,
        size: u64,
    },
    /// This Item's bytes are now at this path with this hash — a move, a rename
    /// or a re-encode, with the Item id untouched.
    Bind {
        item: ItemId,
        by: PersonId,
        at: String,
        path: String,
        hash: String,
        size: u64,
    },
    /// Tombstone: this Item is no longer in this Collection.
    Remove { item: ItemId, by: PersonId, at: String },
}

impl Record {
    /// The Item this record is about, before aliasing.
    pub fn item(&self) -> &ItemId {
        match self {
            Record::Add { item, .. } | Record::Bind { item, .. } | Record::Remove { item, .. } => item,
        }
    }

    /// The writing Device's wall clock — a total order, never a happens-before.
    pub fn at(&self) -> &str {
        match self {
            Record::Add { at, .. } | Record::Bind { at, .. } | Record::Remove { at, .. } => at,
        }
    }
}

/// Append one record to *this* Device's log for `collection`, durably.
///
/// A log whose last line has no `\n` was torn by a local crash mid-append: it is
/// terminated with a newline rather than truncated, so the damage costs one
/// record instead of two and no log is ever rewritten.
pub fn append(root: &Path, collection: &str, device: &str, rec: &Record) -> io::Result<()> {
    let path = log_path(root, collection, device)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let is_new_log = !path.exists();

    let mut file = OpenOptions::new().read(true).append(true).create(true).open(&path)?;
    let _lock = FileLock::acquire(&file)?;

    let (next_seq, terminated) = tail_state(&mut file)?;

    let mut line = String::new();
    if !terminated {
        line.push('\n');
    }
    line.push_str(&encode(rec, next_seq)?);
    line.push('\n');

    // One `write_all` under the lock, and the line carries its own terminator, so
    // no reader ever sees half a record.
    file.write_all(line.as_bytes())?;
    file.sync_data()?;

    // A durable first record inside a directory entry that was never written is
    // still a lost record.
    if is_new_log {
        if let Some(dir) = path.parent() {
            let _ = File::open(dir).and_then(|d| d.sync_all());
        }
    }
    Ok(())
}

/// Every record in the Collection, from every Device's log, conflict copies read
/// as ordinary logs.
///
/// A damaged line costs one record, never the log: unparseable lines, newer
/// schemas and unknown kinds are skipped and left on disk untouched.
pub fn read_all(root: &Path, collection: &str) -> io::Result<Vec<Record>> {
    let dir = collection_dir(root, collection)?;

    let mut logs: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect(),
        // A Collection nobody has written to yet is empty, not broken.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    logs.sort();

    let mut records = Vec::new();
    for log in &logs {
        let bytes = match std::fs::read(log) {
            Ok(b) => b,
            // A log can vanish between the listing and the read: the engine
            // renames, and an owning Device deletes its absorbed conflict copies.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        read_lines(&bytes, &mut records);
    }
    Ok(records)
}

/// Reduce the union of the records to the Items the Gallery shows.
///
/// Records are totally ordered by `at`, Items naming the same bytes alias to the
/// earliest `add`, the first `add` supplies attribution and the newest binding
/// the bytes, and a tombstone wins over anything not stamped strictly later. An
/// Item whose bytes have not arrived keeps its `hash` and `size` but has no
/// `path`, because a path is a claim about this Device's disk.
pub fn derive_items(records: &[Record], root: &Path) -> Vec<Item> {
    // 1. Total order. A reduced record set carries no `(device_id, seq)`, so ties
    //    on `at` break on the record's own canonical JSON — deterministic, and
    //    paid for only by the records that actually collide.
    let mut ordered: Vec<(i128, &Record)> = records.iter().map(|r| (instant(r.at()), r)).collect();
    ordered.sort_by_key(|(when, _)| *when);
    for tied in ordered.chunk_by_mut(|a, b| a.0 == b.0) {
        if tied.len() > 1 {
            tied.sort_by_cached_key(|(_, r)| serde_json::to_string(r).unwrap_or_default());
        }
    }

    // 2. Alias: the canonical Item for a hash is the one from the earliest `add`;
    //    every other Item id naming those bytes points at it.
    let mut canonical_for_hash: BTreeMap<&str, ItemId> = BTreeMap::new();
    let mut alias: BTreeMap<String, ItemId> = BTreeMap::new();
    for (_, rec) in &ordered {
        if let Record::Add { item, hash, .. } = rec {
            if hash.is_empty() {
                continue; // nothing to group on; leave the Item alone
            }
            match canonical_for_hash.get(hash.as_str()) {
                None => {
                    canonical_for_hash.insert(hash.as_str(), item.clone());
                }
                Some(canonical) if canonical != item => {
                    alias.entry(item.as_str().to_string()).or_insert_with(|| canonical.clone());
                }
                Some(_) => {}
            }
        }
    }

    // 3. Apply.
    let mut drafts: BTreeMap<String, Draft> = BTreeMap::new();
    let mut tombstones: BTreeMap<String, i128> = BTreeMap::new();
    for (when, rec) in &ordered {
        let id = resolve(&alias, rec.item());
        match rec {
            Record::Add { by, at, title, path, hash, size, .. } => {
                let draft = drafts.entry(id.as_str().to_string()).or_insert_with(|| Draft {
                    id: id.clone(),
                    added_by: by.clone(),
                    added_at: at.clone(),
                    title: title.clone(),
                    binding: None,
                    bound_at: i128::MIN,
                });
                draft.bind(*when, path, hash, *size);
            }
            Record::Bind { path, hash, size, .. } => {
                // A binding whose `add` was lost or has not arrived invents no
                // Item; `wallsync doctor` is where a sequence gap gets named.
                if let Some(draft) = drafts.get_mut(id.as_str()) {
                    draft.bind(*when, path, hash, *size);
                }
            }
            Record::Remove { .. } => {
                // Recorded whether or not its `add` is here, so the tombstone
                // survives an `add` that arrives afterwards.
                let latest = tombstones.entry(id.as_str().to_string()).or_insert(i128::MIN);
                *latest = (*latest).max(*when);
            }
        }
    }

    // Newest first, with the Item id as a stable tie-break so two Devices print
    // the same list. Each `added_at` is parsed once rather than per comparison.
    let mut keyed: Vec<(i128, Item)> = drafts
        .into_values()
        .filter(|d| match tombstones.get(d.id.as_str()) {
            Some(removed_at) => d.bound_at > *removed_at, // revived, strictly later
            None => true,
        })
        .map(|d| (instant(&d.added_at), d.into_item(root)))
        .collect();
    keyed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.id.as_str().cmp(a.1.id.as_str())));
    keyed.into_iter().map(|(_, item)| item).collect()
}

/// One Item under construction, `binding` being the winning `add`/`bind`.
///
/// A missing path simply means the bytes are not on this Device; finding a
/// duplicate copy of them elsewhere in the tree is reconciliation's job.
struct Draft {
    id: ItemId,
    added_by: PersonId,
    added_at: String,
    title: String,
    binding: Option<Binding>,
    bound_at: i128,
}

struct Binding {
    path: String,
    hash: String,
    size: u64,
}

impl Draft {
    /// Newest binding wins; records arrive in total order, so "newest" is "last".
    fn bind(&mut self, when: i128, path: &str, hash: &str, size: u64) {
        self.binding = Some(Binding { path: path.to_string(), hash: hash.to_string(), size });
        self.bound_at = when;
    }

    fn into_item(self, root: &Path) -> Item {
        let (path, hash, bytes) = match self.binding {
            Some(b) => (local_path(root, &b.path), Some(b.hash), Some(b.size)),
            None => (None, None, None),
        };
        Item {
            id: self.id,
            title: self.title,
            added_by: self.added_by,
            added_at: self.added_at,
            path,
            hash,
            bytes,
        }
    }
}

/// Follow an alias chain to the canonical Item, bounded because a hand-edited log
/// could describe a cycle.
fn resolve(alias: &BTreeMap<String, ItemId>, item: &ItemId) -> ItemId {
    let mut current = item.clone();
    for _ in 0..16 {
        match alias.get(current.as_str()) {
            Some(next) => current = next.clone(),
            None => break,
        }
    }
    current
}

/// An `at` as a comparable instant, because RFC 3339 strings of differing
/// precision do not compare correctly as text.
///
/// One that will not parse sorts first, so a damaged line cannot mask every
/// record after it.
fn instant(at: &str) -> i128 {
    at.parse::<jiff::Timestamp>().map(|t| t.as_nanosecond()).unwrap_or(i128::MIN)
}

/// Where a record's relative path lands on this Device, if its bytes are here.
///
/// Any admitted Device can write any path, so a path that escapes the Collection
/// root or is absolute binds nothing, and only a regular file counts as arrived.
fn local_path(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let mut resolved = root.to_path_buf();
    for segment in rel.split('/').filter(|s| !s.is_empty() && *s != ".") {
        let mut parts = Path::new(segment).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(name)), None) => resolved.push(name),
            _ => return None, // `..`, a root, or a platform prefix
        }
    }
    if resolved == root {
        return None;
    }
    match std::fs::symlink_metadata(&resolved) {
        Ok(meta) if meta.is_file() => Some(resolved),
        _ => None,
    }
}

fn collection_dir(root: &Path, collection: &str) -> io::Result<PathBuf> {
    Ok(root.join(".wallsync").join("items").join(segment(collection, "Collection id")?))
}

fn log_path(root: &Path, collection: &str, device: &str) -> io::Result<PathBuf> {
    let device = segment(device, "Device id")?;
    Ok(collection_dir(root, collection)?.join(format!("{device}.jsonl")))
}

/// Ids become path segments, so a malformed one must not name a file outside the
/// Circle.
fn segment<'a>(value: &'a str, what: &str) -> io::Result<&'a str> {
    let unusable = value.is_empty()
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', '\0'])
        || value.starts_with('.');
    if unusable {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unusable {what}: {value:?}")));
    }
    Ok(value)
}

/// Serialise one record as one line, stamped with the schema version and its
/// position in this log.
///
/// Nothing in the reduction reads `seq`; it exists so `wallsync doctor` can name a
/// gap rather than reduce a truncated log as if it were whole.
fn encode(rec: &Record, seq: u64) -> io::Result<String> {
    let mut value = serde_json::to_value(rec).map_err(invalid_data)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_data("a record must serialise as a JSON object"))?;
    object.insert("v".to_string(), serde_json::Value::from(SCHEMA));
    object.insert("seq".to_string(), serde_json::Value::from(seq));
    serde_json::to_string(&value).map_err(invalid_data)
}

/// Read a log's tail under the append lock: the `seq` to write next, and whether
/// the file ends on a complete line.
///
/// Bounded window rather than the whole log, so appending stays O(1) in its
/// length; if no `seq` is recoverable the count restarts at 1.
fn tail_state(file: &mut File) -> io::Result<(u64, bool)> {
    let len = file.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok((1, true));
    }

    let window = len.min(TAIL_WINDOW);
    file.seek(SeekFrom::Start(len - window))?;
    let mut tail = Vec::with_capacity(window as usize);
    file.read_to_end(&mut tail)?;
    file.seek(SeekFrom::End(0))?;

    let terminated = tail.last() == Some(&b'\n');
    let text = String::from_utf8_lossy(&tail);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !terminated {
        lines.pop(); // torn by a crash mid-append; not a record
    }
    if window < len {
        // The window may have opened mid-line; that fragment is not a record either.
        if !lines.is_empty() {
            lines.remove(0);
        }
    }

    let last_seq = lines
        .iter()
        .rev()
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .and_then(|value| value.get("seq").and_then(serde_json::Value::as_u64));

    Ok((last_seq.unwrap_or(0) + 1, terminated))
}

/// Parse one log's bytes, skipping what cannot be applied.
fn read_lines(bytes: &[u8], out: &mut Vec<Record>) {
    // Lossy rather than strict: a line mangled into invalid UTF-8 must not cost
    // the records around it.
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !text.ends_with('\n') {
        lines.pop(); // torn by a local crash mid-append; not a record
    }
    for line in lines {
        if let Some(rec) = parse_line(line.trim()) {
            out.push(rec);
        }
    }
}

fn parse_line(line: &str) -> Option<Record> {
    if line.is_empty() {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_str(line).ok()?;
    let object = value.as_object_mut()?;

    // A record from a newer wallsync is left alone rather than half-applied.
    let v = object.get("v").and_then(serde_json::Value::as_u64).unwrap_or(SCHEMA as u64);
    if v > SCHEMA as u64 {
        return None;
    }

    // wallsync writes the kind as `t` and accepts `k` too, so a line typed by hand
    // from the spec — or by `$EDITOR` during a repair — still reduces.
    if !object.contains_key("t") {
        if let Some(kind) = object.remove("k") {
            object.insert("t".to_string(), kind);
        }
    }

    // An unknown kind, or a record missing a field this build needs, is skipped;
    // unknown *fields* are ignored by serde and survive on disk untouched.
    serde_json::from_value(value).ok()
}

fn invalid_data<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// An advisory `flock` held for one append — two wallsync processes on one Device are
/// the only race there is.
///
/// Holds the descriptor rather than the `File` so the caller can still read its
/// own log's tail; sound because the guard is always declared after the `File`.
struct FileLock {
    #[cfg(unix)]
    fd: std::os::fd::RawFd,
}

#[cfg(unix)]
mod ffi {
    unsafe extern "C" {
        pub fn flock(fd: i32, operation: i32) -> i32;
    }
    pub const LOCK_EX: i32 = 2;
    pub const LOCK_UN: i32 = 8;
}

impl FileLock {
    #[cfg(unix)]
    fn acquire(file: &File) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let fd = file.as_raw_fd();
        // Blocking: the wait is bounded by one `write` plus one `fdatasync`.
        if unsafe { ffi::flock(fd, ffi::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Off Unix the append is still one complete line, but two local wallsync
    /// processes are not serialised.
    #[cfg(not(unix))]
    fn acquire(_file: &File) -> io::Result<Self> {
        Ok(Self {})
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            ffi::flock(self.fd, ffi::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANA_DEVICE: &str = "P56IOI7-MZJNU2Y-IQGDREY-DM2MGTI-MGL3BXN-PQ6W5BM-TBBZ4TJ-XZWICQ2";
    const BEN_DEVICE: &str = "K5J2FVL-B3QTXAO-7SWNDUE-HMR4YZI-6CPGA2N-XQTLB5V-JW3EOHY-RD6MSAK";

    /// A scratch Circle root under the system temp directory, never the Person's home.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("wallsync-records-{}-{}-{n}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn add(item: &ItemId, by: &PersonId, at: &str, title: &str, path: &str, hash: &str) -> Record {
        Record::Add {
            item: item.clone(),
            by: by.clone(),
            at: at.to_string(),
            title: title.to_string(),
            path: path.to_string(),
            hash: hash.to_string(),
            size: 1024,
        }
    }

    fn bind(item: &ItemId, by: &PersonId, at: &str, path: &str, hash: &str) -> Record {
        Record::Bind {
            item: item.clone(),
            by: by.clone(),
            at: at.to_string(),
            path: path.to_string(),
            hash: hash.to_string(),
            size: 2048,
        }
    }

    fn remove(item: &ItemId, by: &PersonId, at: &str) -> Record {
        Record::Remove { item: item.clone(), by: by.clone(), at: at.to_string() }
    }

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(path, b"bytes").unwrap();
    }

    fn log_of(root: &Path, device: &str) -> PathBuf {
        root.join(".wallsync/items/main").join(format!("{device}.jsonl"))
    }

    #[test]
    fn append_writes_one_complete_line_per_record_under_the_devices_own_name() {
        let root = scratch("append");
        let ana = PersonId::generate();
        let item = ItemId::generate();

        append(&root, "main", ANA_DEVICE, &add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa")).unwrap();
        append(&root, "main", ANA_DEVICE, &remove(&item, &ana, "2026-08-07T09:05:00Z")).unwrap();

        let text = std::fs::read_to_string(log_of(&root, ANA_DEVICE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'), "every record carries its own terminator");
        assert!(lines[0].contains(r#""t":"add""#));
        assert!(lines[1].contains(r#""t":"remove""#));

        // Every line carries the schema, and `seq` is gapless from 1.
        for (n, line) in lines.iter().enumerate() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["v"], 1);
            assert_eq!(value["seq"], n as u64 + 1);
        }
    }

    #[test]
    fn append_terminates_a_torn_line_rather_than_truncating_it() {
        let root = scratch("torn");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        append(&root, "main", ANA_DEVICE, &add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa")).unwrap();

        // A local crash mid-append leaves a stump with no newline.
        let log = log_of(&root, ANA_DEVICE);
        let mut torn = std::fs::read_to_string(&log).unwrap();
        torn.push_str(r#"{"v":1,"t":"add","seq":2,"item":"#);
        std::fs::write(&log, torn).unwrap();

        let second = ItemId::generate();
        append(&root, "main", ANA_DEVICE, &add(&second, &ana, "2026-08-07T09:01:00Z", "forest", "forest.jpg", "b3:bb")).unwrap();

        // W2: the stump is still on disk, and the new record parses.
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains(r#""seq":2,"item":"#), "the damaged bytes are not rewritten away");
        let records = read_all(&root, "main").unwrap();
        assert_eq!(records.len(), 2, "one damaged line costs one record, never the next one");
    }

    #[test]
    fn concurrent_appends_on_one_device_never_tear_each_other() {
        let root = scratch("concurrent");
        let ana = PersonId::generate();

        std::thread::scope(|s| {
            for _ in 0..8 {
                let root = root.clone();
                let ana = ana.clone();
                s.spawn(move || {
                    for _ in 0..10 {
                        let item = ItemId::generate();
                        let rec = add(&item, &ana, "2026-08-07T09:00:00Z", "t", "t.png", "b3:aa");
                        append(&root, "main", ANA_DEVICE, &rec).unwrap();
                    }
                });
            }
        });

        let text = std::fs::read_to_string(log_of(&root, ANA_DEVICE)).unwrap();
        assert_eq!(text.lines().count(), 80);
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("no line was torn by another process");
        }
        assert_eq!(read_all(&root, "main").unwrap().len(), 80);
    }

    #[test]
    fn a_log_named_by_an_unusable_id_is_refused_rather_than_written_outside_the_circle() {
        let root = scratch("segment");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        let rec = add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa");
        assert!(append(&root, "../escape", ANA_DEVICE, &rec).is_err());
        assert!(append(&root, "main", "..", &rec).is_err());
    }

    #[test]
    fn a_collection_nobody_has_written_to_reads_as_empty() {
        let root = scratch("empty");
        assert!(read_all(&root, "main").unwrap().is_empty());
    }

    #[test]
    fn read_all_skips_a_damaged_line_and_keeps_going() {
        let root = scratch("damaged");
        let ana = PersonId::generate();
        let dir = root.join(".wallsync/items/main");
        std::fs::create_dir_all(&dir).unwrap();

        let first = add(&ItemId::generate(), &ana, "2026-08-07T09:00:00Z", "a", "a.png", "b3:aa");
        let last = add(&ItemId::generate(), &ana, "2026-08-07T09:02:00Z", "c", "c.png", "b3:cc");
        let text = format!(
            "{}\n{{ not json at all\n\n{}\n",
            encode(&first, 1).unwrap(),
            encode(&last, 3).unwrap()
        );
        std::fs::write(dir.join(format!("{ANA_DEVICE}.jsonl")), text).unwrap();

        let records = read_all(&root, "main").unwrap();
        assert_eq!(records.len(), 2, "one bad line costs one record, never the log");
    }

    #[test]
    fn read_all_discards_a_trailing_line_that_was_never_terminated() {
        let root = scratch("unterminated");
        let ana = PersonId::generate();
        let dir = root.join(".wallsync/items/main");
        std::fs::create_dir_all(&dir).unwrap();
        let good = add(&ItemId::generate(), &ana, "2026-08-07T09:00:00Z", "a", "a.png", "b3:aa");
        let stump = encode(&good, 2).unwrap();
        let text = format!("{}\n{}", encode(&good, 1).unwrap(), &stump[..stump.len() / 2]);
        std::fs::write(dir.join(format!("{ANA_DEVICE}.jsonl")), text).unwrap();

        assert_eq!(read_all(&root, "main").unwrap().len(), 1);
    }

    #[test]
    fn read_all_absorbs_a_conflict_copy_as_one_more_log() {
        let root = scratch("conflict");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        append(&root, "main", ANA_DEVICE, &add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa")).unwrap();

        // The shape the engine leaves behind when one device id wrote twice.
        let dir = root.join(".wallsync/items/main");
        let copy = dir.join(format!("{ANA_DEVICE}.sync-conflict-20260807-091402-{BEN_DEVICE}.jsonl"));
        std::fs::copy(log_of(&root, ANA_DEVICE), copy).unwrap();

        // Merged, not resolved: the union is what the reducer wanted anyway.
        assert_eq!(read_all(&root, "main").unwrap().len(), 2);
        assert_eq!(derive_items(&read_all(&root, "main").unwrap(), &root).len(), 1);
    }

    #[test]
    fn records_from_a_newer_wallsync_are_skipped_rather_than_half_applied() {
        let root = scratch("newer");
        let dir = root.join(".wallsync/items/main");
        std::fs::create_dir_all(&dir).unwrap();
        let text = concat!(
            r#"{"v":2,"t":"add","seq":1,"at":"2026-08-07T09:00:00Z","by":"p-01k1yfq2m7vj3w8t0pz4rxab6c","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVD","title":"sunset","path":"sunset.png","hash":"b3:aa","size":1,"mood":"warm"}"#,
            "\n",
            r#"{"v":1,"t":"add","seq":2,"at":"2026-08-07T09:01:00Z","by":"p-01k1yfq2m7vj3w8t0pz4rxab6c","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVE","title":"forest","path":"forest.jpg","hash":"b3:bb","size":1}"#,
            "\n",
        );
        std::fs::write(dir.join(format!("{ANA_DEVICE}.jsonl")), text).unwrap();

        let records = read_all(&root, "main").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].at(), "2026-08-07T09:01:00Z");
        // Skipped, never rewritten: the upgrade still finds it.
        assert!(std::fs::read_to_string(dir.join(format!("{ANA_DEVICE}.jsonl"))).unwrap().contains(r#""v":2"#));
    }

    #[test]
    fn a_line_written_with_the_adr_spelling_of_the_kind_still_reduces() {
        let root = scratch("kind-alias");
        let dir = root.join(".wallsync/items/main");
        std::fs::create_dir_all(&dir).unwrap();
        let text = concat!(
            r#"{"v":1,"k":"add","seq":1,"at":"2026-08-07T09:00:00Z","by":"p-01k1yfq2m7vj3w8t0pz4rxab6c","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVD","title":"sunset","path":"sunset.png","hash":"b3:aa","size":7}"#,
            "\n",
        );
        std::fs::write(dir.join(format!("{ANA_DEVICE}.jsonl")), text).unwrap();

        let records = read_all(&root, "main").unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0], Record::Add { .. }));
    }

    #[test]
    fn an_unknown_kind_costs_its_own_line_and_nothing_else() {
        let root = scratch("unknown-kind");
        let ana = PersonId::generate();
        let dir = root.join(".wallsync/items/main");
        std::fs::create_dir_all(&dir).unwrap();
        let good = add(&ItemId::generate(), &ana, "2026-08-07T09:00:00Z", "a", "a.png", "b3:aa");
        let text = format!(
            "{}\n{}\n",
            r#"{"v":1,"t":"meta","seq":1,"at":"2026-08-07T09:00:00Z","by":"p-01k1yf","item":"01K1YFQ2M9CQ2E7B5NK0YH3RVD","title":"new"}"#,
            encode(&good, 2).unwrap()
        );
        std::fs::write(dir.join(format!("{ANA_DEVICE}.jsonl")), text).unwrap();

        assert_eq!(read_all(&root, "main").unwrap().len(), 1);
    }

    #[test]
    fn the_newest_binding_wins_for_an_items_bytes() {
        let root = scratch("bind");
        touch(&root, "nature/forest-4k.jpg");
        let ana = PersonId::generate();
        let ben = PersonId::generate();
        let item = ItemId::generate();

        let records = vec![
            add(&item, &ana, "2026-08-07T09:00:00Z", "forest", "forest.jpg", "b3:aa"),
            bind(&item, &ana, "2026-08-07T09:10:00Z", "old.jpg", "b3:bb"),
            bind(&item, &ben, "2026-08-07T09:20:00Z", "nature/forest-4k.jpg", "b3:cc"),
        ];
        let items = derive_items(&records, &root);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, Some(root.join("nature/forest-4k.jpg")));
        assert_eq!(items[0].hash.as_deref(), Some("b3:cc"));
        // An Item survives being moved and re-encoded: same id, same attribution.
        assert_eq!(items[0].id.as_str(), item.as_str());
        assert_eq!(items[0].added_by, ana);
        assert_eq!(items[0].added_at, "2026-08-07T09:00:00Z");
        assert_eq!(items[0].title, "forest");
    }

    #[test]
    fn a_tombstone_written_by_someone_who_did_not_add_the_item_still_removes_it() {
        let root = scratch("tombstone");
        touch(&root, "sunset.png");
        let ana = PersonId::generate();
        let ben = PersonId::generate();
        let item = ItemId::generate();

        // Ben is not the adder; readers honour the tombstone anyway.
        let records = vec![
            add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa"),
            remove(&item, &ben, "2026-08-07T09:30:00Z"),
        ];
        assert!(derive_items(&records, &root).is_empty());
        // …and the bytes outliving the tombstone change nothing.
        assert!(root.join("sunset.png").is_file());
    }

    #[test]
    fn a_tombstone_holds_even_when_its_add_is_read_afterwards() {
        let root = scratch("tombstone-order");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        // Same instant, remove first in the file.
        let records = vec![
            remove(&item, &ana, "2026-08-07T09:00:00Z"),
            add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa"),
        ];
        assert!(derive_items(&records, &root).is_empty(), "the same instant does not revive");
    }

    #[test]
    fn re_adding_removed_content_revives_the_original_item_and_its_adder() {
        let root = scratch("revive");
        touch(&root, "sunset.png");
        let ana = PersonId::generate();
        let ben = PersonId::generate();
        let item = ItemId::generate();

        let records = vec![
            add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa"),
            remove(&item, &ben, "2026-08-07T09:30:00Z"),
            add(&item, &ben, "2026-08-07T10:00:00Z", "sunset-again", "sunset.png", "b3:aa"),
        ];
        let items = derive_items(&records, &root);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].added_by, ana, "first add wins: Ana added it, Ben brought it back");
        assert_eq!(items[0].title, "sunset");
    }

    #[test]
    fn the_derived_items_are_the_same_whatever_order_the_records_arrive_in() {
        let root = scratch("converge");
        touch(&root, "sunset.png");
        touch(&root, "forest.jpg");
        let ana = PersonId::generate();
        let ben = PersonId::generate();
        let (a, b) = (ItemId::generate(), ItemId::generate());

        let records = vec![
            add(&a, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa"),
            add(&b, &ben, "2026-08-07T09:10:00Z", "forest", "forest.jpg", "b3:bb"),
            bind(&a, &ana, "2026-08-07T09:20:00Z", "sunset.png", "b3:cc"),
            remove(&b, &ana, "2026-08-07T09:30:00Z"),
            bind(&b, &ben, "2026-08-07T09:40:00Z", "forest.jpg", "b3:dd"),
        ];

        let expected = fingerprint(&derive_items(&records, &root));
        let mut permutations = 0;
        permute(&mut records.clone(), 0, &mut |shuffled| {
            permutations += 1;
            assert_eq!(fingerprint(&derive_items(shuffled, &root)), expected);
        });
        assert_eq!(permutations, 120, "every ordering of five records was checked");
        // The last bind is later than the tombstone, so `forest` came back.
        assert_eq!(derive_items(&records, &root).len(), 2);
    }

    #[test]
    fn two_devices_adopting_the_same_bytes_converge_on_one_item() {
        let root = scratch("adopt");
        touch(&root, "sunset.png");
        let ana = PersonId::generate();
        let ben = PersonId::generate();

        // `at` is the file's mtime, so the two records differ only in Item id and Person.
        let mtime = "2026-01-02T03:04:05Z";
        let ana_record = add(&ItemId::generate(), &ana, mtime, "sunset", "sunset.png", "b3:aa");
        let ben_record = add(&ItemId::generate(), &ben, mtime, "sunset", "sunset.png", "b3:aa");

        let one = derive_items(&[ana_record.clone(), ben_record.clone()], &root);
        let other = derive_items(&[ben_record, ana_record], &root);
        assert_eq!(one.len(), 1, "one tile, not two, once both logs have crossed");
        assert_eq!(fingerprint(&one), fingerprint(&other), "both Devices reach it without talking");
    }

    #[test]
    fn a_record_whose_bytes_have_not_arrived_yields_an_item_with_no_path() {
        let root = scratch("byteless");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        let records = vec![add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa")];

        let items = derive_items(&records, &root);
        assert_eq!(items.len(), 1);
        assert!(items[0].path.is_none(), "a path is a claim about this Device's disk");
        // The record's own facts survive, so the placeholder tile still has a size.
        assert_eq!(items[0].bytes, Some(1024));
        assert_eq!(items[0].hash.as_deref(), Some("b3:aa"));
    }

    #[test]
    fn a_record_path_that_escapes_the_collection_root_binds_nothing() {
        let root = scratch("escape");
        touch(&root, "sunset.png");
        let ana = PersonId::generate();
        let item = ItemId::generate();

        for hostile in ["../sunset.png", "/etc/passwd", "nature/../../sunset.png"] {
            let records = vec![add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", hostile, "b3:aa")];
            let items = derive_items(&records, &root);
            assert_eq!(items.len(), 1, "the Item is still an Item");
            assert!(items[0].path.is_none(), "{hostile} must not resolve to bytes");
        }
    }

    #[test]
    fn a_binding_with_no_add_is_not_an_item() {
        let root = scratch("orphan-bind");
        touch(&root, "sunset.png");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        let records = vec![bind(&item, &ana, "2026-08-07T09:00:00Z", "sunset.png", "b3:aa")];
        assert!(derive_items(&records, &root).is_empty(), "nothing is invented for a lost add");
    }

    #[test]
    fn items_come_back_newest_first() {
        let root = scratch("order");
        let ana = PersonId::generate();
        let records = vec![
            add(&ItemId::generate(), &ana, "2026-08-07T09:00:00Z", "first", "a.png", "b3:aa"),
            add(&ItemId::generate(), &ana, "2026-08-07T11:00:00Z", "third", "c.png", "b3:cc"),
            add(&ItemId::generate(), &ana, "2026-08-07T10:00:00Z", "second", "b.png", "b3:bb"),
        ];
        let titles: Vec<String> = derive_items(&records, &root).into_iter().map(|i| i.title).collect();
        assert_eq!(titles, vec!["third", "second", "first"]);
    }

    #[test]
    fn timestamps_of_differing_precision_still_order_by_time() {
        let root = scratch("precision");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        let records = vec![
            add(&item, &ana, "2026-08-07T09:00:00Z", "sunset", "sunset.png", "b3:aa"),
            // Later by 117 ms, and lexicographically *earlier* than the line above.
            bind(&item, &ana, "2026-08-07T09:00:00.117Z", "moved.png", "b3:bb"),
        ];
        let items = derive_items(&records, &root);
        assert_eq!(items[0].hash.as_deref(), Some("b3:bb"));
    }

    #[test]
    fn a_record_round_trips_through_one_written_line() {
        let root = scratch("roundtrip");
        let ana = PersonId::generate();
        let item = ItemId::generate();
        // A title with a newline in it must not be able to tear a log.
        let rec = add(&item, &ana, "2026-08-07T09:00:00Z", "sun\nset", "sun set.png", "b3:aa");
        append(&root, "main", BEN_DEVICE, &rec).unwrap();

        let text = std::fs::read_to_string(log_of(&root, BEN_DEVICE)).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert_eq!(read_all(&root, "main").unwrap(), vec![rec]);
    }

    /// The whole derived view as comparable text, so a convergence assertion says
    /// what actually differs.
    fn fingerprint(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .map(|i| {
                format!(
                    "{}|{}|{}|{}|{:?}|{:?}|{:?}",
                    i.id, i.title, i.added_by, i.added_at, i.path, i.hash, i.bytes
                )
            })
            .collect()
    }

    /// Heap's algorithm: every ordering of the records.
    fn permute(records: &mut Vec<Record>, k: usize, visit: &mut impl FnMut(&[Record])) {
        if k + 1 >= records.len() {
            visit(records);
            return;
        }
        for i in k..records.len() {
            records.swap(k, i);
            permute(records, k + 1, visit);
            records.swap(k, i);
        }
    }
}
