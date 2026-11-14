// The parser behind Markdown.tsx: AI replies in Markdown, turned into plain
// objects that Markdown.tsx draws with React elements.
//
// A reply comes from a model that may have read hostile content (a web page, a
// file on a server), so nothing here makes markup out of it: raw HTML stays
// text, and a link keeps its URL only when that is http or https (safeUrl).
// Hostile text may also try to be slow to parse, so everything here reads left
// to right, no regular expression can backtrack over a long run, and nesting
// (quotes, lists, emphasis) is capped.
//
// What it reads: CommonMark blocks and inlines, plus GitHub's tables,
// strikethrough (~~), task items and bare http(s) links. Left out: setext
// headings (a "---" under text is a rule here, which is what models mean by
// it), HTML blocks and entities (shown as written), and link definitions that
// go over more than one line.

export type Align = "left" | "center" | "right" | null;

export interface ListItem {
  task: boolean | null; // "- [x]" true, "- [ ]" false, any other item null
  c: Block[];
}

export interface ListBlock {
  t: "list";
  ordered: boolean;
  start: number;
  // No blank lines between or inside its items: the items' text sits in them
  // without paragraph spacing, as in most chat replies.
  tight: boolean;
  items: ListItem[];
}

export interface TableBlock {
  t: "table";
  align: Align[];
  head: string[]; // inline Markdown, one per column
  rows: string[][]; // padded or cut to the number of columns
  hidden: number; // rows left out past MAX_TABLE_CELLS
}

// `text` in a paragraph, heading or table cell is inline Markdown: it is parsed
// when drawn (parseInline), since links in it may use definitions from
// anywhere in the reply.
export type Block =
  | { t: "p"; text: string }
  | { t: "h"; level: number; text: string }
  // open: the reply is still streaming and the block may still grow: its
  // closing fence hasn't come yet, or it is indented code at the end
  | { t: "code"; lang: string; code: string; open: boolean }
  | { t: "hr" }
  | { t: "quote"; c: Block[] }
  | ListBlock
  | TableBlock
  // quotes and lists nested past MAX_NEST, shown as written
  | { t: "plain"; text: string };

// "[label]: url" — label already normalised (normLabel), url as written.
export interface LinkDef {
  label: string;
  url: string;
}

// One top-level block, parsed: usually one Block, none for a paragraph of
// link definitions only.
export interface Chunk {
  blocks: Block[];
  defs: LinkDef[];
}

// Links and images only ever carry a URL that safeUrl let through.
export type Inline =
  | { t: "text"; v: string }
  | { t: "code"; v: string }
  | { t: "em" | "strong" | "del"; c: Inline[] }
  | { t: "link"; url: string; c: Inline[] }
  | { t: "img"; url: string; alt: string }
  | { t: "br" };

export type RefMap = ReadonlyMap<string, string>;

// Quotes and lists inside each other deeper than this are shown as written:
// each level is another pass over its lines, and a few thousand ">" would make
// the parse slow and the page too deep to draw.
const MAX_NEST = 12;
// The same for emphasis and links inside each other.
const MAX_INLINE_NEST = 20;
// A table wider than this stays text. A longer one shows its first rows only,
// since every short row is padded out to the full width: a wide delimiter row
// over thousands of one-word lines would make millions of cells.
const MAX_TABLE_COLS = 64;
const MAX_TABLE_CELLS = 20_000;

// ---------------------------------------------------------------------------
// Blocks

// A text split into its top-level blocks.
export interface Split {
  text: string;
  srcs: string[]; // each block's source text
  starts: number[]; // where each block's first line starts in `text`
}

// The text split into its top-level blocks, each as its own source text.
// Markdown.tsx parses each one by itself (parseChunk) and keeps the result for
// as long as that text doesn't change, so a streaming reply re-parses its last
// block only. A block's extent depends only on its own lines and the line
// after it, so parsing it alone gives what parsing the whole reply would.
//
// `prev`: the last split. When the text has only grown, all blocks but its
// last two stay as they were and the scan starts there, so an update costs
// the end of the reply, not all of it. Text added can change the last block,
// and through that block's first line the one before it ("1. a" and a "1"
// that grows into "1. b"); every block before those ended on lines that were
// already complete.
export function splitBlocks(text: string, prev: Split | null = null): Split {
  let keep = 0;
  let from = 0;
  if (prev && prev.srcs.length > 2 && text.startsWith(prev.text)) {
    keep = prev.srcs.length - 2;
    from = prev.starts[keep];
  }
  const srcs = prev ? prev.srcs.slice(0, keep) : [];
  const starts = prev ? prev.starts.slice(0, keep) : [];
  const { lines, offsets } = linesFrom(text, from);
  let i = 0;
  while (i < lines.length) {
    if (isBlank(lines[i])) {
      i++;
      continue;
    }
    const end = scan(lines, i).end;
    srcs.push(lines.slice(i, end).join("\n"));
    starts.push(offsets[i]);
    i = end;
  }
  return { text, srcs, starts };
}

// One block from splitBlocks, parsed. `open`: it is the last block of a reply
// that is still streaming, so a code block that runs to its end is still
// arriving: its fence isn't left unclosed, and indented code may go on.
export function parseChunk(src: string, open: boolean): Chunk {
  const defs: LinkDef[] = [];
  const blocks = parseBlocks(toLines(src), 0, open, defs).blocks;
  return { blocks, defs };
}

// NUL becomes U+FFFD, as CommonMark says; that also keeps "\0" free for
// Markdown.tsx to mark keys with.
function toLines(text: string): string[] {
  return text.replace(/\r\n?/g, "\n").replace(/\0/g, "\uFFFD").split("\n");
}

const LINE_BREAK = /\r\n?|\n/g;

// The lines of `text` from `from` on (a line start), and where each starts.
function linesFrom(text: string, from: number): { lines: string[]; offsets: number[] } {
  const lines: string[] = [];
  const offsets: number[] = [];
  const nul = text.indexOf("\0", from) >= 0;
  let start = from;
  LINE_BREAK.lastIndex = from;
  for (let m = LINE_BREAK.exec(text); ; m = LINE_BREAK.exec(text)) {
    const line = text.slice(start, m ? m.index : text.length);
    lines.push(nul ? line.replace(/\0/g, "\uFFFD") : line);
    offsets.push(start);
    if (!m) break;
    start = m.index + m[0].length;
  }
  return { lines, offsets };
}

const BLANK = /^[ \t]*$/;
const isBlank = (s: string) => BLANK.test(s);

const NOT_BLANK = /[^ \t]/g;

// Columns of leading blanks (a tab goes on to the next multiple of 4) and
// where the text starts. Every line is measured several times on each level
// of nesting, so a long run of blanks is searched natively rather than one
// character at a time: that loop was most of the time on deep lists.
function indentOf(s: string): { cols: number; at: number } {
  let cols = 0;
  let at = 0;
  for (; at < s.length && at < 16; at++) {
    const c = s.charCodeAt(at);
    if (c === 32) cols++;
    else if (c === 9) cols += 4 - (cols % 4);
    else return { cols, at };
  }
  if (at === s.length) return { cols, at };
  NOT_BLANK.lastIndex = at;
  const end = NOT_BLANK.exec(s)?.index ?? s.length;
  // Without a tab in the rest of the run, each blank is one column.
  if (s.lastIndexOf("\t", end - 1) < at) return { cols: cols + end - at, at: end };
  for (; at < end; at++) cols += s.charCodeAt(at) === 9 ? 4 - (cols % 4) : 1;
  return { cols, at };
}

// `s` from index i on, where it is at column `col`, without its blanks up to
// column n (all of them, if there are fewer). A tab only partly inside them
// leaves the rest of its width as spaces, so code in a list item keeps its
// shape.
function dedentAt(s: string, i: number, col: number, n: number): string {
  while (i < s.length && col < n) {
    const c = s[i];
    if (c === " ") col++;
    else if (c === "\t") {
      const w = 4 - (col % 4);
      if (col + w > n) return " ".repeat(col + w - n) + s.slice(i + 1);
      col += w;
    } else break;
    i++;
  }
  return s.slice(i);
}

const dedent = (s: string, n: number) => dedentAt(s, 0, 0, n);

function trimEndBlanks(s: string): string {
  // Not /[ \t]+$/: on a long run of blanks not at the end that backtracks
  // once per blank.
  let e = s.length;
  while (e > 0 && (s[e - 1] === " " || s[e - 1] === "\t")) e--;
  return s.slice(0, e);
}

interface Fence {
  ch: string;
  len: number;
  indent: number;
  info: string;
}

function fenceOpen(s: string): Fence | null {
  const { cols, at } = indentOf(s);
  const ch = s[at];
  if (cols > 3 || (ch !== "`" && ch !== "~")) return null;
  let j = at;
  while (s[j] === ch) j++;
  if (j - at < 3) return null;
  const info = s.slice(j).trim();
  // "```foo`" is inline code, not a fence
  if (ch === "`" && info.includes("`")) return null;
  return { ch, len: j - at, indent: cols, info };
}

function fenceClose(s: string, f: Fence): boolean {
  const { cols, at } = indentOf(s);
  if (cols > 3) return false;
  let j = at;
  while (s[j] === f.ch) j++;
  return j - at >= f.len && isBlank(s.slice(j));
}

function heading(s: string): { level: number; text: string } | null {
  const { cols, at } = indentOf(s);
  if (cols > 3 || s[at] !== "#") return null;
  let j = at;
  while (s[j] === "#") j++;
  const level = j - at;
  // "#hashtag" is text
  if (level > 6 || (j < s.length && s[j] !== " " && s[j] !== "\t")) return null;
  let text = s.slice(j).trim();
  // a closing run of #s goes, when a blank comes before it
  let e = text.length;
  while (e > 0 && text[e - 1] === "#") e--;
  if (e === 0) text = "";
  else if (e < text.length && (text[e - 1] === " " || text[e - 1] === "\t")) text = trimEndBlanks(text.slice(0, e));
  return { level, text };
}

function isRule(s: string): boolean {
  const { cols, at } = indentOf(s);
  const ch = s[at];
  if (cols > 3 || (ch !== "-" && ch !== "*" && ch !== "_")) return false;
  let n = 0;
  for (let i = at; i < s.length; i++) {
    if (s[i] === ch) n++;
    else if (s[i] !== " " && s[i] !== "\t") return false;
  }
  return n >= 3;
}

interface Marker {
  ordered: boolean;
  ch: string; // "-", "+" or "*"; for numbers the "." or ")" after them
  start: number;
  markerCols: number; // columns before the marker
  contentCols: number; // the column the item's text starts at
  first: string; // the first line's text
  empty: boolean; // nothing after the marker
}

function listMarker(s: string): Marker | null {
  const { cols, at } = indentOf(s);
  if (cols > 3) return null;
  let j = at;
  let ordered = false;
  let ch = s[j];
  let start = 1;
  if (ch === "-" || ch === "+" || ch === "*") j++;
  else {
    while (j - at < 9 && s[j] >= "0" && s[j] <= "9") j++;
    ch = s[j];
    if (j === at || (ch !== "." && ch !== ")")) return null;
    ordered = true;
    start = parseInt(s.slice(at, j), 10);
    j++;
  }
  if (j < s.length && s[j] !== " " && s[j] !== "\t") return null;
  const markerEnd = cols + (j - at);
  NOT_BLANK.lastIndex = j;
  const text = NOT_BLANK.exec(s)?.index ?? -1;
  if (text < 0) return { ordered, ch, start, markerCols: cols, contentCols: markerEnd + 1, first: "", empty: true };
  // The blanks after the marker, counted from the column it ends at so a tab
  // keeps the width it has there. Five or more: the text is an indented code
  // block, and the item's own text column is just past the marker.
  let pad = markerEnd;
  for (let p = j; p < text && pad - markerEnd < 5; p++) pad += s[p] === "\t" ? 4 - (pad % 4) : 1;
  const contentCols = pad - markerEnd >= 5 ? markerEnd + 1 : pad;
  return { ordered, ch, start, markerCols: cols, contentCols, first: dedentAt(s, j, markerEnd, contentCols), empty: false };
}

function sameList(a: Marker, b: Marker): boolean {
  return a.ordered === b.ordered && a.ch === b.ch;
}

const TASK = /^\[([ xX])\](?:[ \t]+|$)/;

// Table cells, split on "|" that isn't escaped. "\|" becomes "|" here, even in
// code, as GitHub does: that's the only way to write a pipe in a cell.
function splitCells(s: string): string[] {
  let t = s.trim();
  if (t[0] === "|") t = t.slice(1);
  if (t[t.length - 1] === "|" && t[t.length - 2] !== "\\") t = t.slice(0, -1);
  const cells: string[] = [];
  let cur = "";
  let from = 0;
  for (let i = 0; i < t.length; i++) {
    if (t[i] === "\\" && t[i + 1] === "|") {
      cur += t.slice(from, i) + "|";
      from = ++i + 1;
    } else if (t[i] === "|") {
      cells.push((cur + t.slice(from, i)).trim());
      cur = "";
      from = i + 1;
    }
  }
  cells.push((cur + t.slice(from)).trim());
  return cells;
}

function delimiterRow(s: string): Align[] | null {
  if (indentOf(s).cols > 3 || !s.includes("|")) return null;
  const out: Align[] = [];
  for (const cell of splitCells(s)) {
    const left = cell[0] === ":";
    const right = cell.length > 1 && cell[cell.length - 1] === ":";
    const dashes = cell.slice(left ? 1 : 0, right ? -1 : undefined);
    if (!/^-+$/.test(dashes)) return null;
    out.push(left && right ? "center" : left ? "left" : right ? "right" : null);
  }
  return out;
}

function tableStart(lines: string[], i: number): { align: Align[]; head: string[] } | null {
  const head = lines[i];
  const delim = lines[i + 1];
  if (delim === undefined || !head.includes("|") || indentOf(head).cols > 3) return null;
  const align = delimiterRow(delim);
  if (!align || align.length > MAX_TABLE_COLS) return null;
  const cells = splitCells(head);
  return cells.length === align.length ? { align, head: cells } : null;
}

// Whether line k starts a block that ends a paragraph above it. A numbered
// item does only when it is numbered 1, so "in\n2024. we moved" stays text.
function interrupts(lines: string[], k: number): boolean {
  const s = lines[k];
  const { cols, at } = indentOf(s);
  if (cols > 3) return false;
  const c = s[at];
  if ((c === "`" || c === "~") && fenceOpen(s)) return true;
  if (c === "#" && heading(s)) return true;
  if (c === ">") return true;
  if ((c === "-" || c === "*" || c === "_") && isRule(s)) return true;
  const m = listMarker(s);
  if (m && !m.empty && (!m.ordered || m.start === 1)) return true;
  // GitHub lets a table start right under text: the line above the
  // delimiter row is its head, not the paragraph's.
  return s.includes("|") && tableStart(lines, k) !== null;
}

// Follows the lines going into a quote or list item, to tell whether the last
// one left a paragraph open: only then may a line without the quote's ">" or
// the item's indent carry on (a "lazy" line, in CommonMark's words). Models
// write those under numbered steps. It looks through nested ">" and list
// markers to the text they hold but doesn't parse it, so in odd nestings it
// can guess wrong; that only moves where the quote or list ends.
class ParaTrack {
  open = false;
  private fence: Fence | null = null;
  private table = false;

  feed(line: string): void {
    // One look at each line's leading blanks: on a deeply indented line that
    // look is the cost, and it is paid once per level of nesting.
    let s = line;
    let ind = indentOf(s);
    for (let n = 0; n < MAX_NEST && ind.cols <= 3; n++) {
      if (s[ind.at] === ">") s = s.slice(ind.at + 1);
      else {
        if (this.fence) break;
        const m = listMarker(s);
        if (!m || isRule(s)) break;
        s = m.first;
      }
      ind = indentOf(s);
    }
    if (this.fence) {
      const t = s.trim();
      let n = 0;
      while (t[n] === this.fence.ch) n++;
      if (n === t.length && n >= this.fence.len) this.fence = null;
      this.open = false;
      return;
    }
    if (ind.at === s.length) {
      this.open = false;
      this.table = false;
      return;
    }
    // Indented: carries on a paragraph, or is indented code; either way
    // nothing changes.
    if (ind.cols > 3) return;
    const fence = fenceOpen(s);
    if (fence) {
      this.fence = fence;
      this.open = false;
    } else if (this.table || heading(s) || isRule(s)) {
      this.open = false;
    } else if (this.open && delimiterRow(s)) {
      this.table = true;
      this.open = false;
    } else this.open = true;
  }
}

// Where the block starting at line i ends, and what scan found to build it.
type Scan =
  | { k: "fence"; end: number; fence: Fence; closed: boolean }
  | { k: "icode"; end: number }
  | { k: "h"; end: number }
  | { k: "hr"; end: number }
  | { k: "quote"; end: number; inner: string[] }
  | { k: "list"; end: number; first: Marker; items: string[][]; gapBetween: boolean }
  | { k: "table"; end: number; align: Align[]; head: string[] }
  | { k: "p"; end: number };

function scan(lines: string[], i: number): Scan {
  const s = lines[i];
  const { cols, at } = indentOf(s);
  if (cols > 3) return scanIndentedCode(lines, i);
  const c = s[at];
  if (c === "`" || c === "~") {
    const fence = fenceOpen(s);
    if (fence) return scanFence(lines, i, fence);
  }
  if (c === "#" && heading(s)) return { k: "h", end: i + 1 };
  if (c === ">") return scanQuote(lines, i);
  if ((c === "-" || c === "*" || c === "_") && isRule(s)) return { k: "hr", end: i + 1 };
  const m = listMarker(s);
  if (m) return scanList(lines, i, m);
  if (s.includes("|")) {
    const t = tableStart(lines, i);
    if (t) {
      let k = i + 2;
      while (k < lines.length && !isBlank(lines[k]) && !interrupts(lines, k)) k++;
      return { k: "table", end: k, align: t.align, head: t.head };
    }
  }
  let k = i + 1;
  while (k < lines.length && !isBlank(lines[k]) && !interrupts(lines, k)) k++;
  return { k: "p", end: k };
}

function scanFence(lines: string[], i: number, fence: Fence): Scan {
  for (let k = i + 1; k < lines.length; k++) {
    if (fenceClose(lines[k], fence)) return { k: "fence", end: k + 1, fence, closed: true };
  }
  return { k: "fence", end: lines.length, fence, closed: false };
}

function scanIndentedCode(lines: string[], i: number): Scan {
  let end = i + 1;
  for (let k = i + 1; k < lines.length; k++) {
    if (isBlank(lines[k])) continue;
    if (indentOf(lines[k]).cols < 4) break;
    end = k + 1;
  }
  return { k: "icode", end };
}

function scanQuote(lines: string[], i: number): Scan {
  const inner: string[] = [];
  const para = new ParaTrack();
  let k = i;
  for (; k < lines.length; k++) {
    const s = lines[k];
    const { cols, at } = indentOf(s);
    let d: string;
    if (cols < 4 && s[at] === ">") {
      // The ">" and one blank after it go; a tab there gives up one column.
      d = dedent(" ".repeat(cols + 1) + s.slice(at + 1), cols + 2);
    } else if (at < s.length && para.open && !interrupts(lines, k)) {
      d = s;
    } else break;
    inner.push(d);
    para.feed(d);
  }
  return { k: "quote", end: k, inner };
}

function scanList(lines: string[], i: number, first: Marker): Scan {
  const items: string[][] = [];
  let gapBetween = false;
  let m = first;
  let j = i;
  let end = i + 1;
  for (;;) {
    const item = [m.first];
    const para = new ParaTrack();
    para.feed(m.first);
    let k = j + 1;
    while (k < lines.length) {
      const s = lines[k];
      const { cols, at } = indentOf(s);
      if (at === s.length) {
        // Blank lines stay in the item when the text after them is indented
        // to the item's text. An item empty on its marker line ends at one.
        let b = k + 1;
        while (b < lines.length && isBlank(lines[b])) b++;
        if (b === lines.length || (m.empty && item.length === 1) || indentOf(lines[b]).cols < m.contentCols) break;
        for (; k < b; k++) item.push(dedent(lines[k], m.contentCols));
        para.feed("");
        continue;
      }
      let d: string;
      if (cols >= m.contentCols) d = dedent(s, m.contentCols);
      else {
        const sub = listMarker(s);
        if (sub && sameList(sub, first)) break; // the next item
        if (sub && cols >= m.markerCols + 2 && !isRule(s)) {
          // A sub-list of the other kind, indented short of the text above
          // it, as models write "  - point" under "1. step": nested, where
          // CommonMark would start a new list beside this one.
          d = dedent(s, cols);
        } else if (para.open && !interrupts(lines, k)) {
          d = s; // a lazy line
        } else break;
      }
      item.push(d);
      para.feed(d);
      k++;
    }
    items.push(item);
    end = k;
    let b = k;
    while (b < lines.length && isBlank(lines[b])) b++;
    const next = b < lines.length ? listMarker(lines[b]) : null;
    if (!next || !sameList(next, first) || isRule(lines[b])) break;
    if (b > k) gapBetween = true;
    m = next;
    j = b;
  }
  return { k: "list", end, first, items, gapBetween };
}

function parseBlocks(lines: string[], depth: number, openEnd: boolean, defs: LinkDef[]): { blocks: Block[]; gaps: boolean } {
  const blocks: Block[] = [];
  let gaps = false; // a blank line between two blocks: a loose list item
  let blank = false;
  let i = 0;
  while (i < lines.length) {
    if (isBlank(lines[i])) {
      blank = blocks.length > 0;
      i++;
      continue;
    }
    const sc = scan(lines, i);
    const before = blocks.length;
    build(sc, lines.slice(i, sc.end), depth, openEnd && sc.end === lines.length, defs, blocks);
    if (blank && blocks.length > before) gaps = true;
    blank = false;
    i = sc.end;
  }
  return { blocks, gaps };
}

// `atEnd`: this block runs to the end of a block that is still arriving.
function build(sc: Scan, lines: string[], depth: number, atEnd: boolean, defs: LinkDef[], out: Block[]): void {
  switch (sc.k) {
    case "fence": {
      const body = lines.slice(1, sc.closed ? -1 : undefined).map((l) => dedent(l, sc.fence.indent));
      const open = !sc.closed && atEnd;
      if (open) dropPartialFence(body, sc.fence);
      const lang = sc.fence.info.split(/[ \t]/, 1)[0];
      out.push({ t: "code", lang, code: body.join("\n"), open });
      return;
    }
    case "icode":
      // No fence ends it, so at the end more lines may come.
      out.push({ t: "code", lang: "", code: lines.map((l) => dedent(l, 4)).join("\n"), open: atEnd });
      return;
    case "h": {
      const h = heading(lines[0]);
      if (h) out.push({ t: "h", level: h.level, text: h.text });
      return;
    }
    case "hr":
      out.push({ t: "hr" });
      return;
    case "quote":
      if (depth >= MAX_NEST) out.push({ t: "plain", text: lines.join("\n") });
      else out.push({ t: "quote", c: parseBlocks(sc.inner, depth + 1, atEnd, defs).blocks });
      return;
    case "list": {
      if (depth >= MAX_NEST) {
        out.push({ t: "plain", text: lines.join("\n") });
        return;
      }
      let tight = !sc.gapBetween;
      const items = sc.items.map((item, n): ListItem => {
        let task: boolean | null = null;
        let body = item;
        const tm = TASK.exec(item[0]);
        if (tm) {
          task = tm[1] !== " ";
          body = item.slice();
          body[0] = item[0].slice(tm[0].length);
        }
        const r = parseBlocks(body, depth + 1, atEnd && n === sc.items.length - 1, defs);
        if (r.gaps) tight = false;
        return { task, c: r.blocks };
      });
      out.push({ t: "list", ordered: sc.first.ordered, start: sc.first.start, tight, items });
      return;
    }
    case "table": {
      const width = sc.align.length;
      const room = Math.max(1, Math.floor(MAX_TABLE_CELLS / width));
      const rows: string[][] = [];
      for (let r = 2; r < lines.length && rows.length < room; r++) {
        const cells = splitCells(lines[r]);
        const row: string[] = [];
        for (let c = 0; c < width; c++) row.push(cells[c] ?? "");
        rows.push(row);
      }
      out.push({ t: "table", align: sc.align, head: sc.head, rows, hidden: lines.length - 2 - rows.length });
      return;
    }
    case "p":
      paragraph(lines, defs, out);
      return;
  }
}

// While a code block is still arriving, its last line may be the start of the
// closing fence ("``"), or the line break before it: neither is code, and
// showing them would flicker.
function dropPartialFence(body: string[], f: Fence): void {
  const last = body[body.length - 1];
  if (last === undefined) return;
  const t = last.trim();
  let n = 0;
  while (t[n] === f.ch) n++;
  if (n === t.length && n < f.len) body.pop();
}

// Link definitions start a paragraph and are taken out of it.
function paragraph(lines: string[], defs: LinkDef[], out: Block[]): void {
  let i = 0;
  for (; i < lines.length; i++) {
    const d = linkDef(lines[i]);
    if (!d) break;
    defs.push(d);
  }
  if (i === lines.length) return;
  const text = lines
    .slice(i)
    .map((l) => l.replace(/^[ \t]+/, ""))
    .join("\n");
  out.push({ t: "p", text: trimEndBlanks(text) });
}

// "[label]: url" or "[label]: <url> "title"", on one line.
function linkDef(s: string): LinkDef | null {
  const { cols, at } = indentOf(s);
  if (cols > 3 || s[at] !== "[") return null;
  let j = at + 1;
  for (; j < s.length && s[j] !== "]"; j++) {
    if (s[j] === "[") return null;
    if (s[j] === "\\") j++;
  }
  const label = s.slice(at + 1, j);
  if (j >= s.length || s[j + 1] !== ":" || label.length > 999 || !/[^ \t]/.test(label)) return null;
  j += 2;
  while (s[j] === " " || s[j] === "\t") j++;
  let url: string;
  if (s[j] === "<") {
    const e = s.indexOf(">", j + 1);
    if (e < 0) return null;
    url = s.slice(j + 1, e);
    if (url.includes("<")) return null;
    j = e + 1;
  } else {
    const st = j;
    while (j < s.length && s.charCodeAt(j) > 32) j++;
    if (j === st) return null;
    url = s.slice(st, j);
  }
  const rest = s.slice(j);
  if (!isBlank(rest)) {
    // a title, after a blank, in "", '' or ()
    const t = rest.trim();
    const close = t[0] === "(" ? ")" : t[0];
    if (rest[0] !== " " && rest[0] !== "\t") return null;
    if ((close !== '"' && close !== "'" && close !== ")") || t.length < 2 || t[t.length - 1] !== close) return null;
  }
  return { label: normLabel(label), url: unescapeMd(url) };
}

// ---------------------------------------------------------------------------
// Inlines

const ASCII_PUNCT = /[!-/:-@[-`{-~]/;
const WS = /\s/;
const PUNCT = /[\p{P}\p{S}]/u;
// Where something other than plain text may start. "!" only before "[", and
// "http(s)://" for bare links; everything between is copied as is.
const SPECIAL = /[\n\\`*_~[\]<]|!\[|https?:\/\//gi;
const AUTOLINK = /<([A-Za-z][A-Za-z0-9+.-]{1,31}:[^\s<>\x00-\x1f\x7f]*)>/y;
// Left off the end of a bare link: sentence punctuation (also Persian, Arabic
// and CJK) and closing quotes.
const URL_TRAIL = new Set("?!.,:;*_~'\"،؛؟۔。、，；：！？”’»›…");

// The working tree: a linked list, so wrapping a run of nodes in emphasis or
// a link moves pointers instead of copying arrays.
interface INode {
  t: "root" | "text" | "code" | "br" | "em" | "strong" | "del" | "link" | "img";
  v: string; // text, code, or a link's URL as written
  parent: INode | null;
  prev: INode | null;
  next: INode | null;
  first: INode | null;
  last: INode | null;
}

// A run of * _ or ~~ that may open or close emphasis (CommonMark's delimiter
// stack).
interface Delim {
  node: INode;
  ch: string;
  n: number; // delimiters left in the run
  orig: number; // the run's length as written
  canOpen: boolean;
  canClose: boolean;
  prev: Delim | null;
  next: Delim | null;
}

interface Bracket {
  node: INode;
  img: boolean;
  at: number; // where its text starts
  delims: Delim | null; // the delimiter stack under it
  prev: Bracket | null;
}

function mk(t: INode["t"], v: string): INode {
  return { t, v, parent: null, prev: null, next: null, first: null, last: null };
}

function appendChild(p: INode, c: INode): void {
  c.parent = p;
  c.prev = p.last;
  c.next = null;
  if (p.last) p.last.next = c;
  else p.first = c;
  p.last = c;
}

function insertAfter(a: INode, c: INode): void {
  c.parent = a.parent;
  c.prev = a;
  c.next = a.next;
  if (a.next) a.next.prev = c;
  else if (a.parent) a.parent.last = c;
  a.next = c;
}

function unlink(c: INode): void {
  if (c.prev) c.prev.next = c.next;
  else if (c.parent) c.parent.first = c.next;
  if (c.next) c.next.prev = c.prev;
  else if (c.parent) c.parent.last = c.prev;
  c.parent = c.prev = c.next = null;
}

// A paragraph's, heading's or table cell's text, parsed. `refs`: the reply's
// link definitions by normalised label.
export function parseInline(src: string, refs: RefMap): Inline[] {
  const root = mk("root", "");
  let delims: Delim | null = null;
  let brackets: Bracket | null = null;
  // "[" before this position can't make links any more: a link was made
  // after them, and links don't go inside links.
  let noLinksBefore = -1;
  let from = 0; // start of the text not yet added
  let pos = 0;

  const add = (n: INode) => appendChild(root, n);
  const flush = (to: number) => {
    if (to > from) add(mk("text", src.slice(from, to)));
  };

  // Backtick runs by length, found once: a code span's closing run is the next
  // run of the same length, and a cursor per length that only moves forward
  // keeps a line of "` `` ``` ..." with no closers from rescanning to the end.
  let runs: Map<number, number[]> | null = null;
  const cursors = new Map<number, number>();
  const closingTicks = (len: number, after: number): number => {
    if (!runs) {
      const found = new Map<number, number[]>();
      for (let i = src.indexOf("`"); i >= 0; ) {
        let e = i + 1;
        while (src[e] === "`") e++;
        const list = found.get(e - i);
        if (list) list.push(i);
        else found.set(e - i, [i]);
        i = src.indexOf("`", e);
      }
      runs = found;
    }
    const list = runs.get(len);
    if (!list) return -1;
    let c = cursors.get(len) ?? 0;
    while (c < list.length && list[c] < after) c++;
    cursors.set(len, c);
    return c < list.length ? list[c] : -1;
  };

  const removeDelim = (d: Delim) => {
    if (d.prev) d.prev.next = d.next;
    if (d.next) d.next.prev = d.prev;
    else delims = d.prev;
  };

  // CommonMark's "process emphasis": pair closers with openers above `bottom`,
  // with its per-kind floor so a failed search never covers the same ground
  // twice.
  const processEmphasis = (bottom: Delim | null) => {
    if (delims === bottom) return;
    const floors = new Map<string, Delim | null>();
    let closer: Delim | null = delims;
    while (closer && closer.prev !== bottom) closer = closer.prev;
    while (closer) {
      if (!closer.canClose) {
        closer = closer.next;
        continue;
      }
      const key = closer.ch === "~" ? "~" : `${closer.ch}${closer.canOpen ? 3 : 0}${closer.orig % 3}`;
      const floorSet = floors.get(key);
      const floor = floorSet === undefined ? bottom : floorSet;
      let opener = closer.prev;
      let found = false;
      while (opener && opener !== bottom && opener !== floor) {
        if (opener.ch === closer.ch && opener.canOpen) {
          // "rule of 3": *foo**bar* isn't em around "foo**bar"
          const odd = (closer.canOpen || opener.canClose) && closer.orig % 3 !== 0 && (opener.orig + closer.orig) % 3 === 0;
          found = closer.ch === "~" || !odd;
          if (found) break;
        }
        opener = opener.prev;
      }
      const old: Delim = closer;
      if (found && opener) {
        const use = closer.ch === "~" ? 2 : closer.n >= 2 && opener.n >= 2 ? 2 : 1;
        opener.n -= use;
        closer.n -= use;
        opener.node.v = opener.node.v.slice(0, opener.n);
        closer.node.v = closer.node.v.slice(0, closer.n);
        const wrap = mk(closer.ch === "~" ? "del" : use === 2 ? "strong" : "em", "");
        for (let x = opener.node.next; x && x !== closer.node; ) {
          const nx: INode | null = x.next;
          unlink(x);
          appendChild(wrap, x);
          x = nx;
        }
        insertAfter(opener.node, wrap);
        // The runs between them can't pair across the new element.
        opener.next = closer;
        closer.prev = opener;
        if (opener.n === 0) {
          unlink(opener.node);
          removeDelim(opener);
        }
        if (closer.n === 0) {
          unlink(closer.node);
          const nx: Delim | null = closer.next;
          removeDelim(closer);
          closer = nx;
        }
      } else closer = closer.next;
      if (!found) {
        floors.set(key, old.prev);
        if (!old.canOpen) removeDelim(old);
      }
    }
    while (delims && delims !== bottom) removeDelim(delims);
  };

  // The "]" at `at` closes the last "[": where the link ends, or -1 when it
  // makes none and is just text.
  const closeBracket = (at: number): number => {
    const b = brackets;
    if (!b) return -1;
    brackets = b.prev;
    if (!b.img && b.at < noLinksBefore) return -1;
    let url: string | null = null;
    let end = -1;
    if (src[at + 1] === "(") {
      const tail = linkTail(src, at + 1);
      if (tail) {
        url = tail.url;
        end = tail.end;
      }
    }
    if (end < 0 && refs.size > 0) {
      // [text][label], [text][] or [text]
      let label = src.slice(b.at, at);
      let after = at + 1;
      if (src[at + 1] === "[") {
        const l = labelAt(src, at + 1);
        if (l) {
          after = l.end;
          if (l.label) label = l.label;
        }
      }
      if (label.length <= 999) {
        const u = refs.get(normLabel(label));
        if (u !== undefined) {
          url = u;
          end = after;
        }
      }
    }
    if (end < 0 || url === null) return -1;
    const link = mk(b.img ? "img" : "link", url);
    for (let x = b.node.next; x; ) {
      const nx: INode | null = x.next;
      unlink(x);
      appendChild(link, x);
      x = nx;
    }
    insertAfter(b.node, link);
    unlink(b.node);
    processEmphasis(b.delims);
    if (!b.img) noLinksBefore = b.at;
    return end;
  };

  while (pos < src.length) {
    SPECIAL.lastIndex = pos;
    const m = SPECIAL.exec(src);
    if (!m) break;
    const at = m.index;
    const c = src[at];
    if (c === "\n") {
      // Two spaces before a line break make it a hard break.
      let e = at;
      while (e > from && src[e - 1] === " ") e--;
      flush(e);
      add(at - e >= 2 ? mk("br", "") : mk("text", "\n"));
      pos = from = skipBlanks(src, at + 1);
    } else if (c === "\\") {
      const d = src[at + 1];
      if (d === "\n") {
        flush(at);
        add(mk("br", ""));
        pos = from = skipBlanks(src, at + 2);
      } else if (d !== undefined && ASCII_PUNCT.test(d)) {
        flush(at);
        add(mk("text", d));
        pos = from = at + 2;
      } else pos = at + 1;
    } else if (c === "`") {
      let e = at + 1;
      while (src[e] === "`") e++;
      const close = closingTicks(e - at, e);
      if (close < 0) {
        pos = e; // no closing run: the backticks are text
        continue;
      }
      flush(at);
      let code = src.slice(e, close).replace(/\n/g, " ");
      if (code.length > 1 && code[0] === " " && code[code.length - 1] === " " && /[^ ]/.test(code)) code = code.slice(1, -1);
      add(mk("code", code));
      pos = from = close + (e - at);
    } else if (c === "*" || c === "_" || c === "~") {
      let e = at + 1;
      while (src[e] === c) e++;
      // Strikethrough is "~~" only: a single "~" is a home folder or "about".
      if (c === "~" && e - at !== 2) {
        pos = e;
        continue;
      }
      flush(at);
      const run = mk("text", src.slice(at, e));
      add(run);
      const before = at === 0 ? "\n" : charBefore(src, at);
      const after = e >= src.length ? "\n" : String.fromCodePoint(src.codePointAt(e) ?? 32);
      const afterSpace = WS.test(after);
      const beforeSpace = WS.test(before);
      const afterPunct = PUNCT.test(after);
      const beforePunct = PUNCT.test(before);
      const left = !afterSpace && (!afterPunct || beforeSpace || beforePunct);
      const right = !beforeSpace && (!beforePunct || afterSpace || afterPunct);
      // "_" can't open or close inside a word: snake_case_names stay text.
      const canOpen = c === "_" ? left && (!right || beforePunct) : left;
      const canClose = c === "_" ? right && (!left || afterPunct) : right;
      if (canOpen || canClose) {
        const d: Delim = { node: run, ch: c, n: e - at, orig: e - at, canOpen, canClose, prev: delims, next: null };
        if (delims) delims.next = d;
        delims = d;
      }
      pos = from = e;
    } else if (c === "[" || c === "!") {
      const img = c === "!";
      flush(at);
      const n = mk("text", img ? "![" : "[");
      add(n);
      pos = from = at + (img ? 2 : 1);
      brackets = { node: n, img, at: pos, delims, prev: brackets };
    } else if (c === "]") {
      // The text before it goes in first: a link takes everything after its
      // "[". A "]" that makes no link stays in the text after it.
      flush(at);
      from = at;
      const end = closeBracket(at);
      pos = end < 0 ? at + 1 : (from = end);
    } else if (c === "<") {
      AUTOLINK.lastIndex = at;
      const a = AUTOLINK.exec(src);
      if (!a) {
        pos = at + 1; // "<b>", "<script>": text
        continue;
      }
      flush(at);
      // <https://…> is a link; <javascript:…>, <mailto:…> and the like show
      // as plain text.
      const url = safeUrl(a[1]);
      add(url ? linkTo(url, a[1]) : mk("text", a[1]));
      pos = from = at + a[0].length;
    } else {
      // "http://" or "https://", a bare link when it starts a word
      const scheme = m[0].length;
      if (at > 0 && /[A-Za-z0-9]/.test(src[at - 1])) {
        pos = at + 1;
        continue;
      }
      let e = at + scheme;
      while (e < src.length && src[e] !== "<" && !WS.test(src[e])) e++;
      e = bareUrlEnd(src, at, e);
      const url = e > at + scheme ? safeUrl(src.slice(at, e)) : null;
      if (!url) {
        pos = at + scheme;
        continue;
      }
      flush(at);
      add(linkTo(url, src.slice(at, e)));
      pos = from = e;
    }
  }
  flush(src.length);
  processEmphasis(null);
  return convert(root.first, 0, false);
}

function linkTo(url: string, label: string): INode {
  const n = mk("link", url);
  appendChild(n, mk("text", label));
  return n;
}

function skipBlanks(s: string, i: number): number {
  while (s[i] === " " || s[i] === "\t") i++;
  return i;
}

// Blanks with at most one line break among them.
function skipSpace(s: string, i: number): number {
  i = skipBlanks(s, i);
  return s[i] === "\n" ? skipBlanks(s, i + 1) : i;
}

function charBefore(s: string, at: number): string {
  const lo = s.charCodeAt(at - 1);
  if (lo >= 0xdc00 && lo <= 0xdfff && at >= 2) {
    const hi = s.charCodeAt(at - 2);
    if (hi >= 0xd800 && hi <= 0xdbff) return s.slice(at - 2, at);
  }
  return s[at - 1];
}

// The end of a bare link: trailing punctuation goes, and so does a closing
// bracket without its opening one, so "(see https://x.org/a)" ends at "a"
// while "https://en.wikipedia.org/wiki/Bash_(Unix_shell)" stays whole.
function bareUrlEnd(s: string, start: number, e: number): number {
  let parens = 0;
  let squares = 0;
  for (let i = start; i < e; i++) {
    const c = s[i];
    if (c === "(") parens++;
    else if (c === ")") parens--;
    else if (c === "[") squares++;
    else if (c === "]") squares--;
  }
  while (e > start) {
    const c = s[e - 1];
    if (URL_TRAIL.has(c)) e--;
    else if (c === ")" && parens < 0) {
      parens++;
      e--;
    } else if (c === "]" && squares < 0) {
      squares++;
      e--;
    } else break;
  }
  return e;
}

// "(url "title")" after "]", at p. Each loop here stops at the first character
// that ends what it reads, so all the attempts in one text together stay
// linear: a title runs to the next quote, a destination to the next blank or
// past 32 nested "(".
function linkTail(s: string, p: number): { url: string; end: number } | null {
  let i = skipSpace(s, p + 1);
  let url: string;
  if (s[i] === "<") {
    let j = i + 1;
    for (; j < s.length; j++) {
      const c = s[j];
      if (c === "\\") j++;
      else if (c === ">" || c === "<" || c === "\n") break;
    }
    if (s[j] !== ">") return null;
    url = s.slice(i + 1, j);
    i = j + 1;
  } else {
    let depth = 0;
    let j = i;
    for (; j < s.length; j++) {
      const c = s[j];
      const code = s.charCodeAt(j);
      if (c === "\\" && j + 1 < s.length && ASCII_PUNCT.test(s[j + 1])) j++;
      else if (code <= 32 || code === 127) break;
      else if (c === "(") {
        if (++depth > 32) return null;
      } else if (c === ")") {
        if (depth === 0) break;
        depth--;
      }
    }
    if (depth !== 0) return null;
    url = s.slice(i, j);
    i = j;
  }
  const k = skipSpace(s, i);
  if (k > i && (s[k] === '"' || s[k] === "'" || s[k] === "(")) {
    const close = s[k] === "(" ? ")" : s[k];
    let j = k + 1;
    for (; j < s.length; j++) {
      const c = s[j];
      if (c === "\\") j++;
      else if (c === close) break;
      else if (close === ")" && c === "(") return null;
    }
    if (j >= s.length) return null;
    i = skipSpace(s, j + 1);
  } else i = k;
  if (s[i] !== ")") return null;
  return { url: unescapeMd(url), end: i + 1 };
}

// A link label "[...]" at p: its text ("" for "[]") and where it ends.
function labelAt(s: string, p: number): { label: string; end: number } | null {
  for (let j = p + 1; j < s.length && j - p <= 1000; j++) {
    const c = s[j];
    if (c === "\\") j++;
    else if (c === "[") return null;
    else if (c === "]") {
      const label = s.slice(p + 1, j);
      if (label && !/[^ \t\n]/.test(label)) return null;
      return { label, end: j + 1 };
    }
  }
  return null;
}

// Labels match without regard to case or runs of blanks, as CommonMark has it
// (upper-casing after lower-casing folds "ß" and "SS" together).
function normLabel(s: string): string {
  return s.trim().replace(/\s+/g, " ").toLowerCase().toUpperCase();
}

function unescapeMd(s: string): string {
  return s.replace(/\\([!-/:-@[-`{-~])/g, "$1");
}

const checkedUrls = new Map<string, string | null>();

/** Forgets the URLs checked so far. They come from replies, so Lock clears them. */
export function clearUrlCache(): void {
  checkedUrls.clear();
}

// The URL to open, only for http and https: javascript:, data:, file:,
// mailto: and relative links get null and show as plain text. So does a URL
// with a user name or password: "https://github.com@evil.example/" opens
// evil.example, but its start reads like another host. The URL parser has the
// last word, so "java\tscript:" or an odd scheme case can't slip by, and what
// is shown and opened is its normalised form, with the host in punycode where
// it isn't plain ASCII.
export function safeUrl(raw: string): string | null {
  const s = raw.trim();
  if (!/^https?:\/\//i.test(s)) return null;
  // A reference link repeats its URL, and the parser isn't free.
  const known = checkedUrls.get(s);
  if (known !== undefined) return known;
  let href: string | null = null;
  try {
    const u = new URL(s);
    if ((u.protocol === "http:" || u.protocol === "https:") && !u.username && !u.password) href = u.href;
  } catch {
    href = null;
  }
  if (checkedUrls.size >= 500) checkedUrls.clear();
  checkedUrls.set(s, href);
  return href;
}

function convert(first: INode | null, depth: number, inLink: boolean): Inline[] {
  const out: Inline[] = [];
  // Neighbouring text is joined into one piece, once, when something else
  // comes or the list ends.
  let parts: string[] = [];
  const text = (v: string) => {
    if (v) parts.push(v);
  };
  const push = (it: Inline) => {
    if (it.t === "text") return text(it.v);
    if (parts.length) out.push({ t: "text", v: parts.join("") });
    parts = [];
    out.push(it);
  };
  for (let x = first; x; x = x.next) {
    if (x.t === "text") text(x.v);
    else if (x.t === "code") push({ t: "code", v: x.v });
    else if (x.t === "br") push({ t: "br" });
    else if (depth >= MAX_INLINE_NEST) text(plainText(x));
    else if (x.t === "img") {
      // Never an <img>: a link to the picture at most. Inside a link (a
      // badge) the alt text is part of that link's text.
      const url = inLink ? null : safeUrl(x.v);
      const alt = plainText(x);
      if (url) push({ t: "img", url, alt });
      else text(alt || (inLink ? "" : x.v));
    } else if (x.t === "link") {
      // Links don't go inside links: the inner one keeps its text only.
      const url = inLink ? null : safeUrl(x.v);
      if (url) push({ t: "link", url, c: convert(x.first, depth + 1, true) });
      else for (const it of convert(x.first, depth + 1, inLink)) push(it);
    } else if (x.t === "em" || x.t === "strong" || x.t === "del") {
      push({ t: x.t, c: convert(x.first, depth + 1, inLink) });
    }
  }
  if (parts.length) out.push({ t: "text", v: parts.join("") });
  return out;
}

// The text under a node, without a stack frame per level (it is what deeper
// nesting than MAX_INLINE_NEST turns into).
function plainText(n: INode): string {
  const parts: string[] = [];
  const stack: INode[] = [];
  let x = n.first;
  while (x || stack.length) {
    if (!x) {
      x = (stack.pop() as INode).next;
      continue;
    }
    if (x.t === "text" || x.t === "code") parts.push(x.v);
    else if (x.t === "br") parts.push(" ");
    if (x.first) {
      stack.push(x);
      x = x.first;
    } else x = x.next;
  }
  return parts.join("");
}

// The plain text of inlines, for labels.
export function inlineText(nodes: Inline[]): string {
  let s = "";
  for (const n of nodes) {
    if (n.t === "text" || n.t === "code") s += n.v;
    else if (n.t === "img") s += n.alt;
    else if (n.t === "br") s += " ";
    else s += inlineText(n.c);
  }
  return s;
}

const LETTER = /\p{L}/u;
const RTL_LETTER = /[\p{Script=Arabic}\p{Script=Hebrew}\p{Script=Syriac}\p{Script=Thaana}\p{Script=Nko}\p{Script=Samaritan}\p{Script=Mandaic}]/u;

// The direction of the first letter, as dir="auto" finds it; null when there
// is no letter.
export function textDir(s: string): "rtl" | "ltr" | null {
  const m = LETTER.exec(s);
  return m ? (RTL_LETTER.test(m[0]) ? "rtl" : "ltr") : null;
}

// The same for inlines. Code is skipped: it is drawn with dir="ltr", which
// dir="auto" doesn't look into.
export function inlineDir(nodes: Inline[]): "rtl" | "ltr" | null {
  for (const n of nodes) {
    const d =
      n.t === "text" ? textDir(n.v)
      : n.t === "img" ? textDir(n.alt || n.url)
      : n.t === "em" || n.t === "strong" || n.t === "del" || n.t === "link" ? inlineDir(n.c)
      : null;
    if (d) return d;
  }
  return null;
}
