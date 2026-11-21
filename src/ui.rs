use crate::{
    call::{Call, Message},
    config::{self, Room, valid},
    lobby::Lobby,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

struct TextEdit {
    text: String,
    cursor: usize,
    limit: usize,
    error: String,
}

impl Default for TextEdit {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl TextEdit {
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            limit,
            error: String::new(),
        }
    }
    fn boundary_at_or_after(&self, byte: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .map(|(i, _)| i)
            .find(|&i| i >= byte)
            .unwrap_or(self.text.len())
    }

    fn insert(&mut self, text: &str) {
        if text.chars().any(char::is_control) {
            self.error = "Use one line without control characters.".into();
        } else if self.text.len() + text.len() > self.limit {
            self.error = format!("Input exceeds {} bytes.", self.limit);
        } else {
            self.error.clear();
            self.text.insert_str(self.cursor, text);
            self.cursor = self.boundary_at_or_after(self.cursor + text.len());
        }
    }

    fn key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char(ch) => self.insert(ch.encode_utf8(&mut [0; 4])),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            KeyCode::Left | KeyCode::Backspace => {
                let prev = self.text[..self.cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(i, _)| i);
                if key == KeyCode::Backspace {
                    self.error.clear();
                    self.text.drain(prev..self.cursor);
                }
                self.cursor = self.boundary_at_or_after(prev);
            }
            KeyCode::Right | KeyCode::Delete => {
                let next = self.boundary_at_or_after(self.cursor + 1);
                if key == KeyCode::Delete {
                    self.error.clear();
                    self.text.drain(self.cursor..next);
                    self.cursor = self.boundary_at_or_after(self.cursor);
                } else {
                    self.cursor = next;
                }
            }
            _ => {}
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let mut column = 0;
        let mut start = self.cursor;
        for (i, g) in self.text[..self.cursor].grapheme_indices(true).rev() {
            let width = g.width() as u16;
            if column + width >= area.width {
                break;
            }
            column += width;
            start = i;
        }
        frame.render_widget(Paragraph::new(&self.text[start..]), area);
        frame.set_cursor_position((area.x + column, area.y));
    }
}

#[derive(Default)]
struct View {
    focus: Pane,
    chat_input: TextEdit,
    chat_scroll: u16,
    room: usize,
    selected: LevelRow,
    level_rows: Vec<LevelRow>,
    level_area: Rect,
    level_offset: usize,
    draft: Option<TextEdit>,
    form: Option<Form>,
    notice: String,
}

struct PeerRow {
    id: iroh::EndpointId,
    name: String,
    late: bool,
    muted: bool,
    local_mute: bool,
    volume: f32,
    last_speech: Option<Instant>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum LevelRow {
    #[default]
    Output,
    Input,
    Peer(iroh::EndpointId),
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Pane {
    #[default]
    Sessions,
    People,
    Chat,
}

fn direction(key: KeyCode) -> Option<bool> {
    match key {
        KeyCode::Char('h' | 'k') | KeyCode::Left | KeyCode::Up => Some(false),
        KeyCode::Char('j' | 'l') | KeyCode::Down | KeyCode::Right => Some(true),
        _ => None,
    }
}

enum Form {
    Name,
    RoomName,
    Import,
    Share(String),
    Delete,
    Leave,
}

impl Form {
    fn title(&self) -> &str {
        match self {
            Self::Name => "Welcome! Enter your display name",
            Self::RoomName => "New session: enter a name",
            Self::Import => "Paste a private room invite",
            Self::Share(_) => "Secret invite — share privately",
            Self::Delete => "Delete session?",
            Self::Leave => "Leave call?",
        }
    }
}

fn indicator(muted: bool, speech: Option<Instant>) -> &'static str {
    if muted {
        "[mic muted]"
    } else if speech.is_some_and(|at| at.elapsed() < Duration::from_millis(300)) {
        "[talking]"
    } else {
        ""
    }
}

fn adjust(volume: &mut f32, up: bool, max: f32) -> bool {
    let old = *volume;
    *volume = (((*volume * 10.0).round() + if up { 1.0 } else { -1.0 }) / 10.0).clamp(0.0, max);
    old != *volume
}

fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Color::DarkGray)
        .title(Span::styled(
            format!(" {title} "),
            Style::new().cyan().bold(),
        ))
}

impl View {
    fn point_at_level(&mut self, column: u16, row: u16, click: bool) {
        if self.level_area.contains((column, row).into()) {
            let index = self.level_offset + usize::from(row - self.level_area.y);
            if let Some(&level) = self.level_rows.get(index) {
                self.selected = level;
                if click {
                    self.focus = Pane::People;
                }
            }
        }
    }
    fn move_focus(&mut self, forward: bool, lobby: &Lobby) {
        if self.form.is_some() || lobby.active.borrow().is_none() {
            return;
        }
        self.focus = match (self.focus, forward) {
            (Pane::Sessions, true) | (Pane::Chat, false) => Pane::People,
            (Pane::People, true) => Pane::Chat,
            (Pane::People, false) => Pane::Sessions,
            (pane, _) => pane,
        };
    }

    fn pane(&self, pane: Pane, title: &str) -> Block<'static> {
        if self.focus == pane {
            panel(&format!("› {title}")).border_style(Color::Cyan)
        } else {
            panel(&format!("  {title}"))
        }
    }
    fn edit(&mut self, form: Form) {
        self.draft = match form {
            Form::Name | Form::RoomName => Some(TextEdit::new(64)),
            Form::Import => Some(TextEdit::default()),
            Form::Share(_) | Form::Delete | Form::Leave => None,
        };
        self.form = Some(form);
        self.notice.clear();
    }

    fn submit(&mut self, lobby: &mut Lobby) -> Result<()> {
        let value = self.draft.as_ref().map_or("", |edit| edit.text.trim());
        match &self.form {
            Some(Form::Name) => {
                ensure!(valid(value, 64), "Use a display name of 1–64 bytes");
                lobby.config.name = value.to_owned();
                if let Err(error) = lobby.config.persist() {
                    lobby.config.name.clear();
                    return Err(error);
                }
            }
            Some(Form::RoomName) => {
                ensure!(valid(value, 64), "Use a session name of 1–64 bytes");
                let room = Room {
                    name: value.to_owned(),
                    secret: config::random(),
                };
                let invite = room.invite()?;
                self.room = lobby.add_room(room)?;
                self.edit(Form::Share(invite));
                return Ok(());
            }
            Some(Form::Import) => {
                self.room = lobby.add_room(Room::parse(value).context("Invalid room invite")?)?;
            }
            Some(Form::Share(_) | Form::Delete | Form::Leave) => {}
            None => {}
        }
        self.form = None;
        self.draft = None;
        self.notice.clear();
        Ok(())
    }

    fn rooms(&self, lobby: &Lobby, active: Option<(&Call, &[PeerRow])>) -> Vec<Line<'static>> {
        if lobby.config.rooms.is_empty() {
            return vec![
                "  No sessions yet. Press n to create one or i to import an invite.".into(),
            ];
        }
        lobby
            .config
            .rooms
            .iter()
            .enumerate()
            .map(|(index, room)| {
                let mut names = Vec::new();
                if let Some((call, peers)) = active.filter(|(c, _)| c.room.secret == room.secret) {
                    names.push(Span::styled("[active] ", Style::new().green()));
                    names.push(Span::raw(format!("{} (you)", call.name)));
                    for peer in peers {
                        names.push(Span::raw(", "));
                        let name = Span::raw(peer.name.clone());
                        names.push(if peer.late { name.dim() } else { name });
                    }
                } else {
                    names.push(Span::raw(match &*lobby.rosters[index].borrow() {
                        None => "unknown".into(),
                        Some(names) if names.is_empty() => "empty".into(),
                        Some(names) => names.join(", "),
                    }));
                }
                let selected = index == self.room;
                let mut spans = vec![
                    Span::raw(if selected { " › " } else { "   " }),
                    Span::raw(room.name.clone()).bold(),
                    Span::raw("  ·  "),
                ];
                spans.extend(names);
                Line::from(spans).style(if selected {
                    Style::new().cyan()
                } else {
                    Style::default()
                })
            })
            .collect::<Vec<_>>()
    }

    fn draw(&mut self, frame: &mut ratatui::Frame, lobby: &Lobby) {
        self.draw_view(frame, lobby);
        if let Some(form @ (Form::Delete | Form::Leave)) = &self.form {
            let content = if matches!(form, Form::Delete) {
                let room = &lobby.config.rooms[self.room];
                let leave = if lobby
                    .active
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.room.secret == room.secret)
                {
                    " You will leave the call."
                } else {
                    ""
                };
                format!(
                    "Remove ‘{}’ from this device?{leave}\nOthers keep access.\n\ny delete · n/Esc cancel\n{}",
                    room.name, self.notice
                )
            } else {
                "y leave · n/Esc stay".into()
            };
            let popup = Paragraph::new(content)
                .wrap(Wrap { trim: false })
                .block(panel(form.title()));
            let screen = frame.area();
            let width = screen.width.min(56);
            let height = screen.height.min(popup.line_count(width) as u16);
            let area = Rect::new(
                screen.x + (screen.width - width) / 2,
                screen.y + (screen.height - height) / 2,
                width,
                height,
            );
            frame.render_widget(ratatui::widgets::Clear, area);
            frame.render_widget(popup, area);
        }
    }

    fn draw_view(&mut self, frame: &mut ratatui::Frame, lobby: &Lobby) {
        self.level_rows.clear();
        let [header, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(frame.area());
        let profile = if lobby.config.temporary {
            "temporary profile"
        } else {
            "private voice rooms"
        };
        frame.render_widget(
            Line::from(vec![
                " ◉ VOICE ".black().on_cyan().bold(),
                Span::raw(format!("  {profile}")).dark_gray(),
            ]),
            header,
        );
        if let Some(form) = &self.form
            && !matches!(form, Form::Delete | Form::Leave)
        {
            let (text, help) = match form {
                Form::Share(invite) => (invite.as_str(), "c copy to clipboard · Esc close"),
                _ => ("", "Enter confirm · Esc cancel"),
            };
            let notice = self
                .draft
                .as_ref()
                .map(|edit| edit.error.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.notice);
            let content = format!("{text}\n\n{help}\n{notice}");
            let block = panel(form.title());
            let inner = block.inner(body);
            frame.render_widget(
                Paragraph::new(content)
                    .wrap(Wrap { trim: false })
                    .block(block),
                body,
            );
            if let Some(edit) = &self.draft {
                edit.draw(
                    frame,
                    Rect {
                        height: inner.height.min(1),
                        ..inner
                    },
                );
            }
            return;
        }
        let active = lobby.active.borrow().clone();
        let draw_rooms =
            |frame: &mut ratatui::Frame, rooms: Rect, active: Option<(&Call, &[PeerRow])>| {
                let offset = self
                    .room
                    .saturating_sub(rooms.height.saturating_sub(3) as usize);
                frame.render_widget(
                    Paragraph::new(self.rooms(lobby, active))
                        .scroll((offset as u16, 0))
                        .block(self.pane(Pane::Sessions, "Sessions")),
                    rooms,
                );
            };
        let Some(call) = active else {
            let help = if lobby.config.rooms.is_empty() {
                "n new · i import · q quit"
            } else {
                "↑↓ select · Enter join · s share · d delete · n new · i import · q quit"
            };
            let footer = Paragraph::new(help).wrap(Wrap { trim: false });
            let [rooms, keys] = Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(footer.line_count(body.width) as u16),
            ])
            .areas(body);
            frame.render_widget(footer, keys);
            draw_rooms(frame, rooms, None);
            return;
        };
        let [rooms, people, chat, keys] = Layout::vertical([
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Fill(1),
            Constraint::Length(4),
        ])
        .areas(body);
        let (audio_available, muted, speech, master, gain, peers, messages) = {
            let state = call.state.lock();
            let peers = state
                .peers
                .iter()
                .map(|(&id, p)| PeerRow {
                    id,
                    name: p.name.clone(),
                    late: p.is_late(),
                    muted: p.muted,
                    local_mute: p.local_mute,
                    volume: p.volume,
                    last_speech: p.last_speech,
                })
                .collect::<Vec<_>>();
            (
                state.audio_available,
                state.muted,
                state.last_speech,
                state.levels.output,
                state.levels.input,
                peers,
                state.chat.clone(),
            )
        };
        draw_rooms(frame, rooms, Some((&call, &peers)));
        {
            let mut lines = Vec::new();
            if audio_available {
                self.level_rows.extend([LevelRow::Output, LevelRow::Input]);
                lines.push(Line::from(format!("Output {:.0}%", master * 100.0)));
                lines.push(Line::from(format!("Input {:.0}%", gain * 100.0)));
            }
            for peer in &peers {
                self.level_rows.push(LevelRow::Peer(peer.id));
                let local = if peer.local_mute {
                    "[muted by you]"
                } else {
                    ""
                };
                let row = Line::from(format!(
                    "{} {} {local} {:.0}%",
                    peer.name,
                    indicator(peer.muted, peer.last_speech),
                    peer.volume * 100.0
                ));
                lines.push(if peer.late { row.dim() } else { row });
            }
            let status = if audio_available {
                indicator(muted, speech)
            } else {
                "[chat-only]"
            };
            let block = self.pane(
                Pane::People,
                &format!("{} | {} (you) {status}", call.room.name, call.name),
            );
            self.level_area = block.inner(people);
            let selected = self
                .level_rows
                .iter()
                .position(|r| *r == self.selected)
                .unwrap_or(0);
            if let Some(row) = self.level_rows.get(selected) {
                self.selected = *row;
            }
            for (i, line) in lines.iter_mut().enumerate() {
                line.spans.insert(
                    0,
                    if i == selected {
                        "› ".cyan()
                    } else {
                        "  ".into()
                    },
                );
            }
            let height = usize::from(self.level_area.height).max(1);
            self.level_offset = self
                .level_offset
                .min(lines.len().saturating_sub(height))
                .clamp(selected.saturating_sub(height - 1), selected);
            frame.render_widget(
                Paragraph::new(lines)
                    .scroll((self.level_offset as u16, 0))
                    .block(block),
                people,
            );
            let messages = messages
                .iter()
                .map(|line| Line::from(line.as_ref()))
                .collect::<Vec<_>>();
            let paragraph = Paragraph::new(messages)
                .wrap(Wrap { trim: false })
                .block(self.pane(Pane::Chat, "Chat"));
            let rows = paragraph
                .line_count(chat.width)
                .saturating_sub(chat.height as usize)
                .min(u16::MAX as usize) as u16;
            self.chat_scroll = self.chat_scroll.min(rows);
            frame.render_widget(paragraph.scroll((rows - self.chat_scroll, 0)), chat);
        }
        let help = match self.focus {
            Pane::Chat => "\nEnter send · ↑↓ scroll · Alt+arrows/hjkl change pane · Esc back",
            Pane::Sessions if !audio_available => {
                "Alt+arrows/hjkl focus · arrows/hjkl select · Enter join · n new · i import · s share · d delete\nt chat · Esc leave · q quit"
            }
            Pane::People if !audio_available => {
                "Alt+arrows/hjkl focus · arrows/hjkl select · t chat · Esc leave · q quit"
            }
            Pane::Sessions => {
                "Alt+arrows/hjkl focus · arrows/hjkl select · Enter join · n new · i import · s share · d delete\nt chat · m mic mute · Esc leave · q quit"
            }
            Pane::People => {
                "Alt+arrows/hjkl focus · hover or arrows/hjkl select · +/- selected level\nx mute participant · m mic mute · t chat · Esc leave · q quit"
            }
        };
        let help = if self.focus == Pane::Chat && !self.chat_input.error.is_empty() {
            format!("\n{}", self.chat_input.error)
        } else {
            help.into()
        };
        frame.render_widget(Paragraph::new(help).wrap(Wrap { trim: false }), keys);
        if self.focus == Pane::Chat {
            let label = if keys.width > 9 {
                frame.render_widget(Line::from("Message: "), keys);
                9
            } else {
                0
            };
            self.chat_input.draw(
                frame,
                Rect::new(
                    keys.x + label,
                    keys.y,
                    keys.width - label,
                    keys.height.min(1),
                ),
            );
        }
    }

    fn key(&mut self, event: impl Into<KeyEvent>, lobby: &mut Lobby) -> Result<bool> {
        let event = event.into();
        let key = event.code;
        if event.modifiers == KeyModifiers::CONTROL && key == KeyCode::Char('c') {
            return Ok(false);
        }
        if event.modifiers == KeyModifiers::ALT
            && let Some(forward) = direction(key)
        {
            self.move_focus(forward, lobby);
            return Ok(true);
        }
        if !(event.modifiers - KeyModifiers::SHIFT).is_empty() {
            return Ok(true);
        }
        if matches!(self.form, Some(Form::Delete)) {
            match key {
                KeyCode::Char('y') => match lobby.remove_room(self.room) {
                    Ok(()) => {
                        self.room = self.room.min(lobby.config.rooms.len().saturating_sub(1));
                        if lobby.active.borrow().is_none() {
                            self.selected = LevelRow::Output;
                            self.chat_input = TextEdit::default();
                            self.chat_scroll = 0;
                        }
                        self.form = None;
                    }
                    Err(error) => self.notice = format!("Could not delete session: {error}"),
                },
                KeyCode::Char('n') | KeyCode::Esc => self.form = None,
                _ => {}
            }
            return Ok(true);
        }
        if matches!(self.form, Some(Form::Leave)) {
            match key {
                KeyCode::Char('y') => {
                    lobby.leave();
                    self.focus = Pane::Sessions;
                    self.chat_input = TextEdit::default();
                    self.chat_scroll = 0;
                    self.form = None;
                }
                KeyCode::Char('n') | KeyCode::Esc => self.form = None,
                _ => {}
            }
            return Ok(true);
        }
        if let Some(Form::Share(invite)) = &self.form {
            match key {
                KeyCode::Char('c') => {
                    use crossterm::clipboard::{
                        ClipboardSelection, ClipboardType, CopyToClipboard,
                    };
                    let command = CopyToClipboard {
                        content: invite,
                        destination: ClipboardSelection(vec![ClipboardType::Clipboard]),
                    };
                    self.notice = match crossterm::execute!(std::io::stdout(), command) {
                        Ok(()) => {
                            "Copy requested. If unsupported, select and copy the invite above."
                                .into()
                        }
                        Err(error) => error.to_string(),
                    };
                }
                KeyCode::Esc | KeyCode::Enter => {
                    self.form = None;
                    self.draft = None;
                }
                _ => {}
            }
            return Ok(true);
        }
        let active = lobby.active.borrow().clone();
        if let Some(draft) = &mut self.draft {
            match key {
                KeyCode::Esc => {
                    if matches!(self.form, Some(Form::Name)) {
                        return Ok(false);
                    }
                    self.form = None;
                    self.draft = None;
                }
                KeyCode::Enter => {
                    draft.error.clear();
                    if let Err(error) = self.submit(lobby) {
                        self.notice = error.to_string();
                    }
                }
                _ => draft.key(key),
            }
            return Ok(true);
        }
        if self.focus == Pane::Chat {
            match key {
                KeyCode::Enter => {
                    if let Some(call) = active
                        && valid(&self.chat_input.text, 1024)
                    {
                        let text = std::mem::take(&mut self.chat_input).text;
                        let mut state = call.state.lock();
                        state.log(format!("{}: {text}", call.name));
                        state.broadcast(Message::Chat(text));
                        self.chat_scroll = 0;
                    }
                }
                KeyCode::Up => self.chat_scroll = self.chat_scroll.saturating_add(1),
                KeyCode::Down => self.chat_scroll = self.chat_scroll.saturating_sub(1),
                KeyCode::Esc => self.focus = Pane::People,
                _ => self.chat_input.key(key),
            }
            return Ok(true);
        }
        if let Some(forward) = direction(key) {
            if self.focus == Pane::People {
                if let Some(i) = self.level_rows.iter().position(|r| *r == self.selected) {
                    let next = if forward {
                        (i + 1).min(self.level_rows.len() - 1)
                    } else {
                        i.saturating_sub(1)
                    };
                    self.selected = self.level_rows[next];
                }
            } else if forward {
                self.room = (self.room + 1).min(lobby.config.rooms.len().saturating_sub(1));
            } else {
                self.room = self.room.saturating_sub(1);
            }
            return Ok(true);
        }
        match key {
            KeyCode::Char('q') => return Ok(false),
            KeyCode::Char('d') | KeyCode::Delete
                if self.focus == Pane::Sessions && self.room < lobby.config.rooms.len() =>
            {
                self.edit(Form::Delete)
            }
            KeyCode::Char('n') => self.edit(Form::RoomName),
            KeyCode::Char('i') => self.edit(Form::Import),
            KeyCode::Char('s') => {
                if let Some(room) = lobby.config.rooms.get(self.room) {
                    self.edit(Form::Share(room.invite()?));
                }
            }
            KeyCode::Enter if self.focus == Pane::Sessions => {
                if lobby.join(self.room)? {
                    self.selected = LevelRow::Output;
                    self.chat_input = TextEdit::default();
                    self.chat_scroll = 0;
                }
                if lobby.active.borrow().is_some() {
                    self.focus = Pane::People;
                }
            }
            KeyCode::Esc if active.is_some() => self.edit(Form::Leave),
            _ => {
                if let Some(call) = active {
                    let mut state = call.state.lock();
                    match key {
                        KeyCode::Char('t') => self.focus = Pane::Chat,
                        _ if !state.audio_available => {}
                        KeyCode::Char('m') => state.toggle_mute(),
                        KeyCode::Char('x') if self.focus == Pane::People => {
                            if let LevelRow::Peer(id) = self.selected
                                && let Some(peer) = state.peers.get_mut(&id)
                            {
                                peer.local_mute = !peer.local_mute;
                            }
                        }
                        KeyCode::Char(ch @ ('+' | '-')) if self.focus == Pane::People => {
                            let up = ch == '+';
                            let changed = match self.selected {
                                LevelRow::Output => adjust(&mut state.levels.output, up, 4.0),
                                LevelRow::Input => adjust(&mut state.levels.input, up, 4.0),
                                LevelRow::Peer(id) => {
                                    let Some(peer) = state.peers.get_mut(&id) else {
                                        return Ok(true);
                                    };
                                    if !adjust(&mut peer.volume, up, 1.0) {
                                        return Ok(true);
                                    }
                                    let volume = peer.volume;
                                    state.levels.peers.insert(id, volume);
                                    true
                                }
                            };
                            if !changed {
                                return Ok(true);
                            }
                            lobby.config.levels = state.levels.clone();
                            drop(state);
                            if let Err(error) = lobby.config.persist() {
                                call.state
                                    .lock()
                                    .log(format!("Could not save volume: {error}"));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(true)
    }
}

pub fn run(lobby: &mut Lobby) -> Result<()> {
    let mut terminal = ratatui::try_init()?;
    let mut view = View::default();
    if lobby.config.name.is_empty() {
        view.edit(Form::Name);
    }
    let result = (|| {
        crossterm::execute!(std::io::stdout(), event::EnableBracketedPaste)?;
        let mut mouse_enabled = false;
        loop {
            terminal.draw(|frame| view.draw(frame, lobby))?;
            let capture = view.form.is_none() && !view.level_rows.is_empty();
            if capture != mouse_enabled {
                if capture {
                    crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)?;
                } else {
                    crossterm::execute!(std::io::stdout(), event::DisableMouseCapture)?;
                }
                mouse_enabled = capture;
            }
            if !event::poll(Duration::from_millis(150))? {
                continue;
            }
            match event::read()? {
                Event::Mouse(mouse)
                    if capture
                        && matches!(
                            mouse.kind,
                            event::MouseEventKind::Moved
                                | event::MouseEventKind::Down(event::MouseButton::Left)
                        ) =>
                {
                    view.point_at_level(
                        mouse.column,
                        mouse.row,
                        mouse.kind != event::MouseEventKind::Moved,
                    );
                }
                Event::Paste(text) => {
                    let text = text.trim_end_matches(['\r', '\n']);
                    let input = if view.form.is_none() && view.focus == Pane::Chat {
                        Some(&mut view.chat_input)
                    } else {
                        view.draft.as_mut()
                    };
                    if let Some(edit) = input {
                        edit.insert(text);
                    }
                }
                Event::Key(key)
                    if key.kind != KeyEventKind::Release && !view.key(key, lobby)? =>
                {
                    break;
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = crossterm::execute!(
        std::io::stdout(),
        event::DisableMouseCapture,
        event::DisableBracketedPaste
    );
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{Endpoint, RelayMode, endpoint::presets};

    #[test]
    fn edit_in_the_middle_of_unicode_text() {
        let mut edit = TextEdit::default();
        edit.insert("a👩‍💻z");
        edit.key(KeyCode::Left);
        edit.key(KeyCode::Backspace);
        edit.insert("界e\u{301}");
        edit.key(KeyCode::Left);
        edit.key(KeyCode::Delete);
        assert_eq!(edit.text, "a界z");
        edit.insert(&"x".repeat(1024));
        assert!(edit.error.contains("1024"));
        edit.insert("\n");
        assert!(edit.error.contains("one line"));
        assert_eq!(&edit.text[..edit.cursor], "a界");
    }

    #[tokio::test]
    async fn two_temporary_clients_can_share_a_room_from_the_lobby() -> Result<()> {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await?;
        let mut first = Lobby::start(
            endpoint.clone(),
            config::Config::open(true)?,
            Default::default(),
        );
        let mut second = Lobby::start(
            endpoint.clone(),
            config::Config::open(true)?,
            Default::default(),
        );
        assert_ne!(first.config.identity, second.config.identity);
        let mut view = View {
            focus: Pane::Chat,
            level_area: Rect::new(1, 1, 10, 1),
            level_rows: vec![LevelRow::Input],
            ..Default::default()
        };
        view.point_at_level(1, 1, false);
        view.key(KeyCode::Char('m'), &mut first)?;
        assert_eq!(view.chat_input.text, "m");
        view.point_at_level(1, 1, true);
        assert!(view.focus == Pane::People);
        let mut view = View::default();
        view.edit(Form::Name);
        view.draft.as_mut().unwrap().insert(&"é".repeat(33));
        assert!(view.draft.as_ref().unwrap().error.contains("64"));
        view.draft.as_mut().unwrap().insert("Alice");
        view.submit(&mut first)?;
        view.key(KeyCode::Char('n'), &mut first)?;
        view.draft.as_mut().unwrap().insert("Friday");
        view.submit(&mut first)?;
        let Some(Form::Share(invite)) = view.form.take() else {
            panic!("missing invite")
        };

        view.edit(Form::Import);
        view.draft.as_mut().unwrap().insert(&invite);
        view.submit(&mut second)?;
        assert_eq!(first.config.rooms[0].secret, second.config.rooms[0].secret);
        let call = crate::call::Call::new(first.config.rooms[0].clone(), "Alice".into());
        first.active.send_replace(Some(call.clone()));
        view.chat_input.insert("pending");
        view.key(KeyCode::Enter, &mut first)?;
        assert!(std::sync::Arc::ptr_eq(
            first.active.borrow().as_ref().unwrap(),
            &call
        ));
        assert_eq!(std::mem::take(&mut view.chat_input).text, "pending");
        assert!(view.focus == Pane::People);
        view.move_focus(true, &first);
        view.key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut first,
        )?;
        for text in ["jkl", "hello"] {
            for ch in text.chars() {
                view.key(KeyCode::Char(ch), &mut first)?;
            }
            view.key(KeyCode::Enter, &mut first)?;
        }
        assert!(view.focus == Pane::Chat);
        assert!(
            call.state
                .lock()
                .chat
                .iter()
                .map(|s| s.as_ref())
                .eq(["Alice: jkl", "Alice: hello"])
        );
        call.state.lock().log("wrapped message ".repeat(40));
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(32, 20))?;
        view.chat_scroll = u16::MAX;
        terminal.draw(|f| view.draw(f, &first))?;
        let top = view.chat_scroll;
        assert!((1..u16::MAX).contains(&top));
        view.key(KeyCode::Up, &mut first)?;
        terminal.draw(|f| view.draw(f, &first))?;
        view.key(KeyCode::Down, &mut first)?;
        assert_eq!(view.chat_scroll, top - 1);
        view.focus = Pane::Sessions;
        view.key(KeyCode::Delete, &mut first)?;
        terminal.draw(|f| view.draw(f, &first))?;
        view.key(KeyCode::Enter, &mut first)?;
        view.key(KeyCode::Esc, &mut first)?;
        assert_eq!(first.config.rooms.len(), 1);
        assert!(first.active.borrow().is_some());
        view.key(KeyCode::Esc, &mut first)?;
        assert!(matches!(view.form, Some(Form::Leave)));
        terminal.draw(|f| view.draw(f, &first))?;
        view.key(KeyCode::Esc, &mut first)?;
        assert!(first.active.borrow().is_some());
        view.key(KeyCode::Esc, &mut first)?;
        view.key(KeyCode::Char('y'), &mut first)?;
        assert!(first.active.borrow().is_none());
        view.key(KeyCode::Char('d'), &mut first)?;
        view.key(KeyCode::Char('y'), &mut first)?;
        assert!(first.config.rooms.is_empty() && first.rosters.is_empty());
        assert!(first.active.borrow().is_none());
        assert_eq!(second.config.rooms.len(), 1);
        assert!(config::Config::open(true)?.rooms.is_empty());
        endpoint.close().await;
        Ok(())
    }
}
