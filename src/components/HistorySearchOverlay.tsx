import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { Search, X, History } from "lucide-react";

// ─────────────────────────────────────────────────────────────────────────────
// Command History Search — Ctrl+R (Cmd+R on macOS) overlay
//
// Portaled modal that queries the encrypted profile vault's `cmd_history`
// table via the tauri commands defined in lib.rs. Fuzzy-ish (LIKE-based)
// server-side filter plus client-side highlight; keyboard-driven picker.
//
//   Enter        → insert command into the terminal, no newline
//   Shift+Enter  → insert + newline (execute)
//   Esc          → close
//   ↑ / ↓        → move selection
//
// Insert is done by the parent (TerminalView) via the `onInsert` callback
// so we don't have to know about xterm / write_terminal_data plumbing here.
// ─────────────────────────────────────────────────────────────────────────────

export interface CmdHistoryEntry {
  id: number;
  server_id: number | null;
  server_name: string | null;
  command: string;
  ts: number;
  exit_code: number | null;
}

interface Props {
  isOpen: boolean;
  onClose: () => void;
  /** Restrict history list to just this server's rows. When null, all rows. */
  filterServerId?: number | null;
  /** Fires when the user picks a row. `execute=true` means the caller should
   *  append a newline (Shift+Enter path). */
  onInsert: (command: string, execute: boolean) => void;
}

const relTime = (ts: number): string => {
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
};

const HistorySearchOverlay = ({ isOpen, onClose, filterServerId, onInsert }: Props) => {
  const [query, setQuery] = useState("");
  const [entries, setEntries] = useState<CmdHistoryEntry[]>([]);
  const [selectedIdx, setSelectedIdx] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [restrictToServer, setRestrictToServer] = useState<boolean>(!!filterServerId);
  const listRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);

  // Reset state on every open — leftover selection / error from a previous
  // session would confuse the user (row 3 highlighted for a totally different
  // list of results).
  useEffect(() => {
    if (!isOpen) return;
    setQuery("");
    setSelectedIdx(0);
    setError(null);
    setRestrictToServer(!!filterServerId);
    // Focus the input a tick after mount so the browser has finished the
    // portal render — otherwise focus() lands on an unmounted element.
    requestAnimationFrame(() => {
      inputRef.current?.focus();
    });
  }, [isOpen, filterServerId]);

  // Query the backend whenever the search text or the "this server only"
  // toggle changes. We debounce with a tiny setTimeout so fast typing
  // doesn't fire an IPC round-trip per keystroke.
  useEffect(() => {
    if (!isOpen) return;
    const handle = setTimeout(() => {
      invoke<CmdHistoryEntry[]>("cmd_history_list", {
        query: query || null,
        limit: 200,
      }).then((rows) => {
        const filtered = restrictToServer && filterServerId != null
          ? rows.filter(r => r.server_id === filterServerId)
          : rows;
        setEntries(filtered);
        setSelectedIdx(0);
        setError(null);
      }).catch((e) => {
        setError(String(e));
        setEntries([]);
      });
    }, 60);
    return () => clearTimeout(handle);
  }, [query, isOpen, restrictToServer, filterServerId]);

  const visibleEntries = entries;

  const commit = useCallback((execute: boolean) => {
    const row = visibleEntries[selectedIdx];
    if (!row) return;
    onInsert(row.command, execute);
    onClose();
  }, [visibleEntries, selectedIdx, onInsert, onClose]);

  // Keep the selected row scrolled into view so ↓ past the visible area
  // doesn't hide the highlight off-screen.
  useEffect(() => {
    if (!listRef.current) return;
    const el = listRef.current.querySelector<HTMLDivElement>(`[data-idx="${selectedIdx}"]`);
    el?.scrollIntoView({ block: "nearest" });
  }, [selectedIdx, visibleEntries.length]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelectedIdx(i => Math.min(visibleEntries.length - 1, i + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelectedIdx(i => Math.max(0, i - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      commit(e.shiftKey);
    }
    e.stopPropagation();
  };

  const emptyMsg = useMemo(() => {
    if (error) return `Error: ${error}`;
    if (query) return `No commands match "${query}".`;
    return "Nothing here yet. Commands you run in a terminal will appear in this list.";
  }, [error, query]);

  if (!isOpen) return null;

  return createPortal(
    <div
      className="fixed inset-0 z-[9998] bg-black/60 backdrop-blur-sm flex items-start justify-center pt-[10vh] p-4"
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="cmd-history-title"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
        className="w-full max-w-[720px] bg-[#121214] border border-white/10 rounded-xl shadow-2xl font-mono text-[12px] animate-in zoom-in-95 fade-in duration-150 overflow-hidden flex flex-col max-h-[70vh]"
      >
        <div className="flex items-center gap-2 px-3 py-2 border-b border-white/10">
          <History size={14} className="text-primary" />
          <span id="cmd-history-title" className="text-[11px] font-black uppercase tracking-widest text-zinc-200">
            Command History
          </span>
          <div className="flex-1" />
          {filterServerId != null && (
            <label className="flex items-center gap-1.5 text-[10px] font-bold uppercase tracking-wider text-zinc-400 cursor-pointer select-none">
              <input
                type="checkbox"
                className="accent-primary"
                checked={restrictToServer}
                onChange={(e) => setRestrictToServer(e.target.checked)}
              />
              This server only
            </label>
          )}
          <button
            onClick={onClose}
            className="text-zinc-500 hover:text-white shrink-0 p-0.5"
            aria-label="Close"
          >
            <X size={14} />
          </button>
        </div>
        <div className="px-3 py-2 border-b border-white/10 flex items-center gap-2">
          <Search size={13} className="text-zinc-500 shrink-0" />
          <input
            ref={inputRef}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search command history…"
            className="flex-1 bg-transparent outline-none text-[13px] text-zinc-100 placeholder:text-zinc-600"
          />
          <span className="text-[10px] text-zinc-500 font-bold uppercase tracking-wider">
            {visibleEntries.length}
          </span>
        </div>
        <div ref={listRef} className="flex-1 overflow-y-auto custom-scrollbar">
          {visibleEntries.length === 0 ? (
            <div className="p-6 text-center text-zinc-500 text-[12px] italic">
              {emptyMsg}
            </div>
          ) : (
            visibleEntries.map((row, i) => (
              <div
                key={row.id}
                data-idx={i}
                onMouseEnter={() => setSelectedIdx(i)}
                onClick={() => {
                  setSelectedIdx(i);
                  onInsert(row.command, false);
                  onClose();
                }}
                onDoubleClick={() => {
                  onInsert(row.command, true);
                  onClose();
                }}
                className={`px-3 py-2 border-b border-white/[0.03] cursor-pointer ${
                  i === selectedIdx ? "bg-primary/10" : "hover:bg-white/[0.04]"
                }`}
              >
                <div className={`text-[13px] whitespace-pre-wrap break-all leading-snug ${
                  i === selectedIdx ? "text-primary" : "text-zinc-200"
                }`}>
                  {row.command}
                </div>
                <div className="mt-1 flex items-center gap-2 text-[10px] text-zinc-500 font-mono">
                  <span title={new Date(row.ts).toISOString()}>{relTime(row.ts)} ago</span>
                  {row.server_name && (
                    <>
                      <span className="text-zinc-700">·</span>
                      <span className="truncate">{row.server_name}</span>
                    </>
                  )}
                  {row.exit_code != null && (
                    <>
                      <span className="text-zinc-700">·</span>
                      <span className={row.exit_code === 0 ? "text-emerald-400" : "text-rose-400"}>
                        exit {row.exit_code}
                      </span>
                    </>
                  )}
                </div>
              </div>
            ))
          )}
        </div>
        <div className="px-3 py-1.5 border-t border-white/10 flex items-center gap-3 text-[10px] text-zinc-500 uppercase tracking-wider">
          <span><kbd className="text-zinc-400">Enter</kbd> insert</span>
          <span><kbd className="text-zinc-400">Shift+Enter</kbd> insert + run</span>
          <span><kbd className="text-zinc-400">Esc</kbd> close</span>
        </div>
      </div>
    </div>,
    document.body
  );
};

export default HistorySearchOverlay;
