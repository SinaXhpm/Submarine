//! A filter for what a terminal session shows (SSH, docker exec, telnet).
//!
//! xterm.js repeats some control sequences once per unit of the number in them:
//! insert and delete lines (`CSI L`, `CSI M`), scroll up and down (`CSI S`,
//! `CSI T`), tab forward and back (`CSI I`, `CSI Z`), repeat the last character
//! (`CSI b`). The number comes from the server, and the page has one thread for
//! every tab: 7 KB of `ESC [ 9999 S` stopped the whole window for 48 seconds. The
//! newest xterm.js releases (5.5, 6.0) have the same loops, so the numbers are
//! cut down here, before the output goes to the page.
//!
//! [`CsiLimiter`] copies the stream, and what it changes is this:
//!   - Every number inside a CSI sequence is cut off at four digits (9999). No
//!     real program prints a bigger one.
//!   - The count of the sequences above is cut to the screen's size, past which
//!     they do what the screen's size does. `CSI @`, `P` and `X` aren't among
//!     them: xterm stops those at the end of the line, whatever the number.
//!   - Lines, repeats and full resets (`ESC c`) are held to a budget of one line
//!     per byte of the stream and per millisecond, for what the two above leave:
//!     a stream of `ESC [ 110 S` on a 110-row screen, or two bytes that rebuild
//!     the whole screen. A program that draws what it scrolled in sends far more
//!     bytes than lines, and one that clears the screen now and then has the time.
//!
//! It works on a stream, so a sequence may come split across reads: the state is
//! kept between calls. Everything else (text, other escape sequences, strings)
//! passes through untouched.

use std::io::Write;
use std::time::Instant;

/// How many digits a number in a CSI sequence may keep (leading zeros are free).
const MAX_DIGITS: u8 = 4;

/// What counts are cut to while the terminal's size isn't known.
const UNKNOWN_ROWS: u16 = 500;
const UNKNOWN_COLS: u16 = 1000;

/// The lines a stream may spend at once; it earns one per byte and one per
/// millisecond. A line costs the page 4 to 15 µs by the size of the screen, so
/// a stream of them costs 4 to 15 ms per KiB, where cutting to the screen alone
/// leaves 35 on a 120x40 screen and past 150 on a large one, and a full reset 95
/// and past 350. The clock can't earn more than 1.5% of the page's time, and
/// it's what keeps `ESC c` working: on bytes alone a slow loop that clears the
/// screen would run out for good. 100 rows can have ten of those a second.
const WORK_MAX: u32 = 16 * 1024;
/// How many repeated characters (`CSI b`) count as one line of the budget: each
/// costs about 0.2 µs, and a whole line filled this way is a few bytes.
const REPEATS_PER_LINE: u16 = 64;

/// CAN: ends a sequence, for xterm too.
const CAN: u8 = 0x18;

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
    /// The sequence's first number, the only one the counted sequences read.
    first: u16,
    /// `first` isn't written out yet: what it counts shows at the final byte.
    /// Only the digits wait: control characters in the middle of a sequence
    /// run there in xterm too, so they go on ahead of them.
    holding: bool,
    /// The sequence has a private marker or an intermediate: xterm doesn't take
    /// it for one of the counted ones.
    marked: bool,
    /// The terminal's size; 0 until it's known.
    cols: u16,
    rows: u16,
    /// Lines spent and not yet earned back (`WORK_MAX`).
    work: u32,
    /// When the last read came, for what the time since has earned.
    last_read: Option<Instant>,
}

impl CsiLimiter {
    /// The terminal's current size (`PtySize`). A program learns of a new size
    /// after this does, so a number it means can't be past this.
    pub fn set_size(&mut self, cols: u32, rows: u32) {
        self.cols = u16::try_from(cols).unwrap_or(u16::MAX);
        self.rows = u16::try_from(rows).unwrap_or(u16::MAX);
    }

    /// Appends `input` to `out`, with the numbers in CSI sequences cut down.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        self.filter_at(Instant::now(), input, out);
    }

    /// `filter` for a read that came at `now`.
    fn filter_at(&mut self, now: Instant, input: &[u8], out: &mut Vec<u8>) {
        out.reserve(input.len());
        let waited = self.last_read.map_or(0, |at| now.saturating_duration_since(at).as_millis());
        self.last_read = Some(now);
        self.work = self.work.saturating_sub(u32::try_from(input.len() as u128 + waited).unwrap_or(u32::MAX));
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
        self.first = 0;
        self.holding = true;
        self.marked = false;
    }

    /// Write out the number held back.
    fn release(&mut self, out: &mut Vec<u8>) {
        if self.holding {
            self.holding = false;
            if self.digits > 0 {
                let _ = write!(out, "{}", self.first);
            }
        }
    }

    fn rows(&self) -> u16 {
        if self.rows == 0 { UNKNOWN_ROWS } else { self.rows }
    }

    fn cols(&self) -> u16 {
        if self.cols == 0 { UNKNOWN_COLS } else { self.cols }
    }

    /// How much of `lines` the budget has room for, spent.
    fn spend(&mut self, lines: u16) -> u16 {
        let lines = lines.min(u16::try_from(WORK_MAX.saturating_sub(self.work)).unwrap_or(u16::MAX));
        self.work += u32::from(lines);
        lines
    }

    /// What xterm may run of a sequence ending in `last` that asks for `wanted`.
    fn allowed(&mut self, last: u8, wanted: u16) -> u16 {
        match last {
            b'L' | b'M' | b'S' | b'T' => self.spend(wanted.min(self.rows())).max(1),
            // Each stop moves at least a column, and costs little.
            b'I' | b'Z' => wanted.min(self.cols()),
            // Past a screenful the screen shows what it would have shown.
            b'b' => {
                let count = wanted.min(self.cols().saturating_mul(self.rows()));
                count.min(self.spend(count.div_ceil(REPEATS_PER_LINE)) * REPEATS_PER_LINE).max(1)
            }
            _ => wanted,
        }
    }

    /// The final byte of a CSI sequence.
    fn finish(&mut self, last: u8, out: &mut Vec<u8>) {
        self.state = State::Ground;
        let wanted = self.first.max(1);
        let count = if self.marked { wanted } else { self.allowed(last, wanted) };
        if count == wanted {
            self.release(out);
            out.push(last);
        } else if self.holding {
            self.first = count;
            self.release(out);
            out.push(last);
        } else {
            // The number went out before a second one, which no real program
            // sends here. Too late to cut it, so the sequence is dropped.
            out.push(CAN);
        }
    }

    /// A byte of 0x80 or up after an ESC or inside a sequence. A character ends
    /// what was going on, but xterm drops a byte that isn't valid UTF-8 and
    /// carries on, so a number after one would get past this filter: CAN ends
    /// it for xterm too.
    fn end_at_high_byte(&mut self, b: u8, out: &mut Vec<u8>) {
        out.push(CAN);
        self.state = State::Ground;
        self.step(b, out);
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
            State::Esc => match b {
                b'[' => {
                    out.push(b);
                    self.start_csi();
                }
                // A full reset, all of a screen of the budget or nothing: past
                // it the ESC went out, CAN ends it, and what was earned stays
                // for the next one.
                b'c' => {
                    let rows = u32::from(self.rows());
                    if WORK_MAX.saturating_sub(self.work) >= rows {
                        self.work += rows;
                        out.push(b);
                    } else {
                        out.push(CAN);
                    }
                    self.state = State::Ground;
                }
                // Control characters run between the ESC and what follows it,
                // and another ESC starts again.
                0x00..=0x17 | 0x19 | 0x1b..=0x1f | 0x7f => out.push(b),
                0x80..=0xff => self.end_at_high_byte(b, out),
                _ => {
                    out.push(b);
                    self.state = State::Ground;
                }
            },
            State::Csi => match b {
                b'0'..=b'9' => {
                    if self.digits == 0 && b == b'0' {
                        // A leading zero keeps the value as it is.
                        out.push(b);
                    } else if self.digits < MAX_DIGITS {
                        self.digits += 1;
                        if self.holding {
                            self.first = self.first * 10 + u16::from(b - b'0');
                        } else {
                            out.push(b);
                        }
                    }
                    // Past the limit the digit is dropped.
                }
                b';' | b':' => {
                    self.release(out);
                    out.push(b);
                    self.digits = 0;
                }
                // Intermediates, and the private markers `<` `=` `>` `?`.
                0x20..=0x2f | 0x3c..=0x3f => {
                    self.release(out);
                    out.push(b);
                    self.digits = 0;
                    self.marked = true;
                }
                // The final byte ends it.
                0x40..=0x7e => self.finish(b, out),
                // A new ESC cancels it and starts another sequence; CAN and SUB cancel it.
                0x1b => {
                    self.release(out);
                    out.push(b);
                    self.state = State::Esc;
                }
                0x18 | 0x1a => {
                    self.release(out);
                    out.push(b);
                    self.state = State::Ground;
                }
                // The other control characters run in the middle of a sequence.
                0x00..=0x1f | 0x7f => out.push(b),
                0x80..=0xff => {
                    self.release(out);
                    self.end_at_high_byte(b, out);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        CsiLimiter::default().filter(input, &mut out);
        out
    }

    fn sized(cols: u32, rows: u32) -> CsiLimiter {
        let mut limiter = CsiLimiter::default();
        limiter.set_size(cols, rows);
        limiter
    }

    /// `input` through a limiter for a screen of that size.
    fn on_screen(cols: u32, rows: u32, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        sized(cols, rows).filter(input, &mut out);
        out
    }

    /// `input` as a pump feeds it, a read at a time, with no time for the clock
    /// to earn anything.
    fn by_reads(cols: u32, rows: u32, input: &[u8]) -> Vec<u8> {
        let mut limiter = sized(cols, rows);
        let mut out = Vec::new();
        let now = Instant::now();
        for read in input.chunks(16 * 1024) {
            limiter.filter_at(now, read, &mut out);
        }
        out
    }

    /// The same bytes fed in pieces of the given sizes.
    fn run_split(input: &[u8], cuts: &[usize]) -> Vec<u8> {
        let mut limiter = sized(120, 40);
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
            (&b"\x1b[999999999L"[..], &b"\x1b[40L"[..]),
            (b"\x1b[999999999M", b"\x1b[40M"),
            (b"\x1b[999999999S", b"\x1b[40S"),
            (b"\x1b[999999999T", b"\x1b[40T"),
            (b"\x1b[999999999I", b"\x1b[120I"),
            (b"\x1b[999999999Z", b"\x1b[120Z"),
            (b"A\x1b[999999999b", b"A\x1b[4800b"),
            (b"\x1b[1;999999999H", b"\x1b[1;9999H"),
            (b"\x1b[?999999999;5h", b"\x1b[?9999;5h"),
            (b"\x1b[1:99999999:3m", b"\x1b[1:9999:3m"),
            // The one-byte CSI, as UTF-8.
            (b"\xc2\x9b999999999L", b"\xc2\x9b40L"),
        ] {
            assert_eq!(on_screen(120, 40, given), wanted, "{:?}", String::from_utf8_lossy(given));
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
            // Counts a program means: within the screen.
            b"\x1b[S\x1b[T\x1b[L\x1b[M\x1b[1S\x1b[39T\x1b[40L\x1b[0M",
            b"\x1b[I\x1b[8I\x1b[120Z=\x1b[4799b\x1b[@\x1b[9999P\x1b[9999X",
            // Not the counted ones: a marker or an intermediate makes it another sequence.
            b"\x1b[?9999S\x1b[>9999L\x1b[9999 @\x1b[9999 A\x1b[9999'}\x1b[9999'~",
        ] {
            assert_eq!(on_screen(120, 40, input), input, "{:?}", String::from_utf8_lossy(input));
        }
    }

    #[test]
    fn leading_zeros_do_not_count() {
        assert_eq!(run(b"\x1b[0000000000005L"), b"\x1b[0000000000005L");
        // Four zeros, then five significant digits: the fifth is dropped.
        assert_eq!(run(b"\x1b[000012345H"), b"\x1b[00001234H");
        assert_eq!(on_screen(120, 40, b"\x1b[000012345L"), b"\x1b[000040L");
    }

    #[test]
    fn a_sequence_split_anywhere_comes_out_the_same() {
        let input: &[u8] = b"abc\x1b[999999999L def\x1b[1;88888888;3H \xc2\x9b777777777M \x1b[?2004h \x1b[00000001234567T \x1b[99\n99S x\x1b[99999b\x1b[9;9S\x1bc end";
        let mut whole = Vec::new();
        sized(120, 40).filter(input, &mut whole);
        for first in 0..input.len() {
            for second in 0..input.len() - first {
                assert_eq!(run_split(input, &[first, second]), whole, "cut after {first} and {second} more");
            }
        }
    }

    #[test]
    fn a_new_escape_or_cancel_ends_a_sequence() {
        // The ESC starts a new sequence; the digits after it are not a number of the first.
        assert_eq!(on_screen(120, 40, b"\x1b[12\x1b[999999999L"), b"\x1b[12\x1b[40L");
        // CAN ends it: what follows is text.
        assert_eq!(run(b"\x1b[12\x18999999999L"), b"\x1b[12\x18999999999L");
        // A UTF-8 byte ends it too, for xterm as well.
        assert_eq!(run("\x1b[12\u{e9}999999999L".as_bytes()), "\x1b[12\x18\u{e9}999999999L".as_bytes());
    }

    #[test]
    fn the_count_follows_the_screen() {
        let mut limiter = sized(120, 40);
        let mut out = Vec::new();
        limiter.filter(b"\x1b[9999S", &mut out);
        limiter.set_size(200, 66);
        limiter.filter(b"\x1b[9999S\x1b[9999I", &mut out);
        assert_eq!(out, b"\x1b[40S\x1b[66S\x1b[200I");
    }

    #[test]
    fn a_size_that_isnt_known_gets_a_fixed_cut() {
        assert_eq!(run(b"\x1b[9999S\x1b[9999I"), format!("\x1b[{UNKNOWN_ROWS}S\x1b[{UNKNOWN_COLS}I").as_bytes());
        assert_eq!(on_screen(0, 0, b"\x1b[9999S"), format!("\x1b[{UNKNOWN_ROWS}S").as_bytes());
    }

    #[test]
    fn control_characters_in_a_sequence_stay_where_they_run() {
        // A line feed inside the number: it still runs, and the number still counts.
        assert_eq!(on_screen(120, 40, b"\x1b[99\n99S"), b"\x1b[\n40S");
        assert_eq!(on_screen(120, 40, b"\x1b[1\r2S"), b"\x1b[\r12S");
        assert_eq!(on_screen(120, 40, b"\x1b[\x7f9999S"), b"\x1b[\x7f40S");
    }

    #[test]
    fn a_second_number_doesnt_get_a_count_past_the_filter() {
        // The first number has gone out by the time the final byte shows, so
        // the sequence is dropped.
        assert_eq!(on_screen(120, 40, b"\x1b[9999;1S"), b"\x1b[9999;1\x18");
        assert_eq!(on_screen(120, 40, b"\x1b[9999:1L"), b"\x1b[9999:1\x18");
        // Within the screen it stands, and nothing else is touched.
        assert_eq!(on_screen(120, 40, b"\x1b[5;1S"), b"\x1b[5;1S");
        assert_eq!(on_screen(120, 40, b"\x1b[;9999S"), b"\x1b[;9999S");
        assert_eq!(on_screen(120, 40, b"\x1b[9999;1H"), b"\x1b[9999;1H");
    }

    #[test]
    fn a_byte_xterm_drops_doesnt_hide_a_sequence() {
        for input in [
            &b"\x1b[\xff9999S"[..],
            b"\x1b[99\x8099S",
            b"\x1b\xff[9999S",
            b"\x1b\xe2[9999S",
            b"\x1b\n[9999S",
            b"\x1b\x7f[9999S",
            b"\x1b\xc2\x9b9999S",
            b"\x1b[\xc2\x9b9999S",
        ] {
            for (last, count) in xterm_runs(&on_screen(120, 40, input)) {
                assert!(last != b'S' || count <= 40, "{input:?}");
            }
        }
    }

    /// What xterm runs of `bytes` (bytes it drops, then its parser's table):
    /// each counted sequence and full reset, with its count. Read here apart
    /// from the filter, to check it against.
    fn xterm_runs(bytes: &[u8]) -> Vec<(u8, u32)> {
        // Utf8ToUtf32: what isn't valid UTF-8 is dropped, with no trace.
        let mut chars = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let lead = bytes[i];
            i += 1;
            let (more, mut c) = match lead {
                0..=0x7f => (0, u32::from(lead)),
                0xc0..=0xdf => (1, u32::from(lead & 0x1f)),
                0xe0..=0xef => (2, u32::from(lead & 0x0f)),
                0xf0..=0xf7 => (3, u32::from(lead & 0x07)),
                _ => continue,
            };
            let mut got = 0;
            while got < more && bytes.get(i).is_some_and(|b| b & 0xc0 == 0x80) {
                c = c << 6 | u32::from(bytes[i] & 0x3f);
                i += 1;
                got += 1;
            }
            let least = [0, 0x80, 0x800, 0x10000][more];
            if got == more && c >= least {
                chars.push(c);
            }
        }
        enum P {
            Ground,
            Esc,
            EscMore,
            Csi,
            CsiMore,
            Ignore,
        }
        let mut runs = Vec::new();
        let (mut state, mut first, mut number, mut params, mut marked) = (P::Ground, 0u32, true, false, false);
        for c in chars {
            match c {
                0x18 | 0x1a | 0x80..=0x9a | 0x9c..=0x9f => {
                    state = P::Ground;
                    continue;
                }
                0x1b => {
                    state = P::Esc;
                    continue;
                }
                0x9b => {
                    (state, first, number, params, marked) = (P::Csi, 0, true, false, false);
                    continue;
                }
                _ => {}
            }
            let control = c < 0x20 || c == 0x7f;
            state = match state {
                P::Esc if c == 0x5b => {
                    (first, number, params, marked) = (0, true, false, false);
                    P::Csi
                }
                P::Esc | P::EscMore | P::Csi | P::CsiMore if control => state,
                P::Esc if (0x20..=0x2f).contains(&c) => P::EscMore,
                P::Esc => {
                    if c == 0x63 {
                        runs.push((b'c', 1));
                    }
                    P::Ground
                }
                P::EscMore if (0x20..=0x2f).contains(&c) => P::EscMore,
                P::EscMore => P::Ground,
                P::Csi => match c {
                    0x30..=0x39 => {
                        if number {
                            first = first.saturating_mul(10).saturating_add(c - 0x30).min(0x7fff_ffff);
                        }
                        params = true;
                        P::Csi
                    }
                    0x3a | 0x3b => {
                        number = false;
                        params = true;
                        P::Csi
                    }
                    0x3c..=0x3f if !params => {
                        marked = true;
                        params = true;
                        P::Csi
                    }
                    0x3c..=0x3f => P::Ignore,
                    0x20..=0x2f => P::CsiMore,
                    0x40..=0x7e => {
                        if !marked && b"LMSTIZb".contains(&(c as u8)) {
                            runs.push((c as u8, first.max(1)));
                        }
                        P::Ground
                    }
                    _ => P::Ground,
                },
                P::CsiMore => match c {
                    0x20..=0x2f => P::CsiMore,
                    0x30..=0x3f => P::Ignore,
                    _ => P::Ground,
                },
                P::Ignore if (0x40..=0x7e).contains(&c) => P::Ground,
                P::Ignore => P::Ignore,
                P::Ground => P::Ground,
            };
        }
        runs
    }

    /// What `runs` asks of the budget, in lines; every count within the screen.
    fn lines_within(runs: &[(u8, u32)], cols: u32, rows: u32) -> u32 {
        let mut lines = 0;
        for &(last, count) in runs {
            match last {
                b'L' | b'M' | b'S' | b'T' => {
                    assert!(count <= rows, "{} {count}", last as char);
                    lines += count;
                }
                b'I' | b'Z' => assert!(count <= cols, "{} {count}", last as char),
                b'b' => {
                    assert!(count <= cols * rows, "b {count}");
                    lines += count.div_ceil(u32::from(REPEATS_PER_LINE));
                }
                _ => lines += rows,
            }
        }
        lines
    }

    #[test]
    fn a_stream_of_short_sequences_comes_to_little_work() {
        for (cols, rows) in [(80u32, 24u32), (120, 40), (380, 110), (760, 220)] {
            for one in [&b"\x1b[9999S"[..], b"\x1b[9999T", b"\x1b[9999L", b"\x1b[9999M", b"\x1b[;S", b"\x1b[30;2M", b"A\x1b[9999b", b"\x1bc"] {
                let stream = one.repeat(20_000);
                let runs = xterm_runs(&by_reads(cols, rows, &stream));
                let lines = lines_within(&runs, cols, rows);
                // One line per byte, the first burst, and what's left at count 1.
                let most = WORK_MAX as usize + stream.len() + runs.len() * usize::from(one != b"\x1bc");
                assert!((lines as usize) <= most, "{one:?} on {cols}x{rows}: {lines} lines");
            }
            // Tabs have no budget, only the screen.
            for one in [&b"\x1b[9999I"[..], b"\x1b[9999Z"] {
                let runs = xterm_runs(&on_screen(cols, rows, &one.repeat(5000)));
                assert_eq!(runs.len(), 5000);
                lines_within(&runs, cols, rows);
            }
        }
    }

    #[test]
    fn a_program_that_draws_what_it_scrolls_stays_within_the_budget() {
        // A pager on a large screen: a page at a time, each drawn in. Well past
        // the burst, and nothing is touched.
        let mut page = b"\x1b[1;110r\x1b[109S".to_vec();
        for row in 2..=110 {
            page.extend_from_slice(format!("\x1b[{row};1H~\x1b[K").as_bytes());
        }
        let stream = page.repeat(2000);
        assert!(by_reads(380, 110, &stream) == stream);
        // Line by line, nothing but scrolls.
        let lines = b"\x1b[S\x1b[1T".repeat(20_000);
        assert!(by_reads(120, 40, &lines) == lines);
    }

    #[test]
    fn the_budget_comes_back_with_the_bytes() {
        let mut limiter = sized(120, 40);
        let mut out = Vec::new();
        let now = Instant::now();
        limiter.filter_at(now, &b"\x1b[40S".repeat(WORK_MAX as usize / 40 + 100), &mut out);
        out.clear();
        // Spent, near enough: what comes now is cut.
        limiter.filter_at(now, b"\x1b[40S", &mut out);
        assert_eq!(out, b"\x1b[5S");
        limiter.filter_at(now, &vec![b'x'; 64 * 1024], &mut out);
        out.clear();
        limiter.filter_at(now, b"\x1b[40S\x1bc", &mut out);
        assert_eq!(out, b"\x1b[40S\x1bc");
    }

    #[test]
    fn the_budget_comes_back_with_time() {
        let mut limiter = sized(120, 40);
        let mut out = Vec::new();
        let now = Instant::now();
        limiter.filter_at(now, &b"\x1b[40S".repeat(WORK_MAX as usize / 40 + 100), &mut out);
        out.clear();
        limiter.filter_at(now + Duration::from_millis(34), b"\x1b[40S", &mut out);
        assert_eq!(out, b"\x1b[39S", "a line for each millisecond and byte");
    }

    #[test]
    fn a_slow_loop_of_resets_always_passes_and_a_flood_doesnt() {
        const ROWS: usize = 50;
        let frame: &[u8] = b"\x1bcSun Nov  1 12:00:00 UTC 2026\r\n";
        let mut limiter = sized(120, ROWS as u32);
        let mut out = Vec::new();
        let mut at = Instant::now();
        // With the budget spent to the last line, as a long run may leave it.
        limiter.filter_at(at, &b"\x1b[50S".repeat(2 * WORK_MAX as usize / ROWS), &mut out);
        // Once a second for two hours, then five times a second.
        for (every, frames) in [(1000, 7200), (200, 1000)] {
            for _ in 0..frames {
                at += Duration::from_millis(every);
                out.clear();
                limiter.filter_at(at, frame, &mut out);
                assert_eq!(out, frame);
            }
        }
        // A flood, with no time to earn anything: most go, what passes is
        // what its bytes and the budget left pay for.
        let flood = b"\x1bc".repeat(50_000);
        out.clear();
        for read in flood.chunks(16 * 1024) {
            limiter.filter_at(at, read, &mut out);
        }
        let passed = xterm_runs(&out).len();
        assert!(passed > 0 && passed * ROWS <= WORK_MAX as usize + flood.len(), "{passed}");
        assert!(passed < 50_000 / 10, "{passed}");
        // One refused takes nothing away from the next: at two bytes a reset,
        // one in 25 gets its 50 lines.
        let passed = (0..250)
            .filter(|_| {
                out.clear();
                limiter.filter_at(at, b"\x1bc", &mut out);
                out == b"\x1bc"
            })
            .count();
        assert!((10..=11).contains(&passed), "{passed}");
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
        let alphabet: &[u8] = b"\x1b\x1b[[;:0123456789999999LMSTIZbcH?>m \x18\x1a\x07\xc2\x9b\xe9\xff\x80a\n";
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
            let mut whole = Vec::new();
            sized(120, 40).filter(&input, &mut whole);
            assert!(longest_number(&whole) <= MAX_DIGITS as usize, "{input:?} -> {whole:?}");
            lines_within(&xterm_runs(&whole), 120, 40);
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
