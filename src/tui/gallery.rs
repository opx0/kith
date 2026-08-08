//! The Gallery — a Collection rendered as a grid.
//!
//! Three things here are load-bearing. The thumbnail cache is keyed on content
//! hash at two canonical sizes, so a resize never invalidates an entry. Selection
//! is anchored to an Item rather than an index, so content arriving from a Member
//! cannot move what is under a Person's cursor. And selection is drawn outside
//! the image, because on the kitty and iTerm2 rungs the image widget owns its
//! cells and the TUI may not paint over them.
//!
//! This module produces no `Cmd::Perform`-shaped effect of its own: `handle_key`
//! returns a [`GalleryAction`] and the caller performs it, so every Action begins
//! at a keystroke.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{FilterType, Resize, StatefulImage};

use crate::domain::{Item, ItemId};
use crate::provider::{PixelBudget, Preview, Provider, wallpaper::WallpaperProvider};

// ── what the Gallery asks the app to do ──────────────────────────────

/// An Action the Person asked for on the selected Item, or the request to leave.
///
/// The Gallery decides *what was asked for*; it never performs — only the caller
/// knows who added an Item and can append the tombstone a Delete needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GalleryAction {
    /// Open the Item in Preview, which is what marks it seen.
    Open(ItemId),
    /// Make this Item active on *this* Device, and nowhere else.
    Apply(ItemId),
    /// Toggle the Person's private mark.
    Favourite(ItemId),
    /// Show the bytes in the Person's desktop.
    Reveal(ItemId),
    /// Remove from the Collection — for every Member. Confirm before acting.
    Delete(ItemId),
    /// `q` at the Gallery: the stack root, so this is the way out.
    Quit,
}

// ── the preview ladder ───────────────────────────────────────────────

/// Which rung of the preview ladder this Device landed on.
///
/// Halfblocks is the shipped fallback, never a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rung {
    Kitty,
    Iterm2,
    Sixel,
    Halfblocks,
}

impl Rung {
    /// The permanent right-hand fact on the status row.
    pub fn label(self) -> &'static str {
        match self {
            Rung::Kitty => "kitty",
            Rung::Iterm2 => "iterm2",
            Rung::Sixel => "sixel",
            Rung::Halfblocks => "halfblocks (degraded)",
        }
    }
}

impl From<ProtocolType> for Rung {
    fn from(p: ProtocolType) -> Self {
        match p {
            ProtocolType::Kitty => Rung::Kitty,
            ProtocolType::Iterm2 => Rung::Iterm2,
            ProtocolType::Sixel => Rung::Sixel,
            ProtocolType::Halfblocks => Rung::Halfblocks,
        }
    }
}

// ── grid geometry ────────────────────────────────────────────────────

/// The terminal's real cell size in pixels; 8×16 where it will not report one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSize {
    pub w_px: u16,
    pub h_px: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Tiles per row.
    pub cols: u16,
    /// Cells wide, per tile.
    pub tile_w: u16,
    /// Cells of image.
    pub img_h: u16,
    /// `img_h` plus one caption row.
    pub tile_h: u16,
    /// The grid is centred in the content area.
    pub pad_left: u16,
}

const GUTTER_W: u16 = 2; // cells between tiles
const GUTTER_H: u16 = 1; // cells between tile rows
const MIN_TILE_W: u16 = 14;
const MAX_TILE_W: u16 = 30;
const MAX_COLS: u16 = 10;

/// Below this the grid stops drawing and says the measured size.
const MIN_TERM_W: u16 = 60;
const MIN_TERM_H: u16 = 18;

/// Target tile width. Wider on halfblocks, where a cell is worth two pixels and
/// a small tile stops being recognisable.
fn target_tile_w(rung: Rung) -> u16 {
    if rung == Rung::Halfblocks { 26 } else { 20 }
}

fn geometry(area: Rect, cell: CellSize, rung: Rung) -> Geometry {
    let inner_w = area.width.saturating_sub(2).max(1); // one cell of padding each side
    let t = target_tile_w(rung);
    let cols = (((inner_w + GUTTER_W) as f32 / (t + GUTTER_W) as f32).round() as u16)
        .clamp(1, MAX_COLS);
    let tile_w = (inner_w.saturating_sub((cols - 1) * GUTTER_W) / cols).clamp(MIN_TILE_W, MAX_TILE_W);
    // 16:9 is the wallpaper norm; the tile frames it, letterboxing anything else.
    let img_h = (((tile_w * cell.w_px) as f32 * 9.0 / (16.0 * cell.h_px.max(1) as f32)).round()
        as u16)
        .clamp(3, 10);
    let grid_w = cols * tile_w + (cols - 1) * GUTTER_W;
    Geometry {
        cols,
        tile_w,
        img_h,
        tile_h: img_h + 1,
        pad_left: 1 + inner_w.saturating_sub(grid_w) / 2,
    }
}

/// Whole tile rows that fit; a partial row is never drawn.
fn visible_rows(content_h: u16, tile_h: u16, rung: Rung) -> u16 {
    // On the sixel rung one content row is reserved and left blank: an image on
    // the terminal's last line can scroll the screen, and never putting one
    // there is the cheapest fix.
    let usable = if rung == Rung::Sixel {
        content_h.saturating_sub(1)
    } else {
        content_h
    };
    ((usable + GUTTER_H) / (tile_h + GUTTER_H)).max(1)
}

// ── the thumbnail pipeline ───────────────────────────────────────────

/// The two canonical, geometry-independent preview sizes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Class {
    /// Gallery tiles: fits within 512×512 px.
    Thumb,
    /// The Preview pane: fits within 2048×2048 px.
    Full,
}

impl Class {
    pub fn budget(self) -> PixelBudget {
        match self {
            Class::Thumb => PixelBudget { w_px: 512, h_px: 512 },
            Class::Full => PixelBudget { w_px: 2048, h_px: 2048 },
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Class::Thumb => "thumb",
            Class::Full => "full",
        }
    }
}

/// kith refuses to decode beyond these; the tile carries the reason instead of
/// hanging. Apply is still offered — the backend decides what it can set.
const MAX_MEGAPIXELS: u64 = 128;
const MAX_BYTES_ON_DISK: u64 = 512 * 1024 * 1024;

/// Memory bounds for decoded previews, whichever binds first.
const MEM_MAX_ENTRIES: usize = 128;
const MEM_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The on-disk cache ceiling.
const DISK_MAX_BYTES: u64 = 512 * 1024 * 1024;
const DISK_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct CacheKey {
    /// The content hash where there is one, else `id:<ItemId>` — an Item whose
    /// record carries no hash still gets a decode, just not a disk entry.
    id: String,
    class: Class,
}

/// What the Gallery may draw for one Item right now.
#[derive(Clone, Debug)]
pub enum Slot {
    Ready(Arc<DynamicImage>),
    /// A job is queued or running; the caller draws a placeholder.
    Pending,
    /// These bytes are not a readable image, or are past the decode guard.
    Failed(String),
}

struct Job {
    key: CacheKey,
    item: Item,
    cache_file: Option<PathBuf>,
    /// Distance from the selection: the tile a Person is looking at resolves first.
    priority: u32,
}

struct Done {
    key: CacheKey,
    result: Result<Arc<DynamicImage>, String>,
}

struct Queue {
    jobs: Vec<Job>,
    stop: bool,
}

/// The decode pool and the two-tier cache in front of it.
///
/// Nothing here is authoritative: deleting the cache costs re-decodes and no
/// information. A fixed pool of native threads keeps the render loop from
/// decoding without requiring a Tokio runtime, which keeps this module testable.
pub struct Thumbs {
    dir: Option<PathBuf>,
    unwritable: Option<String>,
    mem: HashMap<CacheKey, Arc<DynamicImage>>,
    lru: VecDeque<CacheKey>,
    mem_bytes: u64,
    inflight: HashSet<CacheKey>,
    failed: HashMap<CacheKey, String>,
    queue: Arc<(Mutex<Queue>, Condvar)>,
    done_rx: Receiver<Done>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Thumbs {
    /// The cache under `$XDG_CACHE_HOME/kith/thumbs`, outside every Circle root:
    /// a thumbnail inside the synced tree is bytes every Member pays to receive.
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        let dir = directories::BaseDirs::new().map(|b| b.cache_dir().join("kith/thumbs"));
        Self::with_dir(dir, provider)
    }

    /// The same cache rooted somewhere explicit. `None` decodes into memory only.
    pub fn with_dir(dir: Option<PathBuf>, provider: Arc<dyn Provider>) -> Self {
        let (dir, unwritable) = match dir {
            Some(d) => match std::fs::create_dir_all(&d) {
                Ok(()) => (Some(d), None),
                Err(e) => (
                    None,
                    Some(format!(
                        "the preview cache at {} is unwritable ({e}) — kith is decoding into memory instead",
                        d.display()
                    )),
                ),
            },
            None => (None, None),
        };

        if let Some(d) = &dir {
            sweep_in_background(d.clone());
        }

        let queue = Arc::new((Mutex::new(Queue { jobs: Vec::new(), stop: false }), Condvar::new()));
        let (done_tx, done_rx) = channel();

        // Enough to fill a screen quickly, few enough not to saturate a laptop.
        let n = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1).min(4);
        let mut workers = Vec::with_capacity(n);
        for _ in 0..n {
            let queue = Arc::clone(&queue);
            let tx = done_tx.clone();
            let provider = Arc::clone(&provider);
            workers.push(std::thread::spawn(move || worker(queue, tx, provider)));
        }

        Self {
            dir,
            unwritable,
            mem: HashMap::new(),
            lru: VecDeque::new(),
            mem_bytes: 0,
            inflight: HashSet::new(),
            failed: HashMap::new(),
            queue,
            done_rx,
            workers,
        }
    }

    /// The one `warn` note this cache can raise (`cache.unwritable`).
    pub fn warning(&self) -> Option<&str> {
        self.unwritable.as_deref()
    }

    /// Take delivery of everything the workers finished, once per frame.
    pub fn poll(&mut self) {
        while let Ok(done) = self.done_rx.try_recv() {
            self.inflight.remove(&done.key);
            match done.result {
                Ok(img) => self.insert(done.key, img),
                Err(why) => {
                    self.failed.insert(done.key, why);
                }
            }
        }
    }

    /// Never blocks the render loop.
    pub fn get(&mut self, item: &Item, class: Class, priority: u32) -> Slot {
        // No bytes on this Device: nothing to decode, and the caller is already
        // drawing the "bytes not here yet" field.
        let Some(key) = Self::key(item, class) else {
            return Slot::Pending;
        };
        if let Some(img) = self.mem.get(&key) {
            let img = Arc::clone(img);
            self.touch(&key);
            return Slot::Ready(img);
        }
        if let Some(why) = self.failed.get(&key) {
            return Slot::Failed(why.clone());
        }
        if !self.inflight.contains(&key) {
            self.enqueue(key, item.clone(), class, priority);
        }
        Slot::Pending
    }

    /// Queue ahead of need, at lower priority than anything visible.
    pub fn prefetch(&mut self, items: impl Iterator<Item = Item>, class: Class) {
        for item in items {
            let Some(key) = Self::key(&item, class) else { continue };
            if self.mem.contains_key(&key) || self.failed.contains_key(&key) {
                continue;
            }
            if !self.inflight.contains(&key) {
                self.enqueue(key, item, class, u32::MAX / 2);
            }
        }
    }

    /// Drop queued jobs for Items no longer near the viewport. A decode already
    /// running finishes and is cached — the work is paid for either way.
    pub fn retain(&mut self, keep: &HashSet<String>) {
        let mut dropped: Vec<CacheKey> = Vec::new();
        let mut q = self.queue.0.lock().unwrap_or_else(|e| e.into_inner());
        q.jobs.retain(|j| {
            let keeping = keep.contains(&j.key.id);
            if !keeping {
                dropped.push(j.key.clone());
            }
            keeping
        });
        drop(q);
        // Only a job that left the *queue* stops being in flight. A job already
        // running is still in flight and will report itself done.
        for key in dropped {
            self.inflight.remove(&key);
        }
    }

    fn key(item: &Item, class: Class) -> Option<CacheKey> {
        item.path.as_ref()?;
        Some(CacheKey { id: content_key(item), class })
    }

    fn enqueue(&mut self, key: CacheKey, item: Item, class: Class, priority: u32) {
        let cache_file = match (&self.dir, item.hash.as_deref()) {
            (Some(dir), Some(hash)) => Some(cache_file(dir, hash, class)),
            _ => None,
        };
        self.inflight.insert(key.clone());
        let mut q = self.queue.0.lock().unwrap_or_else(|e| e.into_inner());
        q.jobs.push(Job { key, item, cache_file, priority });
        drop(q);
        self.queue.1.notify_one();
    }

    fn insert(&mut self, key: CacheKey, img: Arc<DynamicImage>) {
        let cost = decoded_bytes(&img);
        if let Some(replaced) = self.mem.insert(key.clone(), img) {
            self.mem_bytes = self.mem_bytes.saturating_sub(decoded_bytes(&replaced));
            self.touch(&key);
        } else {
            self.lru.push_back(key);
        }
        self.mem_bytes += cost;
        while (self.mem.len() > MEM_MAX_ENTRIES || self.mem_bytes > MEM_MAX_BYTES)
            && self.lru.len() > 1
        {
            let Some(old) = self.lru.pop_front() else { break };
            if let Some(dropped) = self.mem.remove(&old) {
                self.mem_bytes = self.mem_bytes.saturating_sub(decoded_bytes(&dropped));
            }
        }
    }

    fn touch(&mut self, key: &CacheKey) {
        if let Some(i) = self.lru.iter().position(|k| k == key) {
            let k = self.lru.remove(i).expect("index came from position");
            self.lru.push_back(k);
        }
    }
}

impl Drop for Thumbs {
    fn drop(&mut self) {
        {
            let mut q = self.queue.0.lock().unwrap_or_else(|e| e.into_inner());
            q.stop = true;
            q.jobs.clear();
        }
        self.queue.1.notify_all();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

/// What one decoded preview costs in memory.
fn decoded_bytes(img: &DynamicImage) -> u64 {
    u64::from(img.width()) * u64::from(img.height()) * 4
}

/// The content hash where there is one, else the Item id.
fn content_key(item: &Item) -> String {
    match &item.hash {
        Some(h) => h.strip_prefix("b3:").unwrap_or(h).to_string(),
        None => format!("id:{}", item.id),
    }
}

/// `<content-hash>-<class>.png`. No tile width, no cell size, no Item id and no
/// path: a resize must never invalidate an entry, nor a rename.
fn cache_file(dir: &Path, hash: &str, class: Class) -> PathBuf {
    let bare = hash.strip_prefix("b3:").unwrap_or(hash);
    dir.join(format!("{bare}-{}.png", class.suffix()))
}

fn worker(queue: Arc<(Mutex<Queue>, Condvar)>, tx: Sender<Done>, provider: Arc<dyn Provider>) {
    loop {
        let job = {
            let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if q.stop {
                    return;
                }
                if let Some(i) = lowest_priority(&q.jobs) {
                    break q.jobs.swap_remove(i);
                }
                q = queue.1.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let result = decode(&job, provider.as_ref());
        if tx.send(Done { key: job.key, result }).is_err() {
            return; // the Gallery is gone
        }
    }
}

fn lowest_priority(jobs: &[Job]) -> Option<usize> {
    jobs.iter()
        .enumerate()
        .min_by_key(|(_, j)| j.priority)
        .map(|(i, _)| i)
}

fn decode(job: &Job, provider: &dyn Provider) -> Result<Arc<DynamicImage>, String> {
    if let Some(cached) = &job.cache_file {
        match image::open(cached) {
            Ok(img) => return Ok(Arc::new(img)),
            Err(_) if cached.exists() => {
                // A truncated or unreadable entry is a miss, not a hole.
                let _ = std::fs::remove_file(cached);
            }
            Err(_) => {}
        }
    }

    let path = job
        .item
        .path
        .as_ref()
        .ok_or_else(|| "bytes not here yet".to_string())?;

    // The decode guard is the core's policy, not a Provider's.
    if let Ok(md) = std::fs::metadata(path)
        && md.len() > MAX_BYTES_ON_DISK
    {
        return Err("too large to preview".into());
    }
    if let Ok((w, h)) = image::image_dimensions(path)
        && u64::from(w) * u64::from(h) > MAX_MEGAPIXELS * 1_000_000
    {
        return Err(format!("too large to preview ({w}×{h})"));
    }

    match provider.preview(&job.item, job.class_budget()) {
        Ok(Preview::Image(img)) => {
            if let Some(cached) = &job.cache_file {
                write_png_atomically(cached, &img);
            }
            Ok(Arc::new(*img))
        }
        // The Provider had nothing to picture: a caption, not a crash.
        Ok(Preview::Text(t)) => Err(t),
        Err(e) => Err(e.to_string()),
    }
}

impl Job {
    fn class_budget(&self) -> PixelBudget {
        self.key.class.budget()
    }
}

/// Write to `.tmp`, `fsync`, `rename(2)`, so two kith processes never see half a
/// PNG. Failure is silent: the cache is an optimisation, never a precondition.
fn write_png_atomically(final_path: &Path, img: &DynamicImage) {
    let mut buf = std::io::Cursor::new(Vec::new());
    if img.write_to(&mut buf, image::ImageFormat::Png).is_err() {
        return;
    }
    let tmp = final_path.with_extension("png.tmp");
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(buf.get_ref())?;
        f.sync_all()?;
        std::fs::rename(&tmp, final_path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Bound the directory: drop stale entries, then evict oldest-first until it is
/// under the ceiling. Write time stands in for read time, which is more
/// aggressive than needed — and over a rebuildable cache that costs a re-decode.
fn sweep_in_background(dir: PathBuf) {
    std::thread::spawn(move || {
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let now = std::time::SystemTime::now();
        let mut kept: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total: u64 = 0;
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if !md.is_file() {
                continue;
            }
            let modified = md.modified().unwrap_or(now);
            let age = now.duration_since(modified).unwrap_or_default().as_secs();
            if age > DISK_MAX_AGE_SECS {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            total += md.len();
            kept.push((modified, md.len(), e.path()));
        }
        kept.sort_by_key(|(m, _, _)| *m);
        for (_, len, path) in kept {
            if total <= DISK_MAX_BYTES {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    });
}

// ── tiles ────────────────────────────────────────────────────────────

/// What kith knows about one tile — and, as importantly, what it does not.
///
/// Collapsing the three states would make kith lie in one of two directions:
/// pretending an Item is here when its bytes are not, or hiding bytes that are
/// here because no record names them yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Knowledge {
    /// Record and bytes both present. The ordinary case.
    Recorded,
    /// The record arrived and the bytes have not. A full citizen of the grid: it
    /// sorts, it selects, it previews, and Favourite and Delete work on it.
    RecordOnly,
    /// Bytes on disk that no record names yet, which resolves itself in seconds.
    BytesOnly,
}

#[derive(Clone, Debug)]
struct Tile {
    /// The selection anchor: an Item id where there is one, else the path.
    key: String,
    id: Option<ItemId>,
    title: String,
    item: Option<Item>,
    /// Whole seconds since the epoch, after the clock-honesty clamp.
    sort_at: i64,
    /// The record claimed a time this Device's clock cannot believe.
    skewed: bool,
    knowledge: Knowledge,
}

/// What the content area says when the grid has nothing in it — never a bare
/// empty grid, which a Person cannot tell from a bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Emptiness {
    /// A Circle exists and holds no Items.
    #[default]
    NoItems,
    /// This Device is in no Circles at all.
    NoCircles,
    /// Joined, and nothing has arrived yet.
    JustJoined,
}

// ── the Gallery ──────────────────────────────────────────────────────

/// The root screen: a scrolling grid of every Item in the active Circle's
/// Collection, newest first, with a favourite marker and an unseen dot.
pub struct Gallery {
    tiles: Vec<Tile>,
    /// Indices into `tiles`, in sort order, after the favourites filter. Held
    /// rather than recomputed: it is read several times per frame.
    view: Vec<usize>,
    /// The selection, anchored to a tile key rather than to an index. Arrival
    /// re-sorts the grid around it and cannot move it.
    sel_key: Option<String>,
    /// First visible tile row.
    top_row: usize,

    favourites: HashSet<ItemId>,
    unseen: HashSet<ItemId>,
    /// Items unfavourited while the filter is on. They keep their place until the
    /// Person leaves the favourites view, so nothing vanishes under the cursor as
    /// a result of the Person's own keystroke.
    sticky: HashSet<ItemId>,
    filter: bool,

    /// When this Device first saw each tile. Rebuildable: after a rebuild it is
    /// the rebuild time, which only moves records already lying about their date.
    first_seen: HashMap<String, i64>,

    picker: Picker,
    rung: Rung,
    cell: CellSize,
    thumbs: Thumbs,
    /// The encoded escape payload per visible tile, keyed by tile and geometry —
    /// what keeps a scroll from re-encoding every image every frame.
    protocols: HashMap<(String, u16, u16), StatefulProtocol>,

    /// Geometry of the last painted frame — the geometry the Person was looking
    /// at when they pressed the key, which is the one movement must use.
    cols: usize,
    rows: usize,

    pending_g: bool,
    status: Option<String>,
    /// The reason the last thumbnail refused to decode, for the `!` overlay.
    last_failure: Option<String>,
    said_favourite_promise: bool,
    /// Items marked seen since the caller last drained them. Local, and the
    /// Circle never learns it.
    newly_seen: Vec<ItemId>,

    emptiness: Emptiness,
    other_members: Vec<String>,
}

impl Gallery {
    /// A Gallery over the Collection's Items, newest first. The rung defaults to
    /// halfblocks until the app calls [`Gallery::set_picker`].
    pub fn new(items: Vec<Item>) -> Self {
        Self::with_thumbs(items, Thumbs::new(Arc::new(WallpaperProvider::default())))
    }

    /// The same, over a cache the caller owns — which is how Preview shares one
    /// pipeline with the grid instead of decoding the same wallpaper twice.
    pub fn with_thumbs(items: Vec<Item>, thumbs: Thumbs) -> Self {
        let picker = Picker::halfblocks();
        let mut g = Self {
            tiles: Vec::new(),
            view: Vec::new(),
            sel_key: None,
            top_row: 0,
            favourites: HashSet::new(),
            unseen: HashSet::new(),
            sticky: HashSet::new(),
            filter: false,
            first_seen: HashMap::new(),
            rung: Rung::from(picker.protocol_type()),
            cell: CellSize { w_px: picker.font_size().width, h_px: picker.font_size().height },
            picker,
            thumbs,
            protocols: HashMap::new(),
            cols: 1,
            rows: 1,
            pending_g: false,
            status: None,
            last_failure: None,
            said_favourite_promise: false,
            newly_seen: Vec::new(),
            emptiness: Emptiness::default(),
            other_members: Vec::new(),
        };
        g.rebuild(items, Vec::new());
        // The Gallery opens at the newest Item.
        g.sel_key = g.tiles.first().map(|t| t.key.clone());
        g
    }

    // ── wiring the app supplies ──────────────────────────────────────

    /// Adopt the detected preview rung and cell size. Detection runs once at
    /// startup: a terminal does not change protocol mid-session.
    pub fn set_picker(&mut self, picker: Picker) {
        self.rung = Rung::from(picker.protocol_type());
        self.cell = CellSize { w_px: picker.font_size().width, h_px: picker.font_size().height };
        self.picker = picker;
        // Every encoded payload was made for the old rung and the old cell size.
        self.protocols.clear();
    }

    /// The one `warn` note the preview cache can raise (`cache.unwritable`).
    pub fn cache_warning(&self) -> Option<&str> {
        self.thumbs.warning()
    }

    /// The last thumbnail that would not decode, for the `!` detail overlay.
    pub fn last_failure(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }

    /// This Person's private marks. Never synced, never announced.
    pub fn set_favourites(&mut self, favourites: HashSet<ItemId>) {
        self.favourites = favourites;
        self.recompute_view();
    }

    /// The Items this Device has not shown the Person yet.
    pub fn set_unseen(&mut self, unseen: HashSet<ItemId>) {
        self.unseen = unseen;
    }

    /// Bytes sitting in the Circle that no record names yet. Paths, because there
    /// is no Item to name.
    pub fn set_arriving(&mut self, paths: Vec<PathBuf>) {
        let items = self.recorded_items();
        self.rebuild(items, paths);
    }

    pub fn set_emptiness(&mut self, emptiness: Emptiness) {
        self.emptiness = emptiness;
    }

    /// The other Members' display names, for the empty states. One name is used
    /// verbatim; more than one becomes "the other Members'".
    pub fn set_other_members(&mut self, names: Vec<String>) {
        self.other_members = names;
    }

    // ── live arrival ─────────────────────────────────────────────────

    /// Take a new set of Items after the tree changed.
    ///
    /// **The selected Item stays selected and stays where it is on screen.** New
    /// Items sort to the top and the viewport index shifts to compensate: pressing
    /// `a` must apply what was under the cursor when the Person decided to press
    /// it, not whatever a Member added half a second earlier.
    pub fn update(&mut self, items: Vec<Item>) {
        let arriving = self.arriving_paths();
        let before = self.screen_row_of_selection();
        self.rebuild(items, arriving);
        self.restore_screen_row(before);
    }

    /// Drop a tile after the caller has confirmed and performed a Delete. The
    /// selection moves to the next Item in sort order, or the previous one if the
    /// deleted Item was last.
    pub fn remove(&mut self, id: &ItemId) {
        let pos = self.sel_pos();
        let was_selected = self
            .view
            .get(pos)
            .map(|&i| self.tiles[i].id.as_ref() == Some(id))
            .unwrap_or(false);
        self.tiles.retain(|t| t.id.as_ref() != Some(id));
        self.protocols.retain(|(k, _, _), _| k.as_str() != id.as_str());
        self.recompute_view();
        if was_selected {
            if self.view.is_empty() {
                self.sel_key = None;
            } else {
                let next = pos.min(self.view.len() - 1);
                self.sel_key = Some(self.tiles[self.view[next]].key.clone());
            }
        }
    }

    /// Marks an Item seen. Opening it in Preview or performing an Action on it
    /// are the only two things that do: a dot that cleared because a grid
    /// reflowed under a held-down `j` is a dot nobody can trust.
    pub fn mark_seen(&mut self, id: &ItemId) {
        if self.unseen.remove(id) {
            self.newly_seen.push(id.clone());
        }
    }

    /// Hand the caller the marks to flush to `state.toml`. Local state, written
    /// outside every Circle root, so there is nothing inside the tree to leak.
    pub fn drain_newly_seen(&mut self) -> Vec<ItemId> {
        std::mem::take(&mut self.newly_seen)
    }

    // ── what the frame around this screen shows ──────────────────────

    /// The title row's right-aligned count. The unseen figure is this Person's
    /// dots on this Device and is not derivable by anyone else.
    pub fn title_row(&self) -> String {
        let total = self.tiles.iter().filter(|t| t.id.is_some()).count();
        if self.filter {
            let shown = self.view.len();
            return format!("{shown} favourites of {total}");
        }
        let unseen = self
            .tiles
            .iter()
            .filter_map(|t| t.id.as_ref())
            .filter(|id| self.unseen.contains(*id))
            .count();
        let noun = if total == 1 { "Item" } else { "Items" };
        if unseen == 0 {
            format!("{total} {noun}")
        } else {
            format!("{total} {noun} · {unseen} unseen")
        }
    }

    /// The hint row: the keys that matter here, always ending `? keys`.
    pub fn hints(&self) -> &'static str {
        "j k h l move · enter preview · a apply · f fav · F favourites · d delete · ? keys"
    }

    /// A transient line for the status row, if this screen has one to say. The
    /// caller owns the four-second revert.
    pub fn take_status(&mut self) -> Option<String> {
        self.status.take()
    }

    /// The selected Item, if the selected tile has a record. An arriving tile
    /// (bytes with no record) cannot be the subject of an Action.
    pub fn selected(&self) -> Option<&ItemId> {
        self.view.get(self.sel_pos()).and_then(|&i| self.tiles[i].id.as_ref())
    }

    /// Whether the favourites filter is on. Deliberately not persisted: a
    /// remembered invisible filter is how People conclude their content vanished.
    pub fn filtered(&self) -> bool {
        self.filter
    }

    // ── keys ─────────────────────────────────────────────────────────

    /// Route one key. Returns the Action the Person asked for, if any.
    ///
    /// Keys this screen does not claim — `y`, `Space`, `!`, `?`, `c`, `m`, `Esc`,
    /// `Ctrl-C` — fall through as `None` so the app's global handler sees them.
    /// Movement at a boundary is silent; only a genuinely unbound key is named,
    /// and that judgement belongs to the handler that knows the whole keymap.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<GalleryAction> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.view.len();
        let pos = self.sel_pos();
        let cols = self.cols.max(1);
        let rows = self.rows.max(1);

        // The one chord in the keymap. Any other key ends it.
        let pending_g = std::mem::take(&mut self.pending_g);
        if pending_g && !ctrl && key.code == KeyCode::Char('g') {
            self.select(0);
            return None;
        }

        if ctrl {
            match key.code {
                KeyCode::Char('d') => {
                    self.select((pos + cols * rows / 2).min(len.saturating_sub(1)));
                    return None;
                }
                KeyCode::Char('u') => {
                    self.select(pos.saturating_sub(cols * rows / 2));
                    return None;
                }
                // Ctrl-C is the app's: it quits immediately and restores the
                // terminal, which is stronger than this screen's `q`.
                _ => return None,
            }
        }

        match key.code {
            // The grid is a linear list wrapped for display, so `h` and `l` never
            // dead-end in a corner; `j` and `k` keep the column.
            KeyCode::Char('h') | KeyCode::Left => {
                self.select(pos.saturating_sub(1));
                None
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.select((pos + 1).min(len.saturating_sub(1)));
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.select((pos + cols).min(len.saturating_sub(1)));
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if pos >= cols {
                    self.select(pos - cols);
                }
                None
            }
            KeyCode::PageDown => {
                self.select((pos + cols * rows).min(len.saturating_sub(1)));
                None
            }
            KeyCode::PageUp => {
                self.select(pos.saturating_sub(cols * rows));
                None
            }
            KeyCode::Char('g') => {
                self.pending_g = true;
                None
            }
            KeyCode::Home => {
                self.select(0);
                None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.select(len.saturating_sub(1));
                None
            }

            // v0.1's only filter.
            KeyCode::Char('F') => {
                self.toggle_filter();
                None
            }

            // Each Item-focused Action marks the Item seen, because each is a
            // deliberate engagement with that specific Item.
            KeyCode::Enter => self.act(GalleryAction::Open),
            KeyCode::Char('a') => self.act(GalleryAction::Apply),
            KeyCode::Char('r') => self.act(GalleryAction::Reveal),
            KeyCode::Char('d') => self.act(GalleryAction::Delete),
            KeyCode::Char('f') => {
                let action = self.act(GalleryAction::Favourite);
                if let Some(GalleryAction::Favourite(id)) = action.clone() {
                    self.toggle_favourite_mark(&id);
                }
                action
            }

            // The stack root: `q` is the way out.
            KeyCode::Char('q') => Some(GalleryAction::Quit),

            _ => None,
        }
    }

    // ── rendering ────────────────────────────────────────────────────

    /// Draw the grid inside `area` and nowhere else; the frame rows are the app's.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.thumbs.poll();

        let term = frame.area();
        if term.width < MIN_TERM_W || term.height < MIN_TERM_H {
            lines(
                frame,
                area,
                &[
                    format!(
                        "This terminal is {}×{}. kith needs {MIN_TERM_W}×{MIN_TERM_H} to draw the Gallery.",
                        term.width, term.height
                    ),
                    "Nothing is lost — resize and the grid returns with the same Item selected."
                        .into(),
                ],
            );
            return;
        }

        if self.view.is_empty() {
            lines(frame, area, &self.empty_state());
            return;
        }

        let g = geometry(area, self.cell, self.rung);
        let rows = visible_rows(area.height, g.tile_h, self.rung) as usize;
        self.cols = g.cols as usize;
        self.rows = rows;

        // Reflow keeps the selection: the grid is re-laid around the same Item.
        let len = self.view.len();
        let pos = self.sel_pos();
        self.clamp_viewport(len, pos);
        let top = self.top_row;

        let first = (top * self.cols).min(len);
        let last = ((top + rows) * self.cols).min(len).max(first);
        // The margin is one screenful either side, which is also what prefetch
        // fills; everything outside it is dropped below.
        let margin = self.cols * rows;
        let visible: Vec<usize> = self.view[first..last].to_vec();
        let near: HashSet<String> = self.view[first.saturating_sub(margin)..(last + margin).min(len)]
            .iter()
            .filter_map(|&i| self.tiles[i].item.as_ref())
            .map(content_key)
            .collect();
        let onscreen: HashSet<String> = visible.iter().map(|&i| self.tiles[i].key.clone()).collect();
        let ahead: Vec<Item> = self.view[last..(last + margin).min(len)]
            .iter()
            .filter_map(|&i| self.tiles[i].item.clone())
            .collect();

        // A decode already running finishes and is cached anyway. An encoded
        // payload, though, is worth keeping only for a tile on screen *at this
        // geometry*: a reflow makes every one of them stale.
        self.thumbs.retain(&near);
        self.protocols
            .retain(|(k, w, h), _| onscreen.contains(k) && *w == g.tile_w && *h == g.img_h);
        self.thumbs.prefetch(ahead.into_iter(), Class::Thumb);

        for (n, &tile_idx) in visible.iter().enumerate() {
            let col = (n % self.cols) as u16;
            let row = (n / self.cols) as u16;
            let x = area.x + g.pad_left + col * (g.tile_w + GUTTER_W);
            let y = area.y + row * (g.tile_h + GUTTER_H);
            if y + g.tile_h > area.y + area.height {
                break;
            }
            let selected = first + n == pos;
            let distance = (first + n).abs_diff(pos) as u32;
            self.draw_tile(frame, tile_idx, Rect::new(x, y, g.tile_w, g.tile_h), g, selected, distance);
        }
    }

    fn draw_tile(
        &mut self,
        frame: &mut Frame,
        idx: usize,
        rect: Rect,
        g: Geometry,
        selected: bool,
        distance: u32,
    ) {
        let img_rect = Rect::new(rect.x, rect.y, g.tile_w, g.img_h);
        let cap_rect = Rect::new(rect.x, rect.y + g.img_h, g.tile_w, 1);

        // Selection is drawn *outside* the image: on the pixel rungs the image
        // widget owns its cells and cannot be painted over.
        if selected && rect.x > 0 {
            let bar = Rect::new(rect.x - 1, rect.y, 1, g.tile_h);
            let body: Vec<Line<'static>> = (0..g.tile_h).map(|_| Line::from("▌")).collect();
            frame.render_widget(Paragraph::new(body), bar);
        }

        let knowledge = self.tiles[idx].knowledge;

        match knowledge {
            // No progress bar: the Sync Engine reports completion per Circle and
            // per peer, never per path, and a per-Item percentage derived from a
            // Circle-level one would be a fabricated number.
            Knowledge::RecordOnly => {
                frame.render_widget(field(g.tile_w, g.img_h, '▒', Some('↓')), img_rect);
            }
            // Bytes with no record: nothing has said this is an Item of this
            // Collection, so kith names it and waits rather than guessing.
            Knowledge::BytesOnly => {
                frame.render_widget(field(g.tile_w, g.img_h, '░', Some('⋯')), img_rect);
            }
            Knowledge::Recorded => {
                let item = self.tiles[idx].item.clone().expect("Recorded implies a record");
                match self.thumbs.get(&item, Class::Thumb, distance) {
                    Slot::Ready(img) => {
                        let key = (self.tiles[idx].key.clone(), g.tile_w, g.img_h);
                        // Only a ≤512 px downscale happens here; the expensive
                        // decode already happened on a worker.
                        let proto = self
                            .protocols
                            .entry(key)
                            .or_insert_with(|| self.picker.new_resize_protocol((*img).clone()));
                        frame.render_stateful_widget(
                            StatefulImage::default().resize(Resize::Fit(Some(FilterType::Triangle))),
                            img_rect,
                            proto,
                        );
                    }
                    // No spinner: a screenful of spinners is noise, and the image
                    // is about to appear.
                    Slot::Pending => {
                        frame.render_widget(field(g.tile_w, g.img_h, '░', None), img_rect)
                    }
                    // Bytes present, not decodable. The tile carries `!` and
                    // Preview explains; kith never leaves a hole.
                    Slot::Failed(why) => {
                        frame.render_widget(field(g.tile_w, g.img_h, '▒', Some('!')), img_rect);
                        self.last_failure = Some(why);
                    }
                }
            }
        }

        // Always drawn: an Item kith cannot picture is still one kith can name.
        let caption = self.caption(idx, g.tile_w);
        let style = if selected { Style::new().reversed() } else { Style::new() };
        frame.render_widget(Paragraph::new(Line::from(caption)).style(style), cap_rect);
    }

    /// Markers then title, in a fixed order: `★` favourite, `●` unseen,
    /// `?` clock-skewed, `↓` bytes not here.
    fn caption(&self, idx: usize, width: u16) -> String {
        let t = &self.tiles[idx];
        let mut markers = String::new();
        if let Some(id) = &t.id {
            if self.favourites.contains(id) {
                markers.push('★');
            }
            if self.unseen.contains(id) {
                markers.push('●');
            }
        }
        if t.skewed {
            markers.push('?');
        }
        match t.knowledge {
            Knowledge::RecordOnly => markers.push('↓'),
            Knowledge::BytesOnly => markers.push('⋯'),
            Knowledge::Recorded => {}
        }
        if !markers.is_empty() {
            markers.push(' ');
        }
        let room = (width as usize).saturating_sub(markers.chars().count());
        format!("{markers}{}", truncate(&t.title, room))
    }

    fn empty_state(&self) -> Vec<String> {
        match self.emptiness {
            Emptiness::NoCircles => vec![
                "No Circles yet. Run kith create <name>, or kith join <code> if someone invited you."
                    .into(),
            ],
            Emptiness::JustJoined => vec![
                format!(
                    "Waiting for the first Items. {} Device has to be connected too.",
                    match self.other_members.first() {
                        Some(name) if self.other_members.len() == 1 => format!("{name}'s"),
                        _ => "the other Members'".into(),
                    }
                ),
            ],
            Emptiness::NoItems if self.filter => vec![
                "No favourites yet. Press f on an Item to mark it — favourites are private to you."
                    .into(),
            ],
            Emptiness::NoItems => vec![format!(
                "Nothing here yet. kith add <paths…>, or wait — {} Items appear as they arrive.",
                match self.other_members.first() {
                    Some(name) if self.other_members.len() == 1 => format!("{name}'s"),
                    _ => "the other Members'".into(),
                }
            )],
        }
    }

    // ── internals ────────────────────────────────────────────────────

    fn recorded_items(&self) -> Vec<Item> {
        self.tiles.iter().filter_map(|t| t.item.clone()).collect()
    }

    fn arriving_paths(&self) -> Vec<PathBuf> {
        self.tiles
            .iter()
            .filter(|t| t.knowledge == Knowledge::BytesOnly)
            .filter_map(|t| t.key.strip_prefix("bytes:").map(PathBuf::from))
            .collect()
    }

    fn rebuild(&mut self, items: Vec<Item>, arriving: Vec<PathBuf>) {
        let now = now_secs();
        // The clock-honesty guard: a record claiming a time more than 24 hours
        // ahead sorts at its *arrival* position, so one Device with a wrong clock
        // cannot pin itself to the top of everyone's screen forever.
        let horizon = now + 24 * 60 * 60;

        let mut tiles = Vec::with_capacity(items.len() + arriving.len());
        for item in items {
            let key = item.id.to_string();
            let seen_at = *self.first_seen.entry(key.clone()).or_insert(now);
            let claimed = parse_timestamp(&item.added_at);
            let (sort_at, skewed) = match claimed {
                Some(at) if at > horizon => (seen_at, true),
                Some(at) => (at, false),
                // An unreadable date is not a dishonest one: no skew marker.
                None => (seen_at, false),
            };
            let knowledge = if item.path.is_some() {
                Knowledge::Recorded
            } else {
                Knowledge::RecordOnly
            };
            tiles.push(Tile {
                key,
                id: Some(item.id.clone()),
                title: item.title.clone(),
                item: Some(item),
                sort_at,
                skewed,
                knowledge,
            });
        }
        for path in arriving {
            let key = format!("bytes:{}", path.display());
            let seen_at = *self.first_seen.entry(key.clone()).or_insert(now);
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_string();
            tiles.push(Tile {
                key,
                id: None,
                title,
                item: None,
                sort_at: seen_at,
                skewed: false,
                knowledge: Knowledge::BytesOnly,
            });
        }

        // Newest first. Ties break on the key descending — Item ids are ULIDs and
        // so time-ordered, which keeps the Gallery and `kith list items` from ever
        // disagreeing.
        tiles.sort_by(|a, b| b.sort_at.cmp(&a.sort_at).then_with(|| b.key.cmp(&a.key)));
        self.tiles = tiles;
        self.recompute_view();

        if self.sel_key.is_none() {
            self.sel_key = self.tiles.first().map(|t| t.key.clone());
        }
    }

    /// Indices into `tiles`, in sort order, after the favourites filter.
    fn recompute_view(&mut self) {
        let view: Vec<usize> =
            (0..self.tiles.len()).filter(|&i| !self.filter || self.in_filter(i)).collect();
        self.view = view;
    }

    fn in_filter(&self, i: usize) -> bool {
        match &self.tiles[i].id {
            Some(id) => self.favourites.contains(id) || self.sticky.contains(id),
            None => false,
        }
    }

    fn sel_pos(&self) -> usize {
        self.sel_key
            .as_ref()
            .and_then(|k| self.view.iter().position(|&i| self.tiles[i].key == *k))
            .unwrap_or(0)
    }

    fn select(&mut self, pos: usize) {
        let Some(&i) = self.view.get(pos) else { return };
        self.sel_key = Some(self.tiles[i].key.clone());
        let len = self.view.len();
        self.clamp_viewport(len, pos);
    }

    /// The viewport follows the selection with no scroll margin: tiles are tall
    /// enough that a margin would waste a third of the screen.
    fn clamp_viewport(&mut self, len: usize, pos: usize) {
        let cols = self.cols.max(1);
        let rows = self.rows.max(1);
        let row = pos / cols;
        if row < self.top_row {
            self.top_row = row;
        } else if row >= self.top_row + rows {
            self.top_row = row + 1 - rows;
        }
        let total_rows = len.div_ceil(cols);
        self.top_row = self.top_row.min(total_rows.saturating_sub(rows));
    }

    fn screen_row_of_selection(&self) -> usize {
        (self.sel_pos() / self.cols.max(1)).saturating_sub(self.top_row)
    }

    fn restore_screen_row(&mut self, screen_row: usize) {
        self.top_row = (self.sel_pos() / self.cols.max(1)).saturating_sub(screen_row);
    }

    fn toggle_filter(&mut self) {
        self.filter = !self.filter;
        // Re-entering the filter drops anything that was only being kept in view.
        self.sticky.clear();
        self.recompute_view();
        if self.view.is_empty() {
            self.sel_key = None;
            return;
        }
        // The selected Item is preserved if it is in the filtered set; otherwise
        // the selection moves to the nearest Item in sort order that is.
        let anchor = self
            .sel_key
            .as_ref()
            .and_then(|k| self.tiles.iter().position(|t| t.key == *k));
        if let Some(anchor) = anchor
            && !self.view.contains(&anchor)
        {
            let nearest = self
                .view
                .iter()
                .copied()
                .min_by_key(|&i| i.abs_diff(anchor))
                .expect("view is not empty");
            self.sel_key = Some(self.tiles[nearest].key.clone());
        }
        let len = self.view.len();
        let pos = self.sel_pos();
        self.clamp_viewport(len, pos);
    }

    /// Every Action is a deliberate engagement with the selected Item, so every
    /// Action marks it seen.
    fn act(&mut self, make: fn(ItemId) -> GalleryAction) -> Option<GalleryAction> {
        let pos = self.sel_pos();
        let tile = self.view.get(pos).map(|&i| &self.tiles[i])?;
        let Some(id) = tile.id.clone() else {
            // An arriving tile has bytes and no record: until the record lands
            // there is nothing honest to act on.
            self.status =
                Some("no record for these bytes yet — it arrives with that Member's log".into());
            return None;
        };
        self.mark_seen(&id);
        Some(make(id))
    }

    /// The `★` moves on the keystroke; the append to `favourites.jsonl` is the
    /// caller's and cannot fail for a remote reason.
    fn toggle_favourite_mark(&mut self, id: &ItemId) {
        if self.favourites.remove(id) {
            if self.filter {
                // It keeps its place and loses its `★`. Pressing `f` again
                // restores it; leaving the filter drops it.
                self.sticky.insert(id.clone());
                self.status = Some(
                    "unfavourited — still shown until you leave the favourites view".into(),
                );
            } else {
                self.status = Some("☆ unfavourited".into());
            }
        } else {
            self.favourites.insert(id.clone());
            self.sticky.remove(id);
            // The promise is worth stating; it is not worth repeating forty times.
            self.status = Some(if self.said_favourite_promise {
                "★ favourited".into()
            } else {
                self.said_favourite_promise = true;
                "★ favourited — private to you; nothing is announced.".to_string()
            });
        }
        self.recompute_view();
    }
}

// ── drawing helpers ──────────────────────────────────────────────────

/// A dim field of `glyph`, with `centre` in the middle where there is one. The
/// placeholder states differ by that glyph, which needs no colour to read.
fn field(w: u16, h: u16, glyph: char, centre: Option<char>) -> Paragraph<'static> {
    let mid_row = h / 2;
    let mid_col = (w / 2) as usize;
    let body: Vec<Line<'static>> = (0..h)
        .map(|r| {
            let mut s: String = std::iter::repeat_n(glyph, w as usize).collect();
            if let Some(c) = centre
                && r == mid_row
                && w > 0
            {
                s = s.chars().enumerate().map(|(i, g)| if i == mid_col { c } else { g }).collect();
            }
            Line::from(Span::raw(s))
        })
        .collect();
    Paragraph::new(body).style(Style::new().dim())
}

fn lines(frame: &mut Frame, area: Rect, text: &[String]) {
    let body: Vec<Line<'static>> = text.iter().map(|s| Line::from(s.clone())).collect();
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: true }), area);
}

fn truncate(s: &str, room: usize) -> String {
    if room == 0 {
        return String::new();
    }
    if s.chars().count() <= room {
        return s.to_string();
    }
    let keep = room.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

fn now_secs() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn parse_timestamp(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PersonId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn cell() -> CellSize {
        CellSize { w_px: 8, h_px: 17 }
    }

    /// An ItemId with a value we chose. `ItemId` is a newtype over its string
    /// and derives `Deserialize`, so this is the honest way in from a test
    /// without widening the domain's API for our convenience.
    fn iid(s: &str) -> ItemId {
        serde_json::from_str(&format!("\"{s}\"")).expect("ItemId is a newtype over its string")
    }

    fn item(id: &str, title: &str, at: &str, bytes_here: bool) -> Item {
        Item {
            id: iid(id),
            title: title.into(),
            added_by: PersonId::generate(),
            added_at: at.into(),
            path: bytes_here.then(|| PathBuf::from(format!("/nowhere/{title}.png"))),
            hash: bytes_here.then(|| format!("b3:{}", "a".repeat(64))),
            bytes: bytes_here.then_some(1024u64),
        }
    }

    /// Every test Gallery caches under /tmp. A test must never write into the
    /// Person's real cache directory.
    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!("kith-gallery-tests-{}", std::process::id()))
    }

    fn gallery(items: Vec<Item>) -> Gallery {
        Gallery::with_thumbs(
            items,
            Thumbs::with_dir(Some(scratch()), Arc::new(WallpaperProvider::default())),
        )
    }

    /// Three Items an hour apart, newest last in the input so the sort has work.
    fn three() -> Vec<Item> {
        vec![
            item("01AAAAAAAAAAAAAAAAAAAAAAAA", "oldest", "2026-08-07T09:00:00Z", true),
            item("01BBBBBBBBBBBBBBBBBBBBBBBB", "middle", "2026-08-07T10:00:00Z", true),
            item("01CCCCCCCCCCCCCCCCCCCCCCCC", "newest", "2026-08-07T11:00:00Z", true),
        ]
    }

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    // ── geometry (§1.2) ──────────────────────────────────────────────

    #[test]
    fn geometry_matches_the_specs_worked_table() {
        // Terminal, rung → cols, tile_w, img_h, tile_h.
        let cases: &[(u16, Rung, u16, u16, u16, u16)] = &[
            (60, Rung::Kitty, 3, 18, 5, 6),
            (80, Rung::Kitty, 4, 18, 5, 6),
            (120, Rung::Kitty, 5, 22, 6, 7),
            (200, Rung::Kitty, 9, 20, 5, 6),
            (80, Rung::Halfblocks, 3, 24, 6, 7),
        ];
        for &(w, rung, cols, tile_w, img_h, tile_h) in cases {
            let g = geometry(Rect::new(0, 0, w, 40), cell(), rung);
            assert_eq!(
                (g.cols, g.tile_w, g.img_h, g.tile_h),
                (cols, tile_w, img_h, tile_h),
                "{w} cells wide on {rung:?}"
            );
        }
    }

    #[test]
    fn tiles_are_wider_on_halfblocks_because_two_pixels_a_cell_needs_the_room() {
        let pixels = geometry(Rect::new(0, 0, 80, 40), cell(), Rung::Kitty);
        let blocks = geometry(Rect::new(0, 0, 80, 40), cell(), Rung::Halfblocks);
        assert!(
            blocks.tile_w > pixels.tile_w,
            "fewer, more legible pictures beat more mush: {} vs {}",
            blocks.tile_w,
            pixels.tile_w
        );
        assert!(blocks.cols < pixels.cols);
    }

    #[test]
    fn geometry_survives_absurd_areas_without_panicking() {
        for w in [0u16, 1, 2, 3, 15, 400, u16::MAX] {
            let g = geometry(Rect::new(0, 0, w, 10), cell(), Rung::Kitty);
            assert!(g.cols >= 1 && g.tile_w >= MIN_TILE_W && g.img_h >= 3);
        }
    }

    #[test]
    fn a_partial_tile_row_is_never_counted() {
        // tile_h 6 plus a 1-cell gutter: 20 cells holds three whole rows.
        assert_eq!(visible_rows(20, 6, Rung::Kitty), 3);
        // The sixel rung gives one row back so no image sits on the last line.
        assert_eq!(visible_rows(21, 6, Rung::Sixel), 3);
    }

    // ── sort (§1.3) ──────────────────────────────────────────────────

    #[test]
    fn the_grid_is_newest_first() {
        let g = gallery(three());
        let titles: Vec<&str> = g.tiles.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["newest", "middle", "oldest"]);
    }

    #[test]
    fn the_gallery_opens_at_the_newest_item() {
        let g = gallery(three());
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01CCCCCCCCCCCCCCCCCCCCCCCC".into()));
    }

    #[test]
    fn a_device_with_a_wrong_clock_cannot_pin_itself_to_the_top() {
        let claim = "2099-01-01T00:00:00Z";
        let mut items = three();
        items.push(item("01DDDDDDDDDDDDDDDDDDDDDDDD", "liar", claim, true));
        let g = gallery(items);

        let liar = g.tiles.iter().find(|t| t.title == "liar").expect("still shown, never hidden");
        assert!(liar.skewed, "it carries the ? marker");
        assert!(
            liar.sort_at < parse_timestamp(claim).expect("a valid instant"),
            "a record claiming 2099 sorts at its arrival position, not its claim"
        );
        assert!(
            (liar.sort_at - now_secs()).abs() < 5,
            "which is when this Device first reduced it — so tomorrow it is simply old"
        );
    }

    #[test]
    fn an_unreadable_date_is_shown_without_being_accused_of_skew() {
        let g = gallery(vec![item("01EEEEEEEEEEEEEEEEEEEEEEEE", "odd", "not a date", true)]);
        assert!(!g.tiles[0].skewed);
    }

    // ── the arrival invariant (§1.5, §7.5) ───────────────────────────

    #[test]
    fn arrival_never_moves_the_selection() {
        let mut g = gallery(three());
        g.cols = 3;
        g.rows = 2;
        g.handle_key(press('l')); // select "middle"
        let before = g.selected().cloned();
        assert_eq!(before.as_ref().map(|i| i.to_string()), Some("01BBBBBBBBBBBBBBBBBBBBBBBB".into()));

        let mut items = three();
        items.push(item("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "ben-added", "2026-08-07T12:00:00Z", true));
        g.update(items);

        assert_eq!(g.tiles[0].title, "ben-added", "the new Item sorts to the top");
        assert_eq!(
            g.selected().cloned(),
            before,
            "and the cursor does not move — pressing a must apply what the Person chose"
        );
    }

    #[test]
    fn arrival_keeps_the_selection_on_the_same_screen_row() {
        let mut g = gallery(three());
        g.cols = 1; // one tile per row makes the shift visible
        g.rows = 2;
        g.handle_key(press('j'));
        g.handle_key(press('j')); // "oldest", row 2, viewport scrolled to top_row 1
        let screen_row_before = g.screen_row_of_selection();

        let mut items = three();
        items.push(item("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "arrived", "2026-08-07T12:00:00Z", true));
        g.update(items);

        assert_eq!(
            g.screen_row_of_selection(),
            screen_row_before,
            "the viewport index shifts; the selection stays where the eye left it"
        );
    }

    #[test]
    fn arrival_of_the_first_item_selects_it() {
        let mut g = gallery(Vec::new());
        assert!(g.selected().is_none());
        g.update(three());
        assert!(g.selected().is_some());
    }

    // ── movement (§1.5) ──────────────────────────────────────────────

    #[test]
    fn h_and_l_move_linearly_and_wrap_across_row_ends() {
        let mut g = gallery(three());
        g.cols = 2; // "newest" "middle" / "oldest"
        g.rows = 2;
        g.handle_key(press('l'));
        g.handle_key(press('l')); // past the end of row 0 into row 1
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01AAAAAAAAAAAAAAAAAAAAAAAA".into()));
    }

    #[test]
    fn movement_at_a_boundary_is_silent_and_stays_put() {
        let mut g = gallery(three());
        g.cols = 3;
        g.rows = 2;
        g.handle_key(press('h')); // already at the first Item
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01CCCCCCCCCCCCCCCCCCCCCCCC".into()));
        assert!(g.take_status().is_none(), "a bound key at a bound is not an unbound key");
        g.handle_key(press('k')); // already on the first row
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01CCCCCCCCCCCCCCCCCCCCCCCC".into()));
    }

    #[test]
    fn j_clamps_to_the_final_item_from_a_short_last_row() {
        let mut g = gallery(three());
        g.cols = 2;
        g.rows = 2;
        g.handle_key(press('l')); // "middle", row 0 col 1
        g.handle_key(press('j')); // row 1 has one tile; clamp rather than dead-end
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01AAAAAAAAAAAAAAAAAAAAAAAA".into()));
    }

    #[test]
    fn gg_is_the_one_chord_and_g_jumps_to_the_oldest() {
        let mut g = gallery(three());
        g.cols = 1;
        g.rows = 3;
        g.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01AAAAAAAAAAAAAAAAAAAAAAAA".into()));
        g.handle_key(press('g'));
        g.handle_key(press('g'));
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01CCCCCCCCCCCCCCCCCCCCCCCC".into()));
    }

    #[test]
    fn a_lone_g_followed_by_something_else_is_not_a_jump() {
        let mut g = gallery(three());
        g.cols = 1;
        g.rows = 3;
        g.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        g.handle_key(press('g'));
        g.handle_key(press('j')); // ends the chord
        g.handle_key(press('g'));
        assert_ne!(
            g.selected().map(|i| i.to_string()),
            Some("01CCCCCCCCCCCCCCCCCCCCCCCC".into()),
            "one g is not two"
        );
    }

    // ── keys and Actions (§6) ────────────────────────────────────────

    #[test]
    fn the_item_focused_keys_ask_for_the_v01_action_set() {
        let newest = iid("01CCCCCCCCCCCCCCCCCCCCCCCC");
        let cases: &[(char, GalleryAction)] = &[
            ('a', GalleryAction::Apply(newest.clone())),
            ('f', GalleryAction::Favourite(newest.clone())),
            ('d', GalleryAction::Delete(newest.clone())),
            ('r', GalleryAction::Reveal(newest.clone())),
            ('q', GalleryAction::Quit),
        ];
        for (key, want) in cases {
            let mut g = gallery(three());
            assert_eq!(g.handle_key(press(*key)).as_ref(), Some(want), "key {key}");
        }
        let mut g = gallery(three());
        assert_eq!(
            g.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(GalleryAction::Open(newest))
        );
    }

    #[test]
    fn keys_this_screen_does_not_own_fall_through_to_the_app() {
        // cli-tui.md §6.2 routes overlay → screen → global; claiming these here
        // would shadow the global handler that actually implements them.
        let mut g = gallery(three());
        for c in ['y', '?', 'c', 'm', '!', ' ', 'z'] {
            assert_eq!(g.handle_key(press(c)), None, "the Gallery must not claim {c:?}");
        }
        assert_eq!(
            g.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None,
            "Ctrl-C quits immediately and restores the terminal — that is the app's"
        );
    }

    #[test]
    fn every_action_marks_the_item_seen_but_merely_selecting_does_not() {
        let items = three();
        let ids: HashSet<ItemId> = items.iter().map(|i| i.id.clone()).collect();
        let mut g = gallery(items);
        g.set_unseen(ids);

        g.handle_key(press('j'));
        g.handle_key(press('k'));
        assert!(g.drain_newly_seen().is_empty(), "a reflow under a held j must not clear dots");

        g.handle_key(press('a'));
        assert_eq!(g.drain_newly_seen().len(), 1, "an Action is a deliberate engagement");
    }

    // ── favourites (§1.6, §6.2) ──────────────────────────────────────

    #[test]
    fn favouriting_moves_the_star_on_the_keystroke_and_says_it_is_private() {
        let mut g = gallery(three());
        let id = g.selected().cloned().expect("something is selected");
        g.handle_key(press('f'));
        assert!(g.favourites.contains(&id), "the ★ does not wait for a write");
        let said = g.take_status().unwrap_or_default();
        assert!(said.contains("private to you"), "the promise is stated once: {said}");
        // …and not forty times.
        g.handle_key(press('f'));
        g.handle_key(press('f'));
        assert_eq!(g.take_status().as_deref(), Some("★ favourited"));
    }

    #[test]
    fn the_title_row_never_hides_the_filter() {
        let mut g = gallery(three());
        assert_eq!(g.title_row(), "3 Items");
        g.handle_key(press('f'));
        g.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(g.title_row(), "1 favourites of 3");
    }

    #[test]
    fn the_title_row_counts_this_persons_dots() {
        let items = three();
        let one: HashSet<ItemId> = items.iter().take(1).map(|i| i.id.clone()).collect();
        let mut g = gallery(items);
        g.set_unseen(one);
        assert_eq!(g.title_row(), "3 Items · 1 unseen");
    }

    #[test]
    fn unfavouriting_while_filtered_does_not_yank_the_tile_from_under_the_cursor() {
        let mut g = gallery(three());
        g.handle_key(press('f')); // favourite the newest
        g.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)); // filter on
        let selected = g.selected().cloned();
        g.handle_key(press('f')); // unfavourite it
        assert_eq!(g.selected().cloned(), selected, "it keeps its place");
        assert_eq!(g.view.len(), 1, "and stays shown");
        let said = g.take_status().unwrap_or_default();
        assert!(said.contains("still shown until you leave"), "{said}");

        // Re-entering the filter drops it.
        g.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        g.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(g.view.len(), 0);
    }

    #[test]
    fn the_filter_is_never_persisted_across_a_new_gallery() {
        let mut g = gallery(three());
        g.handle_key(press('f'));
        g.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert!(g.filtered());
        let fresh = gallery(three());
        assert!(!fresh.filtered(), "opening the Gallery always shows everything");
    }

    #[test]
    fn filtering_moves_the_selection_to_the_nearest_favourite() {
        let mut g = gallery(three());
        g.cols = 1;
        g.rows = 3;
        g.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)); // "oldest"
        g.handle_key(press('f'));
        g.handle_key(press('g'));
        g.handle_key(press('g')); // back to "newest", which is not a favourite
        g.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert_eq!(
            g.selected().map(|i| i.to_string()),
            Some("01AAAAAAAAAAAAAAAAAAAAAAAA".into())
        );
    }

    // ── delete (§1.5) ────────────────────────────────────────────────

    #[test]
    fn delete_moves_the_selection_to_the_next_item_in_sort_order() {
        let mut g = gallery(three());
        let id = g.selected().cloned().expect("selected");
        g.remove(&id);
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01BBBBBBBBBBBBBBBBBBBBBBBB".into()));
    }

    #[test]
    fn deleting_the_last_item_falls_back_to_the_previous_one() {
        let mut g = gallery(three());
        g.cols = 1;
        g.rows = 3;
        g.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let id = g.selected().cloned().expect("selected");
        g.remove(&id);
        assert_eq!(g.selected().map(|i| i.to_string()), Some("01BBBBBBBBBBBBBBBBBBBBBBBB".into()));
    }

    #[test]
    fn deleting_something_that_was_not_selected_leaves_the_cursor_alone() {
        let mut g = gallery(three());
        let selected = g.selected().cloned();
        g.remove(&iid("01AAAAAAAAAAAAAAAAAAAAAAAA"));
        assert_eq!(g.selected().cloned(), selected);
    }

    // ── the three honest states (§4) ─────────────────────────────────

    #[test]
    fn the_three_states_are_told_apart() {
        let mut g = gallery(vec![
            item("01AAAAAAAAAAAAAAAAAAAAAAAA", "here", "2026-08-07T09:00:00Z", true),
            item("01BBBBBBBBBBBBBBBBBBBBBBBB", "coming", "2026-08-07T10:00:00Z", false),
        ]);
        g.set_arriving(vec![PathBuf::from("/circle/unrecorded.png")]);

        let by_title = |t: &str| {
            g.tiles.iter().find(|x| x.title == t).map(|x| x.knowledge).expect("tile present")
        };
        assert_eq!(by_title("here"), Knowledge::Recorded);
        assert_eq!(by_title("coming"), Knowledge::RecordOnly);
        assert_eq!(by_title("unrecorded"), Knowledge::BytesOnly);
    }

    #[test]
    fn a_record_without_bytes_is_a_full_citizen_of_the_grid() {
        let mut g = gallery(vec![item(
            "01BBBBBBBBBBBBBBBBBBBBBBBB",
            "coming",
            "2026-08-07T10:00:00Z",
            false,
        )]);
        // It sorts, it selects, and Favourite works on it — that is exactly when
        // a Person decides they want it (§4.1).
        assert!(g.selected().is_some());
        let action = g.handle_key(press('f'));
        assert!(
            matches!(action, Some(GalleryAction::Favourite(_))),
            "a Favourite is a mark on the Item, not on its bytes"
        );
        assert_eq!(g.caption(0, 20), "★↓ coming");
    }

    #[test]
    fn bytes_with_no_record_are_named_and_never_acted_on() {
        let mut g = gallery(Vec::new());
        g.set_arriving(vec![PathBuf::from("/circle/mystery.png")]);
        assert_eq!(g.tiles.len(), 1, "a Device holding a Circle's content is never hidden");
        assert_eq!(g.tiles[0].title, "mystery");
        assert_eq!(g.handle_key(press('a')), None, "there is nothing honest to apply");
        let said = g.take_status().unwrap_or_default();
        assert!(said.contains("no record for these bytes yet"), "{said}");
    }

    #[test]
    fn markers_appear_in_the_specs_order_and_the_title_truncates() {
        let items = vec![item("01AAAAAAAAAAAAAAAAAAAAAAAA", "a-very-long-title-indeed", "2026-08-07T09:00:00Z", false)];
        let id = items[0].id.clone();
        let mut g = gallery(items);
        g.set_favourites(HashSet::from([id.clone()]));
        g.set_unseen(HashSet::from([id]));
        let cap = g.caption(0, 14);
        assert!(cap.starts_with("★●↓ "), "★ then ● then ↓, then the title: {cap}");
        assert!(cap.ends_with('…'), "truncated with an ellipsis: {cap}");
        assert_eq!(cap.chars().count(), 14);
    }

    // ── the thumbnail cache (§2) ─────────────────────────────────────

    #[test]
    fn the_cache_is_keyed_on_content_and_mentions_no_geometry() {
        let dir = Path::new("/tmp/kith-test-thumbs");
        let hash = format!("b3:{}", "9".repeat(64));
        let thumb = cache_file(dir, &hash, Class::Thumb);
        let full = cache_file(dir, &hash, Class::Full);
        assert_eq!(thumb.file_name().unwrap().to_str().unwrap(), format!("{}-thumb.png", "9".repeat(64)));
        assert_eq!(full.file_name().unwrap().to_str().unwrap(), format!("{}-full.png", "9".repeat(64)));
        // A resize must never invalidate an entry, so no tile width, no cell
        // size, no Item id and no path may appear in the name (§2.1).
        for name in [&thumb, &full] {
            let n = name.file_name().unwrap().to_str().unwrap();
            assert_eq!(n.matches('-').count(), 1, "{n}");
        }
    }

    #[test]
    fn the_b3_prefix_is_stripped_exactly_once() {
        let dir = Path::new("/tmp");
        let bare = "f".repeat(64);
        assert_eq!(
            cache_file(dir, &format!("b3:{bare}"), Class::Thumb),
            cache_file(dir, &bare, Class::Thumb),
            "prefixed and bare hashes are the same content"
        );
    }

    #[test]
    fn the_two_classes_are_canonical_sizes_not_tile_budgets() {
        assert_eq!(Class::Thumb.budget().w_px, 512);
        assert_eq!(Class::Full.budget().w_px, 2048);
    }

    #[test]
    fn an_unwritable_cache_is_a_warning_and_never_a_failure() {
        // A path under a regular file cannot be a directory.
        let mut blocked = std::env::temp_dir();
        blocked.push(format!("kith-blocked-{}", std::process::id()));
        std::fs::write(&blocked, b"not a directory").expect("write the blocker");
        let thumbs = Thumbs::with_dir(
            Some(blocked.join("thumbs")),
            Arc::new(WallpaperProvider::default()),
        );
        assert!(thumbs.warning().is_some(), "one warn note, and kith keeps drawing");
        drop(thumbs);
        let _ = std::fs::remove_file(&blocked);
    }

    // ── empty states (§1.8) ──────────────────────────────────────────

    #[test]
    fn an_empty_grid_always_says_why() {
        let mut g = gallery(Vec::new());
        assert!(g.empty_state()[0].contains("kith add"));

        g.set_other_members(vec!["Ben".into()]);
        assert!(g.empty_state()[0].contains("Ben's"), "one Member is named");

        g.set_other_members(vec!["Ben".into(), "Cass".into()]);
        assert!(g.empty_state()[0].contains("the other Members'"));

        g.set_emptiness(Emptiness::JustJoined);
        assert!(g.empty_state()[0].contains("has to be connected too"));

        g.set_emptiness(Emptiness::NoCircles);
        assert!(g.empty_state()[0].contains("kith create"));
    }

    #[test]
    fn an_empty_favourites_view_explains_the_mark_is_private() {
        let mut g = gallery(three());
        g.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT));
        assert!(g.empty_state()[0].contains("private to you"));
    }

    // ── wording (CONTEXT.md) ─────────────────────────────────────────

    #[test]
    fn no_surface_string_uses_a_word_the_glossary_forbids() {
        let mut g = gallery(three());
        g.set_emptiness(Emptiness::JustJoined);
        let mut strings: Vec<String> = g.empty_state();
        g.set_emptiness(Emptiness::NoCircles);
        strings.extend(g.empty_state());
        g.set_emptiness(Emptiness::NoItems);
        strings.extend(g.empty_state());
        strings.push(g.hints().to_string());
        strings.push(g.title_row());
        strings.push(Rung::Halfblocks.label().into());
        g.handle_key(press('f'));
        strings.extend(g.take_status());

        // The one thing an arriving tile says when asked to act.
        let mut arriving = gallery(Vec::new());
        arriving.set_arriving(vec![PathBuf::from("/x/y.png")]);
        arriving.handle_key(press('a'));
        strings.extend(arriving.take_status());

        for s in &strings {
            let lower = s.to_lowercase();
            for banned in ["user", "account", "folder", "online", "syncthing", "cloud", "server"] {
                assert!(!lower.contains(banned), "{banned:?} has no domain position: {s:?}");
            }
        }
    }

    #[test]
    fn halfblocks_is_never_called_unsupported() {
        let label = Rung::Halfblocks.label().to_lowercase();
        for banned in ["unsupported", "error", "failed"] {
            assert!(!label.contains(banned), "halfblocks is the shipped fallback, not a defect");
        }
    }

    // ── it draws (§1.2, §2.4) ────────────────────────────────────────

    #[test]
    fn the_grid_draws_at_every_size_without_panicking() {
        for (w, h) in [(80u16, 24u16), (60, 18), (200, 50), (40, 10), (1, 1)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
            let mut g = gallery(three());
            term.draw(|f| {
                let area = f.area();
                g.render(f, area);
            })
            .expect("draw");
        }
    }

    #[test]
    fn below_the_floor_the_grid_stops_and_says_the_measured_size() {
        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("test backend");
        let mut g = gallery(three());
        term.draw(|f| {
            let area = f.area();
            g.render(f, area);
        })
        .expect("draw");
        let painted: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(painted.contains("40×10"), "the measured size is stated: {painted:?}");
        assert!(painted.contains("Nothing is lost"));
    }

    #[test]
    fn the_selection_bar_is_drawn_outside_the_image() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test backend");
        let mut g = gallery(three());
        term.draw(|f| {
            let area = f.area();
            g.render(f, area);
        })
        .expect("draw");
        let painted: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            painted.contains('▌'),
            "pixel protocols forbid overpainting image cells, so the gutter carries the selection"
        );
    }

    #[test]
    fn a_tile_with_no_bytes_still_has_a_caption() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test backend");
        let mut g = gallery(vec![item(
            "01BBBBBBBBBBBBBBBBBBBBBBBB",
            "coming",
            "2026-08-07T10:00:00Z",
            false,
        )]);
        term.draw(|f| {
            let area = f.area();
            g.render(f, area);
        })
        .expect("draw");
        let painted: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(painted.contains("coming"), "the grid never has a hole");
        assert!(painted.contains('↓'), "and the missing bytes are stated, not hidden");
    }
}
