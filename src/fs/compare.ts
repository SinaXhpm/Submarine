// Comparing what the two file panes list (#18), like FileZilla's directory
// comparison: each name in one pane against the same name in the other.
// Nothing is read beyond the listings: a file's size and its modified time.
import type { FileEntry } from "./types";

// only:   no entry of that name on the other side
// type:   a file on one side, a folder on the other
// size:   both files, of different sizes
// newer / older: same size, but modified later / earlier than the other one
export type CompareMark = "only" | "type" | "size" | "newer" | "older";

// Modified times this close count as the same: FAT keeps them to 2 s, SFTP
// to 1 s.
const SAME_TIME_S = 2;

// Each entry of `mine`, by path, with what differs from `other`; no mark for
// one that looks the same (folders on both sides always do: they aren't
// looked into).
export function compareListings(mine: FileEntry[], other: FileEntry[]): Map<string, CompareMark> {
  const byName = new Map(other.map((e) => [e.name, e]));
  const marks = new Map<string, CompareMark>();
  for (const e of mine) {
    const o = byName.get(e.name);
    let mark: CompareMark | null = null;
    if (!o) mark = "only";
    else if (e.isDir !== o.isDir) mark = "type";
    else if (e.isDir) mark = null;
    else if (e.size !== o.size) mark = "size";
    else if (e.modified != null && o.modified != null && Math.abs(e.modified - o.modified) > SAME_TIME_S) {
      mark = e.modified > o.modified ? "newer" : "older";
    }
    if (mark) marks.set(e.path, mark);
  }
  return marks;
}

export const COMPARE_LABEL: Record<CompareMark, string> = {
  only: "Only here",
  type: "A file on one side, a folder on the other",
  size: "A different size on the other side",
  newer: "Newer than the other side's",
  older: "Older than the other side's",
};
