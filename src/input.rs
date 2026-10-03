const PREFIX: u8 = 0x02; // Ctrl-b

#[derive(Debug, PartialEq)]
pub enum Action {
    Forward(Vec<u8>),
    Quit,
}

/// Splits keyboard bytes into what goes to the shell and spindle's own commands. The prefix
/// state lives across calls because Ctrl-b and the key after it can arrive in separate reads.
#[derive(Default)]
pub struct Input {
    prefixed: bool,
}

impl Input {
    pub fn feed(&mut self, bytes: &[u8]) -> Action {
        let mut forward = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if !self.prefixed {
                if b == PREFIX {
                    self.prefixed = true;
                } else {
                    forward.push(b);
                }
                continue;
            }
            self.prefixed = false;
            match b {
                b'q' => return Action::Quit,
                // Advertised in the menu but not implemented yet; swallowed so they don't leak to the shell.
                b'n' | b's' => {}
                PREFIX => forward.push(PREFIX),
                other => forward.extend([PREFIX, other]),
            }
        }
        Action::Forward(forward)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_bytes_pass_through() {
        assert_eq!(
            Input::default().feed(b"ls\r"),
            Action::Forward(b"ls\r".to_vec())
        );
    }

    #[test]
    fn prefix_q_quits_even_when_split_across_reads() {
        let mut input = Input::default();
        assert_eq!(input.feed(&[PREFIX]), Action::Forward(vec![]));
        assert_eq!(input.feed(b"q"), Action::Quit);
    }

    #[test]
    fn double_prefix_sends_one_literal_ctrl_b() {
        assert_eq!(
            Input::default().feed(&[PREFIX, PREFIX]),
            Action::Forward(vec![PREFIX])
        );
    }

    #[test]
    fn unbound_key_after_prefix_forwards_both() {
        assert_eq!(
            Input::default().feed(&[PREFIX, b'x', b'y']),
            Action::Forward(vec![PREFIX, b'x', b'y'])
        );
    }

    #[test]
    fn placeholder_commands_are_swallowed() {
        assert_eq!(
            Input::default().feed(&[PREFIX, b'n', PREFIX, b's']),
            Action::Forward(vec![])
        );
    }
}
