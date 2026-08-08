//! The TUI — the app loop, the frame, and the routing between screens.
//!
//! Four things here are load-bearing:
//!
//! 1. **The terminal is restored on every exit path** — quit, `Ctrl-C`, an error
//!    return and a panic all go through the [`Restore`] guard.
//! 2. **Apply is constructed in a key handler and nowhere else.** [`Cmd::Apply`]
//!    appears exactly twice, both inside [`App::on_key`]'s call tree.
//!    [`App::on_sync`] and [`App::on_tick`] return `()` by signature, so arriving
//!    content cannot reach the Provider even by mistake.
//! 3. **Arrival never moves the selection.** Sync events call
//!    [`gallery::Gallery::update`], which is anchored to a tile key rather than
//!    an index, so pressing `a` applies what was under the cursor when the Person
//!    decided to press it.
//! 4. **The engine is optional the whole way down.** With no reachable Sync
//!    Engine a Person still browses, previews, favourites and applies off the
//!    tree. No engine question is awaited inline, so a hung daemon cannot freeze
//!    a keystroke.

pub mod gallery;
pub mod members;
pub mod preview;

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui_image::picker::Picker;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_stream::StreamExt as _;

use crate::config::{self, Config};
use crate::domain::{Item, ItemId, MembershipClaim, Person, PersonId, Presence, Role};
use crate::engine::syncthing::SyncthingEngine;
use crate::engine::{
    Change, CircleId, CircleStatus, JoinRequest, PeerDevice, SyncEngine, SyncError,
};
use crate::identity::Identity;
use crate::provider::{
    ActionError, ApplyTarget, Availability, ImportCandidate, Provider, ProviderFacts,
    wallpaper::WallpaperProvider,
};
use crate::store::{claims, descriptors, records};

use gallery::{Emptiness, Gallery, GalleryAction};
use members::{MemberView, Members, MembersAction, PendingJoin, UnclaimedDevice};
use preview::{ItemAction, Marks, Pane, Preview, PreviewAction, Rung, SidecarFacts};

// Exit codes, sysexits, same dialect as the rest of the binary.
const EX_OK: i32 = 0;
const EX_USAGE: i32 = 64;
const EX_INTERNAL: i32 = 70;

/// The Gallery cannot draw below this, and kith refuses before entering the
/// alternate screen rather than painting a message that vanishes with it.
const MIN_W: u16 = 60;
const MIN_H: u16 = 18;

/// How long a transient line holds the status row before it reverts.
const STATUS_HOLD: Duration = Duration::from_secs(4);

/// The tick. Fast enough for a progress figure, slow enough to cost nothing.
const TICK: Duration = Duration::from_millis(250);

/// How often the engine is re-asked for the facts it alone knows.
const ENGINE_REFRESH: Duration = Duration::from_secs(2);

/// How long the tick keeps redrawing after something changed, so decodes started
/// by that change have somewhere to land.
const SETTLE: Duration = Duration::from_secs(3);

/// How long arrivals are allowed to pile up before the Circle is re-read. The
/// engine reports one path at a time; a Person perceives a batch.
const RELOAD_COALESCE: Duration = Duration::from_millis(250);

// ── entry ────────────────────────────────────────────────────────────

/// Bare `kith`. Returns the process exit code; never calls `exit` itself, so the
/// terminal guard runs.
pub async fn run() -> i32 {
    let me = match crate::identity::load() {
        Ok(Some(id)) => id,
        Ok(None) => {
            eprintln!(
                "Run kith init first — kith needs to know your name before it can show you a Circle."
            );
            return EX_USAGE;
        }
        Err(e) => {
            eprintln!("{e}");
            return EX_USAGE;
        }
    };

    match crossterm::terminal::size() {
        Ok((w, h)) if w < MIN_W || h < MIN_H => {
            eprintln!("kith needs at least {MIN_W}×{MIN_H}; this terminal is {w}×{h}.");
            return EX_USAGE;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("this terminal will not report its size ({e})");
            return EX_USAGE;
        }
    }

    // Inspected, never `load`ed: `load` exits the process on a bad file, and an
    // exit from inside the alternate screen would not restore the terminal.
    let (config_warnings, config) = match config::inspect() {
        Ok(loaded) => (loaded.warnings(), loaded),
        Err(e) => {
            eprintln!("✗ {e}");
            if let Some(fix) = e.fix() {
                eprintln!("→ {fix}");
            }
            return config::EXIT_CONFIG;
        }
    };

    // Neither is required: a Circle kith has a root for is browsable with the
    // daemon stopped.
    let engine = engine_from(&config.config).map(Arc::new);
    let circles = discover_circles(engine.as_deref()).await;
    // This Device's Identity *is* the Sync Engine's device id, asked for while
    // the daemon answers and remembered so a record can be appended without it.
    let device = match &engine {
        Some(e) => e.local_device().await.ok().map(|d| d.0),
        None => None,
    };

    let mut term = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("the terminal could not be put into raw mode: {e}");
            return EX_INTERNAL;
        }
    };
    // `try_init` installs a panic hook; this guard covers ordinary returns.
    let _restore = Restore;

    let picker = detect_rung();

    let (tx, rx) = unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    spawn_input(tx.clone(), stop.clone());
    spawn_ticker(tx.clone());
    if let Some(e) = &engine {
        spawn_observer(e.clone(), tx.clone());
    }

    let mut app = App::new(me, config, config_warnings, engine, circles, device, picker, tx);
    let code = app.event_loop(&mut term, rx).await;

    stop.store(true, Ordering::Relaxed);
    app.flush_state();
    code
}

/// Work out which rung of the preview ladder this terminal is on — and refuse to
/// ask a terminal that will not answer.
///
/// `from_query_stdio` parks a thread on stdin until it sees a Device Status
/// Report, so a terminal that reports nothing (a bare pty, a recording harness)
/// leaves that thread swallowing every key the Person types. `cursor::position()`
/// asks the same question bounded and on the main thread: a terminal that answers
/// it will answer the rest, and one that does not lands on halfblocks.
fn detect_rung() -> Picker {
    match crossterm::cursor::position() {
        Ok(_) => Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()),
        Err(_) => Picker::halfblocks(),
    }
}

/// Leaving raw mode and the alternate screen, whatever happened.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

/// The production engine, honouring the config's override before discovery. The
/// only place in the TUI that names the adapter type.
fn engine_from(config: &Config) -> Option<SyncthingEngine> {
    if let (Some(address), Some(key)) = (&config.engine_address, &config.engine_api_key) {
        return Some(SyncthingEngine::new(crate::engine::syncthing::Credentials {
            base_url: address.clone(),
            api_key: key.clone(),
            source: config::path().unwrap_or_default(),
        }));
    }
    SyncthingEngine::discover().ok().map(SyncthingEngine::new)
}

// ── the Circles this Device holds ────────────────────────────────────

/// An id to ask the engine about and a root to read from. The root is what makes
/// the daemon optional.
#[derive(Clone, Debug)]
struct CircleHandle {
    id: CircleId,
    name: String,
    root: PathBuf,
}

/// The engine's answer, plus the default location on disk so a stopped daemon
/// does not empty the switcher.
async fn discover_circles<E: SyncEngine>(engine: Option<&E>) -> Vec<CircleHandle> {
    let mut out: Vec<CircleHandle> = Vec::new();
    if let Some(engine) = engine
        && let Ok(refs) = engine.circles().await
    {
        for r in refs {
            out.push(CircleHandle { id: r.id, name: r.name, root: r.root });
        }
    }
    if let Some(dir) = default_circles_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let root = entry.path();
            if !root.is_dir() || out.iter().any(|c| c.root == root) {
                continue;
            }
            // Only a directory kith wrote a Circle descriptor into is a Circle;
            // guessing at the rest would be kith inventing Membership.
            let Ok(Some(d)) = descriptors::read_circle(&root) else {
                continue;
            };
            out.push(CircleHandle { id: CircleId(d.id), name: d.name, root });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.0.cmp(&b.id.0)));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

fn default_circles_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_dir().join("kith/circles"))
}

// ── what one Circle's tree says ──────────────────────────────────────

/// Everything the app reads off a Circle root in one pass, on a blocking task.
///
/// Derived, never authoritative: the record logs and the Membership claims are
/// the source of truth, and this is a reduction of them.
#[derive(Default)]
struct Tree {
    items: Vec<Item>,
    people: Vec<Person>,
    claims: Vec<MembershipClaim>,
    collection: String,
    founder_person: Option<PersonId>,
    founder_device: Option<String>,
    /// Bytes in the Circle that no record names yet.
    arriving: Vec<PathBuf>,
    /// Something in the tree could not be read. Shown, never swallowed.
    trouble: Option<String>,
}

fn read_tree(root: &Path, reserved: &[&'static str]) -> Tree {
    let mut tree = Tree { collection: "main".to_string(), ..Tree::default() };

    match descriptors::read_circle(root) {
        Ok(Some(d)) => {
            tree.founder_person = Some(PersonId::from(d.founder_person));
            tree.founder_device = Some(d.founder_device);
        }
        Ok(None) => {}
        Err(e) => tree.trouble = Some(format!("the Circle descriptor is unreadable ({e})")),
    }

    if let Ok(cs) = descriptors::read_collections(root)
        && let Some(first) = cs.first()
    {
        tree.collection = first.collection.clone();
    }

    match records::read_all(root, &tree.collection) {
        Ok(recs) => tree.items = records::derive_items(&recs, root),
        Err(e) => tree.trouble = Some(format!("the record logs are unreadable ({e})")),
    }

    match claims::read_all(root) {
        Ok(cs) => {
            tree.people = claims::derive_people(&cs);
            tree.claims = cs;
        }
        Err(e) => tree.trouble = Some(format!("the Membership claims are unreadable ({e})")),
    }

    tree.arriving = arriving_paths(root, reserved, &tree.items);
    tree
}

/// Bytes present in the Circle that no live record binds. Dot-entries and
/// everything `reserved_paths()` names are skipped, so no engine artefact is ever
/// offered to a Person as content.
fn arriving_paths(root: &Path, reserved: &[&'static str], items: &[Item]) -> Vec<PathBuf> {
    let bound: HashSet<PathBuf> = items.iter().filter_map(|i| i.path.clone()).collect();
    let mut out = Vec::new();
    walk(root, reserved, &bound, &mut out, 0);
    out.sort();
    out
}

fn walk(dir: &Path, reserved: &[&'static str], bound: &HashSet<PathBuf>, out: &mut Vec<PathBuf>, depth: u32) {
    // The cap keeps a symlink loop from turning a redraw into a filesystem crawl.
    if depth > 8 || out.len() > 5000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || reserved.iter().any(|g| glob_match(g, &name)) {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, reserved, bound, out, depth + 1),
            Ok(t) if t.is_file() && !bound.contains(&path) => out.push(path),
            _ => {}
        }
    }
}

/// `*`-only glob, which is the whole vocabulary `reserved_paths()` uses.
fn glob_match(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return false;
    };
    if !name.starts_with(first) {
        return false;
    }
    let mut rest = &name[first.len()..];
    let mut last = "";
    for part in parts {
        last = part;
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    pattern.ends_with('*') || last.is_empty() || rest.is_empty()
}

// ── events ───────────────────────────────────────────────────────────

/// Everything the loop can be woken by. One channel, so ordering is total and
/// there is no race between a keystroke and an arrival.
enum Event {
    Key(KeyEvent),
    Resize,
    /// The tree changed under us; re-read it.
    Sync(Change),
    Tick,
    /// A blocking read of the Circle root came back.
    Tree(Box<Tree>),
    /// Full-class pixels for the Item in Preview.
    Decoded(Box<Decoded>),
    /// The engine answered a question we asked it out of band.
    Status(Option<CircleStatus>),
    Peers(Option<Vec<PeerDevice>>),
    Knocks(Vec<JoinRequest>),
    /// Something we spawned finished and has a sentence for the status row.
    Note(String),
}

struct Decoded {
    item: ItemId,
    image: Option<image::DynamicImage>,
    facts: Option<ProviderFacts>,
    note: Option<String>,
}

/// A dedicated OS thread, because a blocking read cannot live on the runtime. It
/// polls rather than blocks so `stop` is honoured: a thread still sitting in
/// `read()` after the terminal is restored would eat the next keystroke at the
/// Person's shell.
fn spawn_input(tx: UnboundedSender<Event>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(TermEvent::Key(k)) => {
                        if tx.send(Event::Key(k)).is_err() {
                            return;
                        }
                    }
                    Ok(TermEvent::Resize(_, _)) => {
                        if tx.send(Event::Resize).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                },
                Ok(false) => {}
                Err(_) => return,
            }
        }
    });
}

fn spawn_ticker(tx: UnboundedSender<Event>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if tx.send(Event::Tick).is_err() {
                return;
            }
        }
    });
}

/// The live change feed. `Desynced` is forwarded rather than swallowed: the
/// Person is told the feed lost continuity and the tree is re-read.
fn spawn_observer<E: SyncEngine>(engine: Arc<E>, tx: UnboundedSender<Event>) {
    tokio::spawn(async move {
        let Ok(mut changes) = engine.observe(None).await else {
            let _ = tx.send(Event::Note(ENGINE_DOWN.to_string()));
            return;
        };
        while let Some(envelope) = changes.next().await {
            if tx.send(Event::Sync(envelope.change)).is_err() {
                return;
            }
        }
    });
}

const ENGINE_DOWN: &str =
    "the Sync Engine is not reachable — you are seeing what this Device already holds";

// ── screens and overlays ─────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq, Debug)]
enum Screen {
    Gallery,
    Preview,
    Members,
}

/// At most one is open at a time. An overlay shadows the screen and the global
/// layer both, which is why the join prompt is never raised over one.
enum Overlay {
    Help,
    Circles { sel: usize },
    Actions { item: ItemId, sel: usize, entries: Vec<Entry> },
    Monitors { item: ItemId, sel: usize, targets: Vec<ApplyTarget> },
    Confirm(Confirm),
    Detail(String),
}

struct Entry {
    action: ItemAction,
    /// `None` means available. An unavailable Action is greyed with its reason.
    refusal: Option<String>,
}

/// A destructive act, and the sentence that names its consequence. `Enter` is
/// always the safe answer: a confirm whose default deletes will one day delete
/// by accident.
struct Confirm {
    title: String,
    body: Vec<String>,
    /// Typed confirmation, when `y` is not friction enough.
    typed: Option<(String, String)>,
    yes: Cmd,
}

/// An act the Person asked for. Nothing in here is constructed outside
/// [`App::on_key`]'s call tree — see this module's header.
#[derive(Clone, Debug)]
enum Cmd {
    Apply { item: ItemId, target: Option<ApplyTarget> },
    Favourite(ItemId),
    Reveal(ItemId),
    CopyPath(ItemId),
    Delete(ItemId),
    Approve(JoinRequest),
    Reject(JoinRequest),
    Leave,
    Switch(usize),
    Quit,
    Suspend,
    /// The Person asked for something this Device cannot do; say why.
    Refuse(String),
}

// ── the app ──────────────────────────────────────────────────────────

struct App<E: SyncEngine> {
    me: Identity,
    /// The whole config: the monitor picker needs the labels a Person gave their
    /// outputs, which do not fit on `Config`.
    config: config::Loaded,
    provider: Arc<WallpaperProvider>,
    picker: Picker,
    rung: Rung,
    engine: Option<Arc<E>>,
    tx: UnboundedSender<Event>,

    circles: Vec<CircleHandle>,
    active: Option<usize>,

    stack: Vec<Screen>,
    overlay: Option<Overlay>,
    gallery: Gallery,
    preview: Option<Preview>,
    members: Members,

    items: Vec<Item>,
    people: Vec<Person>,
    claims: Vec<MembershipClaim>,
    collection: String,
    founder_person: Option<PersonId>,
    founder_device: Option<String>,

    favourites: HashSet<ItemId>,
    unseen: HashSet<ItemId>,
    state: State,

    peers: Option<Vec<PeerDevice>>,
    engine_status: Option<CircleStatus>,
    knocks: Vec<JoinRequest>,
    engine_reachable: bool,

    status: Option<(String, Instant)>,
    last_failure: Option<String>,
    last_engine_refresh: Instant,
    reload_after: Option<Instant>,
    reload_in_flight: bool,
    backends: Vec<&'static str>,
    /// Redraw on tick until this moment, and not after (see [`App::on_tick`]).
    settle_until: Instant,
    dirty: bool,
    quit: bool,
    exit: i32,
}

impl<E: SyncEngine> App<E> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        me: Identity,
        config: config::Loaded,
        warnings: Vec<String>,
        engine: Option<Arc<E>>,
        circles: Vec<CircleHandle>,
        device: Option<String>,
        picker: Picker,
        tx: UnboundedSender<Event>,
    ) -> Self {
        let rung = Rung::from_protocol(picker.protocol_type());
        let provider = Arc::new(WallpaperProvider::new(config.config.apply_command.clone()));
        // Detected once: asking walked $PATH on every frame.
        let backends = provider.detected();
        let mut state = State::load();
        if device.is_some() {
            state.device = device;
        }

        // The TUI opens on `last_circle`, unlike the CLI: a TUI has a visible,
        // switchable context and a script invocation does not.
        let active = if circles.is_empty() {
            None
        } else {
            Some(
                state
                    .last_circle
                    .as_deref()
                    .and_then(|id| circles.iter().position(|c| c.id.0 == id))
                    .unwrap_or(0),
            )
        };

        let mut gallery = Gallery::new(Vec::new());
        gallery.set_picker(picker.clone());
        gallery.set_emptiness(if circles.is_empty() {
            Emptiness::NoCircles
        } else {
            Emptiness::NoItems
        });

        let mut app = Self {
            me,
            config,
            provider,
            picker,
            rung,
            engine,
            tx,
            circles,
            active,
            stack: vec![Screen::Gallery],
            overlay: None,
            gallery,
            preview: None,
            members: Members::new(Vec::new(), Vec::new()),
            items: Vec::new(),
            people: Vec::new(),
            claims: Vec::new(),
            collection: "main".to_string(),
            founder_person: None,
            founder_device: None,
            favourites: HashSet::new(),
            unseen: HashSet::new(),
            state,
            peers: None,
            engine_status: None,
            knocks: Vec::new(),
            engine_reachable: false,
            status: None,
            last_failure: None,
            last_engine_refresh: Instant::now() - ENGINE_REFRESH,
            reload_after: None,
            reload_in_flight: false,
            backends,
            settle_until: Instant::now() + SETTLE,
            dirty: true,
            quit: false,
            exit: EX_OK,
        };
        for w in warnings {
            app.say(w);
        }
        // The one `warn` the preview cache can raise.
        if let Some(warning) = app.gallery.cache_warning().map(str::to_string) {
            app.say(warning);
        }
        app.load_circle();
        app
    }

    // ── the loop ─────────────────────────────────────────────────────

    async fn event_loop(
        &mut self,
        term: &mut ratatui::DefaultTerminal,
        mut rx: UnboundedReceiver<Event>,
    ) -> i32 {
        loop {
            if self.dirty {
                self.dirty = false;
                if term.draw(|f| self.draw(f)).is_err() {
                    return EX_INTERNAL;
                }
            }
            let Some(event) = rx.recv().await else {
                return self.exit;
            };
            match event {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if let Some(cmd) = self.on_key(key) {
                        self.perform(cmd, term).await;
                    }
                    self.settle();
                    self.dirty = true;
                }
                Event::Resize => {
                    let _ = term.autoresize();
                    self.settle();
                    self.dirty = true;
                }
                Event::Sync(change) => self.on_sync(change),
                Event::Tick => self.on_tick(),
                Event::Tree(tree) => self.adopt_tree(*tree),
                Event::Decoded(d) => self.adopt_decode(*d),
                Event::Status(s) => {
                    self.engine_reachable = s.is_some();
                    self.engine_status = s;
                    self.dirty = true;
                }
                Event::Peers(p) => {
                    self.peers = p;
                    self.rebuild_members();
                    self.dirty = true;
                }
                Event::Knocks(k) => {
                    self.knocks = k;
                    self.rebuild_members();
                    self.dirty = true;
                }
                Event::Note(note) => self.say(note),
            }
            if self.quit {
                return self.exit;
            }
        }
    }

    /// Arrival. **Never constructs a [`Cmd`]** — the signature says so, and that
    /// is the consent invariant made structural rather than promised.
    fn on_sync(&mut self, change: Change) {
        match change {
            Change::Desynced => {
                self.say("resynchronising…".to_string());
                self.load_circle();
            }
            Change::Knock { .. } => self.refresh_engine_facts(),
            Change::Presence { .. } => self.refresh_engine_facts(),
            Change::Path { circle, .. } => {
                // The engine emits one of these per file, so a sync of 500
                // Items would otherwise cost 500 full tree walks and as many
                // rounds of engine questions. Coalesce instead: note that a
                // reload is owed and let the tick decide when to pay it.
                if self.active_circle().map(|c| c.id == circle).unwrap_or(false) {
                    self.reload_after = Some(Instant::now() + RELOAD_COALESCE);
                }
            }
        }
        self.dirty = true;
    }

    /// The 250 ms tick. Also never constructs a [`Cmd`].
    fn on_tick(&mut self) {
        if let Some((_, at)) = &self.status
            && at.elapsed() >= STATUS_HOLD
        {
            self.status = None;
            self.dirty = true;
        }
        if self.last_engine_refresh.elapsed() >= ENGINE_REFRESH {
            self.refresh_engine_facts();
        }
        if let Some(due) = self.reload_after
            && Instant::now() >= due
            && !self.reload_in_flight
        {
            self.reload_after = None;
            self.load_circle();
        }
        // A redraw is the only way a finished decode reaches the screen — but
        // only while something is still moving. A tick that redrew forever would
        // re-transmit every image four times a second on the pixel rungs.
        if Instant::now() < self.settle_until
            && !matches!(self.stack.last(), Some(Screen::Members))
        {
            self.dirty = true;
        }
    }

    /// Something changed; keep redrawing for long enough that the decodes it
    /// started can land.
    fn settle(&mut self) {
        self.settle_until = Instant::now() + SETTLE;
    }

    // ── keys: overlay → screen → global ──────────────────────────────

    fn on_key(&mut self, key: KeyEvent) -> Option<Cmd> {
        // `Ctrl-C` is the one key nothing may shadow: the way out of a wedged
        // overlay.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Cmd::Quit);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
            return Some(Cmd::Suspend);
        }

        if self.overlay.is_some() {
            return self.overlay_key(key);
        }
        match self.stack.last().cloned().unwrap_or(Screen::Gallery) {
            Screen::Gallery => self.gallery_key(key),
            Screen::Preview => self.preview_key(key),
            Screen::Members => self.members_key(key),
        }
    }

    fn gallery_key(&mut self, key: KeyEvent) -> Option<Cmd> {
        if let Some(action) = self.gallery.handle_key(key) {
            self.drain_seen();
            return match action {
                GalleryAction::Open(id) => {
                    self.open_preview(&id);
                    None
                }
                // Apply, constructed inside a key handler. Occurrence 1 of 2.
                GalleryAction::Apply(id) => self.ask_apply(id),
                GalleryAction::Favourite(id) => Some(Cmd::Favourite(id)),
                GalleryAction::Reveal(id) => Some(Cmd::Reveal(id)),
                GalleryAction::Delete(id) => {
                    self.confirm_delete(&id);
                    None
                }
                GalleryAction::Quit => Some(Cmd::Quit),
            };
        }
        self.global_key(key, self.gallery.selected().cloned())
    }

    fn preview_key(&mut self, key: KeyEvent) -> Option<Cmd> {
        let action = self.preview.as_mut()?.handle_key(key);
        let Some(action) = action else {
            let item = self.preview.as_ref().map(|p| p.item().id.clone());
            return self.global_key(key, item);
        };
        let item = self.preview.as_ref()?.item().id.clone();
        match action {
            PreviewAction::Back => {
                self.pop_screen();
                None
            }
            PreviewAction::Next => {
                self.step_preview(1);
                None
            }
            PreviewAction::Previous => {
                self.step_preview(-1);
                None
            }
            PreviewAction::Menu => {
                self.open_actions(item);
                None
            }
            PreviewAction::Perform(a) => self.perform_item_action(a, item),
            PreviewAction::Unavailable { reason, .. } => Some(Cmd::Refuse(reason)),
        }
    }

    fn members_key(&mut self, key: KeyEvent) -> Option<Cmd> {
        if let Some(action) = self.members.handle_key(key) {
            return match action {
                MembersAction::Approve(r) => Some(Cmd::Approve(r)),
                MembersAction::Reject(r) => Some(Cmd::Reject(r)),
                MembersAction::Invite => Some(Cmd::Refuse(
                    "Invites are printed by `kith invite` — a code has to leave this Device by a channel you already trust.".into(),
                )),
                MembersAction::Leave => {
                    self.confirm_leave();
                    None
                }
                MembersAction::Unavailable(reason) => Some(Cmd::Refuse(reason)),
            };
        }
        self.global_key(key, None)
    }

    /// The global layer: the keys that mean the same thing on every screen.
    fn global_key(&mut self, key: KeyEvent, item: Option<ItemId>) -> Option<Cmd> {
        if key.modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        match key.code {
            KeyCode::Char('?') => {
                self.overlay = Some(Overlay::Help);
                None
            }
            KeyCode::Char('q') => {
                if self.stack.len() > 1 {
                    self.pop_screen();
                    None
                } else {
                    Some(Cmd::Quit)
                }
            }
            KeyCode::Esc => {
                if self.stack.len() > 1 {
                    self.pop_screen();
                }
                None
            }
            KeyCode::Char('c') => {
                self.open_switcher();
                None
            }
            KeyCode::Char('m') => {
                self.push_screen(Screen::Members);
                None
            }
            KeyCode::Char('!') => {
                match self.last_failure.clone().or_else(|| self.gallery.last_failure().map(str::to_string)) {
                    Some(detail) => self.overlay = Some(Overlay::Detail(detail)),
                    None => self.say("nothing has failed on this Device yet.".into()),
                }
                None
            }
            KeyCode::Char(' ') => {
                match item {
                    Some(id) => self.open_actions(id),
                    None => self.say("no Item is selected.".into()),
                }
                None
            }
            KeyCode::Char('y') => item.map(Cmd::CopyPath),
            // An unbound key says so rather than doing nothing.
            KeyCode::Char(c) if !c.is_control() => {
                self.say(format!("no binding for '{c}' — press ? for keys"));
                None
            }
            _ => None,
        }
    }

    fn overlay_key(&mut self, key: KeyEvent) -> Option<Cmd> {
        let overlay = self.overlay.take()?;
        match overlay {
            Overlay::Help | Overlay::Detail(_) => {
                match key.code {
                    KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {}
                    _ => self.overlay = Some(overlay),
                }
                None
            }
            Overlay::Circles { mut sel } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => None,
                KeyCode::Enter => Some(Cmd::Switch(sel)),
                KeyCode::Char('j') | KeyCode::Down => {
                    sel = (sel + 1).min(self.circles.len().saturating_sub(1));
                    self.overlay = Some(Overlay::Circles { sel });
                    None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    self.overlay = Some(Overlay::Circles { sel });
                    None
                }
                _ => {
                    self.overlay = Some(Overlay::Circles { sel });
                    None
                }
            },
            Overlay::Actions { item, mut sel, entries } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => None,
                KeyCode::Enter => {
                    let entry = entries.get(sel)?;
                    match &entry.refusal {
                        Some(reason) => Some(Cmd::Refuse(reason.clone())),
                        None => self.perform_item_action(entry.action, item),
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    sel = (sel + 1).min(entries.len().saturating_sub(1));
                    self.overlay = Some(Overlay::Actions { item, sel, entries });
                    None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    self.overlay = Some(Overlay::Actions { item, sel, entries });
                    None
                }
                _ => {
                    self.overlay = Some(Overlay::Actions { item, sel, entries });
                    None
                }
            },
            Overlay::Monitors { item, mut sel, targets } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => None,
                // Apply, constructed inside a key handler. Occurrence 2 of 2.
                KeyCode::Enter => Some(Cmd::Apply { item, target: targets.get(sel).cloned() }),
                KeyCode::Char('j') | KeyCode::Down => {
                    sel = (sel + 1).min(targets.len().saturating_sub(1));
                    self.overlay = Some(Overlay::Monitors { item, sel, targets });
                    None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    self.overlay = Some(Overlay::Monitors { item, sel, targets });
                    None
                }
                _ => {
                    self.overlay = Some(Overlay::Monitors { item, sel, targets });
                    None
                }
            },
            Overlay::Confirm(mut confirm) => {
                if let Some((wanted, typed)) = &mut confirm.typed {
                    match key.code {
                        KeyCode::Esc => return None,
                        KeyCode::Enter => {
                            if typed.trim() == wanted {
                                return Some(confirm.yes);
                            }
                            self.say(format!("that is not \"{wanted}\" — nothing was done."));
                            return None;
                        }
                        KeyCode::Backspace => {
                            typed.pop();
                        }
                        KeyCode::Char(c) => typed.push(c),
                        _ => {}
                    }
                    self.overlay = Some(Overlay::Confirm(confirm));
                    return None;
                }
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => Some(confirm.yes),
                    // Enter is the safe answer, always.
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Enter => None,
                    _ => {
                        self.overlay = Some(Overlay::Confirm(confirm));
                        None
                    }
                }
            }
        }
    }

    // ── Actions ──────────────────────────────────────────────────────

    /// Turn a key-borne [`ItemAction`] into a [`Cmd`]. Reached only from
    /// [`Self::preview_key`] and the action-menu overlay, both key handlers.
    fn perform_item_action(&mut self, action: ItemAction, item: ItemId) -> Option<Cmd> {
        match action {
            ItemAction::Apply => self.ask_apply(item),
            ItemAction::Favourite => Some(Cmd::Favourite(item)),
            ItemAction::Reveal => Some(Cmd::Reveal(item)),
            ItemAction::CopyPath => Some(Cmd::CopyPath(item)),
            ItemAction::Delete => {
                self.confirm_delete(&item);
                None
            }
        }
    }

    /// Raise the monitor picker when there is more than one target, and apply
    /// straight away when there is exactly one. Targets are the Provider's,
    /// widened by the outputs the Person named in their config.
    fn ask_apply(&mut self, item: ItemId) -> Option<Cmd> {
        let Some(it) = self.item(&item).cloned() else {
            return Some(Cmd::Refuse("that Item is no longer in this Collection.".into()));
        };
        if let Some(reason) = self.apply_refusal(&it) {
            return Some(Cmd::Refuse(reason));
        }
        let targets = self.apply_targets();
        match targets.len() {
            0 | 1 => Some(Cmd::Apply { item, target: targets.into_iter().next() }),
            _ => {
                self.overlay = Some(Overlay::Monitors { item, sel: 0, targets });
                None
            }
        }
    }

    fn apply_targets(&self) -> Vec<ApplyTarget> {
        let mut targets = self.provider.apply_targets().unwrap_or_default();
        if !self.config.config.monitors.is_empty() {
            targets = vec![ApplyTarget::AllMonitors];
            targets.extend(self.config.config.monitors.iter().cloned().map(ApplyTarget::Monitor));
        }
        targets
    }

    /// Why Apply cannot run here, if it cannot: no backend, or a config naming
    /// one that is not installed — never a silent fallback to a different one.
    fn apply_refusal(&self, item: &Item) -> Option<String> {
        if item.path.is_none() {
            return Some("the bytes for this Item have not arrived yet".into());
        }
        if let Some(refusal) = self.config.config.backend_refusal(&self.backends) {
            return Some(refusal);
        }
        self.provider.actions(item).into_iter().find_map(|d| match d.availability {
            Availability::Unavailable { reason } => Some(reason),
            Availability::Available => None,
        })
    }

    fn confirm_delete(&mut self, id: &ItemId) {
        let Some(item) = self.item(id) else { return };
        let title = item.title.clone();
        let here = item.path.is_some();
        self.overlay = Some(Overlay::Confirm(Confirm {
            title: format!("Delete {title}?"),
            body: vec![
                "This removes it from the Collection for every Member, not just here.".into(),
                if here {
                    "The bytes go too, on every Device that holds them.".into()
                } else {
                    "The bytes are not on this Device; the record is removed all the same.".into()
                },
                "kith cannot undo it. A Member who still holds an older copy can restore it."
                    .into(),
            ],
            typed: None,
            yes: Cmd::Delete(id.clone()),
        }));
    }

    fn confirm_leave(&mut self) {
        let Some(circle) = self.active_circle().cloned() else { return };
        self.overlay = Some(Overlay::Confirm(Confirm {
            title: format!("Leave {}?", circle.name),
            body: vec![
                "Your Device stops replicating this Circle. The bytes it already holds stay."
                    .into(),
                "Nothing is deleted from anybody else, and nobody is told.".into(),
                format!("Type {} to confirm.", circle.name),
            ],
            typed: Some((circle.name.clone(), String::new())),
            yes: Cmd::Leave,
        }));
    }

    fn open_actions(&mut self, item: ItemId) {
        let Some(it) = self.item(&item).cloned() else { return };
        let apply_refusal = self.apply_refusal(&it);
        let bytes_absent = it.path.is_none();
        let entries = vec![
            Entry { action: ItemAction::Apply, refusal: apply_refusal },
            Entry { action: ItemAction::Favourite, refusal: None },
            Entry {
                action: ItemAction::Reveal,
                refusal: bytes_absent
                    .then(|| "the bytes for this Item have not arrived yet".to_string()),
            },
            Entry {
                action: ItemAction::CopyPath,
                refusal: bytes_absent.then(|| "nothing is at that path yet".to_string()),
            },
            Entry { action: ItemAction::Delete, refusal: None },
        ];
        self.overlay = Some(Overlay::Actions { item, sel: 0, entries });
    }

    fn open_switcher(&mut self) {
        match self.circles.len() {
            0 => self.say("you are in no Circles yet.".into()),
            1 => self.say(format!("{} is your only Circle.", self.circles[0].name)),
            _ => self.overlay = Some(Overlay::Circles { sel: self.active.unwrap_or(0) }),
        }
    }

    // ── performing ───────────────────────────────────────────────────

    async fn perform(&mut self, cmd: Cmd, term: &mut ratatui::DefaultTerminal) {
        match cmd {
            Cmd::Quit => self.quit = true,
            Cmd::Suspend => self.suspend(term),
            Cmd::Refuse(reason) => self.say(reason),
            Cmd::Apply { item, target } => self.apply(item, target).await,
            Cmd::Favourite(item) => self.toggle_favourite(&item),
            Cmd::Reveal(item) => self.reveal(&item),
            Cmd::CopyPath(item) => self.copy_path(&item),
            Cmd::Delete(item) => self.delete(&item),
            Cmd::Approve(request) => self.approve(request).await,
            Cmd::Reject(request) => self.reject(request),
            Cmd::Leave => self.leave().await,
            Cmd::Switch(index) => self.switch(index),
        }
    }

    /// Apply, on this Device and nowhere else. Nothing is written into the Circle
    /// and nothing is announced: a Person's screen is theirs.
    async fn apply(&mut self, id: ItemId, target: Option<ApplyTarget>) {
        let Some(item) = self.item(&id).cloned() else { return };
        self.gallery.mark_seen(&id);
        self.drain_seen();
        let provider = self.provider.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            provider.perform("wallpaper.apply", &item, target.as_ref())
        })
        .await;
        match outcome {
            Ok(Ok(out)) => self.say(out.message),
            Ok(Err(ActionError::NoBackend(detail))) => {
                self.fail(format!("no wallpaper backend on this Device ({detail})"));
            }
            Ok(Err(e)) => self.fail(format!("Apply did not run: {e}")),
            Err(e) => self.fail(format!("Apply did not run: {e}")),
        }
    }

    /// A Favourite is this Person's, on this Device. Nobody is told.
    fn toggle_favourite(&mut self, id: &ItemId) {
        let Some(circle) = self.active_circle().cloned() else { return };
        let now_favourite = !self.favourites.contains(id);
        if now_favourite {
            self.favourites.insert(id.clone());
        } else {
            self.favourites.remove(id);
        }
        self.gallery.set_favourites(self.favourites.clone());
        self.gallery.mark_seen(id);
        self.drain_seen();
        if let Some(p) = &mut self.preview {
            p.set_marks(Marks { favourite: now_favourite, unseen: false });
        }
        if let Err(e) = append_favourite(&circle.id.0, id, now_favourite) {
            self.fail(format!("the Favourite is not remembered on disk: {e}"));
        } else {
            self.say(
                if now_favourite { "Favourited — nobody is told." } else { "Unfavourited." }
                    .to_string(),
            );
        }
    }

    fn reveal(&mut self, id: &ItemId) {
        let Some(item) = self.item(id).cloned() else { return };
        let Some(path) = item.path else {
            self.say("the bytes for this Item have not arrived yet".into());
            return;
        };
        self.gallery.mark_seen(id);
        self.drain_seen();
        match std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(&path))
            .spawn()
        {
            Ok(_) => self.say("asked your desktop to show it.".into()),
            Err(e) => self.fail(format!("nothing on this Device answered xdg-open ({e})")),
        }
    }

    fn copy_path(&mut self, id: &ItemId) {
        let Some(item) = self.item(id).cloned() else { return };
        match item.path {
            Some(path) => {
                osc52(&path.display().to_string());
                self.say("path copied.".into());
            }
            None => self.say("nothing is at that path yet".into()),
        }
    }

    /// A tombstone appended to this Device's own log, which every Member reduces
    /// to the same answer. The bytes are the engine's to propagate.
    fn delete(&mut self, id: &ItemId) {
        let (Some(circle), Some(device)) = (self.active_circle().cloned(), self.device_id())
        else {
            self.say("this Device's Identity is not known right now.".into());
            return;
        };
        let record = records::Record::Remove {
            item: id.clone(),
            by: self.me.person.clone(),
            at: jiff::Timestamp::now().to_string(),
        };
        match records::append(&circle.root, &self.collection, &device, &record) {
            Ok(()) => {
                // A tombstone with the file still there would be re-adopted by
                // the next scan.
                if let Some(item) = self.item(id)
                    && let Some(path) = item.path.clone()
                {
                    let _ = std::fs::remove_file(path);
                }
                self.gallery.remove(id);
                self.items.retain(|i| &i.id != id);
                if matches!(self.stack.last(), Some(Screen::Preview)) {
                    self.pop_screen();
                }
                self.say("removed from the Collection for every Member.".into());
            }
            Err(e) => self.fail(format!("the removal was not recorded: {e}")),
        }
    }

    /// Admission — the one real gate, and the only thing in this loop that
    /// changes what another Device may receive.
    async fn approve(&mut self, request: JoinRequest) {
        let (Some(engine), Some(circle)) = (self.engine.clone(), self.active_circle().cloned())
        else {
            self.say(ENGINE_DOWN.into());
            return;
        };
        match engine.admit(&circle.id, &request).await {
            Ok(()) => {
                crate::cmd::membership::spend_window(&circle.id.0, &request.device.0);
                self.say(format!("{} is in {}.", members::fingerprint(&request.device.0), circle.name));
                self.refresh_engine_facts();
            }
            Err(e) => self.fail(format!("the Sync Engine did not admit it: {e}")),
        }
    }

    /// Local and silent: there is nothing to deliver a "no", and kith will not
    /// pretend it sent one.
    fn reject(&mut self, request: JoinRequest) {
        let Some(circle) = self.active_circle().cloned() else { return };
        crate::cmd::membership::dismiss(
            &circle.id.0,
            &request.device.0,
            &jiff::Timestamp::now().to_string(),
        );
        self.knocks.retain(|k| k.device != request.device);
        self.rebuild_members();
        self.say("hidden here. That Device is not told, and it may keep knocking.".into());
    }

    async fn leave(&mut self) {
        let (Some(engine), Some(circle)) = (self.engine.clone(), self.active_circle().cloned())
        else {
            self.say(ENGINE_DOWN.into());
            return;
        };
        // Stamped before the engine is told: once replication stops, nothing
        // this Device writes reaches anybody.
        if let Some(device) = self.device_id() {
            let _ = claims::stamp_left(&circle.root, &device, &jiff::Timestamp::now().to_string());
        }
        match engine.leave(&circle.id).await {
            Ok(()) => {
                self.circles.retain(|c| c.id != circle.id);
                self.active = (!self.circles.is_empty()).then_some(0);
                self.load_circle();
                self.stack = vec![Screen::Gallery];
                self.say(format!("you have left {}. The bytes here are still yours.", circle.name));
            }
            Err(e) => self.fail(format!("the Sync Engine did not stop replicating it: {e}")),
        }
    }

    /// Switching resets the stack: a Preview of an Item in one Circle must never
    /// survive a switch to another.
    fn switch(&mut self, index: usize) {
        if index >= self.circles.len() || self.active == Some(index) {
            return;
        }
        self.active = Some(index);
        self.stack = vec![Screen::Gallery];
        self.preview = None;
        self.load_circle();
        self.flush_state();
    }

    /// `Ctrl-Z`: hand the terminal back, stop, and pick up where we were.
    fn suspend(&mut self, term: &mut ratatui::DefaultTerminal) {
        ratatui::restore();
        #[cfg(unix)]
        {
            // One libc symbol does not earn a crate dependency.
            const SIGSTOP: i32 = 19;
            unsafe extern "C" {
                fn raise(sig: i32) -> i32;
            }
            unsafe {
                raise(SIGSTOP);
            }
        }
        if ratatui::try_init().is_ok() {
            let _ = term.clear();
        }
        self.dirty = true;
    }

    // ── loading ──────────────────────────────────────────────────────

    /// Re-read the active Circle's tree, off the loop.
    fn load_circle(&mut self) {
        let Some(circle) = self.active_circle().cloned() else {
            self.gallery = Gallery::new(Vec::new());
            self.gallery.set_picker(self.picker.clone());
            self.gallery.set_emptiness(Emptiness::NoCircles);
            self.dirty = true;
            return;
        };
        self.favourites = read_favourites(&circle.id.0);

        let reserved: Vec<&'static str> =
            self.engine.as_ref().map(|e| e.reserved_paths().to_vec()).unwrap_or_default();
        let root = circle.root.clone();
        let tx = self.tx.clone();
        // One walk at a time. Without this a burst of arrivals would put
        // hundreds of concurrent tree walks on the blocking pool, all racing to
        // describe the same directory.
        self.reload_in_flight = true;
        tokio::task::spawn_blocking(move || {
            let tree = read_tree(&root, &reserved);
            let _ = tx.send(Event::Tree(Box::new(tree)));
        });
        self.refresh_engine_facts();
    }

    fn adopt_tree(&mut self, tree: Tree) {
        self.reload_in_flight = false;
        let Some(circle) = self.active_circle().cloned() else { return };
        if let Some(trouble) = &tree.trouble {
            self.fail(trouble.clone());
        }
        self.items = tree.items;
        self.people = tree.people;
        self.claims = tree.claims;
        self.collection = tree.collection;
        self.founder_person = tree.founder_person;
        self.founder_device = tree.founder_device;

        // Anything this Device has never been shown is unseen.
        let seen: HashSet<String> =
            self.state.seen.get(&circle.id.0).cloned().unwrap_or_default().into_iter().collect();
        self.unseen =
            self.items.iter().filter(|i| !seen.contains(i.id.as_str())).map(|i| i.id.clone()).collect();

        self.gallery.set_favourites(self.favourites.clone());
        self.gallery.set_unseen(self.unseen.clone());
        self.gallery.set_other_members(
            self.people
                .iter()
                .filter(|p| p.id != self.me.person)
                .map(|p| p.display_name.clone())
                .collect(),
        );
        self.gallery.set_emptiness(if self.circles.is_empty() {
            Emptiness::NoCircles
        } else if self.people.len() > 1 && self.items.is_empty() {
            Emptiness::JustJoined
        } else {
            Emptiness::NoItems
        });
        // Arrival re-sorts the grid around the selection and cannot move it.
        self.gallery.update(self.items.clone());
        self.gallery.set_arriving(tree.arriving);

        // A Preview whose record just vanished says so rather than showing bytes
        // nobody claims any more.
        if let Some(p) = &mut self.preview {
            let id = p.item().id.clone();
            match self.items.iter().find(|i| i.id == id) {
                Some(item) => p.show(item),
                None => p.set_removed(&self.me.person, &jiff::Timestamp::now().to_string()),
            }
        }
        self.rebuild_members();
        self.settle();
        self.dirty = true;
    }

    /// Ask the engine the three things only it knows. Spawned, never awaited
    /// inline: a hung daemon must not cost a keystroke.
    fn refresh_engine_facts(&mut self) {
        self.last_engine_refresh = Instant::now();
        let (Some(engine), Some(circle)) = (self.engine.clone(), self.active_circle().cloned())
        else {
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let status = engine.status(&circle.id).await.ok();
            let _ = tx.send(Event::Status(status));
            let peers = engine.devices(&circle.id).await.ok();
            let _ = tx.send(Event::Peers(peers));
            match engine.pending_joins().await {
                Ok(k) => {
                    let _ = tx.send(Event::Knocks(k));
                }
                Err(SyncError::Unreachable) => {}
                Err(_) => {}
            }
        });
    }

    /// Rebuild the Members screen from claims, the descriptor and the engine.
    /// Skipped while the join prompt is open: a Person half-way through typing a
    /// fingerprint must not have it taken away by a background refresh.
    fn rebuild_members(&mut self) {
        if self.members.is_prompt_open() {
            return;
        }
        let Some(circle) = self.active_circle().cloned() else { return };

        let presence_of = |devices: &[String]| -> Presence {
            let Some(peers) = &self.peers else { return Presence::Unknown };
            let mut seen = false;
            for d in devices {
                if let Some(p) = peers.iter().find(|p| &p.device.0 == d) {
                    seen = true;
                    if p.connected {
                        return Presence::Connected;
                    }
                }
            }
            if seen { Presence::NotConnected } else { Presence::Unknown }
        };
        let completion_of = |devices: &[String]| -> Option<u8> {
            let status = self.engine_status.as_ref()?;
            devices.iter().find_map(|d| {
                status
                    .peers
                    .iter()
                    .find(|p| &p.device.0 == d)
                    .map(|p| p.percent.clamp(0.0, 100.0) as u8)
            })
        };

        let mut views = Vec::new();
        for person in &self.people {
            let is_you = person.id == self.me.person;
            let role = if self.founder_person.as_ref() == Some(&person.id) {
                Role::Admin
            } else {
                Role::Member
            };
            let steward = self
                .founder_device
                .as_ref()
                .map(|d| person.devices.iter().any(|pd| pd == d))
                .unwrap_or(false);
            let mut view = MemberView::new(
                person.id.clone(),
                person.display_name.clone(),
                role,
                // kith holds no connection to itself and will not pretend to.
                if is_you { Presence::Unknown } else { presence_of(&person.devices) },
            );
            view.is_you = is_you;
            view.steward = steward;
            view.in_sync = if is_you { None } else { completion_of(&person.devices) };
            view.asserted = self
                .claims
                .iter()
                .filter(|c| c.person == person.id)
                .map(|c| c.asserted.clone())
                .max()
                .unwrap_or_default();
            // A Member has left when *every* claim carrying their Person has
            // `left_at`; one with and one without means a Device stopped.
            let mine: Vec<&MembershipClaim> =
                self.claims.iter().filter(|c| c.person == person.id).collect();
            view.left_at = (!mine.is_empty() && mine.iter().all(|c| c.left_at.is_some()))
                .then(|| mine.iter().filter_map(|c| c.left_at.clone()).max().unwrap_or_default());
            view.in_circle = self
                .peers
                .as_ref()
                .map(|peers| {
                    person.devices.iter().any(|d| peers.iter().any(|p| &p.device.0 == d))
                })
                .unwrap_or(true)
                || is_you;
            view.devices = person.devices.clone();
            views.push(view);
        }

        // A Device receiving this Circle's bytes that no claim names is never
        // hidden: it is a fact the Circle is entitled to see.
        let named: HashSet<&str> =
            self.claims.iter().map(|c| c.device.as_str()).collect();
        let unclaimed: Vec<UnclaimedDevice> = self
            .peers
            .as_ref()
            .map(|peers| {
                peers
                    .iter()
                    .filter(|p| !named.contains(p.device.0.as_str()))
                    .map(|p| UnclaimedDevice {
                        device: p.device.clone(),
                        announced_name: p.name.clone(),
                        presence: if p.connected {
                            Presence::Connected
                        } else {
                            Presence::NotConnected
                        },
                        in_sync: None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Only the Steward's Device may act on a knock; everyone else passes an
        // empty list and never sees the prompt.
        let i_steward = self.founder_person.as_ref() == Some(&self.me.person);
        let dismissed = crate::cmd::membership::dismissed(&circle.id.0);
        let solicited = crate::cmd::membership::open_window(&circle.id.0);
        let pending: Vec<PendingJoin> = if i_steward {
            self.knocks
                .iter()
                .filter(|k| !dismissed.iter().any(|d| d == &k.device.0))
                .map(|k| PendingJoin {
                    circle_name: circle.name.clone(),
                    request: k.clone(),
                    solicited: solicited.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut screen = Members::new(views, pending).with_circle(circle.name.clone());
        if !unclaimed.is_empty() {
            screen = screen.with_unclaimed(unclaimed);
        }
        if let Some(founder) = &self.founder_person
            && !self.people.iter().any(|p| &p.id == founder)
        {
            screen = screen.with_unnamed_admin(founder.clone());
        }
        if !self.engine_reachable {
            screen = screen.with_notice(ENGINE_DOWN.to_string());
        }
        // `kith invite` is the CLI's; the screen says so rather than offering a
        // key that does nothing.
        screen = screen.invite_unavailable(
            "run `kith invite` — a code has to leave this Device by a channel you already trust",
        );
        self.members = screen;
    }

    // ── Preview ──────────────────────────────────────────────────────

    fn open_preview(&mut self, id: &ItemId) {
        let Some(item) = self.item(id).cloned() else { return };
        let mut p = Preview::new(&item)
            .with_people(&self.people, Some(&self.me.person))
            .with_rung(self.rung)
            .with_cell_size(self.picker.font_size().width, self.picker.font_size().height);
        p.set_marks(Marks {
            favourite: self.favourites.contains(id),
            unseen: self.unseen.contains(id),
        });
        if item.path.is_some() {
            p.set_pane(Pane::Decoding);
        }
        self.preview = Some(p);
        // Entering Preview is what marks an Item seen.
        self.gallery.mark_seen(id);
        self.drain_seen();
        self.push_screen(Screen::Preview);
        self.decode(item);
    }

    /// `j`/`k` in Preview move to the adjacent Item without leaving the screen.
    fn step_preview(&mut self, delta: isize) {
        let Some(current) = self.preview.as_ref().map(|p| p.item().id.clone()) else { return };
        let Some(pos) = self.items.iter().position(|i| i.id == current) else { return };
        let next = pos as isize + delta;
        if next < 0 || next as usize >= self.items.len() {
            return;
        }
        let item = self.items[next as usize].clone();
        if let Some(p) = &mut self.preview {
            p.show(&item);
            p.set_marks(Marks {
                favourite: self.favourites.contains(&item.id),
                unseen: self.unseen.contains(&item.id),
            });
            if item.path.is_some() {
                p.set_pane(Pane::Decoding);
            }
        }
        self.gallery.mark_seen(&item.id);
        self.drain_seen();
        self.decode(item);
    }

    /// One `Full`-class decode plus the Provider's facts, on a blocking task:
    /// the render loop never decodes.
    fn decode(&mut self, item: Item) {
        let Some(path) = item.path.clone() else { return };
        let provider = self.provider.clone();
        let tx = self.tx.clone();
        let id = item.id.clone();
        tokio::task::spawn_blocking(move || {
            let facts = provider
                .extract_metadata(&ImportCandidate { path: &path, mime: None })
                .ok();
            let budget = gallery::Class::Full.budget();
            let (image, note) = match provider.preview(&item, budget) {
                Ok(crate::provider::Preview::Image(img)) => (Some(*img), None),
                Ok(crate::provider::Preview::Text(t)) => (None, Some(t)),
                Err(e) => (None, Some(e.to_string())),
            };
            let _ = tx.send(Event::Decoded(Box::new(Decoded { item: id, image, facts, note })));
        });
    }

    fn adopt_decode(&mut self, decoded: Decoded) {
        let Some(p) = &mut self.preview else { return };
        if p.item().id != decoded.item {
            return;
        }
        let recorded_path = p.item().path.as_ref().map(|x| x.display().to_string());
        let mut facts: SidecarFacts = decoded.facts.map(SidecarFacts::from).unwrap_or_default();
        facts.recorded_path = recorded_path;
        p.set_facts(facts);
        match decoded.image {
            Some(image) => p.set_pane(Pane::Image(Box::new(self.picker.new_resize_protocol(image)))),
            None => {
                if let Some(note) = decoded.note.clone() {
                    self.last_failure = Some(note.clone());
                    p.set_pane(Pane::Undecodable { note: Some(note) });
                } else {
                    p.set_pane(Pane::Undecodable { note: None });
                }
            }
        }
        self.dirty = true;
    }

    // ── screen stack ─────────────────────────────────────────────────

    fn push_screen(&mut self, screen: Screen) {
        if self.stack.last() == Some(&screen) {
            return;
        }
        // The stack never grows a second copy of a screen: `m` from Members is
        // not a second Members.
        self.stack.retain(|s| s != &screen);
        self.stack.push(screen);
    }

    fn pop_screen(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
        if !matches!(self.stack.last(), Some(Screen::Preview)) {
            self.preview = None;
        }
    }

    // ── frame ────────────────────────────────────────────────────────

    fn draw(&mut self, frame: &mut Frame) {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

        self.draw_title(frame, rows[0]);
        match self.stack.last().cloned().unwrap_or(Screen::Gallery) {
            Screen::Gallery => self.gallery.render(frame, rows[1]),
            Screen::Preview => {
                if let Some(p) = &mut self.preview {
                    p.render(frame, rows[1]);
                }
            }
            Screen::Members => self.members.render(frame, rows[1]),
        }
        self.draw_status(frame, rows[2]);
        self.draw_hints(frame, rows[3]);

        if self.overlay.is_some() {
            let whole = frame.area();
            self.draw_overlay(frame, whole);
        }
    }

    fn draw_title(&mut self, frame: &mut Frame, area: Rect) {
        let name = self.active_circle().map(|c| c.name.clone());
        let left = match &name {
            Some(n) => format!("kith · {n}"),
            None => "kith".to_string(),
        };
        let mut right = self.gallery.title_row();
        let knocks = self.members.pending_count();
        if knocks > 0 {
            let noun = if knocks == 1 { "wants" } else { "want" };
            right = format!("{right} · {knocks} {noun} to join");
        }
        frame.render_widget(
            Paragraph::new(row(&left, &right, area.width))
                .style(Style::default().add_modifier(Modifier::REVERSED)),
            area,
        );
    }

    /// Left: what this Circle is doing. Right: the two permanent degradations —
    /// the preview rung and the apply backend — so neither is discovered on
    /// failure.
    fn draw_status(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(transient) = self.gallery.take_status() {
            self.say(transient);
        }
        let left = match &self.status {
            Some((line, _)) => line.clone(),
            None => self.sync_line(),
        };
        let backend = self
            .config
            .config
            .named_backend()
            .map(str::to_string)
            .or_else(|| self.backends.first().map(|b| b.to_string()))
            .unwrap_or_else(|| "no apply backend".to_string());
        let right = format!("{} · {backend}", self.rung.label());
        frame.render_widget(Paragraph::new(row(&left, &right, area.width)), area);
    }

    fn sync_line(&self) -> String {
        if !self.engine_reachable {
            return ENGINE_DOWN.to_string();
        }
        let connected = self
            .peers
            .as_ref()
            .map(|p| p.iter().filter(|p| p.connected).count())
            .unwrap_or(0);
        let members = self.people.len();
        let state = match &self.engine_status {
            Some(s) if s.bytes_needed > 0 => format!("receiving {} ", human_bytes(s.bytes_needed)),
            Some(s) => format!("{} ", s.state),
            None => String::new(),
        };
        let noun = if members == 1 { "Member" } else { "Members" };
        format!("● {state}· {members} {noun}, {connected} connected")
    }

    fn draw_hints(&mut self, frame: &mut Frame, area: Rect) {
        let hints = match self.stack.last().cloned().unwrap_or(Screen::Gallery) {
            Screen::Gallery => self.gallery.hints(),
            Screen::Preview => "j k adjacent · enter back · a apply · f fav · space actions · ? keys",
            Screen::Members => "j k move · enter decide · c circles · esc back · ? keys",
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::raw(hints)))
                .style(Style::default().add_modifier(Modifier::DIM)),
            area,
        );
    }

    fn draw_overlay(&mut self, frame: &mut Frame, area: Rect) {
        let (title, body): (String, Vec<Line<'static>>) = match self.overlay.as_ref() {
            None => return,
            Some(Overlay::Help) => ("Keys".into(), self.help_lines()),
            Some(Overlay::Detail(detail)) => (
                "What failed".into(),
                detail
                    .lines()
                    .map(|l| Line::from(l.to_string()))
                    .chain([Line::from(""), Line::from("q close")])
                    .collect(),
            ),
            Some(Overlay::Circles { sel }) => (
                "Switch Circle".into(),
                self.circles
                    .iter()
                    .enumerate()
                    .map(|(i, c)| pick(i == *sel, &c.name))
                    .chain([Line::from(""), Line::from("j k move · enter switch · esc cancel")])
                    .collect(),
            ),
            Some(Overlay::Actions { sel, entries, .. }) => (
                "Actions".into(),
                entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| match &e.refusal {
                        Some(reason) => pick_dim(i == *sel, &format!("{} — {reason}", e.action.label())),
                        None => pick(i == *sel, e.action.label()),
                    })
                    .chain([Line::from(""), Line::from("j k move · enter perform · esc cancel")])
                    .collect(),
            ),
            Some(Overlay::Monitors { sel, targets, .. }) => (
                "Apply to".into(),
                targets
                    .iter()
                    .enumerate()
                    .map(|(i, t)| pick(i == *sel, &self.target_label(t)))
                    .chain([
                        Line::from(""),
                        Line::from("This changes your screen and nobody else's."),
                        Line::from("j k move · enter apply · esc cancel"),
                    ])
                    .collect(),
            ),
            Some(Overlay::Confirm(confirm)) => {
                let mut lines: Vec<Line<'static>> =
                    confirm.body.iter().map(|b| Line::from(b.clone())).collect();
                lines.push(Line::from(""));
                match &confirm.typed {
                    Some((_, typed)) => {
                        lines.push(Line::from(format!("> {typed}")));
                        lines.push(Line::from("enter confirm · esc cancel"));
                    }
                    None => lines.push(Line::from("y yes · n no · enter = no · esc cancel")),
                }
                (confirm.title.clone(), lines)
            }
        };

        let w = area.width.saturating_sub(8).clamp(20, 72);
        let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);
        let box_area = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        frame.render_widget(Clear, box_area);
        frame.render_widget(
            Paragraph::new(body)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(title)),
            box_area,
        );
    }

    /// A Person who named `DP-1` "Desk left" is shown *Desk left (DP-1)*, never
    /// the label alone: the raw name is what every other tool will say.
    fn target_label(&self, target: &ApplyTarget) -> String {
        match target {
            ApplyTarget::AllMonitors => "every monitor".to_string(),
            ApplyTarget::Monitor(name) => match self.config.label(name) {
                Some(label) => format!("{label} ({name})"),
                None => name.clone(),
            },
        }
    }

    fn help_lines(&self) -> Vec<Line<'static>> {
        [
            "j k h l / arrows   move",
            "gg / G             first / last",
            "enter              preview (gallery) · back (preview)",
            "a                  apply — this Device only, never anybody else's",
            "f                  favourite (private; nothing is announced)",
            "F                  favourites only",
            "d                  delete, after a confirm that names the consequence",
            "y                  copy path      r  reveal on disk",
            "space              action menu",
            "c                  switch Circle  m  Members",
            "!                  what last failed",
            "q                  back, or quit at the Gallery",
            "Ctrl-C             quit          Ctrl-Z  suspend",
            "",
            "Roles are an agreement, not a lock. Admission is the only gate kith has.",
            "",
            "q close",
        ]
        .into_iter()
        .map(|s| Line::from(s.to_string()))
        .collect()
    }

    // ── small helpers ────────────────────────────────────────────────

    fn active_circle(&self) -> Option<&CircleHandle> {
        self.active.and_then(|i| self.circles.get(i))
    }

    fn item(&self, id: &ItemId) -> Option<&Item> {
        self.items.iter().find(|i| &i.id == id)
    }

    /// This Device's Identity, as the Sync Engine spells it.
    fn device_id(&self) -> Option<String> {
        self.state.device.clone()
    }

    fn say(&mut self, line: String) {
        self.status = Some((line, Instant::now()));
        // Taking the line back down needs a tick still redrawing then.
        self.settle_until = Instant::now() + STATUS_HOLD + TICK;
        self.dirty = true;
    }

    /// Said *and* kept, so `!` can show it in full afterwards.
    fn fail(&mut self, line: String) {
        self.last_failure = Some(line.clone());
        self.say(format!("{line}  (! for detail)"));
    }

    fn drain_seen(&mut self) {
        let marks = self.gallery.drain_newly_seen();
        if marks.is_empty() {
            return;
        }
        let Some(circle) = self.active_circle().cloned() else { return };
        let seen = self.state.seen.entry(circle.id.0).or_default();
        for id in marks {
            let s = id.as_str().to_string();
            if !seen.contains(&s) {
                seen.push(s);
            }
            self.unseen.remove(&id);
        }
        self.gallery.set_unseen(self.unseen.clone());
    }

    fn flush_state(&mut self) {
        self.drain_seen();
        if let Some(circle) = self.active_circle() {
            self.state.last_circle = Some(circle.id.0.clone());
        }
        self.state.save();
    }
}

// ── the app's own rebuildable state ──────────────────────────────────

/// `$XDG_STATE_HOME/kith/state.toml`. Rebuildable: deleting it costs a switcher
/// press and a set of dots, and nothing else.
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_circle: Option<String>,
    /// Remembered so a Delete still works with the daemon stopped: the log is
    /// keyed by Device and kith mints no id of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    /// Item ids this Device has shown the Person, per Circle. Never synced: an
    /// unseen dot nobody else can derive is the point.
    #[serde(default)]
    seen: BTreeMap<String, Vec<String>>,
}

impl State {
    fn path() -> Option<PathBuf> {
        let base = directories::BaseDirs::new()?;
        Some(base.state_dir().unwrap_or_else(|| base.data_dir()).join("kith/state.toml"))
    }

    fn load() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        std::fs::read_to_string(path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let Some(path) = Self::path() else { return };
        let Ok(text) = toml::to_string_pretty(self) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}

// ── favourites: local, private, never announced ──────────────────────

fn favourites_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.data_dir().join("kith/favourites.jsonl"))
}

/// The effective set is the last operation per `(circle, item)`; the log is
/// append-only, so file order *is* that order.
fn read_favourites(circle: &str) -> HashSet<ItemId> {
    let Some(path) = favourites_path() else { return HashSet::new() };
    let Ok(text) = std::fs::read_to_string(path) else { return HashSet::new() };
    let mut set: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        if v.get("circle").and_then(|c| c.as_str()) != Some(circle) {
            continue;
        }
        let Some(item) = v.get("item").and_then(|i| i.as_str()) else { continue };
        match v.get("k").and_then(|k| k.as_str()) {
            Some("fav") => {
                set.insert(item.to_string());
            }
            Some("unfav") => {
                set.remove(item);
            }
            _ => {}
        }
    }
    set.into_iter().map(ItemId::from).collect()
}

fn append_favourite(circle: &str, item: &ItemId, on: bool) -> std::io::Result<()> {
    let Some(path) = favourites_path() else {
        return Err(std::io::Error::other("no data directory for this Person"));
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line = serde_json::json!({
        "k": if on { "fav" } else { "unfav" },
        "circle": circle,
        "item": item.as_str(),
        "at": jiff::Timestamp::now().to_string(),
    });
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")?;
    f.sync_data()
}

// ── rendering odds and ends ──────────────────────────────────────────

/// One row with `left` flush left and `right` flush right.
fn row(left: &str, right: &str, width: u16) -> Line<'static> {
    let width = width as usize;
    let left = truncate(left, width);
    let right = truncate(right, width.saturating_sub(left.chars().count() + 1));
    let gap = width.saturating_sub(left.chars().count() + right.chars().count());
    Line::from(format!("{left}{}{right}", " ".repeat(gap)))
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
}

fn pick(selected: bool, label: &str) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let line = Line::from(format!("{marker}{label}"));
    if selected { line.style(Style::default().add_modifier(Modifier::REVERSED)) } else { line }
}

fn pick_dim(selected: bool, label: &str) -> Line<'static> {
    pick(selected, label).style(Style::default().add_modifier(Modifier::DIM))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// The clipboard through the terminal itself (OSC 52) — the only way that works
/// over ssh and with no helper installed.
fn osc52(text: &str) {
    let encoded = base64(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_row_never_overflows_its_width() {
        let line = row("kith · a very long Circle name indeed", "42 Items · 3 unseen", 30);
        assert_eq!(line.to_string().chars().count(), 30);
    }

    #[test]
    fn the_status_row_pads_to_the_full_width() {
        let line = row("kith", "ok", 20);
        assert_eq!(line.to_string(), "kith              ok");
    }

    #[test]
    fn reserved_globs_are_matched_by_name() {
        assert!(glob_match("*.sync-conflict-*", "walls.sync-conflict-20260807-ABCDEF.png"));
        assert!(glob_match(".stfolder", ".stfolder"));
        assert!(!glob_match("*.sync-conflict-*", "sunset.png"));
    }

    #[test]
    fn the_arriving_walk_skips_dot_entries_and_reserved_paths() {
        let dir = std::env::temp_dir().join(format!("kith-tui-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".kith/members")).unwrap();
        std::fs::write(dir.join("sunset.png"), b"x").unwrap();
        std::fs::write(dir.join(".kith/members/a.toml"), b"x").unwrap();
        std::fs::write(dir.join("dawn.png.sync-conflict-1.png"), b"x").unwrap();

        let found = arriving_paths(&dir, &["*.sync-conflict-*"], &[]);
        assert_eq!(found, vec![dir.join("sunset.png")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bytes_already_bound_to_an_item_are_not_arriving() {
        let dir = std::env::temp_dir().join(format!("kith-tui-bound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sunset.png"), b"x").unwrap();

        let item = Item {
            id: ItemId::generate(),
            title: "sunset".into(),
            added_by: PersonId::generate(),
            added_at: String::new(),
            path: Some(dir.join("sunset.png")),
            hash: None,
            bytes: None,
        };
        assert!(arriving_paths(&dir, &[], std::slice::from_ref(&item)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn favourites_fold_to_the_last_operation_per_item() {
        let text = r#"{"k":"fav","circle":"c1","item":"A"}
{"k":"fav","circle":"c1","item":"B"}
{"k":"unfav","circle":"c1","item":"A"}
{"k":"fav","circle":"c2","item":"Z"}"#;
        let dir = std::env::temp_dir().join(format!("kith-tui-fav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("favourites.jsonl");
        std::fs::write(&path, text).unwrap();

        // Same fold as `read_favourites`, over a file this test owns.
        let mut set: HashSet<String> = HashSet::new();
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            if v["circle"] != "c1" {
                continue;
            }
            match v["k"].as_str() {
                Some("fav") => {
                    set.insert(v["item"].as_str().unwrap().to_string());
                }
                Some("unfav") => {
                    set.remove(v["item"].as_str().unwrap());
                }
                _ => {}
            }
        }
        assert_eq!(set, HashSet::from(["B".to_string()]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base64_matches_the_osc52_examples() {
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"/home/ana/kith/walls/sunset.png").len() % 4, 0);
    }

    #[test]
    fn byte_counts_are_si_and_never_bare_numbers() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1_900_000), "1.9 MB");
    }

    #[test]
    fn state_round_trips_through_toml() {
        let mut state = State {
            last_circle: Some("kith-abc".into()),
            device: Some("AAAA-BBBB".into()),
            ..State::default()
        };
        state.seen.insert("kith-abc".into(), vec!["01H".into()]);
        let text = toml::to_string_pretty(&state).unwrap();
        let back: State = toml::from_str(&text).unwrap();
        assert_eq!(back.last_circle.as_deref(), Some("kith-abc"));
        assert_eq!(back.seen["kith-abc"], vec!["01H".to_string()]);
    }

    /// The consent invariant, asserted against the source rather than promised.
    ///
    /// Every construction of the Apply command must sit above `fn perform`, where
    /// `on_key`'s call tree lives. If this fails, either a new Apply site appeared
    /// outside a key handler or the file was reordered — check which before
    /// touching the number. The needle is assembled at run time so this test's own
    /// text is not one of the sites it counts.
    #[test]
    fn apply_is_only_ever_constructed_in_a_key_handler() {
        let needle = format!("Cmd::{} {{", "Apply");
        let source = include_str!("mod.rs");
        let sites: Vec<usize> = source.match_indices(&needle).map(|(i, _)| i).collect();
        assert_eq!(
            sites.len(),
            3,
            "two constructions plus the one match arm in perform; found {}",
            sites.len()
        );

        // The two constructions are in `ask_apply` and the monitor picker, both
        // reached only from `on_key`. The third is `perform`'s match arm.
        let performs = source.find("async fn perform(").expect("perform is still here");
        assert_eq!(
            sites.iter().filter(|i| **i < performs).count(),
            2,
            "an Apply is constructed somewhere that is not a key handler"
        );

        // The two handlers arrival flows through return nothing at all.
        assert!(source.contains("fn on_sync(&mut self, change: Change) {"));
        assert!(source.contains("fn on_tick(&mut self) {"));
    }
}
