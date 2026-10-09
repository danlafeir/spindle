const PREFIX: u8 = 0x02; // Ctrl-b
const SGR_MOUSE: &[u8] = b"\x1b[<";

#[derive(Debug, PartialEq)]
pub enum Action {
    Bytes(Vec<u8>),
    Mouse(Mouse),
    Quit,
    ToggleReview,
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
/// typed. The prefix state lives across calls because Ctrl-b and the key after it can arrive in
/// separate reads.
#[derive(Default)]
pub struct Input {
    prefixed: bool,
    /// The start of a mouse report whose remainder hasn't been read yet.
    partial: Vec<u8>,
}

impl Input {
    /// Mouse reports never reach the shell as bytes: the outer terminal only sends them because
    /// spindle asked, so a shell would see them as garbage typing. They also leave the prefix
    /// state alone, so moving the mouse between Ctrl-b and its key doesn't cancel the command.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
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
            if bytes[i..].starts_with(SGR_MOUSE) {
                let Some((len, mouse)) = parse_mouse(&bytes[i..]) else {
                    self.partial = bytes[i..].to_vec();
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
                if b == PREFIX {
                    self.prefixed = true;
                } else {
                    plain.push(b);
                }
                continue;
            }
            self.prefixed = false;
            let command = match b {
                b'q' => Action::Quit,
                b'g' => Action::ToggleReview,
                // Advertised in the menu but not implemented yet; swallowed so they don't leak to the shell.
                b'n' | b's' => continue,
                PREFIX => {
                    plain.push(PREFIX);
                    continue;
                }
                other => {
                    plain.extend([PREFIX, other]);
                    continue;
                }
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
            Input::default().feed(&[PREFIX, b'x', b'y']),
            vec![Action::Bytes(vec![PREFIX, b'x', b'y'])]
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
    fn malformed_mouse_report_is_dropped_whole() {
        assert_eq!(
            Input::default().feed(b"\x1b[<0;0;1Mz\x1b[<1;2Mz"),
            vec![Action::Bytes(b"zz".to_vec())]
        );
    }

    #[test]
    fn lone_esc_and_arrows_still_pass_through() {
        assert_eq!(
            Input::default().feed(b"\x1b"),
            vec![Action::Bytes(b"\x1b".to_vec())]
        );
        assert_eq!(
            Input::default().feed(b"\x1b[A"),
            vec![Action::Bytes(b"\x1b[A".to_vec())]
        );
    }

    #[test]
    fn placeholder_commands_are_swallowed() {
        assert_eq!(Input::default().feed(&[PREFIX, b'n', PREFIX, b's']), vec![]);
    }
}
