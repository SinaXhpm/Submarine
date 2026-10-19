//! A filter for what a terminal session shows (SSH, docker exec, telnet).
//!
//! xterm.js repeats some control sequences once per unit of the number in them:
//! insert and delete lines (`CSI L`, `CSI M`), scroll up and down (`CSI S`,
//! `CSI T`), tab forward and back (`CSI I`, `CSI Z`). The number comes from the
//! server: `ESC [ 999999999 L` makes the page loop a billion times, and the
//! whole window (every tab, not only that one) freezes with it. The newest
//! xterm.js releases (5.5, 6.0) have the same loops, so the numbers are cut down
//! here, before the output goes to the page.
//!
//! [`CsiLimiter`] copies the stream and cuts every number inside a CSI sequence
//! off at four digits (9999). No real program prints a bigger one: a terminal
//! has a few hundred rows or columns, and a number past the size of the screen
//! does what 9999 does. It works on a stream, so a sequence may come split across
//! reads: the state is kept between calls. Everything outside a CSI sequence
//! (text, other escape sequences, strings) passes through untouched.

/// How many digits a number in a CSI sequence may keep (leading zeros are free).
const MAX_DIGITS: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Plain output.
    #[default]
    Ground,
    /// After an ESC.
    Esc,
    /// Inside a CSI sequence, before its final byte.
    Csi,
}

/// A stream filter; one per terminal. See the module's notes.
#[derive(Default)]
pub struct CsiLimiter {
    state: State,
    /// Digits of the current number so far, not counting its leading zeros.
    digits: u8,
    /// The last byte was 0xC2: with 0x9B next it is UTF-8 for the one-byte CSI.
    c2: bool,
}

impl CsiLimiter {
    /// Appends `input` to `out`, with the numbers in CSI sequences cut down.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        out.reserve(input.len());
        let mut i = 0;
        while i < input.len() {
            if self.state == State::Ground && !self.c2 {
                // Plain output: copy up to the next byte that can start a sequence.
                match input[i..].iter().position(|&b| b == 0x1b || b == 0xc2) {
                    None => {
                        out.extend_from_slice(&input[i..]);
                        return;
                    }
                    Some(at) => {
                        out.extend_from_slice(&input[i..i + at]);
                        i += at;
                    }
                }
            }
            self.step(input[i], out);
            i += 1;
        }
    }

    fn start_csi(&mut self) {
        self.state = State::Csi;
        self.digits = 0;
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => {
                if self.c2 {
                    self.c2 = false;
                    if b == 0x9b {
                        out.push(b);
                        self.start_csi();
                        return;
                    }
                }
                out.push(b);
                match b {
                    0x1b => self.state = State::Esc,
                    0xc2 => self.c2 = true,
                    _ => {}
                }
            }
            State::Esc => {
                out.push(b);
                self.state = match b {
                    b'[' => {
                        self.start_csi();
                        return;
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                };
            }
            State::Csi => match b {
                b'0'..=b'9' => {
                    if self.digits == 0 && b == b'0' {
                        // A leading zero keeps the value as it is.
                        out.push(b);
                    } else if self.digits < MAX_DIGITS {
                        out.push(b);
                        self.digits += 1;
                    }
                    // Past the limit the digit is dropped.
                }
                b';' | b':' => {
                    out.push(b);
                    self.digits = 0;
                }
                // Intermediates, and the private markers `<` `=` `>` `?`.
                0x20..=0x2f | 0x3c..=0x3f => {
                    out.push(b);
                    self.digits = 0;
                }
                // The final byte ends it.
                0x40..=0x7e => {
                    out.push(b);
                    self.state = State::Ground;
                }
                // A new ESC cancels it and starts another sequence; CAN and SUB cancel it.
                0x1b => {
                    out.push(b);
                    self.state = State::Esc;
                }
                0x18 | 0x1a => {
                    out.push(b);
                    self.state = State::Ground;
                }
                // The other control characters run in the middle of a sequence.
                0x00..=0x1f | 0x7f => out.push(b),
                // 0x80 and up (UTF-8) end it; see it again as plain output.
                _ => {
                    self.state = State::Ground;
                    self.step(b, out);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        CsiLimiter::default().filter(input, &mut out);
        out
    }

    /// The same bytes fed in pieces of the given sizes.
    fn run_split(input: &[u8], cuts: &[usize]) -> Vec<u8> {
        let mut limiter = CsiLimiter::default();
        let mut out = Vec::new();
        let mut at = 0;
        for &cut in cuts {
            let end = (at + cut).min(input.len());
            limiter.filter(&input[at..end], &mut out);
            at = end;
        }
        limiter.filter(&input[at..], &mut out);
        out
    }

    #[test]
    fn the_numbers_that_froze_the_page_are_cut() {
        for (given, wanted) in [
            (&b"\x1b[999999999L"[..], &b"\x1b[9999L"[..]),
            (b"\x1b[999999999M", b"\x1b[9999M"),
            (b"\x1b[999999999S", b"\x1b[9999S"),
            (b"\x1b[999999999T", b"\x1b[9999T"),
            (b"\x1b[999999999I", b"\x1b[9999I"),
            (b"\x1b[999999999Z", b"\x1b[9999Z"),
            (b"A\x1b[999999999b", b"A\x1b[9999b"),
            (b"\x1b[1;999999999H", b"\x1b[1;9999H"),
            (b"\x1b[?999999999;5h", b"\x1b[?9999;5h"),
            (b"\x1b[1:99999999:3m", b"\x1b[1:9999:3m"),
            // The one-byte CSI, as UTF-8.
            (b"\xc2\x9b999999999L", b"\xc2\x9b9999L"),
        ] {
            assert_eq!(run(given), wanted, "{:?}", String::from_utf8_lossy(given));
        }
    }

    #[test]
    fn real_sequences_and_text_pass_unchanged() {
        for input in [
            &b"plain text, ok\r\n"[..],
            b"\x1b[0m\x1b[1;31mred\x1b[m",
            b"\x1b[38;2;255;128;0mtruecolor\x1b[0m",
            b"\x1b[38:2::255:128:0m",
            b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H",
            b"\x1b[999;999H\x1b[6n",
            b"\x1b[9999;9999H",
            b"\x1b[8;50;200t",
            b"\x1b[1;24r\x1b[24;1H",
            b"\x1b]0;a title with ; and 123456789\x07",
            b"\x1bP1;2;3q#0;2;100;100;100~-\x1b\\",
            "caf\u{e9} \u{4e2d}\u{6587} \u{1f600}".as_bytes(),
            b"\xff\xfe binary \x00\x01\x02 \xc2\xa0 \xc2",
        ] {
            assert_eq!(run(input), input, "{:?}", String::from_utf8_lossy(input));
        }
    }

    #[test]
    fn leading_zeros_do_not_count() {
        assert_eq!(run(b"\x1b[0000000000005L"), b"\x1b[0000000000005L");
        // Four zeros, then five significant digits: the fifth is dropped.
        assert_eq!(run(b"\x1b[000012345L"), b"\x1b[00001234L");
    }

    #[test]
    fn a_sequence_split_anywhere_comes_out_the_same() {
        let input: &[u8] = b"abc\x1b[999999999L def\x1b[1;88888888;3H \xc2\x9b777777777M \x1b[?2004h \x1b[00000001234567T end";
        let whole = run(input);
        for first in 0..input.len() {
            for second in 0..input.len() - first {
                assert_eq!(run_split(input, &[first, second]), whole, "cut after {first} and {second} more");
            }
        }
    }

    #[test]
    fn a_new_escape_or_cancel_ends_a_sequence() {
        // The ESC starts a new sequence; the digits after it are not a number of the first.
        assert_eq!(run(b"\x1b[12\x1b[999999999L"), b"\x1b[12\x1b[9999L");
        // CAN ends it: what follows is text.
        assert_eq!(run(b"\x1b[12\x18999999999L"), b"\x1b[12\x18999999999L");
        // A UTF-8 byte ends it too.
        assert_eq!(run("\x1b[12\u{e9}999999999L".as_bytes()), "\x1b[12\u{e9}999999999L".as_bytes());
    }

    /// An independent check of the result: no run of five or more significant
    /// digits is left in the parameters of any CSI sequence.
    fn longest_number(out: &[u8]) -> usize {
        let mut worst = 0;
        let mut i = 0;
        while i < out.len() {
            let start = if out[i] == 0x1b && out.get(i + 1) == Some(&b'[') {
                Some(i + 2)
            } else if out[i] == 0xc2 && out.get(i + 1) == Some(&0x9b) {
                Some(i + 2)
            } else {
                None
            };
            let Some(mut j) = start else {
                i += 1;
                continue;
            };
            let mut run = 0usize;
            while j < out.len() {
                match out[j] {
                    b'0'..=b'9' => {
                        if run > 0 || out[j] != b'0' {
                            run += 1;
                            worst = worst.max(run);
                        }
                    }
                    b';' | b':' | 0x20..=0x2f | 0x3c..=0x3f => run = 0,
                    0x00..=0x1f | 0x7f => {
                        if out[j] == 0x1b || out[j] == 0x18 || out[j] == 0x1a {
                            break;
                        }
                    }
                    _ => break,
                }
                j += 1;
            }
            i = j.max(i + 1);
        }
        worst
    }

    #[test]
    fn random_streams_never_keep_a_long_number_and_split_the_same() {
        let alphabet: &[u8] = b"\x1b\x1b[[;:0123456789999999LMSTbH?>m \x18\x1a\x07\xc2\x9b\xe9a\n";
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3000 {
            let len = (next() % 80) as usize;
            let input: Vec<u8> = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
            let whole = run(&input);
            assert!(whole.len() <= input.len());
            assert!(longest_number(&whole) <= MAX_DIGITS as usize, "{input:?} -> {whole:?}");
            let cut = (next() as usize) % (len + 1);
            let cut2 = (next() as usize) % (len + 1);
            assert_eq!(run_split(&input, &[cut, cut2]), whole, "{input:?} cut {cut},{cut2}");
        }
    }

    #[test]
    fn plain_output_is_copied_in_one_piece() {
        let big = vec![b'x'; 1 << 20];
        assert_eq!(run(&big), big);
    }
}
