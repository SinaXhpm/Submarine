// What of a command or a file the eye can't see. The AI's tool cards
// show these characters as escapes such as ⟨U+202E⟩, so the text a user
// approves is the text that runs, and a paste leaves them out (stripHidden).
//
// Hidden:
//  - control characters: C0 but tab and line feed (so CR and ESC are hidden),
//    DEL and C1;
//  - bidi controls: U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069;
//  - zero-width and other invisible characters: U+00AD, U+200B-U+200D (but
//    U+200C between two Arabic-script letters is the Persian half-space, part
//    of how a word is spelled, so there it isn't hidden),
//    U+2060-U+2064, U+FEFF, and the rest of Unicode's default-ignorable ones:
//    the combining grapheme joiner U+034F, the Hangul fillers (U+115F, U+1160,
//    U+3164, U+FFA0), U+17B4, U+17B5, the Mongolian selectors and separator
//    U+180B-U+180F, U+2065, the old format characters U+206A-U+206F,
//    U+FFF0-U+FFF8, the shorthand and musical format controls;
//  - variation selectors (U+FE00-U+FE0F, U+E0100-U+E01EF), which can carry
//    data unseen; but VS15 and VS16 right after an emoji choose how it looks
//    (⚠️), so there they are part of what shows;
//  - Unicode Tag characters U+E0000-U+E007F, and the rest of that block up to
//    U+E0FFF;
//  - spaces other than the ASCII one (U+00A0, U+1680, U+2000-U+200A, U+202F,
//    U+205F, U+3000): they look like a space, but a shell takes them as part
//    of a word;
//  - the line and paragraph separators U+2028 and U+2029: a page may break
//    the line there, a shell doesn't;
//  - the braille blank U+2800, the interlinear annotation marks
//    U+FFF9-U+FFFB and the Egyptian format controls U+13430-U+1343F, which
//    draw as nothing or a blank;
//  - noncharacters, and half a surrogate pair without its other half.
//
// A run of 16 or more spaces or tabs is shown as ⟨40 spaces⟩: it can push the
// rest of a line out of view, and the Library asks before such a paste too.
// Two or more empty or blank-only lines in a row are shown as one line, such
// as ⟨40 empty lines⟩: they can push the rest of a command out of view.

// Hidden code points from U+00A0 up, as sorted ranges: first, last, first, last, …
const HIDDEN = [
  0x00a0, 0x00a0, // no-break space
  0x00ad, 0x00ad, // soft hyphen
  0x034f, 0x034f, // combining grapheme joiner
  0x061c, 0x061c, // Arabic letter mark
  0x115f, 0x1160, // Hangul fillers
  0x1680, 0x1680, // Ogham space mark
  0x17b4, 0x17b5, // Khmer inherent vowels
  0x180b, 0x180f, // Mongolian variation selectors and vowel separator
  0x2000, 0x200f, // spaces, zero-width space, joiners, LRM, RLM
  0x2028, 0x202f, // line and paragraph separators, bidi embeddings and overrides, narrow no-break space
  0x205f, 0x206f, // medium math space, word joiner, invisible operators, bidi isolates, old format characters
  0x2800, 0x2800, // braille blank
  0x3000, 0x3000, // ideographic space
  0x3164, 0x3164, // Hangul filler
  0xd800, 0xdfff, // half a surrogate pair (a whole pair is read as one code point)
  0xfdd0, 0xfdef, // noncharacters
  0xfe00, 0xfe0f, // variation selectors
  0xfeff, 0xfeff, // zero-width no-break space (byte order mark)
  0xffa0, 0xffa0, // halfwidth Hangul filler
  0xfff0, 0xfffb, // reserved, interlinear annotation
  0x13430, 0x1343f, // Egyptian hieroglyph format controls
  0x1bca0, 0x1bca3, // shorthand format controls
  0x1d173, 0x1d17a, // musical symbol format controls
  0xe0000, 0xe0fff, // tags, variation selectors 17-256, reserved
];

// From this many spaces and tabs in a row, a run is shown by its length.
export const BLANK_RUN = 16;
const LONG_BLANKS = new RegExp(`[ \\t]{${BLANK_RUN}}`);
// Two lines in a row that are empty or hold only spaces and tabs. A line feed
// at the end of the text ends its last line; it doesn't start an empty one.
const BLANK_LINES = /(?:^|\n)[ \t]*\n(?:[ \t]*\n|[ \t]+$)/;
// A character other than printable ASCII, tab and line feed. Text without one
// (most commands and files) needs no look at each character.
const NOT_PLAIN = /[^\t\n\x20-\x7e]/;

// Letters, marks and digits: what can pass for an ASCII letter or digit.
const LETTER = /[\p{L}\p{M}\p{N}]/u;
// An emoji, for the variation selector after it. Built here and not written as
// a literal: an engine without this property would reject the whole file.
const EMOJI: RegExp | null = (() => {
  try { return new RegExp("\\p{Extended_Pictographic}", "u"); } catch { return null; }
})();

function inTable(cp: number): boolean {
  let lo = 0;
  let hi = HIDDEN.length / 2 - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (cp < HIDDEN[2 * mid]) hi = mid - 1;
    else if (cp > HIDDEN[2 * mid + 1]) lo = mid + 1;
    else return true;
  }
  return false;
}

// Letters and marks of the Arabic script, which Persian, Urdu and Kurdish are
// written in: the half-space sits between two of them. The same ranges as the
// backend's command check, so a card shows what that check let through.
const ARABIC = [
  0x0610, 0x061a, 0x0620, 0x065f, 0x066e, 0x06d3, 0x06d5, 0x06dc, 0x06df, 0x06e8,
  0x06ea, 0x06ef, 0x06fa, 0x06fc, 0x06ff, 0x06ff, 0x0750, 0x077f, 0x0870, 0x0887,
  0x0889, 0x088e, 0x0898, 0x08e1, 0x08e3, 0x08ff, 0xfb50, 0xfbb1, 0xfbd3, 0xfd3d,
  0xfd50, 0xfd8f, 0xfd92, 0xfdc7, 0xfdf0, 0xfdfb, 0xfe70, 0xfe74, 0xfe76, 0xfefc,
];
const arabicLetter = (cp: number) => {
  if (cp < 0x0610 || cp > 0xfefc) return false;
  for (let i = 0; i < ARABIC.length; i += 2) if (cp >= ARABIC[i] && cp <= ARABIC[i + 1]) return true;
  return false;
};

/**
 * Whether code point `cp` is hidden. `prev` and `next` are the code points
 * around it (-1 at either end).
 */
export function isHidden(cp: number, prev: number, next = -1): boolean {
  if (cp < 0x20) return cp !== 0x09 && cp !== 0x0a;
  if (cp < 0x7f) return false;
  if (cp <= 0x9f) return true; // DEL and C1
  if ((cp & 0xfffe) === 0xfffe) return true; // U+FFFE, U+FFFF and their like in every plane
  if (!inTable(cp)) return false;
  if (cp === 0x200c) return !(arabicLetter(prev) && arabicLetter(next));
  return !((cp === 0xfe0e || cp === 0xfe0f) && prev >= 0x80 && !!EMOJI?.test(String.fromCodePoint(prev)));
}

/** The code point after the one at `i`, which is `width` units long; -1 at the end. */
const nextOf = (text: string, i: number, width: number) => text.codePointAt(i + width) ?? -1;

// Spaces other than the ASCII one; a paste turns them into it.
const isOtherSpace = (cp: number) =>
  cp === 0xa0 || cp === 0x1680 || (cp >= 0x2000 && cp <= 0x200a) || cp === 0x202f || cp === 0x205f || cp === 0x3000;

const escapeOf = (cp: number) => `⟨U+${cp.toString(16).toUpperCase().padStart(4, "0")}⟩`;

const count = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

// ⟨40 spaces⟩, ⟨20 tabs⟩, or for a run of both ⟨24 blanks: 16 spaces, 8 tabs⟩.
function blanksOf(run: string): string {
  let tabs = 0;
  for (let i = 0; i < run.length; i++) if (run.charCodeAt(i) === 0x09) tabs++;
  const spaces = run.length - tabs;
  if (tabs === 0) return `⟨${count(spaces, "space")}⟩`;
  if (spaces === 0) return `⟨${count(tabs, "tab")}⟩`;
  return `⟨${run.length} blanks: ${count(spaces, "space")}, ${count(tabs, "tab")}⟩`;
}

/**
 * Where a run of two or more blank lines (empty, or only spaces and tabs)
 * that starts at `start`, the start of a line, ends: after the last one's
 * blanks, before its line feed. `start` when there is no such run there.
 */
function blankLinesEnd(text: string, start: number): number {
  let lines = 0;
  let end = start;
  for (let i = start; i < text.length; i++) {
    const from = i;
    while (i < text.length && (text.charCodeAt(i) === 0x20 || text.charCodeAt(i) === 0x09)) i++;
    // A line ends at a line feed; at the end of the text, only one with blanks is a line.
    if (i < text.length ? text.charCodeAt(i) !== 0x0a : i === from) break;
    lines++;
    end = i;
  }
  return lines >= 2 ? end : start;
}

// ⟨40 empty lines⟩, or ⟨40 blank lines⟩ when some hold spaces or tabs.
function linesOf(raw: string): string {
  let lines = 1;
  let blanks = false;
  for (let i = 0; i < raw.length; i++) {
    if (raw.charCodeAt(i) === 0x0a) lines++;
    else blanks = true;
  }
  return `⟨${lines} ${blanks ? "blank" : "empty"} lines⟩`;
}

/** A piece of text as shown: plain, or escapes standing for what is in `raw`. */
export type Segment =
  | { kind: "text"; text: string }
  | { kind: "hidden"; text: string; raw: string } // ⟨U+200B⟩⟨U+202E⟩: one escape per character, a run in one piece
  | { kind: "blanks"; text: string; raw: string }; // ⟨40 spaces⟩, or ⟨40 empty lines⟩ on a line of its own

/**
 * `text` cut into what is shown as it is and what is shown as escapes. Joined
 * back (`text` of plain pieces, `raw` of the others) it is `text` again.
 * `blankRuns: false` leaves runs of blanks and of blank lines as they are: in
 * a file they are mostly indentation and spacing.
 */
export function segments(text: string, { blankRuns = true }: { blankRuns?: boolean } = {}): Segment[] {
  const lineRuns = blankRuns && BLANK_LINES.test(text);
  if (!NOT_PLAIN.test(text) && !lineRuns && !(blankRuns && LONG_BLANKS.test(text))) return text ? [{ kind: "text", text }] : [];
  const out: Segment[] = [];
  let plain = 0; // where the text not yet in `out` starts
  let blanks = -1; // where the current run of spaces and tabs starts
  let prev = -1;
  let lineStart = lineRuns; // at the start of a line, where a run of blank lines can start
  const endBlanks = (end: number) => {
    if (!blankRuns || end - blanks < BLANK_RUN) return;
    if (plain < blanks) out.push({ kind: "text", text: text.slice(plain, blanks) });
    const raw = text.slice(blanks, end);
    out.push({ kind: "blanks", text: blanksOf(raw), raw });
    plain = end;
  };
  for (let i = 0; i < text.length; ) {
    if (lineStart) {
      lineStart = false;
      // The run's escape takes the place of its lines; the line feed after
      // the last one stays, so what follows starts a line of its own.
      const end = blankLinesEnd(text, i);
      if (end > i) {
        if (plain < i) out.push({ kind: "text", text: text.slice(plain, i) });
        const raw = text.slice(i, end);
        out.push({ kind: "blanks", text: linesOf(raw), raw });
        prev = raw.charCodeAt(raw.length - 1);
        plain = i = end;
        continue;
      }
    }
    const cp = text.codePointAt(i)!;
    const width = cp > 0xffff ? 2 : 1;
    if (cp === 0x20 || cp === 0x09) {
      if (blanks < 0) blanks = i;
    } else {
      if (blanks >= 0) {
        endBlanks(i);
        blanks = -1;
      }
      if (cp === 0x0a) lineStart = lineRuns;
      if (isHidden(cp, prev, nextOf(text, i, width))) {
        if (plain < i) out.push({ kind: "text", text: text.slice(plain, i) });
        const raw = text.slice(i, i + width);
        const last = out[out.length - 1];
        if (last?.kind === "hidden") {
          last.text += escapeOf(cp);
          last.raw += raw;
        } else {
          out.push({ kind: "hidden", text: escapeOf(cp), raw });
        }
        plain = i + width;
      }
    }
    prev = cp;
    i += width;
  }
  if (blanks >= 0) endBlanks(text.length);
  if (plain < text.length) out.push({ kind: "text", text: text.slice(plain) });
  return out;
}

export interface Inspection {
  /** A hidden character (see the top of this file). */
  hidden: boolean;
  /** A letter, mark or digit outside ASCII: it can pass for an ASCII one. */
  nonAsciiLetters: boolean;
  /** A run of BLANK_RUN or more spaces and tabs. */
  longBlanks: boolean;
  /** Two or more empty or blank-only lines in a row; looked for only when asked. */
  blankLines: boolean;
}

/**
 * What in `text` needs a warning. `blankLines`: look for runs of blank lines
 * too, as a text drawn with segments' `blankRuns` shows them as one line.
 */
export function inspect(text: string, { blankLines = false }: { blankLines?: boolean } = {}): Inspection {
  const lineRuns = blankLines && BLANK_LINES.test(text);
  if (!NOT_PLAIN.test(text)) return { hidden: false, nonAsciiLetters: false, longBlanks: LONG_BLANKS.test(text), blankLines: lineRuns };
  let hidden = false;
  let nonAsciiLetters = false;
  let longBlanks = false;
  let run = 0;
  let prev = -1;
  for (let i = 0; i < text.length; ) {
    const cp = text.codePointAt(i)!;
    if (cp === 0x20 || cp === 0x09) {
      if (++run >= BLANK_RUN) longBlanks = true;
    } else {
      run = 0;
      if (isHidden(cp, prev, nextOf(text, i, cp > 0xffff ? 2 : 1))) hidden = true;
      // VS15 and VS16 that get here belong to an emoji; they count as marks.
      else if (!nonAsciiLetters && cp >= 0x80 && cp !== 0xfe0e && cp !== 0xfe0f && LETTER.test(String.fromCodePoint(cp))) nonAsciiLetters = true;
    }
    prev = cp;
    i += cp > 0xffff ? 2 : 1;
  }
  return { hidden, nonAsciiLetters, longBlanks, blankLines: lineRuns };
}

/**
 * `text` for a paste: without its hidden characters. Line breaks stay as line
 * feeds (CR LF, a lone CR and the Unicode line and paragraph separators), and
 * a space other than the ASCII one becomes one, so what lands is what was
 * seen. Tabs and runs of blanks stay.
 */
export function stripHidden(text: string): string {
  if (!NOT_PLAIN.test(text)) return text;
  let out = "";
  let plain = 0;
  let prev = -1;
  for (let i = 0; i < text.length; ) {
    const cp = text.codePointAt(i)!;
    const width = cp > 0xffff ? 2 : 1;
    if (isHidden(cp, prev, nextOf(text, i, width))) {
      out += text.slice(plain, i);
      if (cp === 0x0d) {
        if (text.charCodeAt(i + 1) !== 0x0a) out += "\n";
      } else if (cp === 0x2028 || cp === 0x2029) {
        out += "\n";
      } else if (isOtherSpace(cp)) {
        out += " ";
      }
      plain = i + width;
    }
    prev = cp;
    i += width;
  }
  return plain === 0 ? text : out + text.slice(plain);
}
