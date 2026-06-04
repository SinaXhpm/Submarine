import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import {
  Plus, X, RefreshCw, Terminal, Key, Trash2,
  ArrowLeftRight, Shield, User, Cpu, TerminalSquare, List, Edit2,
  StickyNote, Search
} from "lucide-react";

import ProfileSelectPage from "./components/ProfileSelectPage";
import logoUrl from "./assets/logo.png";
import PasswordField from "./components/PasswordField";
import QuickConnectModal, { QuickAuth } from "./components/QuickConnectModal";
import { useConfirm } from "./ui/confirm";
import { useIsNarrow } from "./hooks/useViewport";
import { Sidebar } from "./components/Sidebar";
import { NodeGrid } from "./components/NodeGrid";
import AddNodePanel from "./components/AddNodePanel";
import TerminalView from "./components/TerminalView";
import SettingsPanel from "./components/SettingsPanel";
import { SessionView } from "./components/SessionView";
import MonitoringPanel from "./components/MonitoringPanel";
import { ErrorBoundary } from "./ui/ErrorBoundary";

const appWindow = getCurrentWindow();

// Sessions can be either DB-backed (saved node, `serverId > 0`) or quick
// connect (one-shot, `serverId === 0` and `quickAuth` populated). SessionView
// forwards `quickAuth` to `initiate_connection` which uses it instead of
// looking up the DB row.
type Session = { id: string; serverId: number; serverName: string; quickAuth?: QuickAuth | null };

const hexToRgb = (hex: string) => {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return `${r} ${g} ${b}`;
};

function DesktopApp() {
  const [loading, setLoading] = useState(true);
  const [isUnlocked, setIsUnlocked] = useState(false);
  // `activeProfile` is the name the user picked + unlocked. Stays null
  // until ProfileSelectPage's onUnlocked fires, at which point the app
  // flips straight into the main view — no intermediate state.
  const [activeProfile, setActiveProfile] = useState<string | null>(null);
  const [activeView, setActiveView] = useState<string>("nodes");
  const [sessions, setSessions] = useState<Session[]>([]);
  // Tracks the live status of each open session ('connecting' | 'connected'
  // | 'failed' | 'disconnected'). Updated via the callback every SessionView
  // fires on its own status change — single source of truth for the dot
  // colour on each tab. Cleared when a session is closed.
  const [sessionStatuses, setSessionStatuses] = useState<Record<string, string>>({});
  // Right-click menu pinned to a session tab. Closed by any click outside or
  // by choosing one of the menu items.
  const [tabMenu, setTabMenu] = useState<{ x: number; y: number; sessionId: string } | null>(null);
  const handleSessionStatus = useCallback((sessionId: string, status: string) => {
    setSessionStatuses((prev) => (prev[sessionId] === status ? prev : { ...prev, [sessionId]: status }));
  }, []);
  const [isPanelOpen, setIsPanelOpen] = useState(false);
  const [isQuickConnectOpen, setIsQuickConnectOpen] = useState(false);
  const [servers, setServers] = useState<any[]>([]);
  const [credentials, setCredentials] = useState<any[]>([]);
  const [sshKeys, setSshKeys] = useState<any[]>([]);
  const [folders, setFolders] = useState<any[]>([]);
  const [commands, setCommands] = useState<any[]>([]);
  const [logs, setLogs] = useState<{ msg: string, type: string, time: string }[]>([]);
  const [isCommandPanelOpen, setIsCommandPanelOpen] = useState(false);
  const [editCommandData, setEditCommandData] = useState<{ id: number | null, title: string, content: string }>({ id: null, title: "", content: "" });

  const [notes, setNotes] = useState<any[]>([]);
  const [isNotePanelOpen, setIsNotePanelOpen] = useState(false);
  const [editNoteData, setEditNoteData] = useState<{ id: number | null, title: string, body: string }>({ id: null, title: "", body: "" });
  const [noteQuery, setNoteQuery] = useState("");

  const [isCredPanelOpen, setIsCredPanelOpen] = useState(false);
  const [editCredData, setEditCredData] = useState<any>({ id: null, name: "", auth_type: "password", username: "", password: "", key_id: null });
  
  const [isKeyPanelOpen, setIsKeyPanelOpen] = useState(false);
  const [editKeyData, setEditKeyData] = useState<any>({ id: null, name: "", public_key: "", private_key: "", passphrase: "" });
  const [formError, setFormError] = useState("");

  const defaultNode = {
    id: null as number | null,
    name: "", host: "", port: 22, username: "", password: "",
    // "custom_pass" picked as default because most users adding a new
    // server have a password in hand; the vault option is mainly useful
    // *after* they've saved a few credentials and want to reuse one.
    authType: "custom_pass", credentialId: "", folderId: "", keyId: "",
    proxyType: "none", proxyHost: "", proxyPort: 1080,
    tunnels: [] as { local: string, remote: string, type: string }[],
    autostart: false,
    mirrors: [] as { local: string, remote: string, soft_delete: boolean, excludes: string[], conflict_resolution: string }[],
    color: null as string | null,
  };

  // Live width-based "narrow viewport" flag. Replaces a one-shot UA check
  // that couldn't see a desktop window being shrunk by the user — the new
  // hook updates on every resize so layout swaps follow the actual width.
  const isMobile = useIsNarrow();

  const [newNode, setNewNode] = useState(defaultNode);
  const [appSettings, setAppSettings] = useState({
    primaryColor: localStorage.getItem('submarine-primary-color') || '#60a5fa',
    backgroundColor: localStorage.getItem('submarine-bg-color') || '#0a0a0c',
    terminalFontSize: parseInt(localStorage.getItem('submarine-terminal-font-size') || '14')
  });

  useEffect(() => {
    const rgb = hexToRgb(appSettings.primaryColor);
    document.documentElement.style.setProperty('--primary', rgb);
    document.documentElement.style.setProperty('--primary-hex', appSettings.primaryColor);
    document.documentElement.style.setProperty('--background', appSettings.backgroundColor);
    localStorage.setItem('submarine-primary-color', appSettings.primaryColor);
    localStorage.setItem('submarine-bg-color', appSettings.backgroundColor);
    localStorage.setItem('submarine-terminal-font-size', appSettings.terminalFontSize.toString());
    // Tell already-mounted terminals to re-fit with the new font size.
    // Without this dispatch the listener in TerminalView is dead code and
    // users have to close+reopen every terminal to see a size change.
    window.dispatchEvent(new CustomEvent('submarine-settings-changed'));
  }, [appSettings]);

  useEffect(() => {
    const init = async () => {
      try {
        // Restore window size
        const savedW = localStorage.getItem('submarine-window-width');
        const savedH = localStorage.getItem('submarine-window-height');
        if (savedW && savedH) {
          try {
            await appWindow.setSize(new LogicalSize(parseInt(savedW), parseInt(savedH)));
          } catch(e) { console.error("Window resize failed", e); }
        }

        // Listen for resize (only for persistence, not for mobile detection)
        await appWindow.onResized(async () => {
          const size = await appWindow.innerSize();
          const logical = size.toLogical(await appWindow.scaleFactor());
          localStorage.setItem('submarine-window-width', logical.width.toString());
          localStorage.setItem('submarine-window-height', logical.height.toString());
        });

        // `isMobile` is now driven by `useIsNarrow()` (live viewport hook)
        // — no one-shot UA detection here anymore.

        // Profile picker comes first now — we no longer probe a single
        // global vault file. `dbExists` is set inside `handleProfileSelected`
        // when the user picks a profile.
      } catch (e) {
        console.error("Initialization error", e);
        addLog(`INIT_EXCEPTION: ${e}`, "error"); 
      } finally { 
        setLoading(false); 
      }
    };
    init();
  }, []);

  const addLog = (msg: string, type = "info") => {
    // Each entry gets a stable monotonically-increasing id so React can
    // reconcile rows correctly when the list is rendered reversed. Using
    // `key={index}` on a reverse-then-map list (the previous approach)
    // re-keys every row on every push and busts list reconciliation.
    const id = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    setLogs(prev => [...prev.slice(-99), { id, msg, type, time: new Date().toLocaleTimeString() }]);
  };

  const refreshServers = async () => {
    try { setServers((await invoke("get_servers")) as any[]); }
    catch (e) { addLog(`SYNC_SERVERS: ${e}`, "error"); }
  };
  const refreshCredentials = async () => {
    try { setCredentials((await invoke("get_credentials")) as any[]); }
    catch (e) { addLog(`SYNC_CREDENTIALS: ${e}`, "error"); }
  };
  const refreshSshKeys = async () => {
    try { setSshKeys((await invoke("get_ssh_keys")) as any[]); }
    catch (e) { addLog(`SYNC_SSH_KEYS: ${e}`, "error"); }
  };
  const refreshFolders = async () => {
    try { setFolders((await invoke("get_folders")) as any[]); }
    catch (e) { addLog(`SYNC_FOLDERS: ${e}`, "error"); }
  };
  const refreshCommands = async () => {
    try { setCommands((await invoke("get_commands")) as any[]); }
    catch (e) { addLog(`SYNC_COMMANDS: ${e}`, "error"); }
  };
  const refreshNotes = async () => {
    try { setNotes((await invoke("get_notes")) as any[]); }
    catch (e) { addLog(`SYNC_NOTES: ${e}`, "error"); }
  };
  const refreshAll = async () => {
    await Promise.all([
      refreshServers(),
      refreshCredentials(),
      refreshSshKeys(),
      refreshFolders(),
      refreshCommands(),
      refreshNotes(),
    ]);
  };

  const removeServer = async (id: number) => {
    try {
      await invoke("delete_server", { id });
      refreshServers();
      addLog("Server removed.", "info");
    } catch (e) { addLog(`DELETE_EXCEPTION: ${e}`, "error"); }
  };

  const removeFolder = async (id: number) => {
    try {
      await invoke("delete_folder", { id });
      // delete_folder cascades to its servers (see main.rs), so refresh both.
      await Promise.all([refreshFolders(), refreshServers()]);
      addLog("Folder removed.", "info");
    } catch (e) { addLog(`DELETE_EXCEPTION: ${e}`, "error"); }
  };

  const renameFolder = async (id: number, name: string) => {
    try {
      await invoke("rename_folder", { id, name });
      await refreshFolders();
      addLog(`Folder renamed to "${name}".`, "info");
    } catch (e) {
      addLog(`RENAME_EXCEPTION: ${e}`, "error");
      throw e; // bubble up so NodeGrid can keep the input open if the user wants to retry
    }
  };

  // ProfileSelectPage handles the entire profile pick + password + create
  // flow on a single screen. By the time it fires `onUnlocked`, the
  // backend has both selected the profile AND decrypted the DB — we just
  // flip the UI and refresh data.
  const handleProfileUnlocked = async (name: string) => {
    setActiveProfile(name);
    setIsUnlocked(true);
    addLog(`Profile "${name}" unlocked.`, "success");
    refreshAll();
    // Autostart sweep: load servers directly (refreshAll is also doing this
    // in parallel, but its state update is async and we can't read `servers`
    // back here without a stale-closure race), pick the ones flagged
    // autostart, and stage them all into the sessions tab strip in one
    // setSessions call. The user lands focused on the first autostart node;
    // each new SessionView component then kicks off its own connect on mount.
    try {
      const list = await invoke<any[]>("get_servers");
      const toStart = list.filter((s) => s.autostart);
      if (toStart.length === 0) return;
      const newSessions = toStart.map((s) => ({
        id: `session-${s.id}`,
        serverId: s.id,
        serverName: s.name,
        mirrors: s.mirrors,
      }));
      setSessions((prev: any[]) => {
        const seen = new Set(prev.map((p) => p.id));
        const fresh = newSessions.filter((n) => !seen.has(n.id));
        return [...prev, ...fresh];
      });
      setActiveView(newSessions[0].id);
      addLog(`Autostart: opened ${newSessions.length} node${newSessions.length === 1 ? "" : "s"}.`, "info");
    } catch (e) {
      addLog(`AUTOSTART_LOAD_FAILED: ${e}`, "error");
    }
  };

  const confirm = useConfirm();

  // Lock the current profile and return to the picker. Confirms first when
  // there are open SSH sessions because they will be torn down — accidental
  // double-clicks on the lock icon shouldn't kill the user's terminal work.
  const handleLogout = async () => {
    if (sessions.length > 0) {
      const ok = await confirm({
        title: "Switch profile?",
        message: `You have ${sessions.length} open SSH session${sessions.length === 1 ? "" : "s"}. Switching will disconnect ${sessions.length === 1 ? "it" : "them all"} and take you back to the profile picker.`,
        okLabel: "Switch",
        cancelLabel: "Stay",
        destructive: true,
      });
      if (!ok) return;
      // Best-effort: tell the backend to clean up each session. We don't
      // bail if one fails — `close_profile` will drop the DB anyway, and
      // the OS will eventually reap the sockets.
      for (const s of sessions) {
        try { await invoke("disconnect_session", { sessionId: s.id }); }
        catch (e) { console.error("disconnect on logout failed:", e); }
      }
    }
    try { await invoke("close_profile"); }
    catch (e) { addLog(`LOGOUT_EXCEPTION: ${e}`, "error"); }

    // Reset all client state so the picker starts fresh — no stale
    // servers/credentials/sessions leaking across profile contexts.
    setIsUnlocked(false);
    setActiveProfile(null);
    setSessions([]);
    setActiveView("nodes");
    setServers([]); setCredentials([]); setSshKeys([]); setFolders([]); setCommands([]);
    addLog("Profile locked.", "info");
  };

  const openServer = (server: any) => {
    const sessionId = `session-${server.id}`;
    const existing = sessions.find(s => s.id === sessionId);
    if (!existing) {
      setSessions([...sessions, {
        id: sessionId,
        serverId: server.id,
        serverName: server.name,
        // Pass the raw mirrors JSON through to SessionView so the
        // MirrorsPanel can pre-populate "Saved on this node" without
        // another round-trip to the backend.
        mirrors: server.mirrors,
      }]);
    }

    setActiveView(sessionId);
  };

  // Spawn a one-shot session from inline auth — no DB row created. The
  // session id is timestamped so multiple quick connects to the same host
  // don't collide as separate tabs. `serverId = 0` is our sentinel for
  // "look at quickAuth, not the DB" on the backend side.
  const openQuickConnect = (auth: QuickAuth) => {
    const sessionId = `session-quick-${Date.now()}`;
    const displayName = `${auth.username}@${auth.host}:${auth.port}`;
    setSessions((prev: Session[]) => [...prev, {
      id: sessionId,
      serverId: 0,
      serverName: displayName,
      quickAuth: auth,
    }]);
    setActiveView(sessionId);
    setIsQuickConnectOpen(false);
    addLog(`Quick connect → ${displayName}`, "info");
  };

  const handleEditNode = (server: any) => {
    setNewNode({
      id: server.id,
      name: server.name || "",
      host: server.host || "",
      port: server.port || 22,
      username: server.username || "",
      password: server.password || "",
      authType: server.auth_type || (server.credential_id ? "vault" : "custom_pass"),
      credentialId: server.credential_id?.toString() || "",
      folderId: server.folder_id?.toString() || "",
      keyId: server.key_id?.toString() || "",
      proxyType: server.proxy_type || "none",
      proxyHost: server.proxy_host || "",
      proxyPort: server.proxy_port || 1080,
      tunnels: server.tunnels ? JSON.parse(server.tunnels) : [],
      autostart: !!server.autostart,
      mirrors: (() => {
        try { return JSON.parse(server.mirrors || "[]"); } catch { return []; }
      })(),
      color: server.color ?? null,
    });
    setIsPanelOpen(true);
  };

  const TitleBar = () => (
    <div data-tauri-drag-region className="h-10 bg-[#0d0d10] border-b border-white/5 flex items-center justify-between px-3 select-none shrink-0 z-50 drag absolute top-0 left-0 right-0">
      <div className="flex items-center gap-2 pr-4 pl-[75px] md:pl-2" data-tauri-drag-region>
        <img src={logoUrl} alt="" draggable={false} className="w-6 h-6 select-none" />
        <span className="text-[12px] font-bold text-white tracking-tight">Submarine</span>
      </div>
      
      {/* Tab strip is a drag region; tabs themselves opt out via no-drag so
          clicking a tab selects it instead of dragging the window. */}
      <div
        data-tauri-drag-region
        className="flex-1 flex gap-1 overflow-x-auto no-scrollbar h-full items-end pb-1"
        onWheel={(e) => { e.currentTarget.scrollLeft += e.deltaY; }}
      >
        {sessions.map(s => {
          const st = sessionStatuses[s.id] ?? "connecting";
          // Dot palette: green = connected, amber = connecting, red = failed
          // or disconnected. The pulse animation only runs while connecting
          // so a steady-state tab doesn't draw the eye every half second.
          const dotTone =
            st === "connected"    ? "bg-emerald-400" :
            st === "connecting"   ? "bg-amber-400 animate-pulse" :
            st === "failed"       ? "bg-rose-500" :
            st === "disconnected" ? "bg-rose-500" :
                                    "bg-zinc-500";
          const dotTitle =
            st === "connected"    ? "Connected" :
            st === "connecting"   ? "Connecting…" :
            st === "failed"       ? "Connection failed" :
            st === "disconnected" ? "Disconnected" :
                                    st;
          return (
            <div
              key={s.id}
              onClick={() => setActiveView(s.id)}
              onContextMenu={(e) => {
                e.preventDefault();
                setTabMenu({ x: e.clientX, y: e.clientY, sessionId: s.id });
              }}
              className={`group no-drag flex items-center h-7 px-4 rounded-full cursor-pointer transition-all min-w-[100px] max-w-[180px] mr-1 ${activeView === s.id ? 'bg-primary/15 text-primary border border-primary/40 shadow-inner shadow-primary/10' : 'bg-white/[0.06] text-zinc-300 border border-white/10 hover:bg-white/[0.1] hover:border-white/20 hover:text-white'}`}
            >
              <span
                title={dotTitle}
                className={`w-2 h-2 rounded-full mr-2 shrink-0 ${dotTone}`}
                aria-label={dotTitle}
              />
              <span className="text-[10px] font-bold truncate flex-1 uppercase tracking-tight">{s.serverName}</span>
              <X
                size={10}
                className="ml-2 opacity-0 group-hover:opacity-100 hover:text-red-500"
                onClick={(e) => {
                  e.stopPropagation();
                  setSessions(prev => prev.filter(sess => sess.id !== s.id));
                  setSessionStatuses(prev => {
                    const { [s.id]: _, ...rest } = prev;
                    return rest;
                  });
                  setActiveView(prev => (prev === s.id ? "nodes" : prev));
                }}
              />
            </div>
          );
        })}
      </div>
      <div className="flex items-center h-full gap-1 no-drag">
        <button onClick={() => appWindow.minimize()} className="w-10 h-full flex items-center justify-center hover:bg-white/5 transition-colors"><div className="w-3.5 h-[1.5px] bg-zinc-600" /></button>
        <button onClick={() => appWindow.close()} className="w-10 h-full flex items-center justify-center hover:bg-red-500 group transition-all"><X size={14} className="text-zinc-600 group-hover:text-white" /></button>
      </div>
      {tabMenu && (
        <>
          <div className="fixed inset-0 z-[60]" onClick={() => setTabMenu(null)} onContextMenu={(e) => { e.preventDefault(); setTabMenu(null); }} />
          <div
            className="fixed z-[70] bg-[#15151a] border border-white/10 rounded-md shadow-2xl py-1 min-w-[160px] text-[11px] no-drag"
            style={{ left: Math.min(tabMenu.x, window.innerWidth - 180), top: Math.min(tabMenu.y, window.innerHeight - 110) }}
          >
            <button
              className="w-full text-left px-3 py-1.5 hover:bg-primary/15 hover:text-primary text-zinc-200 flex items-center gap-2"
              onClick={() => {
                window.dispatchEvent(new CustomEvent(`session-reconnect-${tabMenu.sessionId}`));
                setTabMenu(null);
              }}
            >Reconnect</button>
            <button
              className="w-full text-left px-3 py-1.5 hover:bg-white/10 text-zinc-200"
              onClick={() => {
                invoke("disconnect_session", { sessionId: tabMenu.sessionId }).catch(console.error);
                setTabMenu(null);
              }}
            >Disconnect</button>
            <div className="border-t border-white/5 my-1" />
            <button
              className="w-full text-left px-3 py-1.5 hover:bg-rose-500/15 hover:text-rose-300 text-zinc-200"
              onClick={() => {
                const sid = tabMenu.sessionId;
                invoke("disconnect_session", { sessionId: sid }).catch(() => {});
                setSessions(prev => prev.filter(sess => sess.id !== sid));
                setSessionStatuses(prev => { const { [sid]: _, ...rest } = prev; return rest; });
                setActiveView(prev => (prev === sid ? "nodes" : prev));
                setTabMenu(null);
              }}
            >Close tab</button>
          </div>
        </>
      )}
    </div>
  );

  if (loading) return <div className="h-screen bg-black flex items-center justify-center font-mono text-xs text-primary animate-pulse">Loading…</div>;

  return (
    <div className="h-screen w-full bg-background flex flex-col overflow-hidden text-zinc-200 select-none">
      <TitleBar />
      {!isUnlocked ? (
        <ProfileSelectPage onUnlocked={handleProfileUnlocked} />
      ) : (
        <div className="flex-1 flex overflow-hidden pt-10">
          <Sidebar activeTab={activeView.startsWith('session-') ? 'nodes' : activeView} setActiveTab={setActiveView} isMobile={isMobile} onLogout={handleLogout} />

          <main className="flex-1 flex flex-col min-w-0 bg-transparent">
            {activeView === "nodes" && (
              <NodeGrid
                servers={servers}
                folders={folders}
                onOpenServer={openServer}
                onEditServer={handleEditNode}
                onAddClick={(folderId?: number | null) => {
                  // When invoked from inside a folder header, seed the new
                  // node's folderId so the user doesn't have to re-pick the
                  // folder they just clicked into. Bare invocation (from the
                  // root grid's "Add server" card) stays at the empty default.
                  setNewNode({
                    ...defaultNode,
                    folderId: folderId != null ? String(folderId) : "",
                  });
                  setIsPanelOpen(true);
                }}
                onQuickConnect={() => setIsQuickConnectOpen(true)}
                onRemoveServer={removeServer}
                onRemoveFolder={removeFolder}
                onRenameFolder={renameFolder}
                onCloneServer={async (id: number) => {
                  try { await invoke("clone_server", { id }); refreshServers(); addLog("Node cloned.", "success"); }
                  catch (e) { addLog(`CLONE_ERROR: ${e}`, "error"); }
                }}
                onSetServerColor={async (id: number, color: string | null) => {
                  try { await invoke("set_server_color", { id, color }); refreshServers(); }
                  catch (e) { addLog(`COLOR_ERROR: ${e}`, "error"); }
                }}
                onSetFolderColor={async (id: number, color: string | null) => {
                  try { await invoke("set_folder_color", { id, color }); refreshFolders(); }
                  catch (e) { addLog(`COLOR_ERROR: ${e}`, "error"); }
                }}
                isMobile={isMobile}
              />
            )}

            {activeView === "settings" && (
              <SettingsPanel 
                settings={appSettings} 
                setSettings={setAppSettings} 
                isMobile={isMobile}
              />
            )}

            {activeView === "vault" && (
              <div className="flex-1 flex flex-col p-4 sm:p-8 space-y-6 sm:space-y-8 animate-in overflow-y-auto custom-scrollbar">
                <header className="flex flex-col sm:flex-row sm:justify-between sm:items-center gap-3 border-b border-zinc-700 pb-5 sm:pb-6 shrink-0">
                  <div className="min-w-0">
                    <h2 className="text-[18px] sm:text-[22px] font-bold text-white tracking-tight">Logins</h2>
                    <p className="hidden sm:block text-[13px] text-zinc-400">Your saved passwords and SSH keys.</p>
                  </div>
                  <div className="flex flex-wrap gap-2">
                    <button
                      onClick={() => { setEditCredData({ id: null, name: "", auth_type: "password", username: "", password: "", key_id: null }); setIsCredPanelOpen(true); }}
                      title="Add Password"
                      className="h-9 px-2.5 sm:px-4 bg-zinc-900 text-zinc-200 text-[12px] sm:text-[13px] font-bold rounded-xl border border-white/5 hover:bg-zinc-800 transition-all flex items-center gap-1.5"
                    >
                      <Plus size={14} /> <span className="hidden sm:inline">Add Password</span><span className="sm:hidden">Password</span>
                    </button>
                    <button
                      onClick={() => { setEditKeyData({ id: null, name: "", public_key: "", private_key: "", passphrase: "" }); setIsKeyPanelOpen(true); }}
                      title="Add Key"
                      className="h-9 px-2.5 sm:px-4 bg-zinc-900 text-zinc-200 text-[12px] sm:text-[13px] font-bold rounded-xl border border-white/5 hover:bg-zinc-800 transition-all flex items-center gap-1.5"
                    >
                      <Plus size={14} /> <span className="hidden sm:inline">Add Key</span><span className="sm:hidden">Key</span>
                    </button>
                    <button
                      onClick={() => {
                        const name = window.prompt("Enter a name for the new SSH key:");
                        if (name) invoke("generate_ssh_key", { name }).then(() => refreshSshKeys());
                      }}
                      title="Generate Key"
                      className="h-9 px-2.5 sm:px-4 bg-primary text-black text-[12px] sm:text-[13px] font-bold rounded-xl shadow-lg shadow-primary/20 flex items-center gap-1.5"
                    >
                      <Key size={14} /> <span className="hidden sm:inline">Generate Key</span><span className="sm:hidden">Generate</span>
                    </button>
                  </div>
                </header>

                <div className="space-y-10 pb-10">
                  <section>
                    <h3 className="text-[13px] font-bold text-zinc-400 mb-4 flex items-center gap-2">
                      <Shield size={14} /> Saved passwords
                    </h3>
                    <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6 gap-2">
                      {credentials.length === 0 ? (
                        <div className="col-span-full py-10 text-center text-zinc-600 text-[14px] italic border border-dashed border-white/5 rounded-2xl">Nothing here yet.</div>
                      ) : (
                        credentials.map(c => (
                          <div key={c.id} className="bg-[#16161a] border border-white/5 rounded-xl p-3 group relative hover:border-primary/30 transition-all">
                            <div className="flex justify-between items-start mb-1">
                              <h4 className="text-[14px] font-bold text-zinc-100 truncate pr-8">{c.name}</h4>
                              <div className="flex items-center gap-2 opacity-0 group-hover:opacity-100 transition-opacity absolute right-3 top-3">
                                <button onClick={() => { setEditCredData(c); setIsCredPanelOpen(true); }} className="text-zinc-500 hover:text-white"><Edit2 size={14} /></button>
                                <button onClick={() => invoke("delete_credential", { id: c.id }).then(() => refreshCredentials())} className="text-zinc-500 hover:text-red-500"><Trash2 size={14} /></button>
                              </div>
                            </div>
                            <div className="flex flex-col gap-0.5">
                              <div className="flex items-center gap-2 text-[11px] text-zinc-500 font-medium">
                                <User size={10} /> {c.username}
                              </div>
                              <div className="flex items-center gap-2 text-[11px] text-zinc-500 font-medium">
                                <Key size={10} /> {c.password ? "••••••••" : "Using SSH key"}
                              </div>
                            </div>
                          </div>
                        ))
                      )}
                    </div>
                  </section>

                  <section>
                    <h3 className="text-[13px] font-bold text-zinc-400 mb-4 flex items-center gap-2">
                      <Key size={14} /> SSH keys
                    </h3>
                    <div className="grid grid-cols-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-5 xl:grid-cols-6 gap-2">
                      {sshKeys.length === 0 ? (
                        <div className="col-span-full py-10 text-center text-zinc-600 text-[14px] italic border border-dashed border-white/5 rounded-2xl">No keys saved yet.</div>
                      ) : (
                        sshKeys.map((k: any) => (
                          <div key={k.id} className="bg-[#16161a] border border-white/5 rounded-xl p-3 group relative hover:border-primary/30 transition-all">
                            <div className="flex justify-between items-start mb-1">
                              <h4 className="text-[14px] font-bold text-zinc-100 truncate pr-8">{k.name}</h4>
                              <div className="flex items-center gap-2 opacity-0 group-hover:opacity-100 transition-opacity absolute right-3 top-3">
                                <button onClick={() => { setEditKeyData(k); setIsKeyPanelOpen(true); }} className="text-zinc-500 hover:text-white"><Edit2 size={14} /></button>
                                <button onClick={() => invoke("delete_ssh_key", { id: k.id }).then(() => refreshSshKeys())} className="text-zinc-500 hover:text-red-500"><Trash2 size={14} /></button>
                              </div>
                            </div>
                            <div className="flex flex-col gap-0.5">
                              <div className="flex items-center gap-2 text-[10px] text-zinc-500 font-mono truncate">
                                {k.public_key.substring(0, 24)}...
                              </div>
                              <div className="text-[10px] text-primary/50 font-medium">
                                Private key
                              </div>
                            </div>
                          </div>
                        ))
                      )}
                    </div>
                  </section>
                </div>
              </div>
            )}

            {sessions.map(sess => (
              <div key={sess.id} className={`flex-1 flex flex-col overflow-hidden ${activeView === sess.id ? '' : 'hidden'}`}>
                <ErrorBoundary
                  label={sess.serverName}
                  onReset={() => {
                    setSessions(prev => prev.filter(s => s.id !== sess.id));
                    if (activeView === sess.id) setActiveView("nodes");
                  }}
                >
                  <SessionView
                    session={sess}
                    onClose={() => {
                      setSessions(prev => prev.filter(s => s.id !== sess.id));
                      setSessionStatuses(prev => {
                        const { [sess.id]: _, ...rest } = prev;
                        return rest;
                      });
                      setActiveView(prev => (prev === sess.id ? "nodes" : prev));
                    }}
                    addLog={addLog}
                    onStatusChange={handleSessionStatus}
                  />
                </ErrorBoundary>
              </div>
            ))}

            {activeView === "notes" && (() => {
              const q = noteQuery.trim().toLowerCase();
              const visible = q
                ? notes.filter(n =>
                    (n.title || "").toLowerCase().includes(q) ||
                    (n.body || "").toLowerCase().includes(q))
                : notes;
              return (
                <div className="flex-1 flex flex-col p-4 sm:p-8 space-y-5 sm:space-y-6 animate-in overflow-y-auto custom-scrollbar">
                  <header className="flex flex-col sm:flex-row sm:justify-between sm:items-center gap-3 border-b border-zinc-700 pb-5 sm:pb-6 shrink-0">
                    <div className="min-w-0">
                      <h2 className="text-[18px] sm:text-[22px] font-bold text-white tracking-tight">Notes</h2>
                      <p className="hidden sm:block text-[13px] text-zinc-400">Anything you want to remember, stored with this profile.</p>
                    </div>
                    <div className="flex gap-2 items-center">
                      <div className="relative">
                        <Search size={13} className="absolute left-2.5 top-1/2 -translate-y-1/2 text-zinc-500 pointer-events-none" />
                        <input
                          type="text"
                          value={noteQuery}
                          onChange={(e) => setNoteQuery(e.target.value)}
                          placeholder="Search title or content…"
                          className="h-9 pl-7 pr-3 w-44 sm:w-56 bg-black/40 border border-white/10 rounded-xl text-[12px] text-zinc-200 placeholder:text-zinc-600 outline-none focus:border-primary/40"
                        />
                      </div>
                      <button
                        onClick={() => { setEditNoteData({ id: null, title: "", body: "" }); setIsNotePanelOpen(true); }}
                        title="Add Note"
                        className="h-9 px-3 sm:px-4 bg-primary text-black text-[12px] sm:text-[13px] font-bold rounded-xl shadow-lg shadow-primary/20 flex items-center gap-1.5 transition-all hover:bg-primary self-start sm:self-auto"
                      >
                        <Plus size={14} /> Add Note
                      </button>
                    </div>
                  </header>

                  <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4 pb-10">
                    {visible.length === 0 ? (
                      <div className="col-span-full py-12 text-center text-zinc-500 text-[14px] italic border border-dashed border-white/10 rounded-2xl">
                        {q ? `No notes match "${noteQuery}".` : "No notes yet."}
                      </div>
                    ) : (
                      visible.map(n => (
                        <div key={n.id} className="bg-[#16161a] border border-white/5 rounded-2xl p-4 flex flex-col group relative overflow-hidden shadow-inner h-[170px]">
                          <div className="flex justify-between items-start mb-2 gap-2">
                            <h3 className="text-[15px] font-bold text-primary tracking-tight truncate flex-1">{n.title || "Untitled"}</h3>
                            <div className="flex items-center gap-2 opacity-0 group-hover:opacity-100 transition-opacity shrink-0">
                              <button onClick={() => { setEditNoteData({ id: n.id, title: n.title || "", body: n.body || "" }); setIsNotePanelOpen(true); }} className="text-zinc-500 hover:text-white transition-colors"><Edit2 size={14} /></button>
                              <button onClick={() => invoke("delete_note", { id: n.id }).then(() => refreshNotes())} className="text-zinc-500 hover:text-red-500 transition-colors"><Trash2 size={14} /></button>
                            </div>
                          </div>
                          <div className="bg-black/30 rounded-xl p-3 border border-white/5 flex-1 relative group-hover:border-primary/20 transition-colors overflow-hidden cursor-pointer"
                               onClick={() => { setEditNoteData({ id: n.id, title: n.title || "", body: n.body || "" }); setIsNotePanelOpen(true); }}>
                            <pre className="text-[12px] text-zinc-400 whitespace-pre-wrap leading-relaxed line-clamp-4 font-sans">{n.body}</pre>
                          </div>
                        </div>
                      ))
                    )}
                  </div>
                </div>
              );
            })()}

            {activeView === "commands" && (
              <div className="flex-1 flex flex-col p-4 sm:p-8 space-y-5 sm:space-y-6 animate-in overflow-y-auto custom-scrollbar">
                <header className="flex flex-col sm:flex-row sm:justify-between sm:items-center gap-3 border-b border-zinc-700 pb-5 sm:pb-6 shrink-0">
                  <div className="min-w-0">
                    <h2 className="text-[18px] sm:text-[22px] font-bold text-white tracking-tight">Commands</h2>
                    <p className="hidden sm:block text-[13px] text-zinc-400">Snippets you can paste into a terminal.</p>
                  </div>
                  <button
                    onClick={() => { setEditCommandData({ id: null, title: "", content: "" }); setIsCommandPanelOpen(true); }}
                    title="Add Command"
                    className="h-9 px-3 sm:px-4 bg-primary text-black text-[12px] sm:text-[13px] font-bold rounded-xl shadow-lg shadow-primary/20 flex items-center gap-1.5 transition-all hover:bg-primary self-start sm:self-auto"
                  >
                    <Plus size={14} /> Add Command
                  </button>
                </header>

                <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4 pb-10">
                  {commands.length === 0 ? (
                    <div className="col-span-full py-12 text-center text-zinc-500 text-[14px] italic border border-dashed border-white/10 rounded-2xl">
                      No commands yet.
                    </div>
                  ) : (
                    commands.map(cmd => (
                      <div key={cmd.id} className="bg-[#16161a] border border-white/5 rounded-2xl p-4 flex flex-col group relative overflow-hidden shadow-inner h-[150px]">
                        <div className="flex justify-between items-start mb-2">
                          <h3 className="text-[16px] font-bold text-primary tracking-tight">{cmd.title}</h3>
                          <div className="flex items-center gap-2 opacity-0 group-hover:opacity-100 transition-opacity">
                            <button onClick={() => { setEditCommandData(cmd); setIsCommandPanelOpen(true); }} className="text-zinc-500 hover:text-white transition-colors"><Edit2 size={14} /></button>
                            <button onClick={() => invoke("delete_command", { id: cmd.id }).then(() => refreshCommands())} className="text-zinc-500 hover:text-red-500 transition-colors"><Trash2 size={14} /></button>
                          </div>
                        </div>
                        <div className="bg-black/30 rounded-xl p-3 border border-white/5 flex-1 relative group-hover:border-primary/20 transition-colors overflow-hidden">
                          <pre className="text-[12px] text-zinc-400 font-mono whitespace-pre-wrap leading-relaxed line-clamp-3">{cmd.content}</pre>
                        </div>
                      </div>
                    ))
                  )}
                </div>
              </div>
            )}

            {/* Monitor is mounted permanently and just CSS-hidden when not
                active. Unmounting would tear down the sample/history ring
                buffers, the per-node event listeners, and the 1s tick — so
                switching tabs felt like monitoring "stopped". Keeping it
                mounted preserves the live data; the backend pollers were
                always running, only the UI side was losing state. */}
            <div className={`flex-1 flex flex-col overflow-hidden ${activeView === "monitor" ? "" : "hidden"}`}>
              <MonitoringPanel servers={servers} refreshServers={refreshServers} addLog={addLog} />
            </div>

            {activeView === "logs" && (
              <div className="flex-1 flex flex-col p-4 sm:p-8 space-y-5 sm:space-y-6 animate-in overflow-hidden">
                <header className="flex flex-col sm:flex-row sm:justify-between sm:items-center gap-3 border-b border-zinc-700 pb-5 sm:pb-6 shrink-0">
                  <div className="min-w-0">
                    <h2 className="text-[18px] sm:text-[22px] font-bold text-white tracking-tight">Activity</h2>
                    <p className="hidden sm:block text-[13px] text-zinc-400">What the app has been doing.</p>
                  </div>
                  <button onClick={() => setLogs([])} className="h-9 px-3 sm:px-4 bg-zinc-900 text-zinc-200 text-[12px] sm:text-[13px] font-bold rounded-xl border border-white/5 hover:bg-red-500/20 hover:text-red-400 hover:border-primary/50 transition-all flex items-center gap-1.5 self-start sm:self-auto">
                    <Trash2 size={14} /> Clear
                  </button>
                </header>

                <div className="flex-1 overflow-y-auto custom-scrollbar bg-black/50 rounded-2xl border border-white/5 p-4 space-y-1 shadow-inner">
                  {logs.length === 0 ? (
                    <div className="h-full flex items-center justify-center text-zinc-600 text-[14px] font-mono italic">
                      Nothing yet.
                    </div>
                  ) : (
                    [...logs].reverse().map((log) => (
                      <div key={(log as any).id ?? `${log.time}-${log.msg}`} className="flex items-start gap-3 text-[12px] font-mono group hover:bg-white/5 p-1 rounded-md transition-colors">
                        <span className="text-zinc-600 shrink-0 w-20">{log.time}</span>
                        <span className={`shrink-0 w-24 font-bold uppercase ${log.type === 'error' ? 'text-red-500' : log.type === 'success' ? 'text-primary' : log.type === 'warn' ? 'text-amber-500' : 'text-blue-500'}`}>
                          [{log.type}]
                        </span>
                        <span className="text-zinc-300 break-all">{log.msg}</span>
                      </div>
                    ))
                  )}
                </div>
              </div>
            )}

          </main>
        </div>
      )}

      <AddNodePanel
        isOpen={isPanelOpen} onClose={() => { setIsPanelOpen(false); setFormError(""); }}
        newNode={newNode} setNewNode={setNewNode}
        isEditMode={!!newNode.id}
        onSave={async () => {
          if (!newNode.name || !newNode.host) {
            setFormError("Name and Host are required.");
            addLog("Name and Host are required.", "error");
            return;
          }
          if (newNode.authType === "vault" && !newNode.credentialId) {
            setFormError("Please pick a saved login.");
            addLog("Please pick a saved login.", "error");
            return;
          }
          if (newNode.authType === "custom_key" && !newNode.keyId) {
            setFormError("Please pick an SSH key.");
            addLog("Please pick an SSH key.", "error");
            return;
          }
          try {
            // Identity fields are only meaningful for inline (custom_*) modes;
            // in vault mode the credential owns username/password/key and we
            // send nulls so the DB row matches reality. The backend's
            // `normalize_server_identity` enforces this again on the server
            // side as a belt-and-suspenders guarantee.
            const isInline = newNode.authType === "custom_pass" || newNode.authType === "custom_key";
            const payload = {
              name: newNode.name,
              host: newNode.host,
              port: newNode.port,
              username: isInline
                ? (newNode.username?.trim() ? newNode.username.trim() : "root")
                : null,
              password: newNode.authType === "custom_pass" ? (newNode.password || null) : null,
              credentialId: (newNode.authType === "vault" && newNode.credentialId) ? parseInt(newNode.credentialId) : null,
              folderId: newNode.folderId ? parseInt(newNode.folderId) : null,
              proxyType: newNode.proxyType || "none",
              proxyHost: newNode.proxyHost || "",
              proxyPort: newNode.proxyPort || 1080,
              tunnels: newNode.tunnels || [],
              authType: newNode.authType,
              keyId: newNode.authType === "custom_key" && newNode.keyId ? parseInt(newNode.keyId) : null,
              autostart: !!newNode.autostart,
              mirrors: newNode.mirrors || [],
              color: newNode.color ?? null,
            };
            const action = newNode.id 
              ? invoke("edit_server", { id: newNode.id, ...payload }) 
              : invoke("add_server", payload);
              
            await action;
            setIsPanelOpen(false);
            setFormError("");
            refreshServers();
            addLog(`Node ${newNode.id ? 'updated' : 'added'} successfully.`, "success");
          } catch (e) {
            setFormError(`Failed to save: ${e}`);
            addLog(`SAVE_ERROR: ${e}`, "error");
          }
        }}
        formError={formError}
        credentials={credentials} sshKeys={sshKeys} folders={folders} refreshFolders={refreshFolders}
        isMobile={isMobile}
      />

      <QuickConnectModal
        isOpen={isQuickConnectOpen}
        onClose={() => setIsQuickConnectOpen(false)}
        onConnect={openQuickConnect}
      />


      <div className={`fixed inset-0 z-50 transition-all duration-500 flex justify-end ${isCommandPanelOpen ? 'bg-black/80 backdrop-blur-sm' : 'opacity-0 pointer-events-none'}`} onClick={() => setIsCommandPanelOpen(false)}>
        <div className={`w-full max-w-[400px] bg-[#09090b] border-l border-white/5 shadow-2xl transition-all duration-500 h-full flex flex-col ${isCommandPanelOpen ? 'translate-x-0' : 'translate-x-full'}`} onClick={(e) => e.stopPropagation()}>
          <div className="flex justify-between items-center p-6 border-b border-white/5 shrink-0">
            <h2 className="text-[15px] font-bold text-white tracking-tight flex items-center gap-2">
              <TerminalSquare size={18} className="text-primary" />
              {editCommandData.id ? "Edit command" : "New command"}
            </h2>
            <button onClick={() => { setIsCommandPanelOpen(false); setFormError(""); }} className="w-7 h-7 flex items-center justify-center text-zinc-500 hover:text-white bg-black border border-white/5 hover:bg-white/10 rounded-full transition-all">
              <X size={16} />
            </button>
          </div>
          <div className="p-6 flex-1 overflow-y-auto space-y-6">
            {formError && <div className="p-3 bg-red-500/10 border border-red-500/20 rounded-xl text-red-500 text-[12px] font-bold">{formError}</div>}
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Title</label>
              <input type="text" className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-zinc-900/50 transition-all shadow-inner" placeholder="e.g. Update packages" value={editCommandData.title} onChange={e => setEditCommandData({ ...editCommandData, title: e.target.value })} />
            </div>
            <div className="space-y-1.5 flex-1 flex flex-col h-64">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Command</label>
              <textarea className="w-full flex-1 bg-black rounded-lg p-3 text-[13px] text-zinc-300 font-mono border border-white/10 outline-none focus:border-primary/50 focus:bg-zinc-900/50 transition-all shadow-inner custom-scrollbar resize-none" placeholder="sudo apt update && sudo apt upgrade -y" value={editCommandData.content} onChange={e => setEditCommandData({ ...editCommandData, content: e.target.value })} />
            </div>
          </div>
          <div className="p-6 border-t border-white/5 shrink-0">
            <button 
              onClick={async () => {
                if (!editCommandData.title || !editCommandData.content) {
                  addLog("Title and Content are required.", "error");
                  return;
                }
                try {
                  const action = editCommandData.id 
                    ? invoke("edit_command", { id: editCommandData.id, title: editCommandData.title, content: editCommandData.content })
                    : invoke("add_command", { title: editCommandData.title, content: editCommandData.content });
                  await action;
                  setIsCommandPanelOpen(false);
                  refreshCommands();
                  addLog("Command saved.", "success");
                } catch (e) {
                  setFormError(`Failed to save command: ${e}`);
                  addLog(`COMMAND_SAVE_ERROR: ${e}`, "error");
                }
              }} 
              className="w-full h-10 bg-primary text-black font-bold rounded-lg text-[13px] tracking-tight hover:bg-primary transition-all active:scale-[0.98] shadow-[0_0_15px_rgba(var(--primary),0.2)] flex items-center justify-center gap-2"
            >
              Save
            </button>
          </div>
        </div>
      </div>

      <div className={`fixed inset-0 z-50 transition-all duration-500 flex justify-end ${isNotePanelOpen ? 'bg-black/80 backdrop-blur-sm' : 'opacity-0 pointer-events-none'}`} onClick={() => setIsNotePanelOpen(false)}>
        <div className={`w-full max-w-[480px] bg-[#09090b] border-l border-white/5 shadow-2xl transition-all duration-500 h-full flex flex-col ${isNotePanelOpen ? 'translate-x-0' : 'translate-x-full'}`} onClick={(e) => e.stopPropagation()}>
          <div className="flex justify-between items-center p-6 border-b border-white/5 shrink-0">
            <h2 className="text-[15px] font-bold text-white tracking-tight flex items-center gap-2">
              <StickyNote size={18} className="text-primary" />
              {editNoteData.id ? "Edit note" : "New note"}
            </h2>
            <button onClick={() => { setIsNotePanelOpen(false); setFormError(""); }} className="w-7 h-7 flex items-center justify-center text-zinc-500 hover:text-white bg-black border border-white/5 hover:bg-white/10 rounded-full transition-all">
              <X size={16} />
            </button>
          </div>
          <div className="p-6 flex-1 overflow-y-auto space-y-6 flex flex-col">
            {formError && <div className="p-3 bg-red-500/10 border border-red-500/20 rounded-xl text-red-500 text-[12px] font-bold">{formError}</div>}
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Title</label>
              <input type="text" className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-zinc-900/50 transition-all shadow-inner" placeholder="e.g. Production credentials reminder" value={editNoteData.title} onChange={e => setEditNoteData({ ...editNoteData, title: e.target.value })} />
            </div>
            <div className="space-y-1.5 flex-1 flex flex-col min-h-[240px]">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Content</label>
              <textarea className="w-full flex-1 bg-black rounded-lg p-3 text-[13px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 focus:bg-zinc-900/50 transition-all shadow-inner custom-scrollbar resize-none leading-relaxed" placeholder="Write anything — markdown, paths, secrets you'd otherwise forget…" value={editNoteData.body} onChange={e => setEditNoteData({ ...editNoteData, body: e.target.value })} />
            </div>
          </div>
          <div className="p-6 border-t border-white/5 shrink-0">
            <button
              onClick={async () => {
                if (!editNoteData.title.trim() && !editNoteData.body.trim()) {
                  setFormError("Add a title or some content first.");
                  return;
                }
                try {
                  const action = editNoteData.id
                    ? invoke("edit_note", { id: editNoteData.id, title: editNoteData.title, body: editNoteData.body })
                    : invoke("add_note", { title: editNoteData.title, body: editNoteData.body });
                  await action;
                  setIsNotePanelOpen(false);
                  refreshNotes();
                  addLog("Note saved.", "success");
                } catch (e) {
                  setFormError(`Failed to save note: ${e}`);
                  addLog(`NOTE_SAVE_ERROR: ${e}`, "error");
                }
              }}
              className="w-full h-10 bg-primary text-black font-bold rounded-lg text-[13px] tracking-tight hover:bg-primary transition-all active:scale-[0.98] shadow-[0_0_15px_rgba(var(--primary),0.2)] flex items-center justify-center gap-2"
            >
              Save
            </button>
          </div>
        </div>
      </div>

      <div className={`fixed inset-0 z-50 transition-all duration-500 flex justify-end ${isCredPanelOpen ? 'bg-black/80 backdrop-blur-sm' : 'opacity-0 pointer-events-none'}`} onClick={() => setIsCredPanelOpen(false)}>
        <div className={`w-full max-w-[400px] bg-[#09090b] border-l border-white/5 shadow-2xl transition-all duration-500 h-full flex flex-col ${isCredPanelOpen ? 'translate-x-0' : 'translate-x-full'}`} onClick={(e) => e.stopPropagation()}>
          <div className="flex justify-between items-center p-6 border-b border-white/5 shrink-0">
            <h2 className="text-[15px] font-bold text-white tracking-tight flex items-center gap-2">
              <Shield size={18} className="text-primary" />
              {editCredData.id ? "Edit login" : "New login"}
            </h2>
            <button onClick={() => { setIsCredPanelOpen(false); setFormError(""); }} className="w-7 h-7 flex items-center justify-center text-zinc-500 hover:text-white bg-black border border-white/5 hover:bg-white/10 rounded-full transition-all">
              <X size={16} />
            </button>
          </div>
          <div className="p-6 flex-1 overflow-y-auto space-y-6">
            {formError && <div className="p-3 bg-red-500/10 border border-red-500/20 rounded-xl text-red-500 text-[12px] font-bold">{formError}</div>}
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Name</label>
              <input type="text" className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-zinc-900/50 transition-all shadow-inner" placeholder="e.g. Work server" value={editCredData.name} onChange={e => setEditCredData({ ...editCredData, name: e.target.value })} />
            </div>
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Username</label>
              <input type="text" className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-zinc-900/50 transition-all shadow-inner" placeholder="root" value={editCredData.username} onChange={e => setEditCredData({ ...editCredData, username: e.target.value })} />
            </div>
            <div className="space-y-4 pt-2">
              <div className="flex justify-between items-center">
                <label className="text-[12px] font-bold text-zinc-400 ml-1">Sign in with</label>
                <select
                  className="bg-transparent text-[12px] font-bold text-primary outline-none cursor-pointer"
                  value={editCredData.auth_type || "password"}
                  onChange={e => setEditCredData({ ...editCredData, auth_type: e.target.value })}
                >
                  <option value="password" className="bg-[#121215] text-primary">Password</option>
                  <option value="key" className="bg-[#121215] text-primary">SSH key</option>
                </select>
              </div>
            </div>
            {editCredData.auth_type === "key" ? (
              <div className="space-y-1.5 animate-in fade-in">
                <label className="text-[12px] font-bold text-zinc-400 ml-1">Pick a key</label>
                <select
                  className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                  value={editCredData.key_id?.toString() || ""}
                  onChange={e => setEditCredData({ ...editCredData, key_id: e.target.value ? parseInt(e.target.value) : null })}
                >
                  <option value="" className="bg-black text-zinc-500">-- Pick a key --</option>
                  {sshKeys?.map((k: any) => (
                    <option key={k.id} value={k.id.toString()} className="bg-black text-white">{k.name}</option>
                  ))}
                </select>
              </div>
            ) : (
              <div className="space-y-1.5 animate-in fade-in">
                <label className="text-[12px] font-bold text-zinc-400 ml-1">Password</label>
                <PasswordField
                  value={editCredData.password || ""}
                  onChange={(v) => setEditCredData({ ...editCredData, password: v })}
                  className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-zinc-900/50 transition-all shadow-inner"
                />
              </div>
            )}
          </div>
          <div className="p-6 border-t border-white/5 shrink-0">
            <button 
              onClick={async () => {
                if (!editCredData.name) {
                  setFormError("Name is required.");
                  addLog("Name is required.", "error");
                  return;
                }
                if (editCredData.auth_type === "key" && !editCredData.key_id) {
                  setFormError("Please select a linked SSH Private Key.");
                  addLog("Please select a linked SSH Private Key.", "error");
                  return;
                }
                try {
                  const payload = {
                    name: editCredData.name,
                    authType: editCredData.auth_type || "password",
                    username: editCredData.username?.trim() ? editCredData.username.trim() : "root",
                    password: editCredData.auth_type === "key" ? null : (editCredData.password || null),
                    keyId: editCredData.auth_type === "key" ? (editCredData.key_id ? parseInt(editCredData.key_id.toString()) : null) : null
                  };
                  const action = editCredData.id 
                    ? invoke("edit_credential", { id: editCredData.id, ...payload })
                    : invoke("add_credential", payload);
                  await action;
                  setIsCredPanelOpen(false);
                  refreshCredentials();
                  addLog("Credential saved.", "success");
                } catch (e) {
                  setFormError(`Failed to save credential: ${e}`);
                  addLog(`CRED_SAVE_ERROR: ${e}`, "error");
                }
              }} 
              className="w-full h-10 bg-primary text-black font-bold rounded-lg text-[13px] tracking-tight hover:bg-primary transition-all active:scale-[0.98] flex items-center justify-center gap-2"
            >
              Save
            </button>
          </div>
        </div>
      </div>

      <div className={`fixed inset-0 z-50 transition-all duration-500 flex justify-end ${isKeyPanelOpen ? 'bg-black/80 backdrop-blur-sm' : 'opacity-0 pointer-events-none'}`} onClick={() => setIsKeyPanelOpen(false)}>
        <div className={`w-full max-w-[450px] bg-[#09090b] border-l border-white/5 shadow-2xl transition-all duration-500 h-full flex flex-col ${isKeyPanelOpen ? 'translate-x-0' : 'translate-x-full'}`} onClick={(e) => e.stopPropagation()}>
          <div className="flex justify-between items-center p-6 border-b border-white/5 shrink-0">
            <h2 className="text-[15px] font-bold text-white tracking-tight flex items-center gap-2">
              <Key size={18} className="text-primary" />
              {editKeyData.id ? "Edit SSH key" : "New SSH key"}
            </h2>
            <button onClick={() => { setIsKeyPanelOpen(false); setFormError(""); }} className="w-7 h-7 flex items-center justify-center text-zinc-500 hover:text-white bg-black border border-white/5 hover:bg-white/10 rounded-full transition-all">
              <X size={16} />
            </button>
          </div>
          <div className="p-6 flex-1 overflow-y-auto space-y-6">
            {formError && <div className="p-3 bg-red-500/10 border border-red-500/20 rounded-xl text-red-500 text-[12px] font-bold">{formError}</div>}
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Name</label>
              <input type="text" className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-white border border-white/10 outline-none focus:border-primary-500/50 focus:bg-zinc-900/50 transition-all shadow-inner" placeholder="e.g. My laptop key" value={editKeyData.name} onChange={e => setEditKeyData({ ...editKeyData, name: e.target.value })} />
            </div>
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Public key</label>
              <textarea className="w-full h-24 bg-black rounded-lg p-3 text-[13px] text-zinc-400 font-mono border border-white/10 outline-none focus:border-primary-500/50 focus:bg-zinc-900/50 transition-all shadow-inner custom-scrollbar resize-none" placeholder="ssh-ed25519 ..." value={editKeyData.public_key} onChange={e => setEditKeyData({ ...editKeyData, public_key: e.target.value })} />
            </div>
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Private key</label>
              <textarea className="w-full h-32 bg-black rounded-lg p-3 text-[13px] text-zinc-400 font-mono border border-white/10 outline-none focus:border-primary-500/50 focus:bg-zinc-900/50 transition-all shadow-inner custom-scrollbar resize-none" placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" value={editKeyData.private_key} onChange={e => setEditKeyData({ ...editKeyData, private_key: e.target.value })} />
            </div>
            <div className="space-y-1.5">
              <label className="text-[12px] font-bold text-zinc-400 ml-1">Passphrase (if any)</label>
              <PasswordField
                value={editKeyData.passphrase || ""}
                onChange={(v) => setEditKeyData({ ...editKeyData, passphrase: v })}
                className="w-full h-10 bg-black rounded-lg px-3 text-[13px] text-white border border-white/10 outline-none focus:border-primary-500/50 focus:bg-zinc-900/50 transition-all shadow-inner"
              />
            </div>
          </div>
          <div className="p-6 border-t border-white/5 shrink-0">
            <button 
              onClick={async () => {
                if (!editKeyData.name || !editKeyData.private_key) {
                  addLog("Name and Private Key are required.", "error");
                  return;
                }
                try {
                  const payload = {
                    name: editKeyData.name,
                    publicKey: editKeyData.public_key,
                    privateKey: editKeyData.private_key,
                    passphrase: editKeyData.passphrase || null
                  };
                  const action = editKeyData.id 
                    ? invoke("edit_ssh_key", { id: editKeyData.id, ...payload })
                    : invoke("add_ssh_key", payload);
                  await action;
                  setIsKeyPanelOpen(false);
                  refreshSshKeys();
                  addLog("SSH Key saved.", "success");
                } catch (e) {
                  setFormError(`Failed to save SSH Key: ${e}`);
                  addLog(`KEY_SAVE_ERROR: ${e}`, "error");
                }
              }} 
              className="w-full h-10 bg-primary text-black font-bold rounded-lg text-[13px] tracking-tight hover:bg-primary transition-all active:scale-[0.98] flex items-center justify-center gap-2"
            >
              Save
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

export default DesktopApp;