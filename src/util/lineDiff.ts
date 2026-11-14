// A line diff for the AI's write and edit cards: Myers' O(ND)
// algorithm, in its linear-space form (it splits the problem at the middle of
// the shortest edit and diffs both halves), as unified hunks with 3 lines of
// context. The page's thread runs it, so the search has a budget: past it the
// texts are "too different" instead of the page freezing.

/** Above this many lines on either side the texts aren't compared. */
export const DIFF_MAX_LINES = 20_000;
/** Nor above this many bytes (UTF-8) on either side. */
export const DIFF_MAX_BYTES = 1024 * 1024;
// Steps the search may take: each diagonal looked at and each matching line
// followed counts one. Using all of it takes 90-150 ms on a busy 2019 laptop
// for two 20,000-line files. Lines found on one side only aren't searched
// (editScript), so this only runs out when thousands of changes are among
// lines that repeat, such as blank lines and braces.
const BUDGET = 3_000_000;

export interface DiffLine {
  kind: "same" | "add" | "del";
  text: string;
  /** Line number in the old text (from 1); null for an added line. */
  oldNo: number | null;
  /** Line number in the new text; null for a removed line. */
  newNo: number | null;
  /** The text's last line, with no line break after it. */
  noEol: boolean;
}

export interface Hunk {
  /** Old lines before the hunk; the hunk starts at old line oldStart + 1. */
  oldStart: number;
  oldCount: number;
  /** New lines before the hunk. */
  newStart: number;
  newCount: number;
  lines: DiffLine[];
}

export type LineDiff =
  | {
      status: "ok";
      hunks: Hunk[]; // none: no changes
      added: number;
      removed: number;
      oldLines: number;
      newLines: number;
    }
  | { status: "too_large" } // past DIFF_MAX_LINES or DIFF_MAX_BYTES
  | { status: "too_different" }; // past the budget

/** A row to draw: a line, or the unchanged lines between hunks, folded. */
export type DiffRow = DiffLine | { kind: "fold"; count: number };

// The edit script holds one of these per line.
const SAME = 0;
const DEL = 1;
const ADD = 2;

/**
 * How `newText` differs from `oldText`, line by line, with `context` unchanged
 * lines around each change. `oldText` null is a new file: every line is added.
 * Lines end at a line feed only, so a CR at the end of a line is part of it.
 */
export function lineDiff(oldText: string | null, newText: string, context = 3): LineDiff {
  if ((oldText !== null && overBytes(oldText)) || overBytes(newText)) return { status: "too_large" };
  const a = splitLines(oldText ?? "");
  const b = splitLines(newText);
  if (a.lines.length > DIFF_MAX_LINES || b.lines.length > DIFF_MAX_LINES) return { status: "too_large" };

  const ops = oldText === null ? new Array<number>(b.lines.length).fill(ADD) : editScript(a, b);
  if (ops === null) return { status: "too_different" };

  let added = 0;
  let removed = 0;
  for (const op of ops) {
    if (op === ADD) added++;
    else if (op === DEL) removed++;
  }
  return {
    status: "ok",
    hunks: added + removed > 0 ? hunksOf(ops, a, b, context) : [],
    added,
    removed,
    oldLines: a.lines.length,
    newLines: b.lines.length,
  };
}

/** The rows to draw: each hunk's lines, and the unchanged lines around them folded. */
export function diffRows(diff: Extract<LineDiff, { status: "ok" }>): DiffRow[] {
  const rows: DiffRow[] = [];
  let at = 0; // old lines accounted for
  for (const h of diff.hunks) {
    if (h.oldStart > at) rows.push({ kind: "fold", count: h.oldStart - at });
    for (const line of h.lines) rows.push(line);
    at = h.oldStart + h.oldCount;
  }
  if (diff.hunks.length > 0 && diff.oldLines > at) rows.push({ kind: "fold", count: diff.oldLines - at });
  return rows;
}

interface Split {
  lines: string[];
  eol: boolean; // the text ends with a line break (an empty text counts as one)
}

function splitLines(text: string): Split {
  if (text === "") return { lines: [], eol: true };
  const lines = text.split("\n");
  const eol = lines[lines.length - 1] === "";
  if (eol) lines.pop();
  return { lines, eol };
}

// Whether `text` takes more than DIFF_MAX_BYTES in UTF-8, counted without
// encoding it.
function overBytes(text: string): boolean {
  if (text.length > DIFF_MAX_BYTES) return true;
  if (text.length * 3 <= DIFF_MAX_BYTES) return false;
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    // A surrogate pair is 4 bytes: 2 for each half.
    bytes += c < 0x80 ? 1 : c < 0x800 || (c >= 0xd800 && c <= 0xdfff) ? 2 : 3;
    if (bytes > DIFF_MAX_BYTES) return true;
  }
  return false;
}

// A shortest edit script from `a` to `b`, one op per line; null when the
// budget ran out.
function editScript(a: Split, b: Split): number[] | null {
  // Equal lines get equal numbers. The last line without a line break gets
  // its own, so a change of only that break shows.
  const ids = new Map<string, number>();
  const toIds = ({ lines, eol }: Split) => {
    const out = new Int32Array(lines.length);
    for (let i = 0; i < lines.length; i++) {
      const key = !eol && i === lines.length - 1 ? `\n${lines[i]}` : lines[i];
      let id = ids.get(key);
      if (id === undefined) {
        id = ids.size;
        ids.set(key, id);
      }
      out[i] = id;
    }
    return out;
  };
  const x = toIds(a);
  const y = toIds(b);

  // A line found on one side only can't be in common. The search leaves it
  // out, as GNU diff does, and it goes back in as removed or added: the edit
  // is as short, and new lines in place of old ones, the usual change, leave
  // little to search.
  const inX = new Uint8Array(ids.size);
  const inY = new Uint8Array(ids.size);
  for (const id of x) inX[id] = 1;
  for (const id of y) inY[id] = 1;
  const keptX = x.filter((id) => inY[id] === 1);
  const keptY = y.filter((id) => inX[id] === 1);
  const size = keptX.length + keptY.length + 4;
  const search = { v1: new Int32Array(size), v2: new Int32Array(size), left: BUDGET };
  const kept: number[] = [];
  if (!diffPart(keptX, 0, keptX.length, keptY, 0, keptY.length, kept, search)) return null;

  // Each line left out goes back before the next kept line of its side.
  const ops: number[] = [];
  let i = 0;
  let j = 0;
  for (const op of kept) {
    for (; i < x.length && !inY[x[i]]; i++) ops.push(DEL);
    if (op !== DEL) for (; j < y.length && !inX[y[j]]; j++) ops.push(ADD);
    ops.push(op);
    if (op !== ADD) i++;
    if (op !== DEL) j++;
  }
  for (; i < x.length; i++) ops.push(DEL);
  for (; j < y.length; j++) ops.push(ADD);
  return ops;
}

// The search's two arrays, big enough for the whole diff and shared by all its
// steps (one runs at a time), and the budget left.
interface Search {
  v1: Int32Array;
  v2: Int32Array;
  left: number;
}

// Appends the edit script of a[aLo, aHi) to b[bLo, bHi) to `ops`. False when
// the budget ran out.
function diffPart(
  a: Int32Array, aLo: number, aHi: number,
  b: Int32Array, bLo: number, bHi: number,
  ops: number[], search: Search,
): boolean {
  // Lines the same at the start and at the end need no search.
  while (aLo < aHi && bLo < bHi && a[aLo] === b[bLo]) {
    ops.push(SAME);
    aLo++;
    bLo++;
  }
  let tail = 0;
  while (aLo < aHi && bLo < bHi && a[aHi - 1] === b[bHi - 1]) {
    aHi--;
    bHi--;
    tail++;
  }
  if (aLo === aHi) {
    for (let j = bLo; j < bHi; j++) ops.push(ADD);
  } else if (bLo === bHi) {
    for (let i = aLo; i < aHi; i++) ops.push(DEL);
  } else {
    const mid = middle(a, aLo, aHi, b, bLo, bHi, search);
    if (mid === null) return false;
    if (mid === NO_COMMON_LINE) {
      for (let i = aLo; i < aHi; i++) ops.push(DEL);
      for (let j = bLo; j < bHi; j++) ops.push(ADD);
    } else if (
      !diffPart(a, aLo, mid[0], b, bLo, mid[1], ops, search) ||
      !diffPart(a, mid[0], aHi, b, mid[1], bHi, ops, search)
    ) {
      return false;
    }
  }
  for (let t = 0; t < tail; t++) ops.push(SAME);
  return true;
}

const NO_COMMON_LINE = "none";

// A point on a shortest edit path from (aLo, bLo) to (aHi, bHi), found where
// a search from the start and one from the end meet (Myers 1986, 4b; laid out
// as in diff-match-patch's bisect). The two ranges start and end with
// different lines, so the point is never a corner. NO_COMMON_LINE: the ranges
// share no line. Null: the budget ran out.
function middle(
  a: Int32Array, aLo: number, aHi: number,
  b: Int32Array, bLo: number, bHi: number,
  search: Search,
): [number, number] | typeof NO_COMMON_LINE | null {
  const n = aHi - aLo;
  const m = bHi - bLo;
  const maxD = Math.ceil((n + m) / 2);
  const off = maxD;
  const size = 2 * maxD + 2;
  // Furthest x reached on each diagonal k (at index off + k): v1 from the
  // start, v2 from the end (counted backwards); -1 not reached yet.
  const { v1, v2 } = search;
  v1.fill(-1, 0, size);
  v2.fill(-1, 0, size);
  v1[off + 1] = 0;
  v2[off + 1] = 0;
  const delta = n - m;
  // With an odd delta the forward search meets the backward one; with an even
  // one, the backward search meets the forward one.
  const front = (delta & 1) !== 0;
  // Diagonals that ran off the grid aren't walked again.
  let k1start = 0;
  let k1end = 0;
  let k2start = 0;
  let k2end = 0;
  for (let d = 0; d < maxD; d++) {
    let work = 0;
    for (let k1 = -d + k1start; k1 <= d - k1end; k1 += 2) {
      const i1 = off + k1;
      let x1 = k1 === -d || (k1 !== d && v1[i1 - 1] < v1[i1 + 1]) ? v1[i1 + 1] : v1[i1 - 1] + 1;
      let y1 = x1 - k1;
      const from = x1;
      while (x1 < n && y1 < m && a[aLo + x1] === b[bLo + y1]) {
        x1++;
        y1++;
      }
      work += 1 + x1 - from;
      v1[i1] = x1;
      if (x1 > n) {
        k1end += 2; // off the right
      } else if (y1 > m) {
        k1start += 2; // off the bottom
      } else if (front) {
        const i2 = off + delta - k1;
        if (i2 >= 0 && i2 < size && v2[i2] !== -1 && x1 >= n - v2[i2]) return [aLo + x1, bLo + y1];
      }
    }
    for (let k2 = -d + k2start; k2 <= d - k2end; k2 += 2) {
      const i2 = off + k2;
      let x2 = k2 === -d || (k2 !== d && v2[i2 - 1] < v2[i2 + 1]) ? v2[i2 + 1] : v2[i2 - 1] + 1;
      let y2 = x2 - k2;
      const from = x2;
      while (x2 < n && y2 < m && a[aHi - 1 - x2] === b[bHi - 1 - y2]) {
        x2++;
        y2++;
      }
      work += 1 + x2 - from;
      v2[i2] = x2;
      if (x2 > n) {
        k2end += 2; // off the left
      } else if (y2 > m) {
        k2start += 2; // off the top
      } else if (!front) {
        const i1 = off + delta - k2;
        if (i1 >= 0 && i1 < size && v1[i1] !== -1) {
          const x1 = v1[i1];
          if (x1 >= n - x2) return [aLo + x1, bLo + x1 - (i1 - off)];
        }
      }
    }
    search.left -= work;
    if (search.left < 0) return null;
  }
  return NO_COMMON_LINE;
}

// The hunks of an edit script: every change, with up to `context` unchanged
// lines on each side. Unchanged lines between them are left out, unless only
// one would be: folding a single line saves nothing.
function hunksOf(ops: number[], a: Split, b: Split, context: number): Hunk[] {
  const shown = new Uint8Array(ops.length);
  for (let i = 0; i < ops.length; ) {
    if (ops[i] !== SAME) {
      shown[i++] = 1;
      continue;
    }
    let j = i;
    while (j < ops.length && ops[j] === SAME) j++;
    let head = i > 0 ? context : 0; // shown after the change before this run
    let tail = j < ops.length ? context : 0; // shown before the change after it
    if (j - i - head - tail < 2) {
      head = j - i;
      tail = 0;
    }
    for (let k = i; k < Math.min(j, i + head); k++) shown[k] = 1;
    for (let k = Math.max(i, j - tail); k < j; k++) shown[k] = 1;
    i = j;
  }

  const hunks: Hunk[] = [];
  let hunk: Hunk | null = null;
  let oldAt = 0;
  let newAt = 0;
  const oldLast = a.eol ? -1 : a.lines.length - 1;
  const newLast = b.eol ? -1 : b.lines.length - 1;
  for (let i = 0; i < ops.length; i++) {
    const op = ops[i];
    if (!shown[i]) {
      hunk = null;
    } else {
      if (!hunk) {
        hunk = { oldStart: oldAt, oldCount: 0, newStart: newAt, newCount: 0, lines: [] };
        hunks.push(hunk);
      }
      if (op === SAME) {
        hunk.lines.push({ kind: "same", text: a.lines[oldAt], oldNo: oldAt + 1, newNo: newAt + 1, noEol: oldAt === oldLast });
        hunk.oldCount++;
        hunk.newCount++;
      } else if (op === DEL) {
        hunk.lines.push({ kind: "del", text: a.lines[oldAt], oldNo: oldAt + 1, newNo: null, noEol: oldAt === oldLast });
        hunk.oldCount++;
      } else {
        hunk.lines.push({ kind: "add", text: b.lines[newAt], oldNo: null, newNo: newAt + 1, noEol: newAt === newLast });
        hunk.newCount++;
      }
    }
    if (op !== ADD) oldAt++;
    if (op !== DEL) newAt++;
  }
  return hunks;
}
