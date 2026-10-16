// Which servers a saved command is for (#22): every server (no scope), or
// the servers and folders picked for it, by their synced uuids. A folder takes
// in the folders inside it. Kept in `commands.scope` as JSON (the backend
// checks and rewrites it: normalize_command_scope in lib.rs). Read here
// defensively, since a synced peer could send anything: a scope that can't
// be read counts as every server, so no command is lost from view.
import { folderTree } from "./folderTree";

export type CommandScope = { servers: string[]; folders: string[] };

export function parseCommandScope(raw: unknown): CommandScope | null {
  if (typeof raw !== "string" || !raw.trim()) return null;
  try {
    const v = JSON.parse(raw);
    if (!v || typeof v !== "object" || Array.isArray(v)) return null;
    const ids = (x: unknown) => (Array.isArray(x) ? x.filter((s): s is string => typeof s === "string") : []);
    const scope = { servers: ids(v.servers), folders: ids(v.folders) };
    return scope.servers.length || scope.folders.length ? scope : null;
  } catch {
    return null;
  }
}

export function scopeToJson(scope: CommandScope | null): string | null {
  return scope && (scope.servers.length || scope.folders.length) ? JSON.stringify(scope) : null;
}

// The uuids a scope may name for this server: its own and those of the
// folders it is in, at every level.
export function serverScopeIds(server: any, folders: any[]): { server: string | null; folders: string[] } {
  const tree = folderTree(folders);
  const f = tree.serverFolder(server);
  const chain = f == null ? [] : tree.pathOf(f);
  return { server: server?.uuid ?? null, folders: chain.map((x) => x.uuid).filter(Boolean) };
}

export function scopeMatches(scope: CommandScope, ids: { server: string | null; folders: string[] }): boolean {
  return (!!ids.server && scope.servers.includes(ids.server)) || ids.folders.some((u) => scope.folders.includes(u));
}

// "web-1, web-2 and the Acme folder", short enough for a card.
export function describeScope(scope: CommandScope | null, servers: any[], folders: any[]): string {
  if (!scope) return "All servers";
  const names = [
    ...scope.folders.map((u) => folders.find((f) => f.uuid === u)?.name).filter(Boolean).map((n) => `${n} (folder)`),
    ...scope.servers.map((u) => servers.find((s) => s.uuid === u)?.name).filter(Boolean),
  ] as string[];
  if (names.length === 0) return "No server here";
  return names.length <= 3 ? names.join(", ") : `${names.slice(0, 2).join(", ")} and ${names.length - 2} more`;
}
