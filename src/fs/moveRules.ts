// Rules for moving entries into a folder of the same pane (drag-and-drop).
// Pure, so the "is this drop allowed" decisions can be checked without a UI.

export interface PathRules {
  sep: "/" | "\\";
  /** Names differing only by case are the same entry. */
  foldCase: boolean;
}

// The local side always folds case: Windows and the default macOS volume are
// case-insensitive, and a local rename REPLACES an existing destination, so
// on a case-sensitive disk the only cost is refusing a move that would have
// been fine. SFTP paths are compared exactly.
export const rulesFor = (providerId: string, sep: "/" | "\\"): PathRules => ({
  sep,
  foldCase: providerId === "local",
});

export const nameKey = (name: string, rules: PathRules) =>
  rules.foldCase ? name.toLowerCase() : name;

// One spelling per directory: a single separator style (a Windows path may
// mix `\` and `/`), no trailing separator, case folded when the rules say so.
// A root comes out as the bare separator ("/") or the bare drive ("c:").
export function pathKey(path: string, rules: PathRules): string {
  const { sep } = rules;
  let s = sep === "\\" ? path.replace(/\//g, "\\") : path;
  while (s.length > 0 && s.endsWith(sep)) s = s.slice(0, -1);
  if (s === "") s = sep;
  return rules.foldCase ? s.toLowerCase() : s;
}

function parentKey(key: string, sep: string): string {
  const idx = key.lastIndexOf(sep);
  return idx <= 0 ? sep : key.slice(0, idx);
}

// False for a no-op (already in that folder) and for a folder dropped onto
// itself or into its own subtree.
export function canMoveInto(
  item: { path: string; isDir: boolean },
  targetDir: string,
  rules: PathRules,
): boolean {
  const from = pathKey(item.path, rules);
  const to = pathKey(targetDir, rules);
  if (parentKey(from, rules.sep) === to) return false;
  if (!item.isDir) return true;
  if (from === to) return false;
  return !to.startsWith(from + rules.sep);
}

// Names in `names` that the target folder already holds.
export function takenNames(names: string[], existing: string[], rules: PathRules): Set<string> {
  const have = new Set(existing.map((n) => nameKey(n, rules)));
  return new Set(names.filter((n) => have.has(nameKey(n, rules))));
}
