import { Fragment, useState, useEffect, useRef } from "react";
import { parseCommandScope, scopeMatches, serverScopeIds } from "../util/commandScope";
import { invoke } from "@tauri-apps/api/core";
import { Play, RefreshCw, Search, X, Terminal, StickyNote, ClipboardCopy, ChevronDown, ChevronRight, Pencil, Check, Server } from "lucide-react";
import { useConfirm } from "../ui/confirm";
import { pasteIntoTerminal, withoutPasteControls } from "../util/pasteText";
import { serverRowChanged } from "../util/serverJson";

// Side panel that opens from a terminal session and surfaces *both* the
// user's saved Quick Commands and their Notes. Two patterns for editing:
//
//   - Notes are edit-by-default: clicking a row expands it directly into an
//     editable title input + body textarea. No Edit toggle. Save lights up
//     only when the draft diverges from the stored copy, and collapsing the
//     row or switching tabs auto-saves any pending changes so the user
//     can't lose work by clicking away.
//
//   - Commands stay compact by default (one-liner row + action buttons),
//     because they're typically short and a dropped-in textarea per item
//     would bloat the list. An Edit button on each row toggles the same
//     inline editor for that command.
//
// Both kinds get Play (write + CR → execute), Paste (write without CR →
// drop into the prompt for review), and Copy (clipboard) actions. Search
// runs client-side over the active tab's title + body fields.

// `scope`: which servers it's for (util/commandScope), as stored.
interface CommandItem { id: number; title: string; content: string; scope?: string | null; }
interface NoteItem { id: number; title: string; body: string; }
type Tab = "commands" | "notes" | "node";

// Single shared "what's being edited right now" slot. Only one row can be
// editing at a time (notes expand-to-edit, commands toggle-to-edit), so a
// flat tagged union is simpler than two parallel sets of useState fields.
type EditTarget =
  | { kind: "command"; id: number; title: string; body: string }
  | { kind: "note"; id: number; title: string; body: string };

// Whether the command of a card's Run button is cut short on that card. Measured,
// not counted: a card shows about 30 characters in the narrowest panel and far
// more in a wide one.
const cutOnCard = (button: HTMLElement) => {
  const line = button.closest("[data-command-card]")?.querySelector("pre");
  return !line || line.scrollWidth > line.clientWidth;
};

// `headerActions`: extra title-bar buttons from the session view (full screen),
// shown just before Close.
export const CmdsPanel = ({ activeTab, onClose, headerActions, serverId, serverName }: { activeTab: string; onClose: () => void; headerActions?: React.ReactNode; serverId?: number; serverName?: string }) => {
  const confirm = useConfirm();
  const [tab, setTab] = useState<Tab>("commands");
  const [commands, setCommands] = useState<CommandItem[]>([]);
  const [notes, setNotes] = useState<NoteItem[]>([]);
  const [searchQuery, setSearchQuery] = useState("");
  const [loading, setLoading] = useState(false);
  // This session's server and its folders, by uuid, for the commands that
  // are for some servers only (util/commandScope). Null: not a saved server.
  const [scopeIds, setScopeIds] = useState<{ server: string | null; folders: string[] } | null>(null);

  // Node notes — the per-server free-form notes from AddNodePanel, surfaced
  // here so the user can read/edit them next to commands + notes without
  // bouncing back to the server editor. Source of truth is the same SQLite
  // column (servers.notes) — we fetch via get_servers and save via
  // set_server_notes. `nodeOriginal` mirrors the persisted copy so Save lights
  // up only when the draft diverges (same dirty-check pattern as commands/notes).
  const hasNode = typeof serverId === "number" && serverId > 0;
  const [nodeDraft, setNodeDraft] = useState<string>("");
  const [nodeOriginal, setNodeOriginal] = useState<string>("");
  const [nodeSaving, setNodeSaving] = useState(false);
  const nodeDraftRef = useRef<string>("");
  useEffect(() => { nodeDraftRef.current = nodeDraft; }, [nodeDraft]);

  const [edit, setEdit] = useState<EditTarget | null>(null);
  const [saving, setSaving] = useState(false);
  // Why the last save failed, until the next one.
  const [saveError, setSaveError] = useState<string | null>(null);
  // Ref mirror of the edit slot so the cleanup logic in tab-switch /
  // refresh / unmount can read the latest draft without re-binding the
  // effect on every keystroke.
  const editRef = useRef<EditTarget | null>(null);
  useEffect(() => { editRef.current = edit; }, [edit]);

  // Snapshot of the saved title/body for the row currently being edited.
  // Used to gate the Save button (only enabled when the draft differs) and
  // to know whether an auto-save should fire on collapse.
  const originalForEdit = (target: EditTarget): { title: string; body: string } | null => {
    if (target.kind === "command") {
      const c = commands.find((x) => x.id === target.id);
      return c ? { title: c.title || "", body: c.content || "" } : null;
    }
    const n = notes.find((x) => x.id === target.id);
    return n ? { title: n.title || "", body: n.body || "" } : null;
  };

  const isDirty = (target: EditTarget | null): boolean => {
    if (!target) return false;
    const o = originalForEdit(target);
    if (!o) return false;
    return o.title !== target.title || o.body !== target.body;
  };

  const fetchAll = async () => {
    setLoading(true);
    try {
      const tasks: Promise<any>[] = [
        invoke<CommandItem[]>("get_commands"),
        invoke<NoteItem[]>("get_notes"),
      ];
      // Pull node notes from get_servers when this Library is scoped to a
      // session. The backend exposes only the bulk list — cheap enough
      // (handful of rows) and avoids a new single-server command.
      // And the folders, for which of the commands are for this server.
      if (hasNode) tasks.push(invoke<any[]>("get_servers"), invoke<any[]>("get_folders"));
      const out = await Promise.all(tasks);
      setCommands((out[0] as CommandItem[]) || []);
      setNotes((out[1] as NoteItem[]) || []);
      if (hasNode) {
        const servers = (out[2] as any[]) || [];
        const server = servers.find((s) => s.id === serverId);
        const text: string = server?.notes ?? "";
        setNodeOriginal(text);
        // A draft being typed stays: this also runs on Refresh.
        setNodeDraft((draft) => (draft !== nodeOriginal ? draft : text));
        setScopeIds(server ? serverScopeIds(server, (out[3] as any[]) || []) : null);
      } else {
        setScopeIds(null);
      }
    } catch (e) {
      console.error("Failed to fetch Library data:", e);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { fetchAll(); }, [serverId]);

  const isNodeDirty = nodeDraft !== nodeOriginal;
  // False when the notes couldn't be saved (the reason is on screen).
  const saveNodeNotes = async () => {
    if (!hasNode || nodeSaving || !isNodeDirty) return true;
    setNodeSaving(true);
    setSaveError(null);
    try {
      await invoke("set_server_notes", { id: serverId, notes: nodeDraft });
      setNodeOriginal(nodeDraft);
      serverRowChanged();
      return true;
    } catch (e) {
      setSaveError(`Couldn't save the notes: ${e}`);
      return false;
    } finally {
      setNodeSaving(false);
    }
  };

  // Save the currently editing row (if dirty), without touching the edit
  // state itself — the caller decides whether to collapse afterwards. We
  // accept a target arg so callers that already cleared local state can
  // still pass through their snapshot. False when the save failed: the text
  // goes back into the editor, which may have closed already.
  const flushIfDirty = async (target: EditTarget | null) => {
    if (!target) return true;
    const o = originalForEdit(target);
    if (!o || (o.title === target.title && o.body === target.body)) return true;
    setSaveError(null);
    try {
      if (target.kind === "command") {
        await invoke("edit_command", { id: target.id, title: target.title, content: target.body });
        setCommands((prev) => prev.map((c) => c.id === target.id ? { ...c, title: target.title, content: target.body } : c));
      } else {
        await invoke("edit_note", { id: target.id, title: target.title, body: target.body });
        setNotes((prev) => prev.map((n) => n.id === target.id ? { ...n, title: target.title, body: target.body } : n));
      }
      return true;
    } catch (e) {
      setEdit((cur) => cur ?? target);
      setSaveError(`Couldn't save "${target.title || "Untitled"}": ${e}`);
      return false;
    }
  };

  // Everything being typed, saved. False when something couldn't be.
  const flushDrafts = async () => {
    const saved = await flushIfDirty(editRef.current);
    return (await saveNodeNotes()) && saved;
  };

  // Close, another tool and closing the tab all unmount this panel, and what
  // is being typed would go with it. From refs, for the latest drafts and
  // lists; X has saved them already. The panel can't show a failure any more,
  // so a dialog does.
  const flushRef = useRef(flushDrafts);
  flushRef.current = flushDrafts;
  const closedRef = useRef(false);
  useEffect(() => () => {
    if (closedRef.current) return;
    void flushRef.current().then((saved) => {
      if (!saved) void confirm({ title: "Not saved", message: "What you were typing in the Library couldn't be saved.", okLabel: "OK" });
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  // Lock asks before the profile closes, while a save can still land
  // (`saveDrafts` in DesktopApp).
  useEffect(() => {
    const onSaveDrafts = (e: Event) => { (e as CustomEvent<Promise<boolean>[]>).detail.push(flushRef.current()); };
    window.addEventListener("submarine-save-drafts", onSaveDrafts);
    return () => window.removeEventListener("submarine-save-drafts", onSaveDrafts);
  }, []);

  // Tab switch: persist whatever's open in the current tab before clearing
  // the editor and search so the next tab starts clean. Also auto-saves
  // pending node-notes draft when leaving the Node tab so the user can't
  // lose work by switching tabs.
  useEffect(() => {
    const snapshot = editRef.current;
    setSearchQuery("");
    setEdit(null);
    flushIfDirty(snapshot);
    if (hasNode && nodeDraftRef.current !== nodeOriginal) {
      void saveNodeNotes();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  const beginEditCommand = (c: CommandItem) => {
    setEdit({ kind: "command", id: c.id, title: c.title || "", body: c.content || "" });
  };
  const beginEditNote = (n: NoteItem) => {
    setEdit({ kind: "note", id: n.id, title: n.title || "", body: n.body || "" });
  };

  const cancelEdit = () => {
    setEdit(null);
  };

  // Collapse the currently expanded note (auto-saves on the way out). Used
  // when the user clicks the chevron-area of an expanded note to retract it.
  const collapseEditedNote = async () => {
    const snapshot = editRef.current;
    setEdit(null);
    await flushIfDirty(snapshot);
  };

  const saveEdit = async () => {
    if (!edit || saving) return;
    setSaving(true);
    setSaveError(null);
    try {
      if (edit.kind === "command") {
        await invoke("edit_command", { id: edit.id, title: edit.title, content: edit.body });
        setCommands((prev) => prev.map((c) => c.id === edit.id ? { ...c, title: edit.title, content: edit.body } : c));
      } else {
        await invoke("edit_note", { id: edit.id, title: edit.title, body: edit.body });
        setNotes((prev) => prev.map((n) => n.id === edit.id ? { ...n, title: edit.title, body: edit.body } : n));
      }
      setEdit(null);
      fetchAll();
    } catch (e) {
      setSaveError(`Couldn't save: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  // Put `text` in the active terminal as a paste: no control characters, and
  // bracketed when the program there asked for that. `execute=true` presses
  // Enter after it so the shell runs it; false leaves the cursor at the end so
  // the user can review / edit before pressing Enter themselves. Library text
  // isn't typed here (in a shared profile it can be anyone's) and a card shows
  // only the start of it, so what the card can't show is shown whole first:
  // more than one line, a line with a run of blanks, or a line to be run that
  // the card cuts short (`cut`). A long line that is only pasted can be read at
  // the prompt before anything runs.
  const writeToTerminal = async (text: string, execute: boolean, cut = false) => {
    if (!activeTab) return;
    const clean = withoutPasteControls(text).replace(/[\r\n]+$/, "");
    if (!clean) return;
    const lines = clean.split(/\r\n|\r|\n/).length;
    if (lines > 1 || cut || /[ \t]{16,}/.test(clean)) {
      const what = lines > 1 ? `these ${lines} lines` : "this line";
      const ok = await confirm({
        title: execute ? `Run ${what}?` : `Paste ${what}?`,
        message: clean,
        okLabel: execute ? "Run" : "Paste",
        safeDefault: true,
      });
      if (!ok) return;
    }
    pasteIntoTerminal({ terminalId: activeTab, text: clean, enter: execute });
  };

  const copyToClipboard = async (text: string) => {
    try { await navigator.clipboard.writeText(text); }
    catch (e) { console.error("Clipboard write failed:", e); }
  };

  const q = searchQuery.toLowerCase();
  // Commands for this server first (#22), then those for every server; the
  // ones for other servers aren't shown. A session that isn't a saved server
  // (quick connect) shows those for every server.
  const matching = commands.filter(
    (c) => c.title.toLowerCase().includes(q) || c.content.toLowerCase().includes(q)
  );
  const forThisServer = matching.filter((c) => {
    const s = parseCommandScope(c.scope);
    return !!s && !!scopeIds && scopeMatches(s, scopeIds);
  });
  const filteredCommands = [...forThisServer, ...matching.filter((c) => !parseCommandScope(c.scope))];
  const headingAt = (i: number) =>
    forThisServer.length === 0 ? null : i === 0 ? "For this server" : i === forThisServer.length ? "All servers" : null;
  const filteredNotes = notes.filter(
    (n) => (n.title || "").toLowerCase().includes(q) || (n.body || "").toLowerCase().includes(q)
  );

  const tabLabel = tab === "commands" ? "Commands" : tab === "notes" ? "Notes" : "Node";
  const TabIcon = tab === "commands" ? Terminal : tab === "notes" ? StickyNote : Server;

  return (
    <div className="flex-1 flex flex-col h-full overflow-hidden bg-[#09090b]">
      {/* Title Bar */}
      <div className="h-12 px-4 shrink-0 flex items-center justify-between border-b border-white/5 bg-white/5">
        <div className="flex items-center gap-2 min-w-0">
          <TabIcon size={14} className="text-primary shrink-0" />
          <span className="text-[10px] font-bold uppercase tracking-wider text-zinc-300 truncate">
            Library · {tabLabel}{tab === "node" && serverName ? ` · ${serverName}` : ""}
          </span>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <button
            onClick={async () => { await flushDrafts(); fetchAll(); }}
            disabled={loading}
            className="p-1.5 rounded-lg text-zinc-500 hover:text-white hover:bg-white/5 transition-all disabled:opacity-50"
            title="Refresh list"
          >
            <RefreshCw size={14} className={loading ? "animate-spin" : ""} />
          </button>
          {headerActions}
          <button
            onClick={async () => {
              // A failed save keeps the panel open, with the text and the reason.
              if (await flushDrafts()) { closedRef.current = true; onClose(); }
            }}
            className="p-1.5 rounded-lg text-zinc-500 hover:text-white hover:bg-white/5 transition-all"
            title="Close Panel"
          >
            <X size={14} />
          </button>
        </div>
      </div>

      {/* Tabs — three when a server is in scope (Commands / Notes / Node),
          two otherwise (the Library is reachable from any context, not just
          a connected session, so the Node tab is gated on the prop). */}
      <div className={`grid ${hasNode ? "grid-cols-3" : "grid-cols-2"} shrink-0 border-b border-white/5 bg-black/20`}>
        <button
          onClick={() => setTab("commands")}
          className={`h-9 flex items-center justify-center gap-1.5 text-[11px] font-bold uppercase tracking-wider transition-all ${
            tab === "commands"
              ? "text-primary bg-primary/5 border-b border-primary"
              : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
          }`}
        >
          <Terminal size={12} /> Commands
          <span className="text-[9px] opacity-60">{commands.length}</span>
        </button>
        <button
          onClick={() => setTab("notes")}
          className={`h-9 flex items-center justify-center gap-1.5 text-[11px] font-bold uppercase tracking-wider transition-all ${
            tab === "notes"
              ? "text-primary bg-primary/5 border-b border-primary"
              : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
          }`}
        >
          <StickyNote size={12} /> Notes
          <span className="text-[9px] opacity-60">{notes.length}</span>
        </button>
        {hasNode && (
          <button
            onClick={() => setTab("node")}
            className={`h-9 flex items-center justify-center gap-1.5 text-[11px] font-bold uppercase tracking-wider transition-all ${
              tab === "node"
                ? "text-primary bg-primary/5 border-b border-primary"
                : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
            }`}
          >
            <Server size={12} /> Node
            {isNodeDirty && <span className="text-[9px] text-amber-400">●</span>}
          </button>
        )}
      </div>

      {saveError && (
        <div role="alert" className="shrink-0 px-3 py-1.5 border-b border-white/5 bg-rose-500/10 text-[10.5px] text-rose-300 break-words">
          {saveError}
        </div>
      )}

      {/* Search Input — only on list-shaped tabs; the Node tab is a single
          editor so search would have nothing to filter. */}
      {tab !== "node" && (
        <div className="p-3 shrink-0 border-b border-white/5 bg-black/20">
          <div className="relative">
            <Search size={14} className="absolute left-3 top-1/2 -translate-y-1/2 text-zinc-500" />
            <input
              type="text"
              placeholder={tab === "commands" ? "Search commands (title + body)..." : "Search notes (title + body)..."}
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              className="w-full h-8 pl-9 pr-3 bg-white/5 border border-white/5 rounded-lg text-xs text-white placeholder-zinc-500 focus:outline-none focus:border-primary/50 focus:bg-white/10 transition-all"
            />
          </div>
        </div>
      )}

      {/* List */}
      <div className="flex-1 overflow-y-auto p-3 space-y-2 no-scrollbar">
        {loading ? (
          <div className="flex flex-col items-center justify-center py-12 text-zinc-500 gap-2">
            <RefreshCw size={24} className="animate-spin text-primary opacity-60" />
            <span className="text-[11px]">Loading {tabLabel.toLowerCase()}...</span>
          </div>
        ) : tab === "node" ? (
          <div className="flex flex-col h-full gap-2">
            <div className="flex items-center justify-between gap-2 shrink-0">
              <div className="flex items-center gap-2 min-w-0">
                <Server size={12} className="text-primary shrink-0" />
                <span className="text-[11px] font-bold text-zinc-200 truncate">{serverName || `Node #${serverId}`}</span>
              </div>
              <button
                onClick={() => copyToClipboard(nodeDraft || "")}
                disabled={!nodeDraft}
                className="h-7 px-2 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                title="Copy current draft to clipboard"
              >
                <ClipboardCopy size={11} /> Copy
              </button>
            </div>
            <textarea
              value={nodeDraft}
              onChange={(e) => setNodeDraft(e.target.value)}
              onKeyDown={(e) => {
                if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); void saveNodeNotes(); }
              }}
              placeholder={`Notes for this node — paths, credentials hints, runbook steps…\nSaved on tab switch or Ctrl+Enter.`}
              className="flex-1 w-full p-3 bg-black/40 border border-white/10 rounded-lg font-sans text-[12px] text-zinc-200 placeholder-zinc-600 leading-relaxed resize-none focus:outline-none focus:border-primary/50 custom-scrollbar"
            />
            <div className="flex gap-1.5 shrink-0">
              <button
                onClick={() => setNodeDraft(nodeOriginal)}
                disabled={nodeSaving || !isNodeDirty}
                className="flex-1 h-8 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                title="Revert to last saved version"
              >
                <X size={11} /> Revert
              </button>
              <button
                onClick={() => void saveNodeNotes()}
                disabled={nodeSaving || !isNodeDirty}
                className="flex-1 h-8 rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                title="Save (Ctrl+Enter, auto-saves on tab switch)"
              >
                {nodeSaving ? <RefreshCw size={11} className="animate-spin" /> : <Check size={11} />} Save
              </button>
            </div>
          </div>
        ) : tab === "commands" ? (
          filteredCommands.length > 0 ? (
            filteredCommands.map((c, i) => {
              const editing = edit?.kind === "command" && edit.id === c.id;
              const heading = headingAt(i);
              return (
                <Fragment key={c.id}>
                {heading && (
                  <div className="px-1 pt-1 text-[9.5px] font-bold uppercase tracking-widest text-zinc-500" data-command-group>
                    {heading}
                  </div>
                )}
                <div
                  className="group relative rounded-lg border border-white/5 bg-white/[0.02] hover:bg-white/[0.04] hover:border-white/10 transition-all"
                >
                  {editing ? (
                    <div className="p-3 space-y-2">
                      <input
                        type="text"
                        value={edit.title}
                        onChange={(e) => setEdit({ ...edit, title: e.target.value })}
                        onKeyDown={(e) => {
                          if (e.key === "Escape") { e.preventDefault(); cancelEdit(); }
                        }}
                        placeholder="Title"
                        className="w-full h-7 px-2 bg-black/40 border border-white/10 rounded text-[11px] text-white placeholder-zinc-600 focus:outline-none focus:border-primary/50"
                        autoFocus
                      />
                      <textarea
                        value={edit.body}
                        onChange={(e) => setEdit({ ...edit, body: e.target.value })}
                        onKeyDown={(e) => {
                          if (e.key === "Escape") { e.preventDefault(); cancelEdit(); }
                          if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); saveEdit(); }
                        }}
                        placeholder="Command body — runnable in shell"
                        rows={5}
                        className="w-full p-2 bg-black/40 border border-white/10 rounded font-mono text-[11px] text-zinc-200 placeholder-zinc-600 leading-relaxed resize-y focus:outline-none focus:border-primary/50 custom-scrollbar"
                      />
                      <div className="flex gap-1.5">
                        <button
                          onClick={cancelEdit}
                          disabled={saving}
                          className="flex-1 h-7 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-50"
                        >
                          <X size={11} /> Cancel
                        </button>
                        <button
                          onClick={saveEdit}
                          disabled={saving || !isDirty(edit)}
                          className="flex-1 h-7 rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                          title="Save (Ctrl+Enter)"
                        >
                          {saving ? <RefreshCw size={11} className="animate-spin" /> : <Check size={11} />} Save
                        </button>
                      </div>
                    </div>
                  ) : (
                    <div className="p-3" data-command-card>
                      <div className="flex items-start justify-between gap-3">
                        <div className="flex-1 min-w-0">
                          <h4 className="text-xs font-bold text-zinc-200 truncate group-hover:text-primary transition-colors">
                            {c.title}
                          </h4>
                          <pre
                            className="mt-1.5 p-1.5 px-2 bg-black/40 rounded border border-white/5 font-mono text-[10px] text-zinc-400 truncate whitespace-nowrap overflow-hidden"
                            title={c.content}
                          >
                            {c.content.split('\n')[0] || ""}{c.content.split('\n').length > 1 ? " ..." : ""}
                          </pre>
                        </div>
                      </div>
                      <div className="flex gap-1.5 mt-2">
                        <button
                          onClick={(e) => writeToTerminal(c.content, true, cutOnCard(e.currentTarget))}
                          className="flex-1 h-7 rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                          title="Run in active terminal (writes + Enter)"
                        >
                          <Play size={11} fill="currentColor" /> Run
                        </button>
                        <button
                          onClick={() => writeToTerminal(c.content, false)}
                          className="flex-1 h-7 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                          title="Paste into active terminal (no Enter)"
                        >
                          <ClipboardCopy size={11} /> Paste
                        </button>
                        <button
                          onClick={() => beginEditCommand(c)}
                          className="flex-1 h-7 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                          title="Edit command inline"
                        >
                          <Pencil size={11} /> Edit
                        </button>
                      </div>
                    </div>
                  )}
                </div>
                </Fragment>
              );
            })
          ) : (
            <div className="flex flex-col items-center justify-center py-16 text-zinc-600 text-center px-4">
              <Terminal size={32} className="opacity-20 mb-2" />
              <span className="text-xs font-bold text-zinc-500">No Commands Found</span>
              <p className="text-[10px] mt-1 text-zinc-600 max-w-[200px]">
                {searchQuery ? "No matches for your search query." : "Save commands in the main app to surface them here."}
              </p>
            </div>
          )
        ) : filteredNotes.length > 0 ? (
          filteredNotes.map((n) => {
            const expanded = edit?.kind === "note" && edit.id === n.id;
            return (
              <div
                key={n.id}
                className="group relative rounded-lg border border-white/5 bg-white/[0.02] hover:bg-white/[0.04] hover:border-white/10 transition-all"
              >
                <button
                  onClick={async () => {
                    if (expanded) {
                      // Click on row again → collapse + auto-save.
                      collapseEditedNote();
                    } else if (await flushIfDirty(editRef.current)) {
                      // Switching from one note to another also auto-saves;
                      // if that fails the editor stays on the unsaved note.
                      beginEditNote(n);
                    }
                  }}
                  className="w-full flex items-start gap-2 px-3 pt-3 pb-2 text-left"
                >
                  <span className="mt-0.5 text-zinc-500 group-hover:text-zinc-300 shrink-0">
                    {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
                  </span>
                  <div className="flex-1 min-w-0">
                    <h4 className="text-xs font-bold text-zinc-200 truncate group-hover:text-primary transition-colors">
                      {(expanded ? edit.title : n.title) || "Untitled"}
                    </h4>
                    {!expanded && (
                      <p className="mt-1 text-[10px] text-zinc-500 truncate">
                        {(n.body || "").split('\n')[0] || <span className="italic">(empty)</span>}
                      </p>
                    )}
                  </div>
                </button>

                {expanded && (
                  <div className="px-3 pb-3 space-y-2">
                    <input
                      type="text"
                      value={edit.title}
                      onChange={(e) => setEdit({ ...edit, title: e.target.value })}
                      placeholder="Title"
                      className="w-full h-7 px-2 bg-black/40 border border-white/10 rounded text-[11px] text-white placeholder-zinc-600 focus:outline-none focus:border-primary/50"
                    />
                    <textarea
                      value={edit.body}
                      onChange={(e) => setEdit({ ...edit, body: e.target.value })}
                      onKeyDown={(e) => {
                        if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); saveEdit(); }
                      }}
                      placeholder="Note body — markdown, paths, snippets…"
                      rows={6}
                      className="w-full p-2 bg-black/40 border border-white/10 rounded font-sans text-[11px] text-zinc-200 placeholder-zinc-600 leading-relaxed resize-y focus:outline-none focus:border-primary/50 custom-scrollbar"
                      autoFocus
                    />
                    <div className="flex gap-1.5 flex-wrap">
                      <button
                        onClick={() => copyToClipboard(edit.body || "")}
                        className="flex-1 h-7 min-w-[64px] rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                        title="Copy current body to clipboard"
                      >
                        <ClipboardCopy size={11} /> Copy
                      </button>
                      <button
                        onClick={() => writeToTerminal(edit.body || "", false)}
                        className="flex-1 h-7 min-w-[64px] rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                        title="Paste body into active terminal (no Enter)"
                      >
                        <Play size={11} fill="currentColor" /> Paste
                      </button>
                      <button
                        onClick={saveEdit}
                        disabled={saving || !isDirty(edit)}
                        className="flex-1 h-7 min-w-[64px] rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                        title={isDirty(edit) ? "Save changes (Ctrl+Enter)" : "No unsaved changes"}
                      >
                        {saving ? <RefreshCw size={11} className="animate-spin" /> : <Check size={11} />} Save
                      </button>
                    </div>
                  </div>
                )}
              </div>
            );
          })
        ) : (
          <div className="flex flex-col items-center justify-center py-16 text-zinc-600 text-center px-4">
            <StickyNote size={32} className="opacity-20 mb-2" />
            <span className="text-xs font-bold text-zinc-500">No Notes Found</span>
            <p className="text-[10px] mt-1 text-zinc-600 max-w-[200px]">
              {searchQuery ? "No matches for your search query." : "Add notes from the Notes tab to surface them here."}
            </p>
          </div>
        )}
      </div>
    </div>
  );
};
