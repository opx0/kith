//! The fullscreen Preview — one Item, as large as the terminal allows, with the
//! facts its Sidecar carries (`docs/spec/gallery-preview-actions.md` §5, and
//! walkthrough step 10).
//!
//! Three things this screen is responsible for, in order of how much they matter:
//!
//! 1. **The text card never fails.** The image is the top rung of a ladder that
//!    ends in three rows of prose: markers and title, the fact line, the
//!    qualifier line. Whatever the terminal can or cannot draw, whether the bytes
//!    arrived, whether they decode — an Item kith cannot picture is still an Item
//!    kith can name and attribute (ADR-0003 §5).
//! 2. **Attribution names a Person.** Every fact keys on the PersonId carried
//!    inside the Membership claim, never on the Device that wrote it. With no
//!    claim to resolve, the line reads `added by unknown Person (p-01k1yf)` — the
//!    PersonId's short form, never blank and never a device id standing in for a
//!    Person (ADR-0004 §5).
//! 3. **Looking is not acting.** Opening this screen renders pixels and nothing
//!    else. The only producer of a [`PreviewAction::Perform`] is [`Preview::handle_key`],
//!    which is §7's consent invariant made checkable: `grep -n 'Perform' ` over
//!    this file hits the key handler and its enum, and nothing else.
//!
//! **Routing.** This screen claims only the keys the keymap gives it
//! (`docs/spec/cli-tui.md` §6.4) and returns `None` for everything else, so the
//! app's global layer still owns `?`, `c`, `m`, `!`, `Ctrl-C` and `Ctrl-Z`, and
//! still reports an unbound key. A screen that swallowed keys it does not
//! implement would make the hint row a lie.
//!
//! **What this screen does not do.** It decodes nothing (the thumbnail pipeline
//! hands it pixels), writes nothing (marking an Item seen on entry is the app's
//! write to `state.toml`), and performs no Action (it asks; the app performs).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use jiff::{Timestamp, tz::TimeZone};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::StatefulWidget;
use ratatui_image::picker::ProtocolType;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};

use crate::domain::{Item, Person, PersonId};
use crate::provider::{Availability, ProviderFacts};

/// The fact block: markers and title, the fact line, the qualifier line.
const FACT_ROWS: u16 = 3;

/// §1.2's assumption where the terminal will not report its cell size. Used only
/// to shape the placeholder field; a real image is placed by `ratatui-image`
/// against the size it queried for itself.
const DEFAULT_CELL: (u16, u16) = (8, 16);

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

// ── the preview ladder ───────────────────────────────────────────────

/// Which rung of ADR-0001's preview ladder this terminal gave us.
///
/// Detection runs once at startup and belongs to the app, not here; the rung is
/// passed in because a terminal does not change protocol mid-session.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Rung {
    Kitty,
    Iterm2,
    Sixel,
    /// The shipped fallback, and never a defect: ADR-0001 promises kith is never
    /// unusable because of a terminal.
    #[default]
    Halfblocks,
}

impl From<ProtocolType> for Rung {
    fn from(protocol: ProtocolType) -> Self {
        Rung::from_protocol(protocol)
    }
}

impl Rung {
    pub fn from_protocol(protocol: ProtocolType) -> Self {
        match protocol {
            ProtocolType::Kitty => Rung::Kitty,
            ProtocolType::Iterm2 => Rung::Iterm2,
            ProtocolType::Sixel => Rung::Sixel,
            ProtocolType::Halfblocks => Rung::Halfblocks,
        }
    }

    /// The label the status row carries permanently, in the same voice as the
    /// Circle's sync state (cli-tui.md §7.3).
    ///
    /// Passive and never repeated: there is no modal, no toast and no first-run
    /// banner about the rung, and the words *unsupported*, *error* and *failed*
    /// are banned here. Capitalisation follows cli-tui.md, which owns wording.
    pub fn label(self) -> &'static str {
        match self {
            Rung::Kitty => "kitty",
            Rung::Iterm2 => "iTerm2",
            Rung::Sixel => "sixel",
            Rung::Halfblocks => "halfblocks (degraded)",
        }
    }

    /// Rows this screen must leave alone at the bottom of its area.
    ///
    /// One on sixel: `ratatui-image` documents that a sixel image on the
    /// terminal's last line can scroll the screen, and never putting one there is
    /// the cheapest fix (§1.2).
    fn reserved_rows(self) -> u16 {
        u16::from(self == Rung::Sixel)
    }
}

// ── what the screen is given ─────────────────────────────────────────

/// Everything the fact block states that [`Item`] does not already carry — all of
/// it derived from the Item's Sidecar.
///
/// `width`, `height` and `format` are the Provider's facts (ADR-0003 §1);
/// `adopted` and `clock_skewed` are the record's. ADR-0004 §4.2 reserves
/// `adopted` and leaves it unwritten in v0.1, so it is false on every Item today;
/// the *found by* rendering is here so the surface lands with the field rather
/// than after it.
#[derive(Clone, Debug, Default)]
pub struct SidecarFacts {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
    /// Found on disk rather than added through kith. Reads *found by Ana*: being
    /// the first Device to notice something is a weaker claim than having added
    /// it, and the surface makes the weaker claim.
    pub adopted: bool,
    /// The adding Device's clock ran more than a day ahead of this one, so the
    /// Gallery sorted this Item by arrival (§1.3). Preview shows the claimed date,
    /// marked.
    pub clock_skewed: bool,
    /// The path the record declares, relative to the Collection root. Known even
    /// when the bytes are not here, which is the one place §4.1 shows it.
    pub recorded_path: Option<String>,
}

impl From<ProviderFacts> for SidecarFacts {
    fn from(f: ProviderFacts) -> Self {
        Self { width: f.width, height: f.height, format: f.format, ..Self::default() }
    }
}

/// This Person's private marks. Local, authoritative, never synced, never
/// announced — nothing about them is written into anything the Circle shares
/// (ADR-0004 §7, §3.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub favourite: bool,
    pub unseen: bool,
}

/// The state of the image pane — the rungs of the ladder, in order.
///
/// The app owns decoding and hands the result over; this screen renders whichever
/// rung it was given and degrades to the text card without being asked.
pub enum Pane {
    /// Decoded `Full`-class pixels, already encoded for the terminal's rung.
    Image(Box<StatefulProtocol>),
    /// A decode is queued or running. No spinner: the image is about to appear.
    Decoding,
    /// The record is here and the bytes are not (§4.1). Not a failure — the
    /// normal arrival state, because 250 bytes of metadata beat a 4 MB wallpaper
    /// across the wire.
    BytesAbsent,
    /// The bytes are here and are not a readable image, or they tripped the
    /// decode guard (§4.2). `note` replaces the standard sentence when there is
    /// something more specific to say.
    Undecodable { note: Option<String> },
}

impl Pane {
    /// The decode guard's card: kith refuses these bytes, states the size it
    /// refused, and does not stop the Person applying them.
    pub fn too_large(width: u32, height: u32) -> Self {
        Pane::Undecodable { note: Some(format!("too large to preview ({width}×{height})")) }
    }
}

/// The Actions this screen can be asked for. The v0.1 set is closed at five
/// (§6.1); `open` is in ADR-0003 §3's list and in neither ROADMAP's Actions row
/// nor the keymap, so it does not ship.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemAction {
    Apply,
    Favourite,
    Delete,
    Reveal,
    CopyPath,
}

impl ItemAction {
    /// The label the action menu prints.
    pub fn label(self) -> &'static str {
        match self {
            ItemAction::Apply => "Apply",
            ItemAction::Favourite => "Favourite",
            ItemAction::Delete => "Delete",
            ItemAction::Reveal => "Reveal on disk",
            ItemAction::CopyPath => "Copy path",
        }
    }
}

/// What the Preview asks the app to do about a keystroke it claimed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewAction {
    /// `q`, `Esc`, `Enter` — return to the Gallery with this Item selected.
    Back,
    /// The adjacent Item in the Gallery's order, without leaving Preview.
    Next,
    Previous,
    /// `Space` — the action menu overlay.
    Menu,
    /// Perform this Action on the Item on screen. Constructed in
    /// [`Preview::handle_key`] and nowhere else (§7.2).
    Perform(ItemAction),
    /// The key names an Action this Item cannot take right now. `reason` is
    /// verbatim for the status row — the surface never omits an Action, it
    /// explains it (§6.8, ADR-0003 §3).
    Unavailable { action: ItemAction, reason: String },
}

/// A tombstone that arrived for the Item on screen (§5.4).
struct Removal {
    by: PersonId,
    at: String,
}

// ── the screen ───────────────────────────────────────────────────────

/// One Item, fullscreen, with its Sidecar facts.
pub struct Preview {
    item: Item,
    facts: SidecarFacts,
    marks: Marks,
    pane: Pane,
    removed: Option<Removal>,
    /// The Circle's People, folded from its Membership claims. Constant while the
    /// screen lives; attribution resolves through it.
    people: Vec<Person>,
    /// This Device's Person, so attribution can say *you*.
    me: Option<PersonId>,
    rung: Rung,
    cell: (u16, u16),
    /// Held so tests can pin a zone; production always reads the Device's own.
    zone: TimeZone,
}

impl Preview {
    /// Open the Preview on an Item.
    ///
    /// Takes the Item by reference and keeps a clone: the screen outlives any
    /// borrow of the Gallery's Vec, and an Item is a handful of small fields.
    ///
    /// Entering Preview is what marks an Item seen (§3.1), but this constructor
    /// writes nothing — the seen set is the app's local state, and a screen that
    /// wrote to disk on construction could not be rendered in a test.
    pub fn new(item: &Item) -> Self {
        let pane = default_pane(item);
        Self {
            item: item.clone(),
            facts: SidecarFacts::default(),
            marks: Marks::default(),
            pane,
            removed: None,
            people: Vec::new(),
            me: None,
            rung: Rung::default(),
            cell: DEFAULT_CELL,
            zone: TimeZone::system(),
        }
    }

    /// The Circle's People and, optionally, which of them is this Device's.
    pub fn with_people(mut self, people: &[Person], me: Option<&PersonId>) -> Self {
        self.people = people.to_vec();
        self.me = me.cloned();
        self
    }

    /// The rung the app detected once at startup.
    pub fn with_rung(mut self, rung: Rung) -> Self {
        self.rung = rung;
        self
    }

    /// The terminal's real cell size in pixels, queried once and re-queried on
    /// resize. Where the terminal will not report it, leave this alone: the
    /// default is §1.2's 8×16 assumption, which `kith doctor` states.
    pub fn with_cell_size(mut self, w_px: u16, h_px: u16) -> Self {
        if w_px > 0 && h_px > 0 {
            self.cell = (w_px, h_px);
        }
        self
    }

    /// Move to the adjacent Item without leaving the screen (§5.3).
    ///
    /// Clears everything that was about the previous Item — facts, marks, pixels,
    /// tombstone — because carrying any of them over would attribute one Item's
    /// facts to another. The People and the rung stay: they are the Circle's and
    /// the terminal's, not the Item's.
    pub fn show(&mut self, item: &Item) {
        self.pane = default_pane(item);
        self.item = item.clone();
        self.facts = SidecarFacts::default();
        self.marks = Marks::default();
        self.removed = None;
    }

    pub fn set_facts(&mut self, facts: SidecarFacts) {
        self.facts = facts;
    }

    pub fn set_marks(&mut self, marks: Marks) {
        self.marks = marks;
    }

    pub fn set_pane(&mut self, pane: Pane) {
        self.pane = pane;
    }

    /// A tombstone arrived for the Item on screen. kith does not yank the screen
    /// away: it says who and when, and every Action except Copy path goes
    /// unavailable. Being shown *what* disappeared and *who* removed it is the
    /// difference between a sync product and a haunting (§5.4).
    pub fn set_removed(&mut self, by: &PersonId, at: &str) {
        self.removed = Some(Removal { by: by.clone(), at: at.to_string() });
    }

    /// The Item on screen — the app needs its id to perform anything.
    pub fn item(&self) -> &Item {
        &self.item
    }

    pub fn rung(&self) -> Rung {
        self.rung
    }

    /// For the status row's right side, where the rung sits permanently next to
    /// the apply backend so neither degradation is discovered at the moment of
    /// failure (§8.2).
    pub fn rung_label(&self) -> &'static str {
        self.rung.label()
    }

    /// Whether an Action can run on this Item right now, and why not.
    ///
    /// This answers only for the two reasons this screen can see: the bytes and
    /// the tombstone. A Provider's own `Unavailable` — no wallpaper backend found
    /// (§6.8) — comes from `Provider::actions()` and is the app's to check; both
    /// reasons reach the Person the same way, verbatim on the status row.
    pub fn availability(&self, action: ItemAction) -> Availability {
        if self.removed.is_some() && action != ItemAction::CopyPath {
            return unavailable("this Item was removed from the Collection.");
        }
        if self.item.path.is_none() {
            return match action {
                // A Favourite is a mark on the Item, not on its bytes, and it has
                // to work before they land — that is exactly when a Person decides
                // they want it. Delete appends a tombstone; there are no bytes to
                // remove, and the confirm says so.
                ItemAction::Favourite | ItemAction::Delete => Availability::Available,
                ItemAction::Apply | ItemAction::Reveal => {
                    unavailable("the bytes for this Item have not arrived yet")
                }
                ItemAction::CopyPath => match &self.facts.recorded_path {
                    // The recorded path is shown in the reason so it can still be
                    // read, even though nothing is there to copy.
                    Some(path) => unavailable(&format!("nothing is at that path yet ({path})")),
                    None => unavailable("nothing is at that path yet"),
                },
            };
        }
        Availability::Available
    }

    /// Route one keystroke. `None` means this screen does not claim the key, and
    /// the app's global layer gets it next.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PreviewAction> {
        // Key-release and key-repeat events arrive under the kitty keyboard
        // protocol; acting on both would double every Action.
        if key.kind != KeyEventKind::Press {
            return None;
        }
        // Ctrl and Alt combinations belong to the global layer — `Ctrl-d` is half
        // a page, and must never be read as `d`, which deletes.
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return None;
        }

        let action = match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => return Some(PreviewAction::Back),
            // Arrows are always sufficient (cli-tui.md §6.3); `h` and `l` carry
            // the keymap's list meaning of previous/next, which in a screen
            // showing one Item is the same movement as `j` and `k`.
            KeyCode::Char('j') | KeyCode::Char('l') | KeyCode::Down | KeyCode::Right => {
                return Some(PreviewAction::Next);
            }
            KeyCode::Char('k') | KeyCode::Char('h') | KeyCode::Up | KeyCode::Left => {
                return Some(PreviewAction::Previous);
            }
            KeyCode::Char(' ') => return Some(PreviewAction::Menu),
            KeyCode::Char('a') => ItemAction::Apply,
            KeyCode::Char('f') => ItemAction::Favourite,
            KeyCode::Char('d') => ItemAction::Delete,
            KeyCode::Char('r') => ItemAction::Reveal,
            KeyCode::Char('y') => ItemAction::CopyPath,
            _ => return None,
        };

        match self.availability(action) {
            Availability::Available => Some(PreviewAction::Perform(action)),
            Availability::Unavailable { reason } => {
                Some(PreviewAction::Unavailable { action, reason })
            }
        }
    }

    /// Paint the screen into the content area (the terminal minus the frame's
    /// three fixed rows).
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.draw(frame.buffer_mut(), area);
    }

    /// `render` without a `Frame`, so every tier can be asserted in a test.
    /// `Frame::render_widget` is this call and nothing else.
    fn draw(&mut self, buf: &mut Buffer, area: Rect) {
        let area = self.content(area);
        if area.width == 0 || area.height == 0 {
            return;
        }

        let fact_h = area.height.min(FACT_ROWS);
        let facts = Rect { y: area.y + area.height - fact_h, height: fact_h, ..area };
        let pane = Rect { height: area.height - fact_h, ..area };

        // The pane first: an image that will not encode drops a rung, and the
        // fact block below states the reason in the same frame.
        self.draw_pane(buf, pane);
        self.draw_facts(buf, facts);
    }

    /// The rows this screen may paint, which is not always the rows it was given.
    fn content(&self, area: Rect) -> Rect {
        Rect { height: area.height.saturating_sub(self.rung.reserved_rows()), ..area }
    }

    fn draw_pane(&mut self, buf: &mut Buffer, pane: Rect) {
        if pane.width == 0 || pane.height == 0 {
            return;
        }
        let dim = Style::new().add_modifier(Modifier::DIM);

        if let Pane::Image(protocol) = &mut self.pane {
            // Fit, never Scale: an image smaller than the pane is centred at its
            // own size. Upscaling a 1280×720 wallpaper to fill a 4K terminal pane
            // is a lie about what the Person is about to apply.
            let widget = StatefulImage::<StatefulProtocol>::default().resize(Resize::Fit(None));
            widget.render(pane, buf, protocol);

            if let Some(Err(e)) = protocol.last_encoding_result() {
                // These bytes decoded and would not encode for this terminal. The
                // ladder has one more rung below it, and it is the text card.
                self.pane = Pane::Undecodable {
                    note: Some(format!("preview unavailable — these bytes would not encode here ({e})")),
                };
            } else if self.removed.is_some() {
                // §5.4 dims the image. Honestly: on the pixel rungs the image
                // widget owns its cells and a style cannot reach them, so this
                // dims the halfblocks rung only. The qualifier line is what
                // carries the removal on every rung.
                buf.set_style(pane, dim);
            }
            return;
        }

        // Every other rung is a placeholder field, letterboxed exactly where the
        // picture would be, with the marker centred in it. The pane is never a
        // hole: an Item kith cannot picture is still an Item kith can name.
        let (glyph, marker, hint) = match &self.pane {
            Pane::Image(_) => unreachable!("handled above"),
            Pane::Decoding => ("░", None, None),
            Pane::BytesAbsent => ("▒", Some("↓"), None),
            // §4.2's card is the same card either way — unreadable bytes or the
            // decode guard — and it always offers the detail.
            Pane::Undecodable { .. } => ("▒", Some("!"), Some("press ! for detail")),
        };

        let field = self.letterbox(pane);
        let row = glyph.repeat(field.width as usize);
        for y in field.y..field.y + field.height {
            buf.set_stringn(field.x, y, &row, field.width as usize, dim);
        }
        if let Some(marker) = marker {
            let y = field.y + field.height / 2;
            let x = field.x + field.width / 2;
            buf.set_stringn(x, y, marker, 1, Style::new());
            if let Some(hint) = hint {
                let w = hint.chars().count() as u16;
                if field.height >= 3 && field.width >= w {
                    let x = field.x + (field.width - w) / 2;
                    buf.set_stringn(x, y + 1, hint, w as usize, dim);
                }
            }
        }
    }

    /// The rectangle the picture would occupy: its own aspect ratio, fitted to
    /// the pane and centred. 16:9 is the wallpaper norm and stands in when the
    /// facts are absent — a record from a newer or foreign writer.
    fn letterbox(&self, pane: Rect) -> Rect {
        let (w_px, h_px) = match (self.facts.width, self.facts.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w as f32, h as f32),
            _ => (16.0, 9.0),
        };
        let w_cells = w_px / self.cell.0 as f32;
        let h_cells = h_px / self.cell.1 as f32;
        let scale = (pane.width as f32 / w_cells).min(pane.height as f32 / h_cells);
        let width = ((w_cells * scale).round() as u16).clamp(1, pane.width);
        let height = ((h_cells * scale).round() as u16).clamp(1, pane.height);
        Rect {
            x: pane.x + (pane.width - width) / 2,
            y: pane.y + (pane.height - height) / 2,
            width,
            height,
        }
    }

    /// The three rows that must never fail. They are drawn top-down from the
    /// block's own rectangle, so a content area with room for one row still gets
    /// the Item's name.
    fn draw_facts(&self, buf: &mut Buffer, block: Rect) {
        let pad = if block.width > 6 { 2 } else { 0 };
        let x = block.x + pad;
        let width = block.width - pad;
        let now = Timestamp::now();
        let dim = Style::new().add_modifier(Modifier::DIM);

        let rows: [(String, Style); 3] = [
            (self.title_line(), Style::new()),
            (self.fact_line(now), Style::new()),
            (self.qualifier(now), dim),
        ];
        for (i, (text, style)) in rows.iter().enumerate() {
            let i = i as u16;
            if i >= block.height {
                break;
            }
            let text = truncate(text, width as usize);
            buf.set_stringn(x, block.y + i, &text, width as usize, *style);
        }
    }

    /// Markers then the Sidecar title. `★ ● ? ↓` in that order, where they apply.
    fn title_line(&self) -> String {
        let mut markers = String::new();
        if self.marks.favourite {
            markers.push('★');
        }
        if self.marks.unseen {
            markers.push('●');
        }
        if self.facts.clock_skewed {
            markers.push('?');
        }
        if self.item.path.is_none() {
            markers.push('↓');
        }
        if markers.is_empty() {
            self.item.title.clone()
        } else {
            format!("{markers} {}", self.item.title)
        }
    }

    /// Attribution · when · resolution · byte size · format, in that fixed order.
    /// A field kith does not have is omitted rather than guessed; the two that are
    /// never omitted are the two a Person needs to trust the tile — who, and when.
    fn fact_line(&self, now: Timestamp) -> String {
        let mut parts = vec![
            self.attribution(),
            when_label(&self.item.added_at, now, &self.zone, self.facts.clock_skewed),
        ];
        parts.push(match (self.facts.width, self.facts.height) {
            (Some(w), Some(h)) => format!("{w}×{h}"),
            _ => "resolution unknown".to_string(),
        });
        if let Some(bytes) = self.item.bytes {
            parts.push(size_label(bytes));
        }
        if let Some(format) = &self.facts.format {
            parts.push(format.clone());
        }
        parts.join(" · ")
    }

    fn attribution(&self) -> String {
        // *found by*, never *added by*: being the first Device to notice bytes on
        // disk is a weaker claim than having added them.
        let verb = if self.facts.adopted { "found by" } else { "added by" };
        format!("{verb} {}", self.person_label(&self.item.added_by))
    }

    /// A Person's name, resolved through the Membership claims. Never a device id
    /// and never blank (ADR-0004 §5).
    fn person_label(&self, id: &PersonId) -> String {
        if self.me.as_ref() == Some(id) {
            return "you".to_string();
        }
        match self.people.iter().find(|p| &p.id == id) {
            Some(person) => person.display_name.clone(),
            None => format!("unknown Person ({})", id.short()),
        }
    }

    /// The most specific honest thing there is to say about *this* Item; when
    /// there is nothing specific, the standing attribution caveat. §5.2's priority
    /// order exactly.
    ///
    /// Bytes-absence outranks removal, which is §5.2's list and not a contest in
    /// practice: §5.4's Item is one on screen with a picture, so it has bytes.
    fn qualifier(&self, now: Timestamp) -> String {
        if self.item.path.is_none() {
            return "bytes not here yet".to_string();
        }
        if let Pane::Undecodable { note } = &self.pane {
            return note.clone().unwrap_or_else(|| {
                "preview unavailable — these bytes are not a readable image".to_string()
            });
        }
        if let Some(removal) = &self.removed {
            return format!(
                "removed by {}, {} — other Devices keep versions for 30 days",
                self.person_label(&removal.by),
                ago_label(&removal.at, now),
            );
        }
        if self.facts.clock_skewed {
            return "that Device's clock is ahead of yours; kith sorted this by arrival".to_string();
        }
        if self.facts.adopted {
            return "found on disk when this Circle was adopted — nobody added it through kith"
                .to_string();
        }
        // The attribution equivalent of the Role caveat, in the same voice:
        // one dim line, one place, never a modal (ADR-0004 §5).
        "Attribution is what the adding Device claimed; kith cannot prove it.".to_string()
    }
}

// ── helpers ──────────────────────────────────────────────────────────

/// An Item with no path has no bytes on this Device — the normal arrival state,
/// not a failure (§4.1).
fn default_pane(item: &Item) -> Pane {
    if item.path.is_none() { Pane::BytesAbsent } else { Pane::Decoding }
}

fn unavailable(reason: &str) -> Availability {
    Availability::Unavailable { reason: reason.to_string() }
}

/// SI, because a wallpaper's size is a fact about a disk and disks are sold in
/// powers of ten. One decimal below 10, none above.
fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    let rounded = (value * 10.0).round() / 10.0;
    // 999_999 bytes rounds to 1000.0 kB; carry rather than print four digits.
    if rounded >= 1000.0 && unit + 1 < UNITS.len() {
        return format!("1.0 {}", UNITS[unit + 1]);
    }
    if rounded < 10.0 {
        format!("{rounded:.1} {}", UNITS[unit])
    } else {
        format!("{rounded:.0} {}", UNITS[unit])
    }
}

/// `today 09:14`, `yesterday 21:03`, `3 days ago`, then absolute: `3 Aug 2026`.
///
/// A clock-skewed record shows the date it claims, marked — the Gallery sorted it
/// by arrival, and Preview is where the claim itself is stated.
fn when_label(at: &str, now: Timestamp, zone: &TimeZone, clock_skewed: bool) -> String {
    let Ok(when) = at.parse::<Timestamp>() else {
        // Never fabricate a date: an unparseable stamp is shown as it was written.
        return at.trim().to_string();
    };
    let when = when.to_zoned(zone.clone());
    let absolute = format!("{} {} {}", when.day(), MONTHS[(when.month() - 1) as usize], when.year());
    if clock_skewed {
        return format!("dated {absolute} (?)");
    }

    let today = now.to_zoned(zone.clone());
    let days = day_number(today.year(), today.month(), today.day())
        - day_number(when.year(), when.month(), when.day());
    match days {
        0 => format!("today {:02}:{:02}", when.hour(), when.minute()),
        1 => format!("yesterday {:02}:{:02}", when.hour(), when.minute()),
        // A week is where "N days ago" stops being easier to read than a date.
        2..=6 => format!("{days} days ago"),
        _ => absolute,
    }
}

/// `2 minutes ago` — the elapsed form, used where the fact is the elapsing.
fn ago_label(at: &str, now: Timestamp) -> String {
    let Ok(when) = at.parse::<Timestamp>() else {
        return at.trim().to_string();
    };
    let seconds = now.as_second() - when.as_second();
    if seconds < 60 {
        return "just now".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return plural(minutes, "minute");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return plural(hours, "hour");
    }
    plural(hours / 24, "day")
}

fn plural(n: i64, unit: &str) -> String {
    if n == 1 { format!("1 {unit} ago") } else { format!("{n} {unit}s ago") }
}

/// Days from the civil epoch, so two civil dates can be subtracted. Howard
/// Hinnant's algorithm; the calendar is the same everywhere kith runs.
fn day_number(year: i16, month: i8, day: i8) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Truncate to `max` cells with `…`.
///
/// Counts characters rather than display columns: v0.1 carries no width table,
/// and every string this screen prints is a title plus ASCII facts and the
/// marker glyphs. A title of wide characters truncates a little early, which is
/// the safe direction — it never overruns the row.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::layout::Rect;
    use ratatui_image::picker::Picker;

    use crate::domain::ItemId;

    const ANA: &str = "p-01k1yfq2m7vj3w8t0pz4rxab6c";
    const BEN: &str = "p-01k1yg04xkpm7a2zq0b8n6te1f";
    const NOW: &str = "2026-08-07T12:00:00Z";

    fn person_id(s: &str) -> PersonId {
        serde_json::from_str::<PersonId>(&format!("\"{s}\"")).expect("a PersonId is a bare string")
    }

    fn person(id: &str, name: &str) -> Person {
        Person { id: person_id(id), display_name: name.into(), devices: vec!["DEVICE-1".into()] }
    }

    fn now() -> Timestamp {
        NOW.parse().unwrap()
    }

    /// An Item whose bytes are here.
    fn item() -> Item {
        Item {
            id: ItemId::generate(),
            title: "sunset-4k".into(),
            added_by: person_id(ANA),
            added_at: "2026-08-07T09:14:00Z".into(),
            path: Some(std::path::PathBuf::from("/tmp/kith-test/sunset-4k.png")),
            hash: Some("b3:0123456789ab0123456789ab0123456789ab0123456789ab0123456789ab0123".into()),
            bytes: Some(1_900_000),
        }
    }

    fn facts() -> SidecarFacts {
        SidecarFacts {
            width: Some(3840),
            height: Some(2160),
            format: Some("png".into()),
            ..SidecarFacts::default()
        }
    }

    /// A Preview of Ana's Item, seen by Ben, with the clock pinned to UTC.
    fn preview() -> Preview {
        let mut p = Preview::new(&item())
            .with_people(&[person(ANA, "Ana"), person(BEN, "Ben")], Some(&person_id(BEN)));
        p.zone = TimeZone::UTC;
        p.set_facts(facts());
        p
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn rendered(preview: &mut Preview, width: u16, height: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        let area = buf.area;
        preview.draw(&mut buf, area);
        buf
    }

    // ── the fact block ───────────────────────────────────────────────

    #[test]
    fn fact_line_is_attribution_when_resolution_size_format() {
        assert_eq!(
            preview().fact_line(now()),
            "added by Ana · today 09:14 · 3840×2160 · 1.9 MB · png"
        );
    }

    #[test]
    fn this_person_is_you_not_their_own_name() {
        let mut p = preview();
        p.item.added_by = person_id(BEN);
        assert!(p.fact_line(now()).starts_with("added by you · "));
    }

    #[test]
    fn an_unresolvable_person_is_a_person_short_form_never_a_device() {
        let mut p = Preview::new(&item());
        p.zone = TimeZone::UTC;
        // No Membership claim names Ana, which is what an old or foreign writer
        // looks like. The line must still name a Person.
        assert!(p.fact_line(now()).starts_with("added by unknown Person (p-01k1yf) · "));
        assert!(!p.fact_line(now()).contains("DEVICE"));
    }

    #[test]
    fn adopted_items_are_found_not_added() {
        let mut p = preview();
        p.set_facts(SidecarFacts { adopted: true, ..facts() });
        assert!(p.fact_line(now()).starts_with("found by Ana · "));
    }

    #[test]
    fn absent_facts_are_stated_never_guessed() {
        let mut p = preview();
        p.set_facts(SidecarFacts::default());
        // No resolution and no format: one says so, the other is simply absent.
        assert_eq!(p.fact_line(now()), "added by Ana · today 09:14 · resolution unknown · 1.9 MB");
    }

    #[test]
    fn markers_precede_the_title_in_a_fixed_order() {
        let mut p = preview();
        p.set_marks(Marks { favourite: true, unseen: true });
        assert_eq!(p.title_line(), "★● sunset-4k");
    }

    #[test]
    fn a_byte_less_item_carries_the_arrow_marker() {
        let mut absent = item();
        absent.path = None;
        let p = Preview::new(&absent);
        assert_eq!(p.title_line(), "↓ sunset-4k");
    }

    // ── the qualifier ladder ─────────────────────────────────────────

    #[test]
    fn the_standing_caveat_is_the_default_qualifier() {
        assert_eq!(
            preview().qualifier(now()),
            "Attribution is what the adding Device claimed; kith cannot prove it."
        );
    }

    #[test]
    fn qualifier_priority_is_bytes_then_decode_then_removal_then_skew_then_adoption() {
        let mut p = preview();

        p.set_facts(SidecarFacts { adopted: true, ..facts() });
        assert!(p.qualifier(now()).starts_with("found on disk"));

        p.set_facts(SidecarFacts { adopted: true, clock_skewed: true, ..facts() });
        assert!(p.qualifier(now()).starts_with("that Device's clock is ahead"));

        p.set_removed(&person_id(ANA), "2026-08-07T11:58:00Z");
        assert_eq!(
            p.qualifier(now()),
            "removed by Ana, 2 minutes ago — other Devices keep versions for 30 days"
        );

        p.set_pane(Pane::Undecodable { note: None });
        assert_eq!(
            p.qualifier(now()),
            "preview unavailable — these bytes are not a readable image"
        );

        p.item.path = None;
        assert_eq!(p.qualifier(now()), "bytes not here yet");
    }

    #[test]
    fn the_decode_guard_states_the_size_it_refused() {
        let mut p = preview();
        p.set_pane(Pane::too_large(30000, 30000));
        assert_eq!(p.qualifier(now()), "too large to preview (30000×30000)");
    }

    // ── availability ─────────────────────────────────────────────────

    #[test]
    fn a_byte_less_item_can_still_be_favourited_and_deleted() {
        let mut absent = item();
        absent.path = None;
        let p = Preview::new(&absent);
        for action in [ItemAction::Favourite, ItemAction::Delete] {
            assert!(matches!(p.availability(action), Availability::Available), "{action:?}");
        }
    }

    #[test]
    fn a_byte_less_item_refuses_apply_reveal_and_copy_with_reasons() {
        let mut absent = item();
        absent.path = None;
        let mut p = Preview::new(&absent);
        p.set_facts(SidecarFacts { recorded_path: Some("sunset-4k.png".into()), ..facts() });

        for action in [ItemAction::Apply, ItemAction::Reveal] {
            match p.availability(action) {
                Availability::Unavailable { reason } => {
                    assert_eq!(reason, "the bytes for this Item have not arrived yet");
                }
                Availability::Available => panic!("{action:?} must not be offered"),
            }
        }
        match p.availability(ItemAction::CopyPath) {
            // The path is unreachable but readable — it is in the reason.
            Availability::Unavailable { reason } => {
                assert_eq!(reason, "nothing is at that path yet (sunset-4k.png)");
            }
            Availability::Available => panic!("there is nothing at that path to copy"),
        }
    }

    #[test]
    fn a_removed_item_keeps_only_copy_path() {
        let mut p = preview();
        p.set_removed(&person_id(ANA), "2026-08-07T11:58:00Z");
        for action in
            [ItemAction::Apply, ItemAction::Favourite, ItemAction::Delete, ItemAction::Reveal]
        {
            match p.availability(action) {
                Availability::Unavailable { reason } => {
                    assert_eq!(reason, "this Item was removed from the Collection.");
                }
                Availability::Available => panic!("{action:?} must be refused after removal"),
            }
        }
        assert!(matches!(p.availability(ItemAction::CopyPath), Availability::Available));
    }

    // ── keys ─────────────────────────────────────────────────────────

    #[test]
    fn q_esc_and_enter_all_return_to_the_gallery() {
        let mut p = preview();
        for code in [KeyCode::Char('q'), KeyCode::Esc, KeyCode::Enter] {
            assert_eq!(p.handle_key(press(code)), Some(PreviewAction::Back));
        }
    }

    #[test]
    fn movement_reaches_the_adjacent_item_by_arrow_or_letter() {
        let mut p = preview();
        for code in [KeyCode::Char('j'), KeyCode::Char('l'), KeyCode::Down, KeyCode::Right] {
            assert_eq!(p.handle_key(press(code)), Some(PreviewAction::Next));
        }
        for code in [KeyCode::Char('k'), KeyCode::Char('h'), KeyCode::Up, KeyCode::Left] {
            assert_eq!(p.handle_key(press(code)), Some(PreviewAction::Previous));
        }
    }

    #[test]
    fn the_five_actions_are_bound_and_performable() {
        let mut p = preview();
        for (code, action) in [
            (KeyCode::Char('a'), ItemAction::Apply),
            (KeyCode::Char('f'), ItemAction::Favourite),
            (KeyCode::Char('d'), ItemAction::Delete),
            (KeyCode::Char('r'), ItemAction::Reveal),
            (KeyCode::Char('y'), ItemAction::CopyPath),
        ] {
            assert_eq!(p.handle_key(press(code)), Some(PreviewAction::Perform(action)));
        }
        assert_eq!(p.handle_key(press(KeyCode::Char(' '))), Some(PreviewAction::Menu));
    }

    #[test]
    fn a_refused_action_comes_back_with_its_reason_rather_than_silence() {
        let mut absent = item();
        absent.path = None;
        let mut p = Preview::new(&absent);
        assert_eq!(
            p.handle_key(press(KeyCode::Char('a'))),
            Some(PreviewAction::Unavailable {
                action: ItemAction::Apply,
                reason: "the bytes for this Item have not arrived yet".into(),
            })
        );
    }

    #[test]
    fn keys_this_screen_does_not_own_fall_through_to_the_global_layer() {
        let mut p = preview();
        // `?` help, `c` circles, `m` members, `!` detail, `F` the Gallery's filter.
        for code in [
            KeyCode::Char('?'),
            KeyCode::Char('c'),
            KeyCode::Char('m'),
            KeyCode::Char('!'),
            KeyCode::Char('F'),
            KeyCode::Char('z'),
        ] {
            assert_eq!(p.handle_key(press(code)), None, "{code:?} is not this screen's");
        }
    }

    #[test]
    fn ctrl_d_is_never_read_as_delete() {
        let mut p = preview();
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(p.handle_key(ctrl_d), None);
    }

    #[test]
    fn a_key_release_performs_nothing() {
        let mut p = preview();
        let mut release = press(KeyCode::Char('a'));
        release.kind = KeyEventKind::Release;
        assert_eq!(p.handle_key(release), None);
    }

    // ── the ladder ───────────────────────────────────────────────────

    #[test]
    fn the_rung_is_named_without_the_words_that_would_call_it_a_defect() {
        for rung in [Rung::Kitty, Rung::Iterm2, Rung::Sixel, Rung::Halfblocks] {
            let label = rung.label();
            for banned in ["unsupported", "error", "failed"] {
                assert!(!label.contains(banned), "{label} must not say {banned}");
            }
        }
        assert_eq!(Rung::Halfblocks.label(), "halfblocks (degraded)");
        assert_eq!(Rung::from_protocol(ProtocolType::Kitty), Rung::Kitty);
    }

    #[test]
    fn the_sixel_rung_leaves_the_last_row_blank() {
        let area = Rect::new(0, 0, 80, 20);
        assert_eq!(preview().with_rung(Rung::Sixel).content(area).height, 19);
        for rung in [Rung::Kitty, Rung::Iterm2, Rung::Halfblocks] {
            assert_eq!(preview().with_rung(rung).content(area).height, 20, "{rung:?}");
        }
    }

    #[test]
    fn nothing_is_painted_on_the_sixel_rungs_reserved_row() {
        let mut p = preview().with_rung(Rung::Sixel);
        p.set_pane(Pane::BytesAbsent);
        let buf = rendered(&mut p, 60, 18);
        assert_eq!(row(&buf, 17), "", "the last content row is left for the terminal");
        assert!(row(&buf, 16).contains("Attribution is what"));
    }

    // ── rendering, and the tier that must never fail ─────────────────

    #[test]
    fn the_text_card_carries_every_fact_when_there_are_no_pixels() {
        let mut p = preview();
        p.set_pane(Pane::Undecodable { note: None });
        let buf = rendered(&mut p, 70, 12);
        assert_eq!(row(&buf, 9), "  sunset-4k");
        assert_eq!(row(&buf, 10), "  added by Ana · today 09:14 · 3840×2160 · 1.9 MB · png");
        assert_eq!(row(&buf, 11), "  preview unavailable — these bytes are not a readable image");
        // The pane is a field with a marker in it, never a hole.
        let pane: String = (0..9).map(|y| row(&buf, y)).collect();
        assert!(pane.contains('▒') && pane.contains('!'));
    }

    #[test]
    fn one_row_of_content_still_names_the_item() {
        // The floor is enforced by the frame, not here; whatever it gives us, the
        // Item's name is the first thing drawn and the last thing dropped.
        let mut p = preview();
        let buf = rendered(&mut p, 40, 1);
        assert_eq!(row(&buf, 0), "  sunset-4k");
    }

    #[test]
    fn a_long_title_is_truncated_rather_than_overrunning_the_row() {
        let mut p = preview();
        p.item.title = "a-very-long-wallpaper-title-that-will-not-fit".into();
        let buf = rendered(&mut p, 20, 4);
        assert_eq!(row(&buf, 1), "  a-very-long-wallp…");
    }

    #[test]
    fn a_byte_less_item_renders_the_arrow_field_and_says_so() {
        let mut absent = item();
        absent.path = None;
        let mut p = Preview::new(&absent);
        p.zone = TimeZone::UTC;
        let buf = rendered(&mut p, 60, 10);
        let pane: String = (0..7).map(|y| row(&buf, y)).collect();
        assert!(pane.contains('▒') && pane.contains('↓'));
        assert_eq!(row(&buf, 7), "  ↓ sunset-4k");
        assert_eq!(row(&buf, 9), "  bytes not here yet");
    }

    #[test]
    fn pixels_render_on_the_bottom_rung_of_the_ladder() {
        // Halfblocks is the shipped fallback and the one rung that needs no
        // terminal to prove: the picture lands in the buffer as coloured cells.
        let picker = Picker::halfblocks();
        let image = image::DynamicImage::new_rgb8(16, 9);
        let mut p = preview().with_rung(Rung::Halfblocks);
        p.set_pane(Pane::Image(Box::new(picker.new_resize_protocol(image))));

        let buf = rendered(&mut p, 60, 12);
        let painted = buf.content.iter().filter(|c| c.symbol() != " ").count();
        assert!(painted > 0, "the pane must not be empty on the halfblocks rung");
        // And the facts are still there underneath it.
        assert_eq!(row(&buf, 9), "  sunset-4k");
    }

    #[test]
    fn the_placeholder_field_keeps_the_items_own_aspect() {
        let p = preview(); // 3840×2160 at the default 8×16 cell
        let field = p.letterbox(Rect::new(0, 0, 80, 20));
        assert!(field.width <= 80 && field.height <= 20);
        // Cells are twice as tall as they are wide, so 16:9 in pixels is 32:9 on
        // the grid: 480 × 135 cells, fitted to the pane by its tighter side.
        assert_eq!(field.height, 20);
        assert_eq!(field.width, 71);
        assert_eq!(field.x, 4, "centred");

        // With no facts to go on, the 16:9 wallpaper norm stands in, and it is
        // the same shape.
        let mut bare = preview();
        bare.set_facts(SidecarFacts::default());
        let norm = bare.letterbox(Rect::new(0, 0, 80, 20));
        assert_eq!((norm.width, norm.height), (field.width, field.height));
    }

    // ── formatting ───────────────────────────────────────────────────

    #[test]
    fn byte_sizes_are_si_with_one_decimal_below_ten() {
        assert_eq!(size_label(847), "847 B");
        assert_eq!(size_label(12_000), "12 kB");
        assert_eq!(size_label(1_900_000), "1.9 MB");
        assert_eq!(size_label(12_000_000), "12 MB");
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(999), "999 B");
        assert_eq!(size_label(1_000), "1.0 kB");
        assert_eq!(size_label(999_999), "1.0 MB", "carries rather than printing 1000 kB");
    }

    #[test]
    fn dates_are_relative_then_absolute() {
        let z = TimeZone::UTC;
        assert_eq!(when_label("2026-08-07T09:14:00Z", now(), &z, false), "today 09:14");
        assert_eq!(when_label("2026-08-06T21:03:00Z", now(), &z, false), "yesterday 21:03");
        assert_eq!(when_label("2026-08-04T09:14:00Z", now(), &z, false), "3 days ago");
        assert_eq!(when_label("2026-07-30T09:14:00Z", now(), &z, false), "30 Jul 2026");
    }

    #[test]
    fn a_skewed_clock_shows_the_claimed_date_marked() {
        let z = TimeZone::UTC;
        assert_eq!(when_label("2026-08-09T09:14:00Z", now(), &z, true), "dated 9 Aug 2026 (?)");
    }

    #[test]
    fn an_unparseable_stamp_is_shown_as_written_never_invented() {
        let z = TimeZone::UTC;
        assert_eq!(when_label("last tuesday", now(), &z, false), "last tuesday");
    }

    #[test]
    fn elapsed_time_reads_as_elapsed() {
        assert_eq!(ago_label("2026-08-07T11:58:00Z", now()), "2 minutes ago");
        assert_eq!(ago_label("2026-08-07T11:59:30Z", now()), "just now");
        assert_eq!(ago_label("2026-08-07T11:00:00Z", now()), "1 hour ago");
        assert_eq!(ago_label("2026-08-05T12:00:00Z", now()), "2 days ago");
    }

    #[test]
    fn truncation_marks_that_it_truncated() {
        assert_eq!(truncate("sunset", 10), "sunset");
        assert_eq!(truncate("sunset-4k", 6), "sunse…");
        assert_eq!(truncate("sunset", 0), "");
    }

    #[test]
    fn moving_to_the_next_item_carries_no_facts_over_from_the_last() {
        let mut p = preview();
        p.set_marks(Marks { favourite: true, unseen: false });
        p.set_removed(&person_id(ANA), NOW);

        let mut next = item();
        next.title = "dunes".into();
        next.added_by = person_id(BEN);
        p.show(&next);

        assert_eq!(p.title_line(), "dunes");
        assert_eq!(p.marks, Marks::default());
        assert!(p.removed.is_none());
        assert!(matches!(p.availability(ItemAction::Apply), Availability::Available));
        // The People and the terminal survive the move; they are not the Item's.
        assert!(p.fact_line(now()).starts_with("added by you · "));
    }
}
