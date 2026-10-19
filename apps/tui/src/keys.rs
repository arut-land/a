//! The terminal's input bytes as the keys, clicks and pastes the app acts
//! on: one name per binding (`Ctrl('c')`, `WordLeft`) however the terminal
//! encoded it. vte decodes the characters, controls and CSI sequences; an
//! ESC prefix (Alt) and SS3 are told apart here, since vte would read
//! either as the start of a sequence.

use vte::{Params, Parser, Perform};

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

const ESC: u8 = 0x1b;

/// Where in a sequence the last byte left the decoder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    /// After an ESC: a sequence, an Alt chord, or the Esc key itself.
    Esc,
    /// Inside `ESC [`, which vte parses up to its final byte.
    Csi,
    /// After `ESC O`: one byte names the key.
    Ss3,
}

/// Turns input into [`Input`]s as it is read, a sequence split across
/// reads included.
#[derive(Default)]
pub struct Decoder {
    parser: Parser,
    state: State,
    sink: Sink,
}

impl Decoder {
    /// Decodes `bytes`. Unless `more` input is already waiting, an ESC
    /// they end with is the Esc key.
    pub fn feed(&mut self, bytes: &[u8], more: bool) -> Vec<Input> {
        let mut rest = bytes;
        while let Some((&b, tail)) = rest.split_first() {
            match self.state {
                State::Ground => {
                    let n = rest.iter().position(|&b| b == ESC).unwrap_or(rest.len());
                    if n == 0 {
                        self.state = State::Esc;
                        rest = tail;
                    } else {
                        self.parser.advance(&mut self.sink, &rest[..n]);
                        rest = &rest[n..];
                    }
                    continue;
                }
                State::Esc if b == b'[' => {
                    self.parser.advance(&mut self.sink, b"\x1b[");
                    self.state = State::Csi;
                }
                // Inside a paste, an escape that is not its end is text.
                State::Esc if self.sink.paste.is_some() => {
                    self.state = State::Ground;
                    continue;
                }
                State::Esc if b == b'O' => self.state = State::Ss3,
                State::Esc if b == ESC => self.sink.key(Key::Esc),
                State::Esc => {
                    self.state = State::Ground;
                    match b {
                        0x21..=0x7e => self.sink.key(Key::Alt(char::from(b))),
                        b'\r' => self.sink.key(Key::Newline),
                        0x08 | 0x7f => self.sink.key(Key::Ctrl('w')),
                        // Alt with anything else is that key.
                        _ => continue,
                    }
                }
                State::Csi if b == ESC => {
                    self.parser = Parser::new();
                    self.state = State::Esc;
                }
                State::Csi => {
                    self.parser.advance(&mut self.sink, &[b]);
                    if (0x40..=0x7e).contains(&b) {
                        self.state = State::Ground;
                    }
                }
                State::Ss3 => {
                    self.state = State::Ground;
                    if let Some(key) = ss3(b) {
                        self.sink.key(key);
                    }
                }
            }
            rest = tail;
        }
        if self.state == State::Esc && !more && self.sink.paste.is_none() {
            self.state = State::Ground;
            self.sink.key(Key::Esc);
        }
        std::mem::take(&mut self.sink.out)
    }
}

#[derive(Default)]
struct Sink {
    out: Vec<Input>,
    /// Inside a bracketed paste: its text so far.
    paste: Option<String>,
}

impl Sink {
    fn key(&mut self, key: Key) {
        self.out.push(Input::Key(key));
    }
}

impl Perform for Sink {
    fn print(&mut self, c: char) {
        match &mut self.paste {
            Some(text) => text.push(c),
            None if c == '\x7f' => self.key(Key::Backspace),
            None => self.key(Key::Char(c)),
        }
    }

    fn execute(&mut self, byte: u8) {
        if let Some(text) = &mut self.paste {
            if matches!(byte, b'\r' | b'\n' | b'\t') {
                text.push(char::from(byte));
            }
            return;
        }
        let key = match byte {
            b'\r' => Key::Enter,
            b'\n' => Key::Newline,
            b'\t' => Key::Tab,
            0x08 => Key::Backspace,
            0x01..=0x1a => Key::Ctrl(char::from(b'a' + byte - 1)),
            _ => return,
        };
        self.key(key);
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        let mut n = [0u16; 3];
        for (slot, param) in n.iter_mut().zip(params) {
            *slot = param.first().copied().unwrap_or(0);
        }
        match (action, n[0]) {
            ('~', 200) => self.paste = Some(String::new()),
            ('~', 201) => {
                if let Some(text) = self.paste.take() {
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    self.out.push(Input::Paste(text));
                }
            }
            _ if self.paste.is_some() || ignore => {}
            ('M', _) if intermediates == b"<" => {
                // Shift, Alt and Ctrl are bits 4, 8 and 16; motion is 32.
                let mouse = match n[0] & !0b1_1100 {
                    0 => Mouse::Press {
                        x: n[1].saturating_sub(1),
                        y: n[2].saturating_sub(1),
                    },
                    64 => Mouse::WheelUp,
                    65 => Mouse::WheelDown,
                    _ => return,
                };
                self.out.push(Input::Mouse(mouse));
            }
            _ if !intermediates.is_empty() => {}
            _ => {
                if let Some(key) = csi(action, n, params.len()) {
                    self.key(key);
                }
            }
        }
    }
}

/// The key a CSI sequence names, from its final byte and parameters.
fn csi(action: char, n: [u16; 3], count: usize) -> Option<Key> {
    let mods = Mods::from(if count >= 2 { n[1] } else { 1 });
    Some(match action {
        'A' => Key::Up,
        'B' => Key::Down,
        'C' if mods.word() => Key::WordRight,
        'D' if mods.word() => Key::WordLeft,
        'C' => Key::Right,
        'D' => Key::Left,
        'H' => Key::Home,
        'F' => Key::End,
        'Z' => Key::BackTab,
        // `CSI 1 ; m P`: F1 to F4 with modifiers.
        'P'..='S' => Key::F(u8::try_from(action).ok()? - b'P' + 1),
        // kitty's and fixterms' `CSI code ; m u`.
        'u' => other(n[0], mods)?,
        '~' => match n[0] {
            1 | 7 => Key::Home,
            4 | 8 => Key::End,
            3 => Key::Delete,
            5 => Key::PageUp,
            6 => Key::PageDown,
            11..=15 => Key::F(u8::try_from(n[0] - 10).ok()?),
            17..=21 => Key::F(u8::try_from(n[0] - 11).ok()?),
            23 | 24 => Key::F(u8::try_from(n[0] - 12).ok()?),
            // xterm's modifyOtherKeys: `CSI 27 ; m ; code ~`.
            27 => other(n[2], Mods::from(n[1]))?,
            _ => return None,
        },
        _ => return None,
    })
}

/// The xterm modifier parameter, 1 plus the bits.
#[derive(Clone, Copy)]
struct Mods(u16);

impl From<u16> for Mods {
    fn from(param: u16) -> Self {
        Self(param.saturating_sub(1))
    }
}

impl Mods {
    const SHIFT: u16 = 1;
    const ALT: u16 = 2;
    const CTRL: u16 = 4;

    fn has(self, bit: u16) -> bool {
        self.0 & bit != 0
    }

    /// Alt or Ctrl with an arrow or Backspace: by words.
    fn word(self) -> bool {
        self.has(Self::ALT) || self.has(Self::CTRL)
    }
}

/// A key sent as its code point with modifiers.
fn other(code: u16, mods: Mods) -> Option<Key> {
    Some(match code {
        13 if mods.0 != 0 => Key::Newline,
        13 => Key::Enter,
        9 if mods.has(Mods::SHIFT) => Key::BackTab,
        9 => Key::Tab,
        27 => Key::Esc,
        8 | 127 if mods.word() => Key::Ctrl('w'),
        8 | 127 => Key::Backspace,
        _ => {
            let c = char::from_u32(code.into())?;
            match c {
                'j' if mods.has(Mods::CTRL) => Key::Newline,
                'h' if mods.has(Mods::CTRL) => Key::Backspace,
                c if mods.has(Mods::CTRL) && c.is_ascii_alphabetic() => {
                    Key::Ctrl(c.to_ascii_lowercase())
                }
                c if mods.has(Mods::ALT) && c.is_ascii_graphic() => Key::Alt(c),
                c if mods.has(Mods::CTRL) || mods.has(Mods::ALT) || c.is_control() => {
                    return None;
                }
                c => Key::Char(c),
            }
        }
    })
}

/// The key `ESC O` and `byte` name (application cursor and keypad modes).
fn ss3(byte: u8) -> Option<Key> {
    Some(match byte {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'M' => Key::Enter,
        b'P'..=b'S' => Key::F(byte - b'P' + 1),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Input> {
        Decoder::default().feed(bytes, false)
    }

    fn key(bytes: &[u8]) -> Option<Key> {
        match keys(bytes).as_slice() {
            [Input::Key(key)] => Some(*key),
            [] => None,
            other => panic!("{bytes:?} gave {other:?}"),
        }
    }

    #[test]
    fn keys_get_one_name_however_they_are_encoded() {
        let table: &[(&[u8], Key)] = &[
            (b"a", Key::Char('a')),
            (b"A", Key::Char('A')),
            ("é".as_bytes(), Key::Char('é')),
            ("日".as_bytes(), Key::Char('日')),
            (b"\r", Key::Enter),
            (b"\n", Key::Newline),
            (b"\t", Key::Tab),
            (b"\x7f", Key::Backspace),
            (b"\x08", Key::Backspace),
            (b"\x03", Key::Ctrl('c')),
            (b"\x01", Key::Ctrl('a')),
            (b"\x1a", Key::Ctrl('z')),
            (b"\x1b", Key::Esc),
            (b"\x1bx", Key::Alt('x')),
            (b"\x1bb", Key::Alt('b')),
            (b"\x1b\r", Key::Newline),
            (b"\x1b\x7f", Key::Ctrl('w')),
            (b"\x1b[A", Key::Up),
            (b"\x1b[B", Key::Down),
            (b"\x1b[C", Key::Right),
            (b"\x1b[D", Key::Left),
            (b"\x1b[1;5C", Key::WordRight),
            (b"\x1b[1;3D", Key::WordLeft),
            (b"\x1b[1;2A", Key::Up),
            (b"\x1bOA", Key::Up),
            (b"\x1bOD", Key::Left),
            (b"\x1b[H", Key::Home),
            (b"\x1b[F", Key::End),
            (b"\x1bOH", Key::Home),
            (b"\x1b[1~", Key::Home),
            (b"\x1b[4~", Key::End),
            (b"\x1b[3~", Key::Delete),
            (b"\x1b[5~", Key::PageUp),
            (b"\x1b[6~", Key::PageDown),
            (b"\x1b[Z", Key::BackTab),
            (b"\x1bOP", Key::F(1)),
            (b"\x1bOS", Key::F(4)),
            (b"\x1b[1;2P", Key::F(1)),
            (b"\x1b[15~", Key::F(5)),
            (b"\x1b[17~", Key::F(6)),
            (b"\x1b[24~", Key::F(12)),
            (b"\x1b[13;2u", Key::Newline),
            (b"\x1b[13u", Key::Enter),
            (b"\x1b[27;2;13~", Key::Newline),
            (b"\x1b[99;5u", Key::Ctrl('c')),
            (b"\x1b[106;5u", Key::Newline),
            (b"\x1b[120;3u", Key::Alt('x')),
            (b"\x1b[127;3u", Key::Ctrl('w')),
        ];
        for &(bytes, want) in table {
            assert_eq!(key(bytes), Some(want), "{bytes:?}");
        }
        for bytes in [&b"\x1b[I"[..], b"\x1b[?1;2c", b"\x1b[99~", b"\x00"] {
            assert_eq!(key(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn several_keys_in_one_read() {
        use Key::{Char, Esc, Up};
        let got = keys(b"ab\x1b[A\x1b\x1bc");
        let want = [Char('a'), Char('b'), Up, Esc, Key::Alt('c')];
        assert_eq!(got, want.map(Input::Key));
        assert_eq!(keys(b"\x1b\x03"), [Input::Key(Key::Ctrl('c'))]);
    }

    #[test]
    fn a_sequence_split_across_reads_is_one_key() {
        let mut d = Decoder::default();
        assert_eq!(d.feed(b"\x1b", true), []);
        assert_eq!(d.feed(b"[1;5", false), []);
        assert_eq!(d.feed(b"C", false), [Input::Key(Key::WordRight)]);
        let utf8 = "é".as_bytes();
        assert_eq!(d.feed(&utf8[..1], false), []);
        assert_eq!(d.feed(&utf8[1..], false), [Input::Key(Key::Char('é'))]);
        // An ESC that ends a read with nothing more waiting is the key.
        assert_eq!(d.feed(b"\x1b", false), [Input::Key(Key::Esc)]);
        assert_eq!(
            d.feed(b"[A", false),
            [Input::Key(Key::Char('[')), Input::Key(Key::Char('A'))]
        );
    }

    #[test]
    fn clicks_and_wheel_in_sgr() {
        let press = |x, y| Input::Mouse(Mouse::Press { x, y });
        assert_eq!(keys(b"\x1b[<0;3;4M"), [press(2, 3)]);
        assert_eq!(keys(b"\x1b[<0;3;4m"), []);
        assert_eq!(keys(b"\x1b[<64;1;1M"), [Input::Mouse(Mouse::WheelUp)]);
        assert_eq!(keys(b"\x1b[<65;1;1M"), [Input::Mouse(Mouse::WheelDown)]);
        // Ctrl-click is a click; a drag and the right button are not.
        assert_eq!(keys(b"\x1b[<16;300;200M"), [press(299, 199)]);
        assert_eq!(keys(b"\x1b[<32;3;4M"), []);
        assert_eq!(keys(b"\x1b[<2;3;4M"), []);
    }

    #[test]
    fn a_paste_is_its_text_with_newlines() {
        let paste = |text: &str| [Input::Paste(text.into())];
        assert_eq!(
            keys(b"\x1b[200~one\r\ntwo\rthree\x1b[201~"),
            paste("one\ntwo\nthree")
        );
        // Keys and escapes inside are text, or dropped.
        assert_eq!(
            keys(b"\x1b[200~a\x1b[Ab\x1bc\x03\x1b[201~x"),
            [Input::Paste("abc".into()), Input::Key(Key::Char('x'))]
        );
        let mut d = Decoder::default();
        assert_eq!(d.feed(b"\x1b[200~\xe6\x97", false), []);
        assert_eq!(d.feed(b"\xa5\x1b", false), []);
        assert_eq!(d.feed(b"[201~", false), paste("日"));
    }
}
