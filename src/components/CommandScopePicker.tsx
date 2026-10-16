import { useMemo, useState } from "react";
import { Folder, Server } from "lucide-react";
import { CommandScope } from "../util/commandScope";
import { folderTree } from "../util/folderTree";

// Which servers a command is for (#22): all of them, or the folders and
// servers ticked here. A folder takes in what's inside it, sub-folders too.
// Uuids this device doesn't know (a server not synced here yet) are kept as
// they are: only what's shown can be ticked off.
export default function CommandScopePicker({
  scope,
  onChange,
  servers,
  folders,
}: {
  scope: CommandScope | null;
  onChange: (scope: CommandScope | null) => void;
  servers: any[];
  folders: any[];
}) {
  const [someOnly, setSomeOnly] = useState(!!scope);
  const [query, setQuery] = useState("");
  const tree = useMemo(() => folderTree(folders), [folders]);
  // Folders in tree order, each with its depth.
  const folderRows = useMemo(() => {
    const out: { f: any; depth: number }[] = [];
    const walk = (parent: number | null, depth: number) => {
      for (const f of tree.childrenOf(parent)) {
        out.push({ f, depth });
        walk(f.id, depth + 1);
      }
    };
    walk(null, 0);
    return out.filter((r) => r.f.uuid);
  }, [tree]);
  const serverRows = useMemo(
    () => servers.filter((s) => s.uuid).sort((a, b) => String(a.name).localeCompare(String(b.name))),
    [servers],
  );
  const q = query.trim().toLowerCase();
  const chosen = scope ?? { servers: [], folders: [] };
  const toggle = (kind: "servers" | "folders", uuid: string, on: boolean) => {
    const next = { ...chosen, [kind]: on ? [...chosen[kind], uuid] : chosen[kind].filter((u) => u !== uuid) };
    onChange(next.servers.length || next.folders.length ? next : { servers: [], folders: [] });
  };
  const box = (kind: "servers" | "folders", uuid: string) => (
    <input
      type="checkbox"
      checked={chosen[kind].includes(uuid)}
      onChange={(e) => toggle(kind, uuid, e.target.checked)}
      className="accent-[var(--primary-hex)] shrink-0"
    />
  );
  return (
    <div className="space-y-2">
      <label className="text-[12px] font-bold text-zinc-400 ml-1">For</label>
      <div role="radiogroup" aria-label="Which servers" className="grid grid-cols-2 gap-2">
        {[
          { on: !someOnly, label: "All servers", pick: () => { setSomeOnly(false); onChange(null); } },
          { on: someOnly, label: "Some servers", pick: () => { setSomeOnly(true); onChange(scope ?? { servers: [], folders: [] }); } },
        ].map((o) => (
          <button
            key={o.label}
            type="button"
            role="radio"
            aria-checked={o.on}
            onClick={o.pick}
            className={`h-9 rounded-lg border text-[12.5px] transition-colors ${o.on ? "border-primary/60 bg-primary/10 text-white font-semibold" : "border-white/10 text-zinc-400 hover:border-white/25"}`}
          >
            {o.label}
          </button>
        ))}
      </div>
      {someOnly && (
        <div className="rounded-lg border border-white/10 bg-black/40">
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Find a server or folder"
            aria-label="Find a server or folder"
            className="w-full h-8 bg-transparent border-b border-white/10 px-3 text-[12px] text-white outline-none"
          />
          <div className="max-h-56 overflow-y-auto p-1 text-[12.5px]">
            {folderRows.filter((r) => !q || String(r.f.name).toLowerCase().includes(q)).map(({ f, depth }) => (
              <label key={`f${f.id}`} className="flex items-center gap-2 px-2 py-1 rounded hover:bg-white/5 cursor-pointer" style={{ paddingLeft: 8 + (q ? 0 : depth * 14) }}>
                {box("folders", f.uuid)}
                <Folder size={12} className="text-indigo-300 shrink-0" />
                <span className="truncate text-zinc-200">{f.name}</span>
              </label>
            ))}
            {serverRows.filter((s) => !q || `${s.name} ${s.host}`.toLowerCase().includes(q)).map((s) => {
              const f = tree.serverFolder(s);
              return (
                <label key={`s${s.id}`} className="flex items-center gap-2 px-2 py-1 rounded hover:bg-white/5 cursor-pointer">
                  {box("servers", s.uuid)}
                  <Server size={12} className="text-zinc-400 shrink-0" />
                  <span className="truncate text-zinc-200">{s.name}</span>
                  {f != null && <span className="ml-auto truncate text-[10.5px] text-zinc-500">{tree.label(f)}</span>}
                </label>
              );
            })}
            {folderRows.length === 0 && serverRows.length === 0 && (
              <div className="px-2 py-2 text-[11.5px] text-zinc-500">No saved servers yet.</div>
            )}
          </div>
        </div>
      )}
      <p className="text-[11px] text-zinc-500 leading-relaxed ml-1">
        {someOnly
          ? "In a session, the Library shows this command only for the servers ticked here and those in the ticked folders."
          : "Shown in every session's Library."}
      </p>
    </div>
  );
}
