const PREFIX: u8 = 0x02; // Ctrl-b

#[derive(Debug, PartialEq)]
pub enum Action {
    Bytes(Vec<u8>),
    Quit,
}

/// Splits keyboard bytes into plain input and spindle's own commands, in the order they were
/// typed. The prefix state lives across calls because Ctrl-b and the key after it can arrive in
/// separate reads.
#[derive(Default)]
pub struct Input {
    prefixed: bool,
}

impl Input {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut plain = Vec::with_capacity(bytes.len());
        for &b in bytes {
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

    #[test]
    fn placeholder_commands_are_swallowed() {
        assert_eq!(Input::default().feed(&[PREFIX, b'n', PREFIX, b's']), vec![]);
    }
}
