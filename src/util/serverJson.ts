// A server row's `tunnels` and `mirrors` are JSON text holding a list of objects.
// A synced row can hold anything there (whoever shares the profile writes it, and
// the sync only checks that it is text), and one entry that isn't an object
// (`[null]`) took the whole window to the error screen as soon as the row was
// opened for editing. What isn't a list of objects is read as the entries that
// are, or as none.
export function parseObjectList(text: unknown): any[] {
  if (typeof text !== "string" || !text) return [];
  try {
    const value = JSON.parse(text);
    return Array.isArray(value) ? value.filter((e) => e !== null && typeof e === "object" && !Array.isArray(e)) : [];
  } catch {
    return [];
  }
}

// One entry's fields are as unchecked as the list. React writes a text box's
// value as `"" + value`, which throws for an object like {"toString":1}, and
// `excludes` has to be a list for the form to join it: either one, on a synced
// row, took the whole window down on Edit. Only what can't be shown is left
// out, so a row in good order comes back as it was (the save compares them).
export function normalizeTunnel(t: any): any {
  const out = { ...t };
  for (const field of ["rid", "type", "local", "remote", "name"]) {
    if (out[field] !== null && typeof out[field] === "object") delete out[field];
  }
  return out;
}

export function normalizeMirror(m: any): any {
  return {
    ...m,
    local: typeof m.local === "string" ? m.local : "",
    remote: typeof m.remote === "string" ? m.remote : "",
    excludes: Array.isArray(m.excludes) ? m.excludes.filter((s: unknown) => typeof s === "string") : [],
    conflict_resolution: typeof m.conflict_resolution === "string" ? m.conflict_resolution : "local",
  };
}

// A panel wrote a server row itself (its forwards, mirrors, notes): the app's
// list is behind the table now. The event is the one it listens to for that.
export function serverRowChanged() {
  window.dispatchEvent(new Event("submarine-held-changed"));
}
