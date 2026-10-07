// "Filter by name" in the file panels. Plain text keeps every name that
// contains it. Text with a wildcard is matched against the whole name, as a
// shell glob is: `*` stands for any run of characters (none included) and `?`
// for exactly one, so "*.sh" keeps the names that end in ".sh". Both kinds
// ignore case.

const WILDCARD = /[*?]/;

/** Test for the filter text, or null when the text doesn't filter anything. */
export function nameFilterMatcher(filter: string): ((name: string) => boolean) | null {
  const needle = filter.trim().toLowerCase();
  if (!needle) return null;
  if (!WILDCARD.test(needle)) return (name) => name.toLowerCase().includes(needle);
  // Code points, not UTF-16 units, so `?` also stands for one emoji.
  const pattern = Array.from(needle);
  return (name) => wildcardMatch(pattern, Array.from(name.toLowerCase()));
}

// Greedy match that only ever backs up to the latest `*`. That is enough for
// `*` and `?`, and it takes at most pattern × name steps. A RegExp built from
// the pattern would try every way of splitting the name between the `*`s,
// which "*a*a*a*a*a*a*b" on a long run of a's turns into a hang.
function wildcardMatch(pattern: string[], name: string[]): boolean {
  let p = 0;
  let n = 0;
  let star = -1; // pattern index of the latest `*`
  let starEnd = 0; // where the text that `*` covers ends
  while (n < name.length) {
    // `*` first: a name can contain a `*` too, and matching the two as plain
    // characters would cost the pattern its wildcard.
    if (p < pattern.length && pattern[p] === "*") {
      star = p++;
      starEnd = n;
    } else if (p < pattern.length && (pattern[p] === "?" || pattern[p] === name[n])) {
      p++;
      n++;
    } else if (star >= 0) {
      p = star + 1;
      n = ++starEnd;
    } else {
      return false;
    }
  }
  while (p < pattern.length && pattern[p] === "*") p++;
  return p === pattern.length;
}
