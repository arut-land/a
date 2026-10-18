//! Terminal input bytes as keys, mouse events and pastes: the xterm
//! sequences every current terminal sends (CSI and SS3 keys with
//! modifiers, SGR mouse reports, bracketed paste).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Ctrl with a letter (`Ctrl('c')`), other than the ones named below.
    Ctrl(char),
    Alt(char),
    Enter,
    /// Ctrl-J or Alt-Enter: a line break in the input.
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

/// Bytes to [`Input`]s; holds a sequence split across reads until the rest
/// arrives, or [`Parser::flush`] says none will (a lone Esc).
#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
    paste: Option<Vec<u8>>,
}

const PASTE_END: &[u8] = b"\x1b[201~";

impl Parser {
    /// Whether bytes wait for the rest of a sequence: the reader then waits
    /// briefly for more and calls [`Parser::flush`] if none come.
    pub fn pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        self.pending.extend_from_slice(bytes);
        let mut at = 0;
        while at < self.pending.len() {
            match self.next(at, out) {
                Some(used) => at += used,
                None => break,
            }
        }
        self.pending.drain(..at);
    }

    /// Takes what is pending as typed: a lone Esc, or Alt with a key.
    pub fn flush(&mut self, out: &mut Vec<Input>) {
        let pending = std::mem::take(&mut self.pending);
        match pending.as_slice() {
            [] => {}
            [0x1b] => out.push(Input::Key(Key::Esc)),
            [0x1b, rest @ ..] => {
                out.push(Input::Key(Key::Esc));
                self.feed(rest, out);
            }
            rest => self.feed(rest, out),
        }
    }

    /// Parses one input at `at`; returns the bytes it used, or `None` when
    /// it is incomplete.
    fn next(&mut self, at: usize, out: &mut Vec<Input>) -> Option<usize> {
        let bytes = &self.pending[at..];
        if let Some(paste) = &mut self.paste {
            if let Some(end) = find(bytes, PASTE_END) {
                paste.extend_from_slice(&bytes[..end]);
                let text = String::from_utf8_lossy(paste)
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                self.paste = None;
                out.push(Input::Paste(text));
                return Some(end + PASTE_END.len());
            }
            // Keeps a tail that may be the start of the end marker.
            let keep = (1..PASTE_END.len())
                .rev()
                .find(|&n| bytes.len() >= n && PASTE_END.starts_with(&bytes[bytes.len() - n..]))
                .unwrap_or(0);
            let take = bytes.len() - keep;
            paste.extend_from_slice(&bytes[..take]);
            return (take > 0).then_some(take);
        }
        let key = |k| Input::Key(k);
        let (input, used) = match bytes {
            [0x1b, b'[', ..] => return self.csi(at, out),
            [0x1b, b'O', c, ..] => (ss3(*c).map(key), 3),
            [0x1b, b'O'] | [0x1b] | [] => return None,
            [0x1b, b'\r', ..] => (Some(key(Key::Newline)), 2),
            [0x1b, 0x7f, ..] => (Some(key(Key::Ctrl('w'))), 2),
            [0x1b, b, ..] if b.is_ascii_graphic() => (Some(key(Key::Alt(char::from(*b)))), 2),
            [0x1b, ..] => (Some(key(Key::Esc)), 1),
            [b'\r', ..] => (Some(key(Key::Enter)), 1),
            [b'\n', ..] => (Some(key(Key::Newline)), 1),
            [b'\t', ..] => (Some(key(Key::Tab)), 1),
            [0x7f | 0x08, ..] => (Some(key(Key::Backspace)), 1),
            [c @ 0x01..=0x1a, ..] => (Some(key(Key::Ctrl(char::from(b'a' + c - 1)))), 1),
            [c, ..] if *c < 0x20 => (None, 1),
            [c, ..] => {
                let len = match c {
                    0xf0.. => 4,
                    0xe0.. => 3,
                    0xc0.. => 2,
                    _ => 1,
                };
                if bytes.len() < len {
                    return None;
                }
                let ch = std::str::from_utf8(&bytes[..len])
                    .ok()
                    .and_then(|s| s.chars().next());
                (ch.map(|c| key(Key::Char(c))), len)
            }
        };
        out.extend(input);
        Some(used)
    }

    fn csi(&mut self, at: usize, out: &mut Vec<Input>) -> Option<usize> {
        let bytes = &self.pending[at + 2..];
        let end = bytes.iter().position(|b| (0x40..=0x7e).contains(b))?;
        let (params, last) = (&bytes[..end], bytes[end]);
        let used = end + 3;
        let nums = |params: &[u8]| -> Vec<u16> {
            params
                .split(|&b| b == b';')
                .map(|n| {
                    std::str::from_utf8(n)
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0)
                })
                .collect()
        };
        if let [b'<', rest @ ..] = params {
            if let [b, x, y] = nums(rest)[..] {
                let (x, y) = (x.saturating_sub(1), y.saturating_sub(1));
                let mouse = match (b & !0b1_1100, last) {
                    (64, b'M') => Some(Mouse::WheelUp),
                    (65, b'M') => Some(Mouse::WheelDown),
                    (0, b'M') if b & 32 == 0 => Some(Mouse::Press { x, y }),
                    _ => None,
                };
                out.extend(mouse.map(Input::Mouse));
            }
            return Some(used);
        }
        let n = nums(params);
        // Modifiers are 1 + a mask: 2 Alt, 4 Ctrl; either moves by word.
        let ctrl = n.get(1).is_some_and(|m| m.saturating_sub(1) & 0b110 != 0);
        let key = match (last, n.first().copied().unwrap_or(0)) {
            (b'A', _) => Some(Key::Up),
            (b'B', _) => Some(Key::Down),
            (b'C', _) if ctrl => Some(Key::WordRight),
            (b'D', _) if ctrl => Some(Key::WordLeft),
            (b'C', _) => Some(Key::Right),
            (b'D', _) => Some(Key::Left),
            (b'H', _) | (b'~', 1 | 7) => Some(Key::Home),
            (b'F', _) | (b'~', 4 | 8) => Some(Key::End),
            (b'Z', _) => Some(Key::BackTab),
            (b'~', 3) => Some(Key::Delete),
            (b'~', 5) => Some(Key::PageUp),
            (b'~', 6) => Some(Key::PageDown),
            (b'~', 200) => {
                self.paste = Some(Vec::new());
                None
            }
            (b'~', 11) => Some(Key::F(1)),
            // Kitty and xterm's modifyOtherKeys report Enter with a
            // modifier as CSI 13 ; m u and CSI 27 ; m ; 13 ~.
            (b'u', 13) => Some(Key::Newline),
            (b'~', 27) if n.get(2) == Some(&13) => Some(Key::Newline),
            _ => None,
        };
        out.extend(key.map(Input::Key));
        Some(used)
    }
}

fn ss3(c: u8) -> Option<Key> {
    Some(match c {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'P'..=b'S' => Key::F(c - b'P' + 1),
        _ => return None,
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        Parser::default().feed(bytes, &mut out);
        out
    }

    fn keys(bytes: &[u8]) -> Vec<Key> {
        parse(bytes)
            .into_iter()
            .map(|i| match i {
                Input::Key(k) => k,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn keys_and_text() {
        assert_eq!(
            keys("hé\r\n\x03\x7f".as_bytes()),
            [
                Key::Char('h'),
                Key::Char('é'),
                Key::Enter,
                Key::Newline,
                Key::Ctrl('c'),
                Key::Backspace
            ]
        );
        assert_eq!(
            keys(b"\x1b[A\x1bOB\x1b[1;5C\x1b[5~\x1b[3~\x1bx"),
            [
                Key::Up,
                Key::Down,
                Key::WordRight,
                Key::PageUp,
                Key::Delete,
                Key::Alt('x')
            ]
        );
    }

    #[test]
    fn mouse_reports() {
        assert_eq!(
            parse(b"\x1b[<64;10;5M\x1b[<65;1;1M\x1b[<0;3;4M\x1b[<0;3;4m"),
            [
                Input::Mouse(Mouse::WheelUp),
                Input::Mouse(Mouse::WheelDown),
                Input::Mouse(Mouse::Press { x: 2, y: 3 }),
            ]
        );
    }

    #[test]
    fn split_sequences_wait_for_the_rest() {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        parser.feed(b"\x1b[<64;1", &mut out);
        assert!(out.is_empty() && parser.pending());
        parser.feed(b";1M\xe6\x97", &mut out);
        parser.feed(b"\xa5", &mut out);
        assert_eq!(
            out,
            [Input::Mouse(Mouse::WheelUp), Input::Key(Key::Char('日'))]
        );
        parser.feed(b"\x1b", &mut out);
        assert!(parser.pending());
        out.clear();
        parser.flush(&mut out);
        assert_eq!(out, [Input::Key(Key::Esc)]);
    }

    #[test]
    fn bracketed_paste_is_one_input_even_when_split() {
        let mut parser = Parser::default();
        let mut out = Vec::new();
        parser.feed(b"\x1b[200~line one\r\nline\x1b[20", &mut out);
        assert_eq!(out, []);
        parser.feed(b"1~x", &mut out);
        assert_eq!(
            out,
            [
                Input::Paste("line one\nline".into()),
                Input::Key(Key::Char('x'))
            ]
        );
    }
}
