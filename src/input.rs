const SGR_MOUSE: &[u8] = b"\x1b[<";
// Real reports are at most about 20 bytes; anything longer without a final byte is garbage.
const MAX_MOUSE_REPORT: usize = 32;

/// How long a trailing ESC or `ESC [` waits for the rest of a mouse report before it's taken
/// as typed keys. Terminals write a report in one go, so this only has to cover a read split.
pub const ESC_WAIT: std::time::Duration = std::time::Duration::from_millis(25);

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

use crate::config::Prefix;
use crate::workspace::{Axis, Direction};

#[derive(Debug, PartialEq)]
pub enum Action {
    Bytes(Vec<u8>),
    Mouse(Mouse),
    Quit,
    ToggleReview,
    NewWorkspace,
    Split(Axis),
    ClosePane,
    FocusNext,
    Focus(Direction),
    /// 0-based index into the sidebar's workspace list.
    SelectWorkspace(usize),
}

/// One SGR (1006) mouse report. Coordinates are 0-based screen cells; `code` keeps the raw
/// button byte (button, modifiers, motion and wheel bits) so it can be re-encoded for a pane.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Mouse {
    pub code: u16,
    pub col: u16,
    pub row: u16,
    pub release: bool,
}

#[derive(Debug, PartialEq)]
pub enum MouseKind {
    LeftPress,
    WheelUp,
    WheelDown,
    Other,
}

impl Mouse {
    pub fn kind(&self) -> MouseKind {
        // Bits 2-4 are Shift/Meta/Ctrl, which don't change what a click means to spindle.
        match (self.code & !0b11100, self.release) {
            (0, false) => MouseKind::LeftPress,
            (64, _) => MouseKind::WheelUp,
            (65, _) => MouseKind::WheelDown,
            _ => MouseKind::Other,
        }
    }

    pub fn is_wheel(&self) -> bool {
        self.code & 64 != 0
    }

    pub fn is_motion(&self) -> bool {
        self.code & 32 != 0
    }

    /// Re-encodes this report for an app at (`col`, `row`) of its own screen, in the form it
    /// asked for. `None` when its mode doesn't want this kind of event, or its encoding can't
    /// express the position.
    pub fn encode_for(
        &self,
        col: u16,
        row: u16,
        mode: MouseProtocolMode,
        encoding: MouseProtocolEncoding,
    ) -> Option<Vec<u8>> {
        let wanted = match mode {
            MouseProtocolMode::None => false,
            MouseProtocolMode::Press => !self.release && !self.is_motion(),
            MouseProtocolMode::PressRelease => !self.is_motion(),
            MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion => true,
        };
        if !wanted {
            return None;
        }
        let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
        if encoding == MouseProtocolEncoding::Sgr {
            let end = if self.release { 'm' } else { 'M' };
            return Some(format!("\x1b[<{};{x};{y}{end}", self.code).into_bytes());
        }
        // The older encodings can't say which button was released, only that one was.
        let code = u32::from(if self.release {
            self.code | 0b11
        } else {
            self.code
        });
        let mut out = b"\x1b[M".to_vec();
        for value in [code + 32, x + 32, y + 32] {
            if encoding == MouseProtocolEncoding::Default {
                out.push(u8::try_from(value).ok()?);
            } else {
                let mut buf = [0; 4];
                out.extend(char::from_u32(value)?.encode_utf8(&mut buf).as_bytes());
            }
        }
        Some(out)
    }
}

/// The command bound to `key` after the prefix, shared with clicks on the sidebar menu.
pub fn command(key: u8) -> Option<Action> {
    match key {
        b'q' => Some(Action::Quit),
        b'g' => Some(Action::ToggleReview),
        b'n' => Some(Action::NewWorkspace),
        b's' => Some(Action::Split(Axis::SideBySide)),
        b'S' => Some(Action::Split(Axis::Stacked)),
        b'x' => Some(Action::ClosePane),
        b'o' => Some(Action::FocusNext),
        b'1'..=b'9' => Some(Action::SelectWorkspace((key - b'1') as usize)),
        _ => None,
    }
}

/// Parses `ESC [ < code ; col ; row (M|m)` at the start of `bytes`. `None` means the bytes end
/// before the sequence does; otherwise returns how many bytes it spans and the report, which
/// is absent if the sequence was malformed.
fn parse_mouse(bytes: &[u8]) -> Option<(usize, Option<Mouse>)> {
    let body = &bytes[SGR_MOUSE.len()..];
    let end = body
        .iter()
        .position(|b| !(b.is_ascii_digit() || *b == b';'))?;
    let len = SGR_MOUSE.len() + end + 1;
    let fields: Vec<u16> = body[..end]
        .split(|b| *b == b';')
        .filter_map(|f| std::str::from_utf8(f).ok()?.parse().ok())
        .collect();
    let mouse = match (fields.as_slice(), body[end]) {
        (&[code, col, row], final_byte @ (b'M' | b'm')) if col > 0 && row > 0 => Some(Mouse {
            code,
            col: col - 1,
            row: row - 1,
            release: final_byte == b'm',
        }),
        _ => None,
    };
    Some((len, mouse))
}

/// Splits keyboard bytes into plain input and spindle's own commands, in the order they were
/// typed. The prefix state lives across calls because the prefix and the key after it can arrive
/// in separate reads.
pub struct Input {
    prefix: u8,
    prefixed: bool,
    /// The start of a possible mouse report whose remainder hasn't been read yet.
    partial: Vec<u8>,
}

impl Default for Input {
    fn default() -> Self {
        Input::new(Prefix::default())
    }
}

impl Input {
    pub fn new(prefix: Prefix) -> Self {
        Input {
            prefix: prefix.byte(),
            prefixed: false,
            partial: Vec::new(),
        }
    }

    /// Mouse reports never reach the shell as bytes: the outer terminal only sends them because
    /// spindle asked, so a shell would see them as garbage typing. They also leave the prefix
    /// state alone, so moving the mouse between Ctrl-b and its key doesn't cancel the command.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        self.process(bytes, false)
    }

    /// True while bytes are held back waiting to see if they start a mouse report; call
    /// [`Input::flush`] if nothing more arrives within [`ESC_WAIT`].
    pub fn pending(&self) -> bool {
        !self.partial.is_empty()
    }

    /// Gives up waiting: a held ESC or `ESC [` was typed, while an unfinished report is dropped.
    pub fn flush(&mut self) -> Vec<Action> {
        self.process(&[], true)
    }

    fn process(&mut self, bytes: &[u8], at_end: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut plain = Vec::with_capacity(bytes.len());
        let joined;
        let bytes = if self.partial.is_empty() {
            bytes
        } else {
            self.partial.extend_from_slice(bytes);
            joined = std::mem::take(&mut self.partial);
            &joined[..]
        };
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            let rest = &bytes[i..];
            if rest.starts_with(SGR_MOUSE) && at_end {
                break;
            }
            // A read can end anywhere inside a report, even before its `<`.
            if !at_end && rest.len() < SGR_MOUSE.len() && SGR_MOUSE.starts_with(rest) {
                self.partial = rest.to_vec();
                break;
            }
            if rest.starts_with(SGR_MOUSE) {
                let Some((len, mouse)) = parse_mouse(rest) else {
                    if rest.len() <= MAX_MOUSE_REPORT {
                        self.partial = rest.to_vec();
                    }
                    break;
                };
                if let Some(mouse) = mouse {
                    if !plain.is_empty() {
                        actions.push(Action::Bytes(std::mem::take(&mut plain)));
                    }
                    actions.push(Action::Mouse(mouse));
                }
                i += len;
                continue;
            }
            i += 1;
            if !self.prefixed {
                if b == self.prefix {
                    self.prefixed = true;
                } else {
                    plain.push(b);
                }
                continue;
            }
            self.prefixed = false;
            let arrow = match bytes.get(i..i + 2) {
                Some([b'[' | b'O', b'A']) => Some(Direction::Up),
                Some([b'[' | b'O', b'B']) => Some(Direction::Down),
                Some([b'[' | b'O', b'C']) => Some(Direction::Right),
                Some([b'[' | b'O', b'D']) => Some(Direction::Left),
                _ => None,
            };
            let command = match b {
                0x1b if arrow.is_some() => {
                    i += 2;
                    Action::Focus(arrow.expect("checked"))
                }
                b if b == self.prefix => {
                    plain.push(self.prefix);
                    continue;
                }
                other => match command(other) {
                    Some(command) => command,
                    None => {
                        plain.extend([self.prefix, other]);
                        continue;
                    }
                },
            };
            if !plain.is_empty() {
                actions.push(Action::Bytes(std::mem::take(&mut plain)));
            }
            actions.push(command);
        }
        if !plain.is_empty() {
            actions.push(Action::Bytes(plain));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: u8 = 0x02;

    #[test]
    fn plain_bytes_pass_through() {
        assert_eq!(
            Input::default().feed(b"ls\r"),
            vec![Action::Bytes(b"ls\r".to_vec())]
        );
    }

    #[test]
    fn prefix_q_quits_even_when_split_across_reads() {
        let mut input = Input::default();
        assert_eq!(input.feed(&[PREFIX]), vec![]);
        assert_eq!(input.feed(b"q"), vec![Action::Quit]);
    }

    #[test]
    fn bytes_and_commands_keep_their_order() {
        assert_eq!(
            Input::default().feed(&[b'l', b's', PREFIX, b'q', b'x']),
            vec![
                Action::Bytes(b"ls".to_vec()),
                Action::Quit,
                Action::Bytes(b"x".to_vec())
            ]
        );
    }

    #[test]
    fn prefix_g_toggles_review() {
        assert_eq!(
            Input::default().feed(&[PREFIX, b'g', b'j']),
            vec![Action::ToggleReview, Action::Bytes(b"j".to_vec())]
        );
    }

    #[test]
    fn double_prefix_sends_one_literal_ctrl_b() {
        assert_eq!(
            Input::default().feed(&[PREFIX, PREFIX]),
            vec![Action::Bytes(vec![PREFIX])]
        );
    }

    #[test]
    fn unbound_key_after_prefix_forwards_both() {
        assert_eq!(
            Input::default().feed(&[PREFIX, b'z', b'y']),
            vec![Action::Bytes(vec![PREFIX, b'z', b'y'])]
        );
    }

    fn left_press(col: u16, row: u16) -> Action {
        Action::Mouse(Mouse {
            code: 0,
            col,
            row,
            release: false,
        })
    }

    #[test]
    fn mouse_reports_are_decoded_and_kept_from_the_shell() {
        assert_eq!(
            Input::default().feed(b"a\x1b[<0;5;3Mb\x1b[<0;5;3m"),
            vec![
                Action::Bytes(b"a".to_vec()),
                left_press(4, 2),
                Action::Bytes(b"b".to_vec()),
                Action::Mouse(Mouse {
                    code: 0,
                    col: 4,
                    row: 2,
                    release: true
                }),
            ]
        );
    }

    #[test]
    fn mouse_report_split_across_reads_is_joined() {
        let mut input = Input::default();
        assert_eq!(
            input.feed(b"x\x1b[<0;12"),
            vec![Action::Bytes(b"x".to_vec())]
        );
        assert_eq!(input.feed(b"0;4"), vec![]);
        assert_eq!(
            input.feed(b"0Mq"),
            vec![left_press(119, 39), Action::Bytes(b"q".to_vec())]
        );
    }

    #[test]
    fn mouse_report_split_before_its_introducer_ends_is_joined() {
        let mut input = Input::default();
        assert_eq!(input.feed(b"x\x1b["), vec![Action::Bytes(b"x".to_vec())]);
        assert_eq!(input.feed(b"<0;5;3M"), vec![left_press(4, 2)]);

        let mut input = Input::default();
        assert_eq!(input.feed(b"\x1b"), vec![]);
        assert_eq!(input.feed(b"[<0;5;3M"), vec![left_press(4, 2)]);
    }

    #[test]
    fn held_esc_followed_by_a_key_sequence_reaches_the_shell_whole() {
        let mut input = Input::default();
        assert_eq!(input.feed(b"\x1b"), vec![]);
        assert_eq!(input.feed(b"[A"), vec![Action::Bytes(b"\x1b[A".to_vec())]);
    }

    #[test]
    fn prefix_then_held_esc_still_forwards_both() {
        let mut input = Input::default();
        assert_eq!(input.feed(b"\x02\x1b"), vec![]);
        assert_eq!(input.flush(), vec![Action::Bytes(b"\x02\x1b".to_vec())]);
    }

    #[test]
    fn unfinished_reports_are_dropped_not_typed() {
        let mut input = Input::default();
        assert_eq!(input.feed(b"\x1b[<0;5"), vec![]);
        assert_eq!(input.flush(), vec![]);

        let runaway = [&b"\x1b[<"[..], &b"1;".repeat(40)].concat();
        assert_eq!(input.feed(&runaway), vec![]);
        assert!(
            !input.pending(),
            "past any real report's length, so not held"
        );
        assert_eq!(input.feed(b"a"), vec![Action::Bytes(b"a".to_vec())]);
    }

    #[test]
    fn mouse_kinds() {
        let kind = |bytes: &[u8]| match Input::default().feed(bytes).as_slice() {
            [Action::Mouse(m)] => m.kind(),
            other => panic!("{other:?}"),
        };
        assert_eq!(kind(b"\x1b[<0;1;1M"), MouseKind::LeftPress);
        assert_eq!(kind(b"\x1b[<16;1;1M"), MouseKind::LeftPress, "Ctrl-click");
        assert_eq!(kind(b"\x1b[<0;1;1m"), MouseKind::Other, "release");
        assert_eq!(kind(b"\x1b[<2;1;1M"), MouseKind::Other, "right button");
        assert_eq!(kind(b"\x1b[<32;1;1M"), MouseKind::Other, "drag");
        assert_eq!(kind(b"\x1b[<64;1;1M"), MouseKind::WheelUp);
        assert_eq!(kind(b"\x1b[<65;1;1M"), MouseKind::WheelDown);
    }

    #[test]
    fn mouse_between_prefix_and_key_keeps_the_prefix() {
        assert_eq!(
            Input::default().feed(b"\x02\x1b[<0;1;1Mq"),
            vec![left_press(0, 0), Action::Quit]
        );
    }

    #[test]
    fn mouse_is_re_encoded_only_for_what_the_app_asked() {
        use MouseProtocolEncoding as E;
        use MouseProtocolMode as M;
        let press = Mouse {
            code: 0,
            col: 9,
            row: 4,
            release: false,
        };
        let release = Mouse {
            release: true,
            ..press
        };
        let drag = Mouse { code: 32, ..press };

        assert_eq!(press.encode_for(9, 4, M::None, E::Sgr), None);
        assert_eq!(
            press.encode_for(9, 4, M::PressRelease, E::Sgr),
            Some(b"\x1b[<0;10;5M".to_vec())
        );
        assert_eq!(
            release.encode_for(9, 4, M::PressRelease, E::Sgr),
            Some(b"\x1b[<0;10;5m".to_vec())
        );
        assert_eq!(release.encode_for(9, 4, M::Press, E::Sgr), None);
        assert_eq!(drag.encode_for(9, 4, M::PressRelease, E::Sgr), None);
        assert!(drag.encode_for(9, 4, M::ButtonMotion, E::Sgr).is_some());

        assert_eq!(
            press.encode_for(9, 4, M::PressRelease, E::Default),
            Some(b"\x1b[M *%".to_vec())
        );
        assert_eq!(
            release.encode_for(9, 4, M::PressRelease, E::Default),
            Some(b"\x1b[M#*%".to_vec()),
            "release is button 3"
        );
        assert_eq!(
            press.encode_for(300, 4, M::PressRelease, E::Default),
            None,
            "past column 223"
        );
        assert_eq!(
            press.encode_for(300, 4, M::PressRelease, E::Utf8),
            Some("\x1b[M \u{14d}%".as_bytes().to_vec())
        );
    }

    #[test]
    fn malformed_mouse_report_is_dropped_whole() {
        assert_eq!(
            Input::default().feed(b"\x1b[<0;0;1Mz\x1b[<1;2Mz"),
            vec![Action::Bytes(b"zz".to_vec())]
        );
    }

    #[test]
    fn lone_esc_and_arrows_still_pass_through() {
        let mut input = Input::default();
        assert_eq!(input.feed(b"\x1b"), vec![], "might be a report");
        assert!(input.pending());
        assert_eq!(input.flush(), vec![Action::Bytes(b"\x1b".to_vec())]);
        assert!(!input.pending());
        assert_eq!(
            Input::default().feed(b"q\x1b"),
            vec![Action::Bytes(b"q".to_vec())]
        );
        assert_eq!(
            Input::default().feed(b"\x1b[A"),
            vec![Action::Bytes(b"\x1b[A".to_vec())]
        );
    }

    #[test]
    fn pane_and_workspace_commands() {
        assert_eq!(
            Input::default().feed(b"\x02n\x02s\x02S\x02x\x02o\x023"),
            vec![
                Action::NewWorkspace,
                Action::Split(Axis::SideBySide),
                Action::Split(Axis::Stacked),
                Action::ClosePane,
                Action::FocusNext,
                Action::SelectWorkspace(2),
            ]
        );
    }

    #[test]
    fn prefix_arrows_move_focus_but_plain_arrows_reach_the_shell() {
        assert_eq!(
            Input::default().feed(b"\x02\x1b[D\x02\x1bOA\x1b[B"),
            vec![
                Action::Focus(Direction::Left),
                Action::Focus(Direction::Up),
                Action::Bytes(b"\x1b[B".to_vec()),
            ]
        );
    }

    #[test]
    fn configured_prefix_replaces_ctrl_b() {
        let prefix = crate::config::tests::prefix("C-a");
        assert_eq!(Input::new(prefix).feed(b"\x01q"), vec![Action::Quit]);
        assert_eq!(
            Input::new(prefix).feed(b"\x01\x01\x01z\x02q"),
            vec![Action::Bytes(b"\x01\x01z\x02q".to_vec())],
            "double prefix is one literal, unbound keys pass with it, Ctrl-b is plain"
        );
    }
}
