// Server folders nest through `parent_id` (synced as the parent's uuid). The
// tree is read defensively so nothing can vanish from view: a parent that no
// longer exists, or a loop of parents (possible once moves made on two devices
// are merged), puts the folder at the top level; a server whose folder is gone
// shows at the top level too.

export interface FolderTree {
  has: (id: number) => boolean;
  /** The folder a folder shows in (null: top level). */
  parentOf: (id: number) => number | null;
  /** The folders shown in `id` (null: top level), in stored order. */
  childrenOf: (id: number | null) => any[];
  /** The folders from the top level down to `id`, `id` included. */
  pathOf: (id: number) => any[];
  /** `id` and every folder under it. */
  subtreeOf: (id: number) => Set<number>;
  /** "Clients / Acme / Prod". */
  label: (id: number) => string;
  /** The folder a server shows in (null: top level). */
  serverFolder: (server: any) => number | null;
}

export function folderTree(folders: any[] | null | undefined): FolderTree {
  const list = folders ?? [];
  const byId = new Map<number, any>(list.map((f) => [f.id, f]));
  const rawParent = (id: number): number | null => {
    const p = byId.get(id)?.parent_id ?? null;
    return p !== null && byId.has(p) ? p : null;
  };
  // A folder whose chain of parents comes back to itself sits in a loop; such
  // folders go to the top level, which breaks every loop.
  const inLoop = (id: number): boolean => {
    const seen = new Set<number>();
    for (let cur = rawParent(id); cur !== null; cur = rawParent(cur)) {
      if (cur === id) return true;
      if (seen.has(cur)) return false;
      seen.add(cur);
    }
    return false;
  };
  const parent = new Map<number, number | null>();
  for (const f of list) parent.set(f.id, inLoop(f.id) ? null : rawParent(f.id));

  const children = new Map<number | null, any[]>();
  for (const f of list) {
    const p = parent.get(f.id) ?? null;
    if (!children.has(p)) children.set(p, []);
    children.get(p)!.push(f);
  }

  const parentOf = (id: number) => parent.get(id) ?? null;
  const pathOf = (id: number) => {
    const out: any[] = [];
    for (let cur: number | null = id; cur !== null && byId.has(cur); cur = parentOf(cur)) out.unshift(byId.get(cur));
    return out;
  };
  const subtreeOf = (id: number) => {
    const out = new Set<number>([id]);
    const queue = [id];
    while (queue.length) {
      for (const c of children.get(queue.shift()!) ?? []) {
        if (!out.has(c.id)) { out.add(c.id); queue.push(c.id); }
      }
    }
    return out;
  };
  return {
    has: (id) => byId.has(id),
    parentOf,
    childrenOf: (id) => children.get(id) ?? [],
    pathOf,
    subtreeOf,
    label: (id) => pathOf(id).map((f) => f.name).join(" / "),
    serverFolder: (s) => (s?.folder_id != null && byId.has(s.folder_id) ? s.folder_id : null),
  };
}
