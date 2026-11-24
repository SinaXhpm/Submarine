// Selecting terminal text by touch (#324): the buffer side. xterm selects
// text only with a mouse, so a long press finds the word here and hands
// xterm the cells with term.select().
import type { IBuffer } from 'xterm';

/** A cell of the buffer: its column, and its row with the scrollback counted. */
export type Cell = { x: number; y: number };

/** The first cell and the last cell of a selection, both in it. */
export type CellRange = { start: Cell; end: Cell };

/** xterm's own default, the characters that end a double-click's word. */
const WORD_SEPARATORS = ' ()[]{}\',"`';

export const isBefore = (a: Cell, b: Cell) => a.y < b.y || (a.y === b.y && a.x < b.x);

/** The two cells in buffer order. */
export const ordered = (a: Cell, b: Cell): CellRange => (isBefore(b, a) ? { start: b, end: a } : { start: a, end: b });

/** How many cells term.select() takes for a range, the last cell in. */
export const cellCount = (range: CellRange, cols: number) =>
  (range.end.y - range.start.y) * cols + range.end.x - range.start.x + 1;

/**
 * The word at a cell, as a double-click takes it, also where a long line
 * wraps onto the next row. A separator character is a word of its own; a
 * blank cell has none (null). The right half of a wide character goes with
 * its left half.
 */
export function wordAt(buffer: IBuffer, cols: number, at: Cell, separators = WORD_SEPARATORS): CellRange | null {
  const cell = (c: Cell) => buffer.getLine(c.y)?.getCell(c.x);
  const isRightHalf = (c: Cell) => c.x > 0 && cell(c)?.getWidth() === 0;
  const pos = isRightHalf(at) ? { x: at.x - 1, y: at.y } : at;
  const chars = cell(pos)?.getChars() ?? '';
  if (chars === '' || chars.trim() === '') return null;
  // A wide character ends on its right half.
  const charEnd = (c: Cell) => (c.x + 1 < cols && isRightHalf({ x: c.x + 1, y: c.y }) ? { x: c.x + 1, y: c.y } : c);
  if (separators.includes(chars)) return { start: pos, end: charEnd(pos) };
  const inWord = (c: Cell) => {
    const here = cell(c);
    if (!here) return false;
    if (here.getWidth() === 0) return true;
    const ch = here.getChars();
    return ch !== '' && ch.trim() !== '' && !separators.includes(ch);
  };
  const prev = (c: Cell): Cell | null => {
    if (c.x > 0) return { x: c.x - 1, y: c.y };
    return c.y > 0 && buffer.getLine(c.y)?.isWrapped ? { x: cols - 1, y: c.y - 1 } : null;
  };
  const next = (c: Cell): Cell | null => {
    if (c.x < cols - 1) return { x: c.x + 1, y: c.y };
    return buffer.getLine(c.y + 1)?.isWrapped ? { x: 0, y: c.y + 1 } : null;
  };
  let start = pos;
  for (let p = prev(start); p && inWord(p); p = prev(p)) start = p;
  let end = pos;
  for (let n = next(end); n && inWord(n); n = next(n)) end = n;
  return { start, end };
}

/** Everything with text in the buffer, the scrollback included: Select all. */
export function allText(buffer: IBuffer, cols: number): CellRange | null {
  for (let y = buffer.length - 1; y >= 0; y--) {
    const line = buffer.getLine(y);
    for (let x = cols - 1; line && x >= 0; x--) {
      const here = line.getCell(x);
      if (!here || (here.getChars().trim() === '' && here.getWidth() !== 0)) continue;
      return { start: { x: 0, y: 0 }, end: { x, y } };
    }
  }
  return null;
}
