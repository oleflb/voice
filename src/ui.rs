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
    peer: Option<iroh::EndpointId>,
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
}

impl Form {
    fn title(&self) -> &str {
        match self {
            Self::Name => "Welcome! Enter your display name",
            Self::RoomName => "New session: enter a name",
            Self::Import => "Paste a private room invite",
            Self::Share(_) => "Secret invite — share privately",
            Self::Delete => "Delete session?",
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

fn adjust(volume: &mut f32, up: bool) {
    *volume = (*volume + if up { 0.1 } else { -0.1 }).clamp(0.0, 4.0);
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
            Form::Share(_) | Form::Delete => None,
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
            Some(Form::Share(_) | Form::Delete) => {}
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
}
