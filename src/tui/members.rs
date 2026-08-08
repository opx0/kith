//! The Members screen and the pending-join prompt.
//!
//! The screen lists the People in a Circle with their Role and their Presence,
//! where Presence is this Device's own live view of one connection — never
//! "online" — and a Role is an agreement rather than an enforcement. Both
//! caveats are permanent footers.
//!
//! The pending-join prompt is the one real gate wallsync has: it is raised when a
//! Device knocks, shows the fingerprint grouped 4-4 for the out-of-band check,
//! states the consequence of admitting inline, and never auto-dismisses.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::domain::{PersonId, Presence, Role};
use crate::engine::{DeviceId, JoinRequest};

/// `roles.footer` — always visible on the Members screen, never collapsed.
pub const ROLES_FOOTER: &str = "Roles are an agreement, not a lock. Any Member can add or delete \
anything here; every other Device keeps 30 days of previous versions if they do.";

/// The one-line form, used when the screen is too short for the long one.
pub const ROLES_SHORT: &str =
    "Roles are agreements, not enforcement — admission is the only gate.";

/// Presence is pairwise and live, stated where the column is.
pub const PRESENCE_FOOTER: &str = "Presence is this Device's own view, right now. Someone shown \
as not connected may be connected to another Member.";

/// `roles.badge` — next to the admin.
pub const ADMIN_BADGE: &str = "invites and approves";

/// The consequence, stated inside the prompt where the key is pressed.
const ADMIT_CONSEQUENCE: &str = "Admitting adds this Device to {circle}. It receives every Item, \
and can add, change or delete Items — wallsync cannot prevent that, only restore. Approve People, \
not Devices.";

const IDENTIFY_BY_HAND: &str = "wallsync cannot tell you who this is. It sees a Device, not a Person. \
Ask your friend to read you the fingerprint their wallsync printed, and approve only if it matches.";

const UNINVITED_WARNING: &str = "If this is not the Device your friend read to you, you are \
admitting a stranger to every Item in this Circle — and nothing takes bytes back once they have \
arrived.";

/// One Member of a Circle, as the caller derives them from the Membership
/// claims, the Circle descriptor and the Sync Engine's view of the Circle.
#[derive(Clone, Debug)]
pub struct MemberView {
    /// From the claim with the newest `asserted`.
    pub display_name: String,
    /// Derived from the Circle's `founder_person`; there is no roles record.
    pub role: Role,
    pub presence: Presence,
    /// Percent of what this Device knows that their Device holds.
    pub in_sync: Option<u8>,
    /// When a Device of theirs last wrote its Membership claim — not a join date.
    pub asserted: String,
    /// Set once every claim carrying this Person has it.
    pub left_at: Option<String>,
    /// This Device's own Person; Presence renders `—`.
    pub is_you: bool,
    /// Their Device is the Circle's way in — `founder_device`.
    pub steward: bool,
    /// Whether the Sync Engine knows a Device of theirs to be in this Circle.
    pub in_circle: bool,
    /// v0.1 holds exactly one; plural so a second Device is one more claim.
    pub devices: Vec<String>,
}

impl MemberView {
    /// The fields no Member row can render without; everything else has an
    /// honest default.
    pub fn new(
        _person: PersonId,
        display_name: impl Into<String>,
        role: Role,
        presence: Presence,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            role,
            presence,
            in_sync: None,
            asserted: String::new(),
            left_at: None,
            is_you: false,
            steward: false,
            in_circle: true,
            devices: Vec::new(),
        }
    }
}

/// A Device in the Circle that no Membership claim names; never hidden, because
/// a Device receiving the Circle's bytes is a fact the Circle may see.
#[derive(Clone, Debug)]
pub struct UnclaimedDevice {
    pub device: DeviceId,
    /// The name that Device announced about itself — it can say anything.
    pub announced_name: String,
    pub presence: Presence,
    pub in_sync: Option<u8>,
}

/// A Device knocking at this Device, and what wallsync knows about why.
#[derive(Clone, Debug)]
pub struct PendingJoin {
    /// Named in the prompt's title; admitting to the wrong Circle is irreversible.
    pub circle_name: String,
    /// A Device Identity, an announced name and a first-seen time — the whole
    /// truth available, and there is no Person in it.
    pub request: JoinRequest,
    pub solicited: Solicited,
}

impl PendingJoin {
    /// The eight characters both People read to each other, grouped 4-4.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.request.device.0)
    }
}

/// Whether a knock was expected, per the invite window on the Steward's Device.
#[derive(Clone, Debug)]
pub enum Solicited {
    /// An invite window for this Circle is open.
    ByOpenInvite { issued_at: String, expires_at: String },
    /// A window existed and has closed. Approvable, behind a typed confirmation.
    ByClosedInvite { closed_at: String, reason: WindowClose },
    /// No window was ever opened, or the record was lost — which degrades to
    /// unsolicited, the safe and noisier answer.
    Unsolicited,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowClose {
    Expired,
    Spent,
    Superseded,
}

/// What the Person asked for; every variant is an act the caller performs.
#[derive(Clone, Debug)]
pub enum MembersAction {
    /// Admit this knocking Device — already confirmed by hand.
    Approve(JoinRequest),
    /// Hide this knock locally; there is no server to deliver a "no".
    Reject(JoinRequest),
    Invite,
    /// The caller raises the typed confirmation — leaving is confirmed by typing
    /// the Circle's name, not by `y`.
    Leave,
    /// A key whose action is unavailable here, carrying the reason ready for the
    /// status row.
    Unavailable(String),
}

/// The Members screen, with the pending-join prompt as its one overlay.
pub struct Members {
    circle: Option<String>,
    people: Vec<MemberView>,
    unclaimed: Vec<UnclaimedDevice>,
    pending: Vec<PendingJoin>,
    /// A banner for a Circle whose Stewardship is vacant, disputed or unknown,
    /// or for a Sync Engine wallsync cannot reach.
    notice: Option<String>,
    /// `founder_person` with no claim naming them yet, rendered as the id's short
    /// form and never as the `founder_device` Identity.
    unnamed_admin: Option<PersonId>,
    /// Why `[i]` is unavailable on this Device, when it is.
    invite_blocked: Option<String>,
    selected: usize,
    offset: usize,
    /// Rows the last render could show, so paging keys move by what was seen.
    view_height: usize,
    prompt: Option<Prompt>,
    /// The half-typed `gg` chord. No leader keys, no modes — this is the one.
    awaiting_g: bool,
}

struct Prompt {
    index: usize,
    /// `Some` once approving needs the fingerprint typed in full — a knock with
    /// no open window behind it.
    typed: Option<String>,
}

impl Members {
    /// `people` are the Members; `pending` are the Devices knocking at this
    /// Device, and any one of them raises the prompt immediately.
    pub fn new(people: Vec<MemberView>, pending: Vec<PendingJoin>) -> Self {
        let prompt = (!pending.is_empty()).then(|| Prompt { index: 0, typed: None });
        Self {
            circle: None,
            people,
            unclaimed: Vec::new(),
            pending,
            notice: None,
            unnamed_admin: None,
            invite_blocked: None,
            selected: 0,
            offset: 0,
            view_height: 1,
            prompt,
            awaiting_g: false,
        }
    }

    /// The Circle these Members belong to, for the header and the prompt title.
    pub fn with_circle(mut self, name: impl Into<String>) -> Self {
        self.circle = Some(name.into());
        self
    }

    /// Devices in the Circle that no Membership claim names.
    pub fn with_unclaimed(mut self, devices: Vec<UnclaimedDevice>) -> Self {
        self.unclaimed = devices;
        self
    }

    /// A header banner — `no admin`, `this Circle disagrees about who its admin
    /// is`, `waiting for the Circle's records`, or an unreachable Sync Engine.
    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }

    /// The Circle names an admin no Membership claim has named yet.
    pub fn with_unnamed_admin(mut self, person: PersonId) -> Self {
        self.unnamed_admin = Some(person);
        self
    }

    /// Grey `[i]` with its reason rather than hiding it.
    pub fn invite_unavailable(mut self, reason: impl Into<String>) -> Self {
        self.invite_blocked = Some(reason.into());
        self
    }

    /// A Device started knocking while this screen was open; a second request
    /// queues rather than stealing the keystroke meant for the first.
    pub fn push_pending(&mut self, join: PendingJoin) {
        self.pending.push(join);
        if self.prompt.is_none() {
            self.prompt = Some(Prompt { index: self.pending.len() - 1, typed: None });
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn is_prompt_open(&self) -> bool {
        self.prompt.is_some()
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width as usize;

        // Reserved first, so the list gives up rows before the caveats do.
        let footer = self.footer(width, area.height);
        let footer_h = footer.len().min(area.height as usize) as u16;
        let head = self.head(width);
        let head_h = head
            .len()
            .min(area.height.saturating_sub(footer_h + 1) as usize) as u16;

        let [head_area, list_area, footer_area] = Layout::vertical([
            Constraint::Length(head_h),
            Constraint::Min(0),
            Constraint::Length(footer_h),
        ])
        .areas(area);

        frame.render_widget(Paragraph::new(head), head_area);
        frame.render_widget(Paragraph::new(footer), footer_area);

        let (lines, selected_line) = self.body(width);
        self.view_height = list_area.height.max(1) as usize;
        self.offset = scroll(self.offset, selected_line, lines.len(), self.view_height);
        let window: Vec<Line<'static>> =
            lines.into_iter().skip(self.offset).take(self.view_height).collect();
        frame.render_widget(Paragraph::new(window), list_area);

        if let Some(prompt) = &self.prompt {
            self.render_prompt(frame, area, prompt);
        }
    }

    /// Header: the Circle, the counts, any banner, and the admin nobody can name.
    fn head(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        lines.push(Line::from(self.title()).style(Style::new().add_modifier(Modifier::BOLD)));

        if let Some(notice) = &self.notice {
            for line in wrap(notice, width) {
                lines.push(Line::from(line).style(Style::new().add_modifier(Modifier::BOLD)));
            }
        }
        if let Some(admin) = &self.unnamed_admin {
            // A PersonId alone is not a Member — no claim, no row — so the admin
            // is named here, as the id, never as the Steward's Device.
            let text = format!(
                "admin  unknown Person ({}) — no Membership claim names them yet",
                admin.short()
            );
            for line in wrap(&text, width) {
                lines.push(Line::from(line).style(dim()));
            }
        }
        if !self.pending.is_empty() && self.prompt.is_none() {
            let text = if self.pending.len() == 1 {
                "1 Device wants to join — enter on it to decide".to_string()
            } else {
                format!("{} Devices want to join — enter on one to decide", self.pending.len())
            };
            lines.push(Line::from(text).style(Style::new().add_modifier(Modifier::BOLD)));
        }
        lines
    }

    fn title(&self) -> String {
        let members = self.people.len();
        let others: Vec<&MemberView> = self.people.iter().filter(|m| !m.is_you).collect();
        let blind = !others.is_empty() && others.iter().all(|m| m.presence == Presence::Unknown);
        let connected = others.iter().filter(|m| m.presence == Presence::Connected).count();

        let mut title = match &self.circle {
            Some(name) => format!("{name} · "),
            None => String::new(),
        };
        title.push_str(&plural(members, "Member", "Members"));
        // `0 connected` would be a claim wallsync is in no position to make.
        if blind {
            title.push_str(" · presence unknown");
        } else {
            title.push_str(&format!(", {connected} connected"));
        }
        if !self.unclaimed.is_empty() {
            title.push_str(&format!(
                " · {} with no Membership claim",
                plural(self.unclaimed.len(), "Device", "Devices")
            ));
        }
        title
    }

    /// The permanent footer. Degrades in length, never in candour.
    fn footer(&self, width: usize, height: u16) -> Vec<Line<'static>> {
        let keys = self.keys_line();

        let mut full: Vec<Line<'static>> = Vec::new();
        for line in wrap(ROLES_FOOTER, width) {
            full.push(Line::from(line).style(dim()));
        }
        for line in wrap(PRESENCE_FOOTER, width) {
            full.push(Line::from(line).style(dim()));
        }
        full.push(keys.clone());
        if full.len() as u16 + 2 <= height {
            return full;
        }

        let mut medium: Vec<Line<'static>> = wrap(ROLES_SHORT, width)
            .into_iter()
            .map(|l| Line::from(l).style(dim()))
            .collect();
        medium.push(keys);
        if medium.len() as u16 + 1 <= height {
            return medium;
        }

        let mut minimal: Vec<Line<'static>> = wrap(ROLES_SHORT, width)
            .into_iter()
            .map(|l| Line::from(l).style(dim()))
            .collect();
        minimal.truncate(height.saturating_sub(1).max(1) as usize);
        minimal
    }

    fn keys_line(&self) -> Line<'static> {
        let mut parts = Vec::new();
        match &self.invite_blocked {
            Some(reason) => parts.push(format!("i invite · {}", first_clause(reason))),
            None => parts.push("i invite".to_string()),
        }
        parts.push("L leave circle".to_string());
        if !self.pending.is_empty() {
            parts.push("enter decide".to_string());
        }
        parts.push("esc back".to_string());
        Line::from(parts.join(" · ")).style(dim())
    }

    /// The list itself, plus which of its lines the selection sits on.
    fn body(&self, width: usize) -> (Vec<Line<'static>>, usize) {
        let cols = columns(width);
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut selected_line = 0usize;
        let mut row = 0usize;

        if !self.pending.is_empty() {
            lines.push(Line::from("Pending").style(dim()));
            for join in &self.pending {
                if row == self.selected {
                    selected_line = lines.len();
                }
                let text = format!(
                    "→ {}  {}  knocked {}",
                    join.fingerprint(),
                    announced(&join.request.name),
                    ago(&join.request.seen_at)
                );
                lines.push(style_row(text, row == self.selected, true));
                row += 1;
            }
            lines.push(Line::default());
            lines.push(Line::from("Members").style(dim()));
        }

        lines.push(Line::from(header_row(&cols)).style(dim()));

        if self.people.is_empty() && self.unclaimed.is_empty() {
            lines.push(Line::from(
                "No Membership claims have arrived yet. wallsync names nobody it has not been told about.",
            ));
            return (lines, selected_line);
        }

        for member in &self.people {
            if row == self.selected {
                selected_line = lines.len();
            }
            lines.push(style_row(member_row(member, &cols, width), row == self.selected, false));
            row += 1;
        }
        for device in &self.unclaimed {
            if row == self.selected {
                selected_line = lines.len();
            }
            lines.push(style_row(unclaimed_row(device, &cols), row == self.selected, false));
            row += 1;
        }
        (lines, selected_line)
    }

    fn render_prompt(&self, frame: &mut Frame, area: Rect, prompt: &Prompt) {
        let Some(join) = self.pending.get(prompt.index) else {
            return;
        };
        let outer_w = area.width.min(76).max(10);
        let inner_w = outer_w.saturating_sub(4) as usize;
        let body = prompt_body(join, prompt.typed.as_deref(), inner_w);
        let outer_h = (body.len() as u16 + 2).min(area.height);
        let rect = centred(area, outer_w, outer_h);

        let title = match (&prompt.typed, &join.solicited) {
            (Some(_), _) => format!(" Approving an uninvited Device · {} ", join.circle_name),
            (None, Solicited::Unsolicited) => {
                format!(" A Device wants to join {} — uninvited ", join.circle_name)
            }
            (None, _) => format!(" A Device wants to join {} ", join.circle_name),
        };

        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(body).block(
                Block::bordered()
                    .title(title)
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            ),
            rect,
        );
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<MembersAction> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // Keys route overlay → screen: the prompt shadows everything below it.
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        self.list_key(key)
    }

    fn prompt_key(&mut self, key: KeyEvent) -> Option<MembersAction> {
        let index = self.prompt.as_ref()?.index;
        if index >= self.pending.len() {
            self.prompt = None;
            return None;
        }
        let typing = self.prompt.as_ref()?.typed.is_some();

        if typing {
            let expected = self.pending[index].fingerprint();
            match key.code {
                KeyCode::Esc => {
                    // Back to the prompt, not out of it; nothing is decided.
                    if let Some(p) = self.prompt.as_mut() {
                        p.typed = None;
                    }
                }
                KeyCode::Backspace => {
                    if let Some(p) = self.prompt.as_mut()
                        && let Some(buf) = p.typed.as_mut()
                    {
                        buf.pop();
                    }
                }
                KeyCode::Enter => {
                    let typed = self
                        .prompt
                        .as_ref()
                        .and_then(|p| p.typed.clone())
                        .unwrap_or_default();
                    if squash(&typed) == squash(&expected) {
                        return Some(self.decide(index, true));
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(p) = self.prompt.as_mut()
                        && let Some(buf) = p.typed.as_mut()
                        && buf.chars().count() < 32
                    {
                        buf.push(c.to_ascii_uppercase());
                    }
                }
                _ => {}
            }
            return None;
        }

        match key.code {
            KeyCode::Char('a') | KeyCode::Char('A') => {
                match self.pending[index].solicited {
                    // An expected knock is one key; anything else asks for the
                    // fingerprint in full, because friction here is the feature.
                    Solicited::ByOpenInvite { .. } => Some(self.decide(index, true)),
                    _ => {
                        if let Some(p) = self.prompt.as_mut() {
                            p.typed = Some(String::new());
                        }
                        None
                    }
                }
            }
            // Both spellings of reject are accepted; only `x` is printed.
            KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Char('r') | KeyCode::Char('R') => {
                Some(self.decide(index, false))
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                // Decide later: a prompt that punishes hesitation trains people
                // to press yes. The request stays pinned to this screen.
                self.prompt = None;
                None
            }
            _ => None,
        }
    }

    /// Take the decided knock out of the pending list and hand it to the caller,
    /// raising the prompt on the next one if there is one.
    fn decide(&mut self, index: usize, approve: bool) -> MembersAction {
        let join = self.pending.remove(index);
        self.prompt = (!self.pending.is_empty())
            .then(|| Prompt { index: index.min(self.pending.len() - 1), typed: None });
        self.clamp();
        if approve {
            MembersAction::Approve(join.request)
        } else {
            MembersAction::Reject(join.request)
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> Option<MembersAction> {
        let rows = self.row_count();
        let page = self.view_height.max(1);

        // The `gg` chord is the only chord wallsync has; any other key ends it.
        let was_awaiting_g = self.awaiting_g;
        self.awaiting_g = false;

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    self.move_by(page as isize / 2);
                    return None;
                }
                KeyCode::Char('u') => {
                    self.move_by(-(page as isize / 2));
                    return None;
                }
                _ => return None,
            }
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_by(1),
            KeyCode::Char('h') | KeyCode::Left => self.move_by(-1),
            KeyCode::PageDown => self.move_by(page as isize),
            KeyCode::PageUp => self.move_by(-(page as isize)),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = rows.saturating_sub(1),
            KeyCode::Char('G') => self.selected = rows.saturating_sub(1),
            KeyCode::Char('g') => {
                if was_awaiting_g {
                    self.selected = 0;
                } else {
                    self.awaiting_g = true;
                }
            }
            KeyCode::Enter => {
                // Enter decides a knock; there is no Member detail screen, so on
                // a Member row it does nothing.
                if self.selected < self.pending.len() {
                    self.prompt = Some(Prompt { index: self.selected, typed: None });
                }
            }
            KeyCode::Char('i') => {
                return Some(match &self.invite_blocked {
                    Some(reason) => MembersAction::Unavailable(reason.clone()),
                    None => MembersAction::Invite,
                });
            }
            KeyCode::Char('L') => return Some(MembersAction::Leave),
            _ => {}
        }
        None
    }

    fn move_by(&mut self, delta: isize) {
        let rows = self.row_count() as isize;
        if rows == 0 {
            self.selected = 0;
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, rows - 1);
        self.selected = next as usize;
    }

    fn row_count(&self) -> usize {
        self.pending.len() + self.people.len() + self.unclaimed.len()
    }

    fn clamp(&mut self) {
        let rows = self.row_count();
        if self.selected >= rows {
            self.selected = rows.saturating_sub(1);
        }
    }
}

struct Cols {
    name: usize,
    role: usize,
    presence: usize,
    sync: usize,
    /// Room left for the trailing note after the fixed columns.
    trailing: usize,
}

/// Columns shrink from the right as the terminal narrows; Presence goes last.
fn columns(width: usize) -> Cols {
    let (name, role, presence, sync) = if width >= 58 {
        (17, 8, 15, 8)
    } else if width >= 40 {
        (14, 8, 15, 0)
    } else {
        (12, 0, 14, 0)
    };
    let used = name + role + presence + sync;
    Cols { name, role, presence, sync, trailing: width.saturating_sub(used) }
}

fn header_row(cols: &Cols) -> String {
    let mut row = cell("MEMBER", cols.name);
    row.push_str(&cell("ROLE", cols.role));
    row.push_str(&cell("PRESENCE", cols.presence));
    row.push_str(&cell("IN SYNC", cols.sync));
    if cols.trailing >= 8 {
        row.push_str("ASSERTED");
    }
    row.trim_end().to_string()
}

fn member_row(member: &MemberView, cols: &Cols, width: usize) -> String {
    let mut name = member.display_name.clone();
    if member.is_you {
        name.push_str(" (you)");
    }
    let mut row = cell(&name, cols.name);
    row.push_str(&cell(role_word(member.role), cols.role));

    // A Member whose Devices are not in the Circle has nothing to report; the row
    // says which case it is rather than printing a figure it would have to invent.
    let (presence, sync, note) = if let Some(left) = &member.left_at {
        ("—".to_string(), "—".to_string(), format!("left · {}", short_date(left)))
    } else if !member.in_circle {
        ("—".to_string(), "—".to_string(), "not in this Circle".to_string())
    } else if member.is_you {
        // wallsync holds no connection to itself; `—` is the honest column.
        ("—".to_string(), "—".to_string(), asserted_note(member, cols, width))
    } else {
        (
            presence_word(member.presence).to_string(),
            member.in_sync.map_or("—".to_string(), |p| format!("{p}%")),
            asserted_note(member, cols, width),
        )
    };

    row.push_str(&cell(&presence, cols.presence));
    row.push_str(&cell(&sync, cols.sync));
    row.push_str(&note);
    trim_to(&row, width)
}

/// The trailing note on a Member's row: when their claim was last asserted, plus
/// the badges where the terminal is wide enough for them.
fn asserted_note(member: &MemberView, cols: &Cols, width: usize) -> String {
    let mut note = String::new();
    if cols.trailing >= 6 && !member.asserted.is_empty() {
        note.push_str(&short_date(&member.asserted));
    }
    let mut badges = Vec::new();
    if member.role == Role::Admin {
        badges.push(ADMIN_BADGE.to_string());
    }
    if member.steward {
        badges.push("this Circle's way in".to_string());
    }
    if !badges.is_empty() && width >= 74 {
        if !note.is_empty() {
            note.push_str("  ");
        }
        note.push_str(&badges.join(" · "));
    }
    note
}

fn unclaimed_row(device: &UnclaimedDevice, cols: &Cols) -> String {
    let mut row = cell(&format!("· {}", fingerprint(&device.device.0)), cols.name);
    // No claim, no Person, therefore no Role. The dash says so.
    row.push_str(&cell("—", cols.role));
    row.push_str(&cell(presence_word(device.presence), cols.presence));
    row.push_str(&cell(&device.in_sync.map_or("—".to_string(), |p| format!("{p}%")), cols.sync));
    row.push_str("no Membership claim yet");
    if !device.announced_name.trim().is_empty() {
        row.push_str(&format!(" · calls itself \"{}\"", device.announced_name.trim()));
    }
    row
}

/// Never "online" or "offline", and `unknown` is a real answer.
fn presence_word(presence: Presence) -> &'static str {
    match presence {
        Presence::Connected => "connected",
        Presence::NotConnected => "not connected",
        Presence::Unknown => "unknown",
    }
}

fn role_word(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

/// A name a Device announced, worded so it is never read as a Person's name.
fn announced(name: &str) -> String {
    if name.trim().is_empty() {
        "(this Device announced no name)".to_string()
    } else {
        format!("\"{}\"", name.trim())
    }
}

fn style_row(text: String, selected: bool, pending: bool) -> Line<'static> {
    let mut style = Style::new();
    if pending {
        style = style.add_modifier(Modifier::BOLD);
    }
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::from(text).style(style)
}

/// Everything the Steward is given to decide with, and nothing wallsync cannot see.
fn prompt_body(join: &PendingJoin, typed: Option<&str>, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let fp = join.fingerprint();

    if let Some(buf) = typed {
        let lead = match &join.solicited {
            Solicited::Unsolicited => format!(
                "Nobody has been invited to {}. This Device is asking anyway.",
                join.circle_name
            ),
            _ => format!(
                "The invite that expected a knock on {} has closed.",
                join.circle_name
            ),
        };
        push_wrapped(&mut out, &lead, width, Style::new().add_modifier(Modifier::BOLD));
        out.push(Line::default());
        push_wrapped(&mut out, UNINVITED_WARNING, width, Style::new());
        out.push(Line::default());
        out.push(Line::from(format!("Type the fingerprint to confirm:  {fp}")));
        out.push(Line::from(format!("  ▏{buf}")));
        if !buf.is_empty() && !squash(&fp).starts_with(&squash(buf)) {
            push_wrapped(
                &mut out,
                "That is not the fingerprint above. Check it with them again.",
                width,
                Style::new(),
            );
        }
        out.push(Line::default());
        out.push(Line::from("enter confirm · esc back").style(dim()));
        return out;
    }

    // Unsolicited leads with the warning, not with the Device.
    if matches!(join.solicited, Solicited::Unsolicited) {
        push_wrapped(
            &mut out,
            &format!(
                "You have not invited anyone to {}. A Device is asking to join anyway.",
                join.circle_name
            ),
            width,
            Style::new().add_modifier(Modifier::BOLD),
        );
        out.push(Line::default());
    }

    out.push(Line::from(format!("Name         {}", announced(&join.request.name))));
    push_wrapped(
        &mut out,
        "             announced by that Device — it can say anything",
        width,
        dim(),
    );
    out.push(Line::from(format!("Fingerprint  {fp}")));
    out.push(Line::from(format!("First seen   {}", ago(&join.request.seen_at))));
    out.push(Line::from(format!("Invite       {}", invite_line(&join.solicited))));
    out.push(Line::default());
    push_wrapped(&mut out, IDENTIFY_BY_HAND, width, Style::new());
    out.push(Line::default());
    push_wrapped(
        &mut out,
        &ADMIT_CONSEQUENCE.replace("{circle}", &join.circle_name),
        width,
        Style::new(),
    );
    out.push(Line::default());

    let keys = match join.solicited {
        Solicited::ByOpenInvite { .. } => "a approve · x reject · esc decide later".to_string(),
        _ => "a approve (you will be asked to type the fingerprint) · x reject · esc decide later"
            .to_string(),
    };
    push_wrapped(&mut out, &keys, width, dim());
    out
}

fn invite_line(solicited: &Solicited) -> String {
    match solicited {
        Solicited::ByOpenInvite { issued_at, expires_at } => {
            format!("open, issued {}, expires {}", ago(issued_at), remaining(expires_at))
        }
        Solicited::ByClosedInvite { closed_at, reason } => match reason {
            WindowClose::Expired => format!("expired {}", ago(closed_at)),
            WindowClose::Spent => format!("already used, {}", ago(closed_at)),
            WindowClose::Superseded => format!("replaced by a newer one {}", ago(closed_at)),
        },
        Solicited::Unsolicited => "none — you have not invited anyone to this Circle".to_string(),
    }
}

/// The first eight characters of a Device Identity, grouped 4-4: `UJZD-EGXD`.
///
/// Forty bits: enough to catch a transcription error against a known-good
/// source, not enough to resist a deliberately ground matching prefix.
pub fn fingerprint(device: &str) -> String {
    let squashed = squash(device);
    let head: String = squashed.chars().take(8).collect();
    if head.chars().count() <= 4 {
        head
    } else {
        let split: Vec<char> = head.chars().collect();
        format!(
            "{}-{}",
            split[..4].iter().collect::<String>(),
            split[4..].iter().collect::<String>()
        )
    }
}

/// Uppercase, alphanumerics only — hyphens and case are never the difference
/// between what wallsync printed and what a Person types back.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for line in wrap(text, width) {
        out.push(Line::from(line).style(style));
    }
}

/// Word wrap, keeping any leading indent of the source text.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let mut out = Vec::new();
    let mut line = indent.clone();
    let mut empty = true;
    for word in text.split_whitespace() {
        let candidate = line.chars().count() + if empty { 0 } else { 1 } + word.chars().count();
        if empty {
            line.push_str(word);
            empty = false;
        } else if candidate <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(&indent);
            line.push_str(word);
        }
    }
    if !empty {
        out.push(line);
    }
    out
}

/// Pad or truncate to a fixed column, with a trailing space so columns never run
/// together. Truncation is marked.
fn cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let n = text.chars().count();
    if n < width {
        let mut s = text.to_string();
        s.extend(std::iter::repeat_n(' ', width - n));
        s
    } else if n == width {
        format!("{text} ")
    } else {
        let mut s: String = text.chars().take(width.saturating_sub(2)).collect();
        s.push('…');
        s.push(' ');
        s
    }
}

fn trim_to(text: &str, width: usize) -> String {
    if text.chars().count() <= width || width == 0 {
        text.trim_end().to_string()
    } else {
        let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("{n} {one}") } else { format!("{n} {many}") }
}

/// The first sentence of a reason, for a hints row that cannot hold all of it.
fn first_clause(reason: &str) -> String {
    reason.split(['.', '—']).next().unwrap_or(reason).trim().to_string()
}

/// Keep the selected line in view, moving the window by as little as possible.
fn scroll(offset: usize, selected: usize, total: usize, height: usize) -> usize {
    let height = height.max(1);
    let max_offset = total.saturating_sub(height);
    let mut offset = offset.min(max_offset);
    if selected < offset {
        offset = selected;
    } else if selected >= offset + height {
        offset = selected + 1 - height;
    }
    offset
}

fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn seconds_since(at: &str) -> Option<i64> {
    let then = at.parse::<jiff::Timestamp>().ok()?;
    Some(jiff::Timestamp::now().as_second() - then.as_second())
}

fn human_duration(seconds: i64) -> String {
    let s = seconds.max(0);
    match s {
        0..=89 => plural(s as usize, "second", "seconds"),
        90..=5399 => plural((s / 60) as usize, "minute", "minutes"),
        5400..=86399 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => plural((s / 86400) as usize, "day", "days"),
    }
}

fn ago(at: &str) -> String {
    match seconds_since(at) {
        Some(s) if s < 0 => "just now".to_string(),
        Some(s) => format!("{} ago", human_duration(s)),
        // Shown as written rather than replaced by a guess.
        None => at.to_string(),
    }
}

fn remaining(at: &str) -> String {
    match seconds_since(at) {
        Some(s) if s <= 0 => format!("in {}", human_duration(-s)),
        Some(s) => format!("{} ago — expired", human_duration(s)),
        None => at.to_string(),
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn short_date(at: &str) -> String {
    match at.parse::<jiff::Timestamp>() {
        Ok(t) => {
            let date = t.to_zoned(jiff::tz::TimeZone::system()).date();
            let month = MONTHS[(date.month() as usize).clamp(1, 12) - 1];
            format!("{} {month}", date.day())
        }
        Err(_) => at.chars().take(10).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn person(name: &str, role: Role, presence: Presence) -> MemberView {
        MemberView {
            asserted: jiff::Timestamp::now().to_string(),
            in_sync: Some(100),
            devices: vec!["AAAABBB-CCCCDDD".to_string()],
            ..MemberView::new(PersonId::generate(), name, role, presence)
        }
    }

    fn knock(name: &str, solicited: Solicited) -> PendingJoin {
        PendingJoin {
            circle_name: "walls".to_string(),
            request: JoinRequest {
                device: DeviceId("UJZDEGXD-4ZLNKXU-UVRBZOU".to_string()),
                name: name.to_string(),
                seen_at: jiff::Timestamp::now().to_string(),
            },
            solicited,
        }
    }

    fn open_invite() -> Solicited {
        let now = jiff::Timestamp::now();
        Solicited::ByOpenInvite {
            issued_at: now.to_string(),
            expires_at: (now + std::time::Duration::from_secs(86_000)).to_string(),
        }
    }

    fn draw(screen: &mut Members, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                screen.render(frame, area);
            })
            .expect("draw");
        text_of(terminal.backend().buffer())
    }

    fn text_of(buffer: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn fingerprint_is_grouped_four_and_four() {
        assert_eq!(fingerprint("UJZDEGXD4ZLNKXU5"), "UJZD-EGXD");
        assert_eq!(fingerprint("ujzd-egxd-4zln"), "UJZD-EGXD");
        assert_eq!(fingerprint("AB"), "AB");
    }

    #[test]
    fn presence_is_never_the_word_online() {
        for presence in [Presence::Connected, Presence::NotConnected, Presence::Unknown] {
            let word = presence_word(presence);
            assert!(!word.contains("online"), "{word}");
            assert!(!word.contains("offline"), "{word}");
            assert!(!word.contains("seen"), "{word}");
        }
        assert_eq!(presence_word(Presence::Unknown), "unknown");
    }

    #[test]
    fn this_device_claims_no_connection_to_itself() {
        let you = MemberView { is_you: true, ..person("Ana", Role::Admin, Presence::Connected) };
        let row = member_row(&you, &columns(80), 80);
        assert!(row.starts_with("Ana (you)"), "{row}");
        assert!(!row.contains("connected"), "presence to self is `—`, not connected: {row}");
        assert!(row.contains('—'), "{row}");
    }

    #[test]
    fn an_unclaimed_device_is_rendered_not_hidden() {
        let device = UnclaimedDevice {
            device: DeviceId("UJZDEGXD-4ZLNKXU".to_string()),
            announced_name: "ben-thinkpad".to_string(),
            presence: Presence::Connected,
            in_sync: Some(62),
        };
        let screen = Members::new(vec![], vec![]).with_unclaimed(vec![device]);
        let (lines, _) = screen.body(80);
        let rendered: String = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("UJZD-EGXD"), "{rendered}");
        assert!(rendered.contains("no Membership claim yet"), "{rendered}");
        assert!(rendered.contains("calls itself \"ben-thinkpad\""), "{rendered}");
        assert_eq!(screen.row_count(), 1, "an unclaimed Device is a row a key can reach");
    }

    #[test]
    fn a_member_who_left_is_not_reported_as_not_connected() {
        let left = MemberView {
            left_at: Some("2026-08-03T10:00:00Z".to_string()),
            in_circle: false,
            ..person("Cara", Role::Member, Presence::NotConnected)
        };
        let row = member_row(&left, &columns(80), 80);
        assert!(row.contains("left · 3 Aug"), "{row}");
        assert!(!row.contains("not connected"), "left is not a presence claim: {row}");
    }

    #[test]
    fn a_member_with_no_device_in_the_circle_says_so() {
        let absent = MemberView { in_circle: false, ..person("Dee", Role::Member, Presence::Unknown) };
        let row = member_row(&absent, &columns(80), 80);
        assert!(row.contains("not in this Circle"), "{row}");
    }

    #[test]
    fn the_role_caveat_is_in_the_screen_copy() {
        let mut screen = Members::new(vec![person("Ben", Role::Member, Presence::Connected)], vec![])
            .with_circle("walls");
        let rendered = draw(&mut screen, 80, 20);
        assert!(rendered.contains("Roles are an agreement, not a lock"), "{rendered}");
        assert!(rendered.contains("Presence is this Device's own view"), "{rendered}");
        assert!(!rendered.contains("online"), "{rendered}");
    }

    #[test]
    fn the_caveat_survives_a_short_screen_even_when_the_list_does_not() {
        let mut screen = Members::new(
            (0..12).map(|_| person("Ben", Role::Member, Presence::Connected)).collect(),
            vec![],
        );
        let rendered = draw(&mut screen, 70, 6);
        assert!(
            rendered.contains("Roles are agreements, not enforcement"),
            "the short form is a shorter concession, not a dropped one: {rendered}"
        );
    }

    #[test]
    fn the_header_never_counts_connections_it_cannot_see() {
        let screen = Members::new(
            vec![
                MemberView { is_you: true, ..person("Ana", Role::Admin, Presence::Unknown) },
                person("Ben", Role::Member, Presence::Unknown),
            ],
            vec![],
        )
        .with_circle("walls");
        let title = screen.title();
        assert!(title.contains("presence unknown"), "{title}");
        assert!(!title.contains("0 connected"), "unknown is not zero: {title}");
    }

    #[test]
    fn the_header_counts_connections_it_can_see() {
        let screen = Members::new(
            vec![
                MemberView { is_you: true, ..person("Ana", Role::Admin, Presence::Unknown) },
                person("Ben", Role::Member, Presence::Connected),
                person("Cara", Role::Member, Presence::NotConnected),
            ],
            vec![],
        )
        .with_circle("walls");
        assert_eq!(screen.title(), "walls · 3 Members, 1 connected");
    }

    #[test]
    fn an_admin_nobody_can_name_is_the_person_id_never_the_device() {
        let admin = PersonId::generate();
        let mut screen = Members::new(vec![], vec![])
            .with_circle("walls")
            .with_unnamed_admin(admin.clone());
        let rendered = draw(&mut screen, 80, 20);
        assert!(rendered.contains("unknown Person"), "{rendered}");
        assert!(rendered.contains(admin.short()), "{rendered}");
    }

    #[test]
    fn a_knock_raises_the_prompt_by_itself() {
        let screen = Members::new(vec![], vec![knock("ben-thinkpad", open_invite())]);
        assert!(screen.is_prompt_open(), "admission is what this screen is for");
    }

    #[test]
    fn the_prompt_shows_the_fingerprint_and_the_consequence() {
        let mut screen = Members::new(vec![], vec![knock("ben-thinkpad", open_invite())])
            .with_circle("walls");
        let rendered = draw(&mut screen, 80, 30);
        assert!(rendered.contains("UJZD-EGXD"), "{rendered}");
        assert!(rendered.contains("it can say anything"), "{rendered}");
        assert!(rendered.contains("wallsync cannot prevent that"), "{rendered}");
        assert!(rendered.contains("a approve"), "{rendered}");
        assert!(rendered.contains("x reject"), "{rendered}");
        assert!(rendered.contains("esc decide later"), "{rendered}");
    }

    #[test]
    fn an_expected_knock_is_approved_with_one_key() {
        let mut screen = Members::new(vec![], vec![knock("ben-thinkpad", open_invite())]);
        match screen.handle_key(key(KeyCode::Char('a'))) {
            Some(MembersAction::Approve(request)) => {
                assert_eq!(request.name, "ben-thinkpad");
            }
            other => panic!("expected an approval, got {other:?}"),
        }
        assert_eq!(screen.pending_count(), 0);
        assert!(!screen.is_prompt_open());
    }

    #[test]
    fn an_uninvited_knock_asks_for_the_fingerprint_in_full() {
        let mut screen = Members::new(vec![], vec![knock("stranger", Solicited::Unsolicited)]);
        assert!(screen.handle_key(key(KeyCode::Char('a'))).is_none(), "no window, no one key");

        for c in "AAAABBBB".chars() {
            assert!(screen.handle_key(key(KeyCode::Char(c))).is_none());
        }
        assert!(screen.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(screen.pending_count(), 1);

        for _ in 0..8 {
            screen.handle_key(key(KeyCode::Backspace));
        }
        // Typed as it is read aloud: lower case, hyphen and all.
        for c in "ujzd-egxd".chars() {
            screen.handle_key(key(KeyCode::Char(c)));
        }
        assert!(matches!(
            screen.handle_key(key(KeyCode::Enter)),
            Some(MembersAction::Approve(_))
        ));
    }

    #[test]
    fn the_uninvited_prompt_leads_with_the_warning() {
        let join = knock("stranger", Solicited::Unsolicited);
        let body: String = prompt_body(&join, None, 70)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let first = body.lines().next().unwrap_or_default();
        assert!(first.contains("have not invited anyone"), "{body}");
        assert!(body.contains("none — you have not invited anyone"), "{body}");
    }

    #[test]
    fn rejecting_is_one_key_and_tells_the_device_nothing() {
        let mut screen = Members::new(vec![], vec![knock("ben-thinkpad", open_invite())]);
        assert!(matches!(
            screen.handle_key(key(KeyCode::Char('x'))),
            Some(MembersAction::Reject(_))
        ));
        assert_eq!(screen.pending_count(), 0, "the knock is hidden here, not answered");
    }

    #[test]
    fn esc_leaves_the_knock_pinned_to_the_screen() {
        let mut screen = Members::new(vec![], vec![knock("ben-thinkpad", open_invite())]);
        assert!(screen.handle_key(key(KeyCode::Esc)).is_none());
        assert!(!screen.is_prompt_open());
        assert_eq!(screen.pending_count(), 1, "hesitating decides nothing");

        assert!(screen.handle_key(key(KeyCode::Enter)).is_none());
        assert!(screen.is_prompt_open());
    }

    #[test]
    fn enter_on_a_member_row_opens_nothing() {
        let mut screen = Members::new(vec![person("Ben", Role::Member, Presence::Connected)], vec![]);
        assert!(screen.handle_key(key(KeyCode::Enter)).is_none());
        assert!(!screen.is_prompt_open(), "there is no Member detail screen in v0.1");
    }

    #[test]
    fn invite_is_refused_with_its_reason_where_it_is_refused() {
        let reason = "Only walls's admin (Ana) can invite people or approve joins.";
        let mut screen = Members::new(vec![], vec![]).invite_unavailable(reason);
        match screen.handle_key(key(KeyCode::Char('i'))) {
            Some(MembersAction::Unavailable(said)) => assert_eq!(said, reason),
            other => panic!("expected the reason, got {other:?}"),
        }
        let rendered = draw(&mut screen, 80, 20);
        assert!(rendered.contains("i invite · Only walls's admin (Ana) can"), "{rendered}");
    }

    #[test]
    fn invite_and_leave_are_the_two_acts_this_screen_offers() {
        let mut screen = Members::new(vec![person("Ana", Role::Admin, Presence::Unknown)], vec![]);
        assert!(matches!(screen.handle_key(key(KeyCode::Char('i'))), Some(MembersAction::Invite)));
        assert!(matches!(screen.handle_key(key(KeyCode::Char('L'))), Some(MembersAction::Leave)));
    }

    #[test]
    fn movement_stays_inside_the_list() {
        let mut screen = Members::new(
            vec![
                person("Ana", Role::Admin, Presence::Unknown),
                person("Ben", Role::Member, Presence::Connected),
            ],
            vec![],
        );
        screen.handle_key(key(KeyCode::Up));
        assert_eq!(screen.selected, 0);
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(screen.selected, 1);
        screen.handle_key(key(KeyCode::Char('g')));
        screen.handle_key(key(KeyCode::Char('g')));
        assert_eq!(screen.selected, 0, "gg is the one chord");
        screen.handle_key(key(KeyCode::Char('G')));
        assert_eq!(screen.selected, 1);
    }

    #[test]
    fn a_second_knock_queues_rather_than_stealing_the_keystroke() {
        let mut screen = Members::new(vec![], vec![knock("first", open_invite())]);
        screen.push_pending(knock("second", open_invite()));
        assert_eq!(screen.pending_count(), 2);
        match screen.handle_key(key(KeyCode::Char('a'))) {
            Some(MembersAction::Approve(request)) => assert_eq!(request.name, "first"),
            other => panic!("the open prompt decides its own Device, got {other:?}"),
        }
        assert!(screen.is_prompt_open(), "the next knock is raised in turn");
    }

    #[test]
    fn a_knock_arriving_on_a_closed_prompt_raises_it() {
        let mut screen = Members::new(vec![], vec![]);
        assert!(!screen.is_prompt_open());
        screen.push_pending(knock("ben-thinkpad", open_invite()));
        assert!(screen.is_prompt_open());
    }

    #[test]
    fn a_closed_window_is_named_by_what_closed_it() {
        let closed = |reason| Solicited::ByClosedInvite {
            closed_at: "2026-08-05T10:00:00Z".to_string(),
            reason,
        };
        assert!(invite_line(&closed(WindowClose::Expired)).starts_with("expired"));
        assert!(invite_line(&closed(WindowClose::Spent)).starts_with("already used"));
        assert!(invite_line(&closed(WindowClose::Superseded)).starts_with("replaced by a newer one"));
    }

    #[test]
    fn a_timestamp_wallsync_cannot_read_is_shown_as_written() {
        assert_eq!(ago("yesterday-ish"), "yesterday-ish");
        assert_eq!(short_date("some time last Tuesday"), "some time ");
    }

    #[test]
    fn rendering_survives_a_terminal_too_small_to_be_useful() {
        let mut screen = Members::new(
            vec![person("Ben", Role::Member, Presence::Connected)],
            vec![knock("ben-thinkpad", open_invite())],
        )
        .with_circle("walls");
        for (w, h) in [(1, 1), (8, 3), (20, 4), (40, 2)] {
            let _ = draw(&mut screen, w, h);
        }
    }

    #[test]
    fn the_notice_banner_is_rendered_above_the_list() {
        let mut screen = Members::new(vec![], vec![])
            .with_circle("walls")
            .with_notice("Sync Engine unreachable — wallsync cannot see anyone right now.");
        let rendered = draw(&mut screen, 80, 20);
        assert!(rendered.contains("wallsync cannot see anyone right now"), "{rendered}");
    }
}
