import { carryLinkState, linkInfoFromRaw, mergePermissions, permissionOctal, safeLeafName, shellSingleQuote } from "./dirContext.ts";
import type { FileEntry } from "./types.ts";

function eq(actual: unknown, expected: unknown, label: string) {
  if (actual !== expected) {
    throw new Error(`${label}: ${String(actual)} !== ${String(expected)}`);
  }
}

eq(safeLeafName("  notes.txt  "), "notes.txt", "trim");
eq(safeLeafName(".."), null, "dotdot");
eq(safeLeafName("."), null, "dot");
eq(safeLeafName("a/b"), null, "slash");
eq(safeLeafName("a\\b"), null, "backslash");
eq(safeLeafName("a\0b"), null, "nul");
eq(safeLeafName("   "), null, "blank");
eq(shellSingleQuote("/var"), "'/var'", "plain quote");
eq(shellSingleQuote("/tmp/it's"), `'/tmp/it'"'"'s'`, "embedded quote");
eq(mergePermissions(0o2755, 0o755), 0o2755, "keep setgid");
eq(mergePermissions(0o1777, 0o755), 0o1755, "keep sticky");
eq(mergePermissions(undefined, 0o644), 0o644, "unknown mode");
eq(permissionOctal(0o2755), "755", "octal field");
eq(permissionOctal(0), "000", "zero mode");
eq(permissionOctal(undefined), "", "missing mode");

const row = (over: Partial<FileEntry>): FileEntry =>
  ({ name: "x", path: "/d/x", isDir: false, size: 0, ...over });
const fresh = [
  row({ path: "/d/lnk", isSymlink: true, linkState: "pending", size: 7, modified: 5, linkModified: 5 }),
  row({ path: "/d/moved", isSymlink: true, linkState: "pending", linkModified: 9 }),
  row({ path: "/d/new", isSymlink: true, linkState: "pending" }),
  row({ path: "/d/file" }),
];
const known = [
  row({ path: "/d/lnk", isSymlink: true, linkState: "ok", linkTarget: "/t", isDir: true, size: 4096, modified: 1, linkModified: 5 }),
  row({ path: "/d/moved", isSymlink: true, linkState: "ok", linkTarget: "/old", isDir: true, linkModified: 5 }),
  row({ path: "/d/gone", isSymlink: true, linkState: "broken", linkTarget: "nowhere" }),
  row({ path: "/d/file", isSymlink: true, linkState: "ok", isDir: true }),
];
const carried = carryLinkState(fresh, known);
eq(carried[0].linkState, "ok", "carry state");
eq(carried[0].linkTarget, "/t", "carry target");
eq(carried[0].isDir, true, "carry kind");
eq(carried[0].size, 4096, "carry target size");
eq(carried[0].modified, 1, "carry target mtime");
eq(carried[0].linkModified, 5, "link mtime stays");
eq(carried[0].linkStale, true, "carried state is stale");
eq(carried[1].linkState, "pending", "retargeted link is followed again");
eq(carried[1].linkTarget, undefined, "retargeted link drops old target");
eq(carried[2].linkState, "pending", "unknown link stays pending");
eq(carried[3].isDir, false, "plain file ignores stale link row");
eq(carried.length, 4, "removed link not resurrected");

// A directory link that broke: the second STAT answers with the link's own
// lstat data and must undo the folder the row still carries.
const wasDir = carried[0];
const broke = linkInfoFromRaw({
  path: "/d/lnk", state: "broken", target: "/t", error: "No such file",
  is_dir: false, size: 2, permissions: 0o777, modified: 5,
});
const afterBreak = { ...wasDir, ...broke.patch };
eq(broke.path, "/d/lnk", "patch keyed by path");
eq(afterBreak.linkState, "broken", "state updated");
eq(afterBreak.isDir, false, "broken link is no longer a folder");
eq(afterBreak.size, 2, "broken link shows its own size");
eq(afterBreak.linkError, "No such file", "error text kept");
eq(linkInfoFromRaw({ path: "/d/x", state: "ok", is_dir: true }).patch.size, 0, "missing size reads as 0");

console.log("dirContext ok");
