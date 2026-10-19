//! crossterm's terminal events as the keys, clicks and pastes the app
//! acts on: one name per binding (`Ctrl('c')`, `WordLeft`) however the
//! terminal encoded it.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Ctrl with a letter (`Ctrl('c')`), other than the ones named below.
    Ctrl(char),
    Alt(char),
    Enter,
    /// Ctrl-J, or Enter with a modifier: a line break in the input.
    Newline,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mouse {
    Press { x: u16, y: u16 },
    WheelUp,
    WheelDown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Mouse(Mouse),
    Paste(String),
}

impl Input {
    /// The input `event` is, if the app has a use for it (resizes are
    /// the event loop's).
    pub fn from_event(event: Event) -> Option<Self> {
        match event {
            Event::Key(key) => key_of(key).map(Self::Key),
            Event::Mouse(mouse) => mouse_of(mouse).map(Self::Mouse),
            Event::Paste(text) => Some(Self::Paste(text.replace("\r\n", "\n").replace('\r', "\n"))),
            _ => None,
        }
    }
}

fn key_of(key: KeyEvent) -> Option<Key> {
    // Terminals that report releases (Windows, kitty's protocol) send both.
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let m = key.modifiers;
    let (ctrl, alt) = (
        m.contains(KeyModifiers::CONTROL),
        m.contains(KeyModifiers::ALT),
    );
    Some(match key.code {
        KeyCode::Char('j') if ctrl => Key::Newline,
        KeyCode::Char('h') if ctrl => Key::Backspace,
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) if alt && c.is_ascii_graphic() => Key::Alt(c),
        KeyCode::Char(c) if !ctrl && !alt => Key::Char(c),
        KeyCode::Enter
            if m.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            Key::Newline
        }
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace if alt || ctrl => Key::Ctrl('w'),
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Esc => Key::Esc,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left if alt || ctrl => Key::WordLeft,
        KeyCode::Right if alt || ctrl => Key::WordRight,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(n) => Key::F(n),
        _ => return None,
    })
}

fn mouse_of(mouse: MouseEvent) -> Option<Mouse> {
    match mouse.kind {
        MouseEventKind::ScrollUp => Some(Mouse::WheelUp),
        MouseEventKind::ScrollDown => Some(Mouse::WheelDown),
        MouseEventKind::Down(MouseButton::Left) => Some(Mouse::Press {
            x: mouse.column,
            y: mouse.row,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
        key_of(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn keys_get_one_name_however_they_are_encoded() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        assert_eq!(
            key(KeyCode::Char('é'), KeyModifiers::SHIFT),
            Some(Key::Char('é'))
        );
        assert_eq!(key(KeyCode::Char('c'), ctrl), Some(Key::Ctrl('c')));
        assert_eq!(key(KeyCode::Char('j'), ctrl), Some(Key::Newline));
        assert_eq!(key(KeyCode::Enter, alt), Some(Key::Newline));
        assert_eq!(key(KeyCode::Enter, KeyModifiers::SHIFT), Some(Key::Newline));
        assert_eq!(key(KeyCode::Enter, none), Some(Key::Enter));
        assert_eq!(key(KeyCode::Char('x'), alt), Some(Key::Alt('x')));
        assert_eq!(key(KeyCode::Backspace, alt), Some(Key::Ctrl('w')));
        assert_eq!(key(KeyCode::Right, ctrl), Some(Key::WordRight));
        assert_eq!(key(KeyCode::F(1), none), Some(Key::F(1)));
        let mut release = KeyEvent::new(KeyCode::Char('a'), none);
        release.kind = KeyEventKind::Release;
        assert_eq!(key_of(release), None);
    }

    #[test]
    fn clicks_wheel_and_pastes() {
        let mouse = |kind| {
            Input::from_event(Event::Mouse(MouseEvent {
                kind,
                column: 2,
                row: 3,
                modifiers: KeyModifiers::NONE,
            }))
        };
        assert_eq!(
            mouse(MouseEventKind::ScrollUp),
            Some(Input::Mouse(Mouse::WheelUp))
        );
        assert_eq!(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            Some(Input::Mouse(Mouse::Press { x: 2, y: 3 }))
        );
        assert_eq!(mouse(MouseEventKind::Up(MouseButton::Left)), None);
        assert_eq!(
            Input::from_event(Event::Paste("one\r\ntwo".into())),
            Some(Input::Paste("one\ntwo".into()))
        );
    }
}
