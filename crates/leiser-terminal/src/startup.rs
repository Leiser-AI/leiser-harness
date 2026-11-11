//! `ConPTY`'s startup handshake on Windows.
//!
//! `portable-pty` creates every pseudo console with
//! `PSEUDOCONSOLE_INHERIT_CURSOR`. With that flag the console host first
//! asks where the cursor is (`ESC[6n`, a device status report) and starts
//! the shell only once the answer (`ESC[row;colR`) arrives on its input.
//! The terminal answers this one query itself, because whoever reads the
//! output cannot reliably do it: the desktop app's UI learns which terminal
//! to write to only after the query was sent. The query is taken out of
//! the output, so the receiver does not answer it a second time.
//!
//! The answer is the top-left cell: a new shell starts on a cleared screen.

/// The cursor position query, as the console host sends it.
const QUERY: &[u8] = b"\x1b[6n";
/// The cursor is in row 1, column 1.
pub(crate) const REPLY: &[u8] = b"\x1b[1;1R";

/// Finds the startup query in a terminal's output, even when it is split
/// across reads.
#[derive(Debug, Default)]
pub(crate) struct StartupQuery {
    /// Output that may be the start of the query; held back until the next
    /// read shows whether it is.
    held: Vec<u8>,
    answered: bool,
}

/// One read of output, after the startup query was looked for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Filtered {
    /// The output to pass on, without the query.
    pub output: Vec<u8>,
    /// Whether the query was found in this read and needs [`REPLY`] now.
    pub answer: bool,
}

impl StartupQuery {
    /// Takes the next read of output.
    pub(crate) fn filter(&mut self, read: &[u8]) -> Filtered {
        if self.answered {
            return Filtered {
                output: read.to_vec(),
                answer: false,
            };
        }
        let mut output = std::mem::take(&mut self.held);
        output.extend_from_slice(read);
        if let Some(at) = output
            .windows(QUERY.len())
            .position(|window| window == QUERY)
        {
            self.answered = true;
            output.drain(at..at + QUERY.len());
            return Filtered {
                output,
                answer: true,
            };
        }
        let partial = (1..QUERY.len())
            .rev()
            .find(|&length| output.ends_with(&QUERY[..length]))
            .unwrap_or(0);
        self.held = output.split_off(output.len() - partial);
        Filtered {
            output,
            answer: false,
        }
    }

    /// The output still held back, once the output ended.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.held
    }
}

#[cfg(test)]
mod tests {
    use super::{Filtered, StartupQuery};

    fn passed(output: &[u8]) -> Filtered {
        Filtered {
            output: output.to_vec(),
            answer: false,
        }
    }

    #[test]
    fn the_query_is_answered_and_taken_out_of_the_output() {
        let mut startup = StartupQuery::default();
        assert_eq!(
            startup.filter(b"\x1b[?9001h\x1b[6nPS> "),
            Filtered {
                output: b"\x1b[?9001hPS> ".to_vec(),
                answer: true,
            }
        );
    }

    #[test]
    fn a_query_split_across_reads_is_found() {
        let mut startup = StartupQuery::default();
        assert_eq!(startup.filter(b"\x1b"), passed(b""));
        assert_eq!(startup.filter(b"[6"), passed(b""));
        assert_eq!(
            startup.filter(b"nPS> "),
            Filtered {
                output: b"PS> ".to_vec(),
                answer: true,
            }
        );
    }

    #[test]
    fn output_that_only_looks_like_the_start_of_the_query_is_passed_on() {
        let mut startup = StartupQuery::default();
        assert_eq!(startup.filter(b"a\x1b["), passed(b"a"));
        assert_eq!(startup.filter(b"0m"), passed(b"\x1b[0m"));
    }

    #[test]
    fn only_the_first_query_is_answered() {
        let mut startup = StartupQuery::default();
        assert!(startup.filter(b"\x1b[6n").answer);
        assert_eq!(startup.filter(b"\x1b[6n"), passed(b"\x1b[6n"));
    }

    #[test]
    fn held_back_output_comes_out_when_the_output_ends() {
        let mut startup = StartupQuery::default();
        assert_eq!(startup.filter(b"bye\x1b["), passed(b"bye"));
        assert_eq!(startup.finish(), b"\x1b[".to_vec());
    }
}
