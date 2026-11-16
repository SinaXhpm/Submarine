import { Fragment, useState, useEffect, useRef } from "react";
import { parseCommandScope, scopeMatches, serverScopeIds } from "../util/commandScope";
import { invoke } from "@tauri-apps/api/core";
import { Play, RefreshCw, Search, X, Terminal, StickyNote, ClipboardCopy, ChevronDown, ChevronRight, Pencil, Check, Server, Lock, Tag } from "lucide-react";
import { useConfirm } from "../ui/confirm";
import { writeToTerminal } from "../util/pasteText";
import SecretTextarea from "./SecretTextarea";
import { serverRowChanged } from "../util/serverJson";
import { useT } from "../i18n";

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
// `is_secret`: masked in the list (eye toggle to reveal in the editor);
// `tags`: comma-separated list, for filtering.
interface CommandItem { id: number; title: string; content: string; scope?: string | null; is_secret?: boolean; tags?: string | null; }
interface NoteItem { id: number; title: string; body: string; }
type Tab = "commands" | "notes" | "node";

// Single shared "what's being edited right now" slot. Only one row can be
// editing at a time (notes expand-to-edit, commands toggle-to-edit), so a
// flat tagged union is simpler than two parallel sets of useState fields.
type EditTarget =
  | { kind: "command"; id: number; title: string; body: string; is_secret: boolean; tags: string }
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
  const t = useT();
  const confirm = useConfirm();
  const [tab, setTab] = useState<Tab>("commands");
  const [commands, setCommands] = useState<CommandItem[]>([]);
  const [notes, setNotes] = useState<NoteItem[]>([]);
  const [searchQuery, setSearchQuery] = useState("");
  // Visible filters for the commands list: one tag at a time, and/or
  // secrets only. Typing a tag in the search box already matches it, but
  // the chips make filtering discoverable without knowing the tag names.
  const [selectedTag, setSelectedTag] = useState<string | null>(null);
  const [secretsOnly, setSecretsOnly] = useState(false);
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
  const originalForEdit = (target: EditTarget): { title: string; body: string; is_secret?: boolean; tags?: string } | null => {
    if (target.kind === "command") {
      const c = commands.find((x) => x.id === target.id);
      return c ? { title: c.title || "", body: c.content || "", is_secret: !!c.is_secret, tags: c.tags || "" } : null;
    }
    const n = notes.find((x) => x.id === target.id);
    return n ? { title: n.title || "", body: n.body || "" } : null;
  };

  const isDirty = (target: EditTarget | null): boolean => {
    if (!target) return false;
    const o = originalForEdit(target);
    if (!o) return false;
    if (o.title !== target.title || o.body !== target.body) return true;
    if (target.kind === "command") return o.is_secret !== target.is_secret || (o.tags || "") !== target.tags;
    return false;
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
      setSaveError(t("Couldn't save the notes: {error}", { error: String(e) }));
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
    if (!isDirty(target)) return true;
    setSaveError(null);
    try {
      if (target.kind === "command") {
        await persistCommand(target);
      } else {
        await persistNote(target);
      }
      return true;
    } catch (e) {
      setEdit((cur) => cur ?? target);
      setSaveError(t("Couldn't save \"{title}\": {error}", { title: target.title || t("Untitled"), error: String(e) }));
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
      if (!saved) void confirm({ title: t("Not saved"), message: t("What you were typing in the Library couldn't be saved."), okLabel: t("OK") });
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
    setEdit({ kind: "command", id: c.id, title: c.title || "", body: c.content || "", is_secret: !!c.is_secret, tags: c.tags || "" });
  };
  const beginEditNote = (n: NoteItem) => {
    setEdit({ kind: "note", id: n.id, title: n.title || "", body: n.body || "" });
  };

  // Persist one command edit: title/content go through edit_command, and
  // the secret flag/tags only through set_command_secret_tags when they
  // actually changed — so a title edit here can't undo a flag or tags set
  // in the main Library since this panel loaded its list.
  const persistCommand = async (target: Extract<EditTarget, { kind: "command" }>): Promise<void> => {
    const o = originalForEdit(target);
    const secretChanged = !o || o.is_secret !== target.is_secret || (o.tags || "") !== target.tags;
    await invoke("edit_command", { id: target.id, title: target.title, content: target.body });
    if (secretChanged) {
      await invoke("set_command_secret_tags", { id: target.id, isSecret: target.is_secret, tags: target.tags || null });
    }
    setCommands((prev) => prev.map((c) => c.id === target.id ? { ...c, title: target.title, content: target.body, is_secret: target.is_secret, tags: target.tags } : c));
  };

  // Persist one note edit.
  const persistNote = async (target: Extract<EditTarget, { kind: "note" }>): Promise<void> => {
    await invoke("edit_note", { id: target.id, title: target.title, body: target.body });
    setNotes((prev) => prev.map((n) => n.id === target.id ? { ...n, title: target.title, body: target.body } : n));
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
        await persistCommand(edit);
      } else {
        await persistNote(edit);
      }
      setEdit(null);
      fetchAll();
    } catch (e) {
      setSaveError(t("Couldn't save: {error}", { error: String(e) }));
    } finally {
      setSaving(false);
    }
  };

  // Put `text` in the active terminal as a paste (util/pasteText).
  // `execute=true` presses Enter after it so the shell runs it; false leaves
  // the cursor at the end so the user can review / edit before pressing Enter
  // themselves. A card shows only the start of a command: `cut` says its line
  // was cut short there. `secret` hides the content in the confirm dialog
  // (revealable there) and always asks before a run — and a secret has no
  // Paste button, Run only.
  const toTerminal = (text: string, execute: boolean, cut = false, secret = false) =>
    void writeToTerminal({ terminalId: activeTab, text, enter: execute, cut, confirm, secret });

  const copyToClipboard = async (text: string) => {
    try { await navigator.clipboard.writeText(text); }
    catch (e) { console.error("Clipboard write failed:", e); }
  };

  const q = searchQuery.toLowerCase();
  // Distinct tags across all loaded commands, for the filter chips below.
  // Tags are normalised at save time (case-insensitive dedupe), so the
  // stored spellings are already unique per tag.
  const allTags = Array.from(
    new Set(commands.flatMap((c) => (c.tags || "").split(",").map((tag) => tag.trim()).filter(Boolean)))
  ).sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
  const hasSecrets = commands.some((c) => !!c.is_secret);
  // A filter whose chip is gone (its tag was renamed away, or the last
  // secret was unmarked) would leave an empty list with no way back.
  // Clear it as soon as the chip row no longer carries it.
  useEffect(() => {
    if (selectedTag && !allTags.includes(selectedTag)) setSelectedTag(null);
    if (secretsOnly && !hasSecrets) setSecretsOnly(false);
    // allTags is rebuilt every render, so this runs often; the guards
    // above only fire when a filter is actually stale.
  }, [allTags, hasSecrets, selectedTag, secretsOnly]);
  // Commands for this server first (#22), then those for every server; the
  // ones for other servers aren't shown. A session that isn't a saved server
  // (quick connect) shows those for every server.
  const matching = commands.filter((c) => {
    const matchesQuery = c.title.toLowerCase().includes(q) || c.content.toLowerCase().includes(q) || (c.tags || "").toLowerCase().includes(q);
    const matchesTag = !selectedTag || (c.tags || "").split(",").map((tag) => tag.trim().toLowerCase()).includes(selectedTag.toLowerCase());
    const matchesSecret = !secretsOnly || !!c.is_secret;
    return matchesQuery && matchesTag && matchesSecret;
  });
  const forThisServer = matching.filter((c) => {
    const s = parseCommandScope(c.scope);
    return !!s && !!scopeIds && scopeMatches(s, scopeIds);
  });
  const filteredCommands = [...forThisServer, ...matching.filter((c) => !parseCommandScope(c.scope))];
  const headingAt = (i: number) =>
    forThisServer.length === 0 ? null : i === 0 ? t("For this server") : i === forThisServer.length ? t("All servers") : null;
  const filteredNotes = notes.filter(
    (n) => (n.title || "").toLowerCase().includes(q) || (n.body || "").toLowerCase().includes(q)
  );

  const tabLabel = tab === "commands" ? t("Commands") : tab === "notes" ? t("Notes") : t("Node");
  // The command editor, for editing a card. Edits keep the stored scope
  // and start masked.
  const renderCommandEditor = (target: Extract<EditTarget, { kind: "command" }>) => (
    <div className="p-3 space-y-2">
      <input
        type="text"
        value={target.title}
        onChange={(e) => setEdit({ ...target, title: e.target.value })}
        onKeyDown={(e) => {
          if (e.key === "Escape") { e.preventDefault(); cancelEdit(); }
        }}
        placeholder={t("Title")}
        className="w-full h-7 px-2 bg-black/40 border border-white/10 rounded text-[11px] text-white placeholder-zinc-600 focus:outline-none focus:border-primary/50"
        autoFocus
      />
      {target.is_secret ? (
        <SecretTextarea
          value={target.body}
          onChange={(v) => setEdit({ ...target, body: v })}
          onKeyDown={(e) => {
            if (e.key === "Escape") { e.preventDefault(); cancelEdit(); }
            if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); saveEdit(); }
          }}
          placeholder={t("Secret command — masked, eye toggle to reveal")}
          rows={5}
          className="w-full p-2 bg-black/40 border border-white/10 rounded font-mono text-[11px] text-zinc-200 placeholder-zinc-600 leading-relaxed resize-y focus:outline-none focus:border-primary/50 custom-scrollbar"
        />
      ) : (
        <textarea
          value={target.body}
          onChange={(e) => setEdit({ ...target, body: e.target.value })}
          onKeyDown={(e) => {
            if (e.key === "Escape") { e.preventDefault(); cancelEdit(); }
            if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); saveEdit(); }
          }}
          placeholder={t("Command body — runnable in shell")}
          rows={5}
          className="w-full p-2 bg-black/40 border border-white/10 rounded font-mono text-[11px] text-zinc-200 placeholder-zinc-600 leading-relaxed resize-y focus:outline-none focus:border-primary/50 custom-scrollbar"
        />
      )}
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => setEdit({ ...target, is_secret: !target.is_secret })}
          className={`flex items-center gap-1.5 h-10 sm:h-7 px-2.5 rounded-lg border text-[10px] font-bold transition-all ${target.is_secret ? "bg-amber-500/10 text-amber-300 border-amber-500/30" : "bg-white/[0.04] text-zinc-400 border-white/10 hover:text-white"}`}
          title={t("Secret commands are masked in the list")}
        >
          <Lock size={11} /> {target.is_secret ? t("Secret") : t("Standard")}
        </button>
        <input
          type="text"
          value={target.tags}
          onChange={(e) => setEdit({ ...target, tags: e.target.value })}
          placeholder={t("Tags (comma separated)")}
          className="flex-1 min-w-0 h-10 sm:h-7 px-2 bg-black/40 border border-white/10 rounded text-[11px] text-white placeholder-zinc-600 focus:outline-none focus:border-primary/50"
        />
      </div>
      <div className="flex gap-1.5">
        <button
          onClick={cancelEdit}
          disabled={saving}
          className="flex-1 h-7 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-50"
        >
          <X size={11} /> {t("Cancel")}
        </button>
        <button
          onClick={saveEdit}
          disabled={saving || !isDirty(target)}
          className="flex-1 h-7 rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
          title={t("Save (Ctrl+Enter)")}
        >
          {saving ? <RefreshCw size={11} className="animate-spin" /> : <Check size={11} />} {t("Save")}
        </button>
      </div>
    </div>
  );

  // The note editor, for expanding a note. Collapsing auto-saves (see
  // collapseEditedNote); the save button is for an explicit save.
  const renderNoteEditor = (target: Extract<EditTarget, { kind: "note" }>) => (
    <div className="px-3 pb-3 space-y-2">
      <input
        type="text"
        value={target.title}
        onChange={(e) => setEdit({ ...target, title: e.target.value })}
        placeholder={t("Title")}
        className="w-full h-7 px-2 bg-black/40 border border-white/10 rounded text-[11px] text-white placeholder-zinc-600 focus:outline-none focus:border-primary/50"
      />
      <textarea
        value={target.body}
        onChange={(e) => setEdit({ ...target, body: e.target.value })}
        onKeyDown={(e) => {
          if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); saveEdit(); }
        }}
        placeholder={t("Note body — markdown, paths, snippets…")}
        rows={6}
        className="w-full p-2 bg-black/40 border border-white/10 rounded font-sans text-[11px] text-zinc-200 placeholder-zinc-600 leading-relaxed resize-y focus:outline-none focus:border-primary/50 custom-scrollbar"
        autoFocus
      />
      <div className="flex gap-1.5 flex-wrap">
        <button
          onClick={() => copyToClipboard(target.body || "")}
          className="flex-1 h-7 min-w-[64px] rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
          title={t("Copy current body to clipboard")}
        >
          <ClipboardCopy size={11} /> {t("Copy")}
        </button>
        <button
          onClick={() => toTerminal(target.body || "", false)}
          className="flex-1 h-7 min-w-[64px] rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
          title={t("Paste body into active terminal (no Enter)")}
        >
          <Play size={11} fill="currentColor" /> {t("Paste")}
        </button>
        <button
          onClick={saveEdit}
          disabled={saving || !isDirty(target)}
          className="flex-1 h-7 min-w-[64px] rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
          title={isDirty(target) ? t("Save changes (Ctrl+Enter)") : t("No unsaved changes")}
        >
          {saving ? <RefreshCw size={11} className="animate-spin" /> : <Check size={11} />} {t("Save")}
        </button>
      </div>
    </div>
  );

  const TabIcon = tab === "commands" ? Terminal : tab === "notes" ? StickyNote : Server;

  return (
    <div className="flex-1 flex flex-col h-full overflow-hidden bg-[#09090b]">
      {/* Title Bar */}
      <div className="h-12 px-4 shrink-0 flex items-center justify-between border-b border-white/5 bg-white/5">
        <div className="flex items-center gap-2 min-w-0">
          <TabIcon size={14} className="text-primary shrink-0" />
          <span className="text-[11px] font-semibold text-zinc-300 truncate">
            {tab === "node" && serverName ? t("Library · {tab} · {server}", { tab: tabLabel, server: serverName }) : t("Library · {tab}", { tab: tabLabel })}
          </span>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <button
            onClick={async () => { await flushDrafts(); fetchAll(); }}
            disabled={loading}
            className="p-1.5 rounded-lg text-zinc-500 hover:text-white hover:bg-white/5 transition-all disabled:opacity-50"
            title={t("Refresh list")}
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
            title={t("Close panel")}
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
          className={`h-9 flex items-center justify-center gap-1.5 text-[12px] font-semibold transition-all ${
            tab === "commands"
              ? "text-primary bg-primary/5 border-b border-primary"
              : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
          }`}
        >
          <Terminal size={12} /> {t("Commands")}
          <span className="text-[9px] opacity-60">{commands.length}</span>
        </button>
        <button
          onClick={() => setTab("notes")}
          className={`h-9 flex items-center justify-center gap-1.5 text-[12px] font-semibold transition-all ${
            tab === "notes"
              ? "text-primary bg-primary/5 border-b border-primary"
              : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
          }`}
        >
          <StickyNote size={12} /> {t("Notes")}
          <span className="text-[9px] opacity-60">{notes.length}</span>
        </button>
        {hasNode && (
          <button
            onClick={() => setTab("node")}
            className={`h-9 flex items-center justify-center gap-1.5 text-[12px] font-semibold transition-all ${
              tab === "node"
                ? "text-primary bg-primary/5 border-b border-primary"
                : "text-zinc-500 hover:text-zinc-200 hover:bg-white/[0.03] border-b border-transparent"
            }`}
          >
            <Server size={12} /> {t("Node")}
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
              placeholder={tab === "commands" ? t("Search commands (title, body, tags)...") : t("Search notes (title + body)...")}
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              className="w-full h-8 pl-9 pr-3 bg-white/5 border border-white/5 rounded-lg text-xs text-white placeholder-zinc-500 focus:outline-none focus:border-primary/50 focus:bg-white/10 transition-all"
            />
          </div>
          {/* Filter chips — commands tab only. "All" clears both filters;
              "Secrets" shows only secret commands; a tag chip shows only
              commands carrying that tag. The tag and secrets filters
              combine (a tagged secret shows under both). */}
          {tab === "commands" && (allTags.length > 0 || hasSecrets) && (
            <div className="flex gap-1.5 mt-2 overflow-x-auto no-scrollbar">
              <button
                type="button"
                onClick={() => { setSelectedTag(null); setSecretsOnly(false); }}
                className={`shrink-0 h-6 px-2.5 rounded-full border text-[10px] font-bold transition-all ${!selectedTag && !secretsOnly ? "bg-primary/15 text-primary border-primary/40" : "bg-white/5 border-white/10 text-zinc-400 hover:text-white"}`}
              >
                {t("All")}
              </button>
              {hasSecrets && (
                <button
                  type="button"
                  onClick={() => setSecretsOnly((s) => !s)}
                  className={`shrink-0 h-6 px-2.5 rounded-full border text-[10px] font-bold transition-all flex items-center gap-1 ${secretsOnly ? "bg-amber-500/15 text-amber-300 border-amber-500/40" : "bg-white/5 border-white/10 text-zinc-400 hover:text-white"}`}
                >
                  <Lock size={10} /> {t("Secrets")}
                </button>
              )}
              {allTags.map((tag) => (
                <button
                  key={tag}
                  type="button"
                  onClick={() => setSelectedTag((prev) => (prev === tag ? null : tag))}
                  className={`shrink-0 h-6 px-2.5 rounded-full border text-[10px] font-bold transition-all flex items-center gap-1 ${selectedTag === tag ? "bg-primary/15 text-primary border-primary/40" : "bg-white/5 border-white/10 text-zinc-400 hover:text-white"}`}
                >
                  <Tag size={10} /> {tag}
                </button>
              ))}
            </div>
          )}
        </div>
      )}

      {/* List */}
      <div className="flex-1 overflow-y-auto p-3 space-y-2 no-scrollbar">
        {loading ? (
          <div className="flex flex-col items-center justify-center py-12 text-zinc-500 gap-2">
            <RefreshCw size={24} className="animate-spin text-primary opacity-60" />
            <span className="text-[11px]">{tab === "commands" ? t("Loading commands...") : tab === "notes" ? t("Loading notes...") : t("Loading node...")}</span>
          </div>
        ) : tab === "node" ? (
          <div className="flex flex-col h-full gap-2">
            <div className="flex items-center justify-between gap-2 shrink-0">
              <div className="flex items-center gap-2 min-w-0">
                <Server size={12} className="text-primary shrink-0" />
                <span className="text-[11px] font-bold text-zinc-200 truncate">{serverName || t("Node #{id}", { id: String(serverId) })}</span>
              </div>
              <button
                onClick={() => copyToClipboard(nodeDraft || "")}
                disabled={!nodeDraft}
                className="h-7 px-2 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                title={t("Copy current draft to clipboard")}
              >
                <ClipboardCopy size={11} /> {t("Copy")}
              </button>
            </div>
            <textarea
              value={nodeDraft}
              onChange={(e) => setNodeDraft(e.target.value)}
              onKeyDown={(e) => {
                if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); void saveNodeNotes(); }
              }}
              placeholder={t("Notes for this node — paths, credentials hints, runbook steps…\nSaved on tab switch or Ctrl+Enter.")}
              className="flex-1 w-full p-3 bg-black/40 border border-white/10 rounded-lg font-sans text-[12px] text-zinc-200 placeholder-zinc-600 leading-relaxed resize-none focus:outline-none focus:border-primary/50 custom-scrollbar"
            />
            <div className="flex gap-1.5 shrink-0">
              <button
                onClick={() => setNodeDraft(nodeOriginal)}
                disabled={nodeSaving || !isNodeDirty}
                className="flex-1 h-8 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                title={t("Revert to last saved version")}
              >
                <X size={11} /> {t("Revert")}
              </button>
              <button
                onClick={() => void saveNodeNotes()}
                disabled={nodeSaving || !isNodeDirty}
                className="flex-1 h-8 rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
                title={t("Save (Ctrl+Enter, auto-saves on tab switch)")}
              >
                {nodeSaving ? <RefreshCw size={11} className="animate-spin" /> : <Check size={11} />} {t("Save")}
              </button>
            </div>
          </div>
        ) : tab === "commands" ? (
          <>
            {filteredCommands.length > 0 ? (
            filteredCommands.map((c, i) => {
              // Narrowed to the command being edited (or null): the editor
              // reads is_secret/tags, which only the command variant has.
              const cmdEdit = edit?.kind === "command" && edit.id === c.id ? edit : null;
              const heading = headingAt(i);
              return (
                <Fragment key={c.id}>
                {heading && (
                  <div className="px-1 pt-1 text-[11px] font-semibold text-zinc-500" data-command-group>
                    {heading}
                  </div>
                )}
                <div
                  className="group relative rounded-lg border border-white/5 bg-white/[0.02] hover:bg-white/[0.04] hover:border-white/10 transition-all"
                >
                  {cmdEdit ? renderCommandEditor(cmdEdit) : (
                    <div className="p-3" data-command-card>
                      <div className="flex items-start justify-between gap-3">
                        <div className="flex-1 min-w-0">
                          <h4 className="text-xs font-bold text-zinc-200 truncate group-hover:text-primary transition-colors flex items-center gap-1.5">
                            {c.is_secret && <Lock size={11} className="text-amber-400 shrink-0" />}
                            <span className="truncate">{c.title}</span>
                          </h4>
                          <pre
                            className="mt-1.5 p-1.5 px-2 bg-black/40 rounded border border-white/5 font-mono text-[10px] text-zinc-400 truncate whitespace-nowrap overflow-hidden"
                            title={c.is_secret ? t("Secret command — content hidden") : c.content}
                          >
                            {c.is_secret ? "••••••" : <>{c.content.split('\n')[0] || ""}{c.content.split('\n').length > 1 ? " ..." : ""}</>}
                          </pre>
                          {c.tags && (
                            <div className="mt-1.5 flex flex-wrap gap-1">
                              {c.tags.split(",").map((tag) => tag.trim()).filter(Boolean).map((tag) => (
                                <span key={tag} className="inline-flex items-center gap-0.5 h-4 px-1.5 rounded-full bg-white/5 border border-white/10 text-[9px] font-semibold text-zinc-400">
                                  <Tag size={8} /> {tag}
                                </span>
                              ))}
                            </div>
                          )}
                        </div>
                      </div>
                      <div className="flex gap-1.5 mt-2">
                        <button
                          onClick={(e) => toTerminal(c.content, true, cutOnCard(e.currentTarget), !!c.is_secret)}
                          className="flex-1 h-7 rounded-lg bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                          title={t("Run in active terminal (writes + Enter)")}
                        >
                          <Play size={11} fill="currentColor" /> {t("Run")}
                        </button>
                        {!c.is_secret && (
                          <button
                            onClick={() => toTerminal(c.content, false)}
                            className="flex-1 h-7 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                            title={t("Paste into active terminal (no Enter)")}
                          >
                            <ClipboardCopy size={11} /> {t("Paste")}
                          </button>
                        )}
                        <button
                          onClick={() => beginEditCommand(c)}
                          className="flex-1 h-7 rounded-lg bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center justify-center gap-1.5 text-[10px] font-bold transition-all"
                          title={t("Edit command inline")}
                        >
                          <Pencil size={11} /> {t("Edit")}
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
              <span className="text-xs font-bold text-zinc-500">{t("No commands found")}</span>
              <p className="text-[10px] mt-1 text-zinc-600 max-w-[200px]">
                {searchQuery || selectedTag || secretsOnly ? t("No matches for your search or filters.") : t("Save commands in the main app to surface them here.")}
              </p>
            </div>
            )}
          </>
        ) : (
          <>
            {filteredNotes.length > 0 ? (
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
                      {(expanded ? edit.title : n.title) || t("Untitled")}
                    </h4>
                    {!expanded && (
                      <p className="mt-1 text-[10px] text-zinc-500 truncate">
                        {(n.body || "").split('\n')[0] || <span className="italic">{t("(empty)")}</span>}
                      </p>
                    )}
                  </div>
                </button>

                {expanded && renderNoteEditor(edit)}
              </div>
            );
          })
        ) : (
          <div className="flex flex-col items-center justify-center py-16 text-zinc-600 text-center px-4">
            <StickyNote size={32} className="opacity-20 mb-2" />
            <span className="text-xs font-bold text-zinc-500">{t("No notes found")}</span>
            <p className="text-[10px] mt-1 text-zinc-600 max-w-[200px]">
              {searchQuery ? t("No matches for your search query.") : t("Add notes from the Notes tab to surface them here.")}
            </p>
          </div>
        )}
          </>
        )}
      </div>
    </div>
  );
};
