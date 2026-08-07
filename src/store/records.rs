//! The record logs — one append-only log per Device, per Collection (ADR-0004 §§3–6).
//!
//! This is the layer that makes a Sidecar possible on a transport with no
//! coordinator. Three rules from ADR-0004 §1 hold every line of it up:
//!
//! * **W1** — a log names its writing Device in its path, and only that Device
//!   ever writes it. Two Members therefore never write one file, so a conflict is
//!   not something to resolve but something that structurally cannot happen.
//! * **W2** — logs are appended to and never rewritten, so the one remaining
//!   conflict generator (rewrite-in-flight) is gone too.
//! * **W3** — [`derive_items`] is a pure function of the *union* of the records.
//!   Read order, arrival order and which copy of a conflicted file won are all
//!   irrelevant, which is why absorbing a conflict copy is just reading one more
//!   log rather than a merge anybody has to think about.
//!
//! A Sidecar is not a file (ADR-0004 §4): it is what [`derive_items`] makes of
//! the records. Nothing in this module writes one.
//!
//! JSON Lines rather than TOML for the same reason: a damaged line costs exactly
//! one record, never the log, and appending one is a single `write(2)`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{Item, ItemId, PersonId};

/// The record schema this build writes and understands (ADR-0004 §11).
///
/// It rides on every line rather than on the file, so a log whose tail was
/// written by a newer kith is still fully readable up to the point where it
/// stops being understood.
const SCHEMA: u32 = 1;

/// How much of a log's tail is read to recover the next `seq`. ~250 bytes per
/// record (ADR-0004 §4.2), so this covers hundreds of them without reading a
/// large log from the start on every append.
const TAIL_WINDOW: u64 = 64 * 1024;

/// One line of one log: a fact one Device asserted about one Item.
///
/// The Collection is encoded by the log's directory and the writing Device by its
/// filename, so neither is a field (ADR-0004 §4.2).
///
/// `by` is **asserted, not proven**. Records are not signed and never will be in
/// v0.1: the Sync Engine's key material is off limits (ADR-0002 §2) and kith runs
/// no second identity system. Any admitted Device can write any path in the tree,
/// so a `by` is believable because a human admitted that Device — the same
/// honesty the product owes about Roles. Every surface that renders attribution
/// says so.
///
/// Reserved by ADR-0004 §4.2 and deliberately unwritten here: `meta` records
/// (titles and tags, v0.3), `facts` and `adopted` on an `add`, and `sig` on any
/// record (v1.0). Unknown fields and unknown kinds are ignored on read and never
/// rewritten, so a later build adds any of them without a migration.
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
    /// or a re-encode. The Item id is untouched: an Item survives all three.
    Bind {
        item: ItemId,
        by: PersonId,
        at: String,
        path: String,
        hash: String,
        size: u64,
    },
    /// Tombstone: this Item is no longer in this Collection.
    ///
    /// Reversible in data — the `add` still exists — and honoured by every reader
    /// regardless of who wrote it (see [`derive_items`]).
    Remove { item: ItemId, by: PersonId, at: String },
}

impl Record {
    /// The Item this record is about, before aliasing (ADR-0004 §4.4 step 3).
    pub fn item(&self) -> &ItemId {
        match self {
            Record::Add { item, .. } | Record::Bind { item, .. } | Record::Remove { item, .. } => item,
        }
    }

    /// The writing Device's wall clock. A total order, never a happens-before.
    pub fn at(&self) -> &str {
        match self {
            Record::Add { at, .. } | Record::Bind { at, .. } | Record::Remove { at, .. } => at,
        }
    }

    /// The Person the writing Device claims acted. Asserted, not proven.
    pub fn by(&self) -> &PersonId {
        match self {
            Record::Add { by, .. } | Record::Bind { by, .. } | Record::Remove { by, .. } => by,
        }
    }
}

/// Append one record to *this* Device's log for `collection`, durably.
///
/// The protocol is ADR-0004 §3's, exactly: open append-only, take an advisory
/// lock, write one complete line including its `\n`, flush it to the platter,
/// release. The lock guards the only same-Device race there is — two kith
/// processes, say a `kith add` while the TUI deletes — because every other writer
/// in the Circle is on another Device and writes another file (W1).
///
/// A log whose last line has no `\n` was torn by a local crash mid-append. This
/// terminates it with a newline instead of truncating it: W2 forbids rewriting a
/// log, and the alternative — appending straight onto the stump — would splice
/// the damaged bytes onto the new record and cost two records instead of one.
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
    // no reader — local or remote — ever sees half a record. Titles cannot tear a
    // line either: JSON escapes every control character, which is half the reason
    // the format is JSON.
    file.write_all(line.as_bytes())?;
    file.sync_data()?;

    // Flushing the record is not enough the first time: a crash could otherwise
    // leave a durable record inside a directory entry that was never written, and
    // the first record of a log is usually the one that matters.
    if is_new_log {
        if let Some(dir) = path.parent() {
            let _ = File::open(dir).and_then(|d| d.sync_all());
        }
    }
    Ok(())
}

/// Every record in the Collection, from every Device's log.
///
/// Conflict copies are read as ordinary logs and never resolved: W3 makes the
/// union indifferent to which copy the engine kept, so absorbing one is free
/// (ADR-0004 §8). Only the Device that owns a log ever deletes its conflict copy.
///
/// **A damaged line costs one record, never the log.** Unparseable lines, records
/// from a newer schema and unknown kinds are skipped and the read continues —
/// this is the whole reason the format is line-oriented. Nothing here rewrites a
/// source file, so what was skipped survives on disk for a later kith (or a
/// human with `$EDITOR`) to read.
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
            // The engine stages and renames, and an owning Device deletes its own
            // absorbed conflict copies: a log can vanish between the listing and
            // the read. Every other I/O error is real and is reported.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        read_lines(&bytes, &mut records);
    }
    Ok(records)
}

/// Reduce the union of the records to the Items the Gallery shows — ADR-0004
/// §4.4, minus the steps that need a disk walk (those belong to reconciliation).
///
/// `root` is the Collection root; record paths are relative to it and are only
/// ever *stat*ed here, never read or hashed.
///
/// The rules, and why each one is what it is:
///
/// * **Total order** by `(at, record)`. Deterministic on every Device, and a
///   pure function of the record set — so records read in any order reduce to
///   the same Items.
/// * **Alias by content hash.** Two Devices adopting the same pre-existing tree
///   mint two Item ids for one file; the earliest `add` is canonical and the rest
///   alias to it, so both Devices converge on one Item without talking.
/// * **First `add` wins** for `added_by`, `added_at` and `title`; every later
///   `add` contributes only its binding. That is what makes re-adding removed
///   content revive the *original* Item with the original adder's name.
/// * **Newest binding wins** for an Item's bytes.
/// * **A tombstone wins regardless of who wrote it.** Honouring a removal
///   conditionally on the remover's Role would make two Devices disagree about
///   the Gallery depending on when the Membership claims reached them, and a
///   policy check that costs convergence buys nothing because it enforces
///   nothing (ADR-0004 §6). An `add` or `bind` stamped *later* than the tombstone
///   revives the Item; the same instant does not.
///
/// An Item whose bytes have not arrived keeps its `hash` and `size` — they are
/// what the record declares — and has **no `path`**, because a path is a claim
/// about this Device's disk. A 250-byte record beats a 4 MB wallpaper across the
/// wire, so metadata-without-bytes is the normal arrival state and renders as a
/// placeholder rather than as nothing.
pub fn derive_items(records: &[Record], root: &Path) -> Vec<Item> {
    // 1. Total order (ADR-0004 §4.4 step 2). The ADR breaks ties on
    //    `(device_id, seq)`; a reduced record set carries neither, so the
    //    tie-break is the record's own canonical JSON — equally deterministic,
    //    and identical on every Device that holds the record.
    let mut ordered: Vec<(i128, String, &Record)> = records
        .iter()
        .map(|r| (instant(r.at()), serde_json::to_string(r).unwrap_or_default(), r))
        .collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    // 2. Alias (step 3): the canonical Item for a hash is the one from the
    //    earliest `add`; every other Item id naming those bytes points at it.
    let mut canonical_for_hash: BTreeMap<&str, ItemId> = BTreeMap::new();
    let mut alias: BTreeMap<String, ItemId> = BTreeMap::new();
    for (_, _, rec) in &ordered {
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

    // 3. Apply (step 4).
    let mut drafts: BTreeMap<String, Draft> = BTreeMap::new();
    let mut tombstones: BTreeMap<String, i128> = BTreeMap::new();
    for (when, _, rec) in &ordered {
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
                // A binding for an Item no `add` ever introduced is not an Item:
                // its `add` was lost or has not arrived. Nothing is invented for
                // it; `kith doctor` is where a sequence gap gets named.
                if let Some(draft) = drafts.get_mut(id.as_str()) {
                    draft.bind(*when, path, hash, *size);
                }
            }
            Record::Remove { .. } => {
                // Recorded whoever wrote it and whether or not its `add` is here,
                // so the tombstone survives an `add` that arrives afterwards.
                let latest = tombstones.entry(id.as_str().to_string()).or_insert(i128::MIN);
                *latest = (*latest).max(*when);
            }
        }
    }

    let mut items: Vec<Item> = drafts
        .into_values()
        .filter(|d| match tombstones.get(d.id.as_str()) {
            Some(removed_at) => d.bound_at > *removed_at, // revived, strictly later
            None => true,
        })
        .map(|d| d.into_item(root))
        .collect();

    // Newest first, the order every surface that lists Items wants, with the
    // Item id as a stable tie-break so two Devices print the same list.
    items.sort_by(|a, b| {
        instant(&b.added_at)
            .cmp(&instant(&a.added_at))
            .then_with(|| b.id.as_str().cmp(a.id.as_str()))
    });
    items
}

// ── the reduction's working state ────────────────────────────────────

/// One Item under construction. `binding` is the effective binding as ADR-0004
/// §4.4 resolves it: the winning `add`/`bind`. The ADR's fallback for a winning
/// path that is missing while a *duplicate copy* of the same bytes sits elsewhere
/// in the tree needs a hashed disk walk, which is reconciliation's job and not a
/// reducer's; here a missing path simply means the bytes are not on this Device.
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
    /// Newest binding wins. Records arrive here in total order, so "newest" is
    /// simply "last".
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

/// Follow an alias chain to the canonical Item. Bounded, because a hand-edited
/// log could in principle describe a cycle and a Gallery that hangs is worse than
/// one that shows an extra tile.
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

/// An `at` as a comparable instant. RFC 3339 strings of differing precision do
/// not compare correctly as text, so they are parsed. A timestamp that will not
/// parse sorts first: it is the least trustworthy thing in the set, and letting
/// it sort last would let one damaged line mask every later record.
fn instant(at: &str) -> i128 {
    at.parse::<jiff::Timestamp>().map(|t| t.as_nanosecond()).unwrap_or(i128::MIN)
}

/// Where a record's relative path lands on this Device, if its bytes are here.
///
/// Any admitted Device can write any path in the tree (ADR-0004 §5), so a record
/// naming `../.ssh/id_ed25519` is a shape kith must expect rather than trust: a
/// path that escapes the Collection root, or is absolute, binds nothing. Symlinks
/// are not Items either — kith does not sync links — so only a regular file counts
/// as arrived.
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

// ── on-disk plumbing ─────────────────────────────────────────────────

fn collection_dir(root: &Path, collection: &str) -> io::Result<PathBuf> {
    Ok(root.join(".kith").join("items").join(segment(collection, "Collection id")?))
}

fn log_path(root: &Path, collection: &str, device: &str) -> io::Result<PathBuf> {
    let device = segment(device, "Device id")?;
    Ok(collection_dir(root, collection)?.join(format!("{device}.jsonl")))
}

/// Ids become path segments, and a Collection id or Device id arrives from a
/// descriptor or from the engine rather than from this module. One check keeps a
/// malformed one from naming a file outside the Circle.
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
/// `seq` is per-log, monotonic and gapless from 1. Nothing in the reduction reads
/// it — the merge is a pure function of the union and needs no sequence — but it
/// is what lets `kith doctor` say *records 4–6 of this log are missing* instead of
/// silently reducing a truncated log as if it were whole.
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
/// The last complete line is found within a bounded window rather than by reading
/// the whole log, so appending stays O(1) in the log's length. If no `seq` is
/// recoverable the count restarts at 1; a repeated `seq` is a *forked log*, which
/// `doctor` names (ADR-0004 §8) and which costs the reduction nothing.
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

/// Parse one log's bytes, skipping what cannot be applied and counting nothing
/// against the rest.
fn read_lines(bytes: &[u8], out: &mut Vec<Record>) {
    // Lossy rather than strict: a line mangled into invalid UTF-8 must not cost
    // the records around it.
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if !text.ends_with('\n') {
        // A trailing line with no terminator was torn by a local crash
        // mid-append. A remote reader never sees one — the engine stages incoming
        // files and renames them into place.
        lines.pop();
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

    // A record from a newer kith is left alone rather than half-applied: degraded,
    // never broken (ADR-0004 §11). It stays on disk untouched for the upgrade.
    let v = object.get("v").and_then(serde_json::Value::as_u64).unwrap_or(SCHEMA as u64);
    if v > SCHEMA as u64 {
        return None;
    }

    // ADR-0004 §4.2 prints the kind under `k`; the module contract fixes the tag
    // as `t`. kith writes `t` and accepts either, so a line typed by hand from the
    // ADR — or by `$EDITOR` during a repair, which the format exists to allow —
    // still reduces.
    if !object.contains_key("t") {
        if let Some(kind) = object.remove("k") {
            object.insert("t".to_string(), kind);
        }
    }

    // An unknown kind, or a record missing a field this build needs, is skipped.
    // Unknown *fields* are ignored by serde and survive on disk untouched, because
    // nothing here ever rewrites a log.
    serde_json::from_value(value).ok()
}

fn invalid_data<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// An advisory `flock` held for one append.
///
/// Two kith processes on one Device are the only race there is (W1 gives every
/// other writer its own file), and this is the cheapest thing that serialises
/// them. It is declared here rather than pulled in as a dependency: one libc
/// symbol does not earn a crate.
/// Holds the descriptor rather than the `File` so the caller can still read its
/// own log's tail while the lock is held. Sound because the guard is always
/// declared after the `File` it locks and so is dropped before it.
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
        // Blocking: the other holder is another kith process writing one line, so
        // the wait is bounded by one `write` plus one `fdatasync`.
        if unsafe { ffi::flock(fd, ffi::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// v0.1 ships on Linux. Elsewhere the append is still one complete line, but
    /// two local kith processes are not serialised — stated rather than implied.
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

    /// A scratch Circle root under the system temp directory — never the Person's
    /// home, which holds the one file kith cannot rebuild.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kith-records-{}-{}-{n}", std::process::id(), name));
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
        root.join(".kith/items/main").join(format!("{device}.jsonl"))
    }

    // ── the append protocol ──────────────────────────────────────────

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

    // ── reading ──────────────────────────────────────────────────────

    #[test]
    fn a_collection_nobody_has_written_to_reads_as_empty() {
        let root = scratch("empty");
        assert!(read_all(&root, "main").unwrap().is_empty());
    }

    #[test]
    fn read_all_skips_a_damaged_line_and_keeps_going() {
        let root = scratch("damaged");
        let ana = PersonId::generate();
        let dir = root.join(".kith/items/main");
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
        let dir = root.join(".kith/items/main");
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
        let dir = root.join(".kith/items/main");
        let copy = dir.join(format!("{ANA_DEVICE}.sync-conflict-20260807-091402-{BEN_DEVICE}.jsonl"));
        std::fs::copy(log_of(&root, ANA_DEVICE), copy).unwrap();

        // Merged, not resolved: the union is what the reducer wanted anyway.
        assert_eq!(read_all(&root, "main").unwrap().len(), 2);
        assert_eq!(derive_items(&read_all(&root, "main").unwrap(), &root).len(), 1);
    }

    #[test]
    fn records_from_a_newer_kith_are_skipped_rather_than_half_applied() {
        let root = scratch("newer");
        let dir = root.join(".kith/items/main");
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
        let dir = root.join(".kith/items/main");
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
        let dir = root.join(".kith/items/main");
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

    // ── the reduction ────────────────────────────────────────────────

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

        // Ben is not the adder, and in v0.1 he is not even the Steward. Readers
        // honour the tombstone anyway: a Role is policy, not enforcement, and
        // honouring a removal conditionally would cost convergence.
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
        // Same instant, remove first in the file: the reduction must not depend on
        // which line happened to be read first.
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

        // Both adopt a pre-existing wp-sync tree; `at` is the file's mtime, so the
        // two records are identical but for the Item id and the Person.
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

    // ── test helpers ─────────────────────────────────────────────────

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

    /// Heap's algorithm: every ordering of the records, because "any order" is the
    /// claim being tested.
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
