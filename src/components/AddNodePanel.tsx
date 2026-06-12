import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Cpu, X, Link2, ArrowLeftRight, Shield, Key, User, FolderPlus } from "lucide-react";
import PasswordField from "./PasswordField";

const AddNodePanel = ({ isOpen, onClose, newNode, setNewNode, onSave, credentials, sshKeys, folders, refreshFolders, isEditMode, formError, isMobile }: any) => {
  const [isCreatingFolder, setIsCreatingFolder] = useState(false);
  const [newFolderName, setNewFolderName] = useState("");

  const handleCreateFolder = async () => {
    if (!newFolderName.trim()) return;
    try {
      await invoke("add_folder", { name: newFolderName, parentId: null });
      setNewFolderName("");
      setIsCreatingFolder(false);
      refreshFolders();
    } catch (e) {
      console.error(e);
    }
  };

  const updateTunnel = (idx: number, field: string, value: string) => {
    const newT = [...newNode.tunnels];
    newT[idx][field] = value;
    setNewNode({ ...newNode, tunnels: newT });
  };

  return (
    <div className={`fixed inset-0 z-50 transition-all duration-500 flex justify-end ${isOpen ? 'bg-black/60 backdrop-blur-sm' : 'opacity-0 pointer-events-none'}`} onClick={onClose}>
      <div className={`w-full ${isMobile ? 'max-w-full' : 'max-w-[450px]'} bg-background border-l border-white/5 shadow-2xl transition-all duration-500 h-full flex flex-col ${isOpen ? 'translate-x-0' : 'translate-x-full'}`} onClick={(e) => e.stopPropagation()}>

        {/* Header */}
        <div className="flex justify-between items-center p-6 border-b border-white/5 shrink-0">
          <div className="flex items-center gap-3">
            <div className="w-8 h-8 bg-primary/10 rounded-lg flex items-center justify-center text-primary shadow-inner border border-primary/20">
              <Cpu size={16} />
            </div>
            <h2 className="text-[14px] font-bold text-white tracking-tight">Server details</h2>
          </div>
          <button onClick={onClose} className="w-8 h-8 flex items-center justify-center text-zinc-500 hover:text-white bg-black border border-white/5 hover:bg-white/10 rounded-xl transition-all shadow-inner">
            <X size={16} />
          </button>
        </div>

        {/* Scrollable Content */}
        <div className="p-6 flex-1 overflow-y-auto custom-scrollbar space-y-6">
          {formError && <div className="p-3 bg-red-500/10 border border-red-500/20 rounded-xl text-red-500 text-[12px] font-bold">{formError}</div>}

          <div className="grid grid-cols-1 gap-4">
            <div className="space-y-1.5">
              <label className="text-[11px] font-bold text-zinc-400 ml-1">Name</label>
              <input type="text" className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" placeholder="e.g. Work server" value={newNode.name} onChange={e => setNewNode({ ...newNode, name: e.target.value })} />
            </div>

            <div className="space-y-1.5">
              <label className="text-[11px] font-bold text-zinc-400 ml-1">Folder</label>
              {!isCreatingFolder ? (
                <div className="flex gap-2">
                  <select className="flex-1 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" value={newNode.folderId} onChange={e => setNewNode({ ...newNode, folderId: e.target.value })}>
                    <option value="" className="bg-[#1a1a1e] text-zinc-500">-- Root --</option>
                    {folders?.map((f: any) => <option key={f.id} value={f.id.toString()} className="bg-[#1a1a1e] text-white">{f.name}</option>)}
                  </select>
                  <button onClick={() => setIsCreatingFolder(true)} className="w-9 h-9 bg-white/5 border border-white/10 rounded-lg text-primary hover:bg-white/10 transition-all flex items-center justify-center shrink-0">
                    <FolderPlus size={14} />
                  </button>
                </div>
              ) : (
                <div className="flex gap-2 animate-in fade-in">
                  <input type="text" className="flex-1 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" placeholder="Folder name…" value={newFolderName} onChange={e => setNewFolderName(e.target.value)} onKeyDown={e => e.key === 'Enter' && handleCreateFolder()} />
                  <button onClick={handleCreateFolder} className="h-9 px-3 bg-primary text-black font-bold text-[12px] rounded-lg hover:bg-primary transition-all">Add</button>
                  <button onClick={() => setIsCreatingFolder(false)} className="w-9 h-9 bg-white/5 text-zinc-400 rounded-lg hover:bg-white/10 transition-all flex justify-center items-center shrink-0"><X size={14} /></button>
                </div>
              )}
            </div>

            <div className="space-y-1.5">
              <label className="text-[11px] font-bold text-zinc-400 ml-1">Tag colour</label>
              <div className="flex items-center gap-2 flex-wrap">
                {[
                  { name: "Default", value: null,      cls: "bg-zinc-500" },
                  { name: "Rose",    value: "#f43f5e", cls: "bg-rose-500" },
                  { name: "Amber",   value: "#f59e0b", cls: "bg-amber-500" },
                  { name: "Emerald", value: "#10b981", cls: "bg-emerald-500" },
                  { name: "Sky",     value: "#0ea5e9", cls: "bg-sky-500" },
                  { name: "Indigo",  value: "#6366f1", cls: "bg-indigo-500" },
                  { name: "Fuchsia", value: "#d946ef", cls: "bg-fuchsia-500" },
                  { name: "Slate",   value: "#64748b", cls: "bg-slate-500" },
                ].map((p) => {
                  const selected = (newNode.color ?? null) === p.value;
                  return (
                    <button
                      key={p.name}
                      type="button"
                      title={p.name}
                      onClick={() => setNewNode({ ...newNode, color: p.value })}
                      className={`w-6 h-6 rounded-full ${p.cls} ring-2 ring-transparent hover:ring-white/30 transition-all ${selected ? "ring-white" : ""}`}
                    />
                  );
                })}
              </div>
            </div>

            <div className="grid grid-cols-4 gap-4">
              <div className="col-span-3 space-y-1.5">
                <label className="text-[11px] font-bold text-zinc-400 ml-1">Host</label>
                <input type="text" className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" placeholder="192.168.1.1 or example.com" value={newNode.host} onChange={e => setNewNode({ ...newNode, host: e.target.value })} />
              </div>
              <div className="space-y-1.5">
                <label className="text-[11px] font-bold text-zinc-400 ml-1">Port</label>
                <input type="number" className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" value={newNode.port} onChange={e => setNewNode({ ...newNode, port: parseInt(e.target.value) })} />
              </div>
            </div>

          </div>

          <div className="pt-5 border-t border-white/5 space-y-4">
            <div className="flex justify-between items-center">
              <label className="text-[11px] font-bold text-zinc-400">How to log in</label>
              <select
                className="bg-transparent text-[12px] font-bold text-primary outline-none cursor-pointer"
                value={newNode.authType}
                onChange={(e) => setNewNode({ ...newNode, authType: e.target.value })}
              >
                <option value="vault" className="bg-[#121215] text-primary">Use a saved login</option>
                <option value="custom_pass" className="bg-[#121215] text-primary">Type a password</option>
                <option value="custom_key" className="bg-[#121215] text-primary">Use an SSH key</option>
              </select>
            </div>

            {newNode.authType === 'vault' && (() => {
              const selectedCred = credentials?.find((c: any) => c.id?.toString() === newNode.credentialId?.toString());
              return (
                <div className="space-y-3 animate-in fade-in slide-in-from-top-1">
                  <div className="space-y-1.5">
                    <label className="text-[11px] font-bold text-zinc-500 ml-1">Saved login</label>
                    <select className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" value={newNode.credentialId} onChange={e => setNewNode({ ...newNode, credentialId: e.target.value })}>
                      <option value="" className="bg-[#1a1a1e] text-zinc-500">-- Pick one --</option>
                      {credentials?.map((c: any) => <option key={c.id} value={c.id.toString()} className="bg-[#1a1a1e] text-zinc-300">{c.name} ({c.username})</option>)}
                    </select>
                  </div>
                  {selectedCred && (
                    <div className="px-3 py-2 bg-primary/5 border border-primary/20 rounded-lg text-[11px] font-mono text-zinc-300 flex items-center gap-2">
                      <User size={11} className="text-primary shrink-0" />
                      <span className="text-zinc-500">Sign in as</span>
                      <span className="text-primary font-bold">{selectedCred.username || "—"}</span>
                      <span className="text-zinc-600 ml-auto text-[10px]">{selectedCred.auth_type === 'key' ? 'using key' : 'using password'}</span>
                    </div>
                  )}
                </div>
              );
            })()}

            {newNode.authType === 'custom_pass' && (
              <div className="space-y-3 animate-in fade-in">
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-500 ml-1">Username</label>
                  <input type="text" placeholder="root" value={newNode.username || ""} onChange={e => setNewNode({ ...newNode, username: e.target.value })} className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" />
                </div>
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-500 ml-1">Password</label>
                  <PasswordField
                    value={newNode.password || ""}
                    onChange={(v) => setNewNode({ ...newNode, password: v, password_dirty: true })}
                    className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                    placeholder="••••••"
                  />
                </div>
              </div>
            )}

            {newNode.authType === 'custom_key' && (
              <div className="space-y-3 animate-in fade-in">
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-500 ml-1">Username</label>
                  <input type="text" placeholder="root" value={newNode.username || ""} onChange={e => setNewNode({ ...newNode, username: e.target.value })} className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" />
                </div>
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-500 ml-1">SSH key</label>
                  <select className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" value={newNode.keyId} onChange={e => setNewNode({ ...newNode, keyId: e.target.value })}>
                    <option value="" className="bg-[#1a1a1e] text-zinc-500">-- Pick a key --</option>
                    {sshKeys?.map((k: any) => <option key={k.id} value={k.id.toString()} className="bg-[#1a1a1e] text-zinc-300">{k.name}</option>)}
                  </select>
                </div>
              </div>
            )}
          </div>

          <div className="pt-5 border-t border-white/5 space-y-4">
            <div className="flex justify-between items-center">
              <label className="text-[11px] font-bold text-zinc-400">Proxy</label>
              <select className="bg-transparent text-[12px] font-bold text-zinc-300 outline-none cursor-pointer" value={newNode.proxyType} onChange={e => setNewNode({ ...newNode, proxyType: e.target.value })}>
                <option value="none" className="bg-[#121215] text-zinc-400">No proxy</option>
                <option value="socks5" className="bg-[#121215] text-zinc-400">SOCKS5</option>
                <option value="http" className="bg-[#121215] text-zinc-400">HTTP</option>
              </select>
            </div>

            {newNode.proxyType !== 'none' && (
              <div className="grid grid-cols-4 gap-3 animate-in fade-in">
                <input
                  className="col-span-3 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                  placeholder="Proxy Host"
                  value={newNode.proxyHost || ""}
                  onChange={e => setNewNode({ ...newNode, proxyHost: e.target.value })}
                />
                <input
                  type="number"
                  className="h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                  placeholder="Port"
                  value={newNode.proxyPort || ""}
                  onChange={e => setNewNode({ ...newNode, proxyPort: parseInt(e.target.value) || 0 })}
                />
              </div>
            )}

            <div className="space-y-2.5">
              <div className="flex justify-between items-center">
                <span className="text-[11px] font-bold text-zinc-400">Port forwarding</span>
                <button onClick={() => setNewNode({ ...newNode, tunnels: [...newNode.tunnels, { rid: `t-${Date.now()}-${Math.random().toString(36).slice(2,7)}`, local: "1080", remote: "", type: "D" }] })} className="text-[11px] font-bold text-primary hover:text-primary transition-colors">+ Add</button>
              </div>

              <div className="space-y-2">
                {newNode.tunnels?.map((t: any, idx: number) => (
                  // `rid` is a stable per-row id added on Add. Falling
                  // back to idx for legacy rows that lack it; new rows
                  // always carry rid so deletions / reorders preserve
                  // each input's focus and cursor state.
                  <div key={t.rid ?? idx} className="flex items-center gap-2 p-2 bg-[#1a1a1e] rounded-lg border border-white/5 group transition-all hover:border-primary/30">
                    <select
                      className="bg-transparent text-[11px] font-black outline-none text-primary uppercase tracking-wider cursor-pointer pl-1"
                      value={t.type}
                      onChange={e => updateTunnel(idx, 'type', e.target.value)}
                    >
                      <option value="D" className="bg-[#1a1a1e] text-primary">Dynamic</option>
                      <option value="L" className="bg-[#1a1a1e] text-primary">Local</option>
                      <option value="R" className="bg-[#1a1a1e] text-primary">Remote</option>
                    </select>

                    <div className="h-4 w-px bg-white/10 mx-1" />

                    <input
                      className="flex-1 min-w-0 bg-transparent text-[12px] font-mono outline-none text-zinc-300 placeholder:text-zinc-700"
                      placeholder={
                        t.type === 'D' ? "Port (1080)" :
                        t.type === 'R' ? "Server bind (8080 or 0.0.0.0:8080)" :
                                         "Local (8080 or 0.0.0.0:8080)"
                      }
                      value={t.local}
                      onChange={e => updateTunnel(idx, 'local', e.target.value)}
                    />

                    {t.type !== 'D' && (
                      <>
                        <ArrowLeftRight size={10} className="text-zinc-700 shrink-0 mx-1" />
                        <input
                          className="flex-1 min-w-0 bg-transparent text-[12px] font-mono outline-none text-primary placeholder:text-zinc-700"
                          placeholder="Remote (80)"
                          value={t.remote}
                          onChange={e => updateTunnel(idx, 'remote', e.target.value)}
                        />
                      </>
                    )}

                    <button onClick={() => setNewNode({ ...newNode, tunnels: newNode.tunnels.filter((_: any, i: number) => i !== idx) })} className="opacity-0 group-hover:opacity-100 w-6 h-6 flex items-center justify-center text-red-500 hover:bg-red-500/10 rounded transition-all shrink-0">
                      <X size={12} />
                    </button>
                  </div>
                ))}
              </div>
            </div>

            {/* Folder mirrors saved on this node — one-way local → remote
                sync. Stored alongside tunnels so they roam with the vault.
                Live management (start / stop / log) lives in the Mirror
                panel inside an open session. */}
            <div className="space-y-2.5">
              <div className="flex justify-between items-center">
                <span className="text-[11px] font-bold text-zinc-400">Folder mirrors</span>
                <button
                  type="button"
                  onClick={() => setNewNode({
                    ...newNode,
                    mirrors: [...(newNode.mirrors || []), { local: "", remote: "", soft_delete: true, excludes: [], conflict_resolution: "local" }],
                  })}
                  className="text-[11px] font-bold text-primary hover:text-primary transition-colors"
                >+ Add</button>
              </div>
              <div className="space-y-2">
                {(newNode.mirrors || []).map((m: any, idx: number) => (
                  <div key={idx} className="p-2.5 bg-[#1a1a1e] rounded-lg border border-white/5 group space-y-1.5">
                    <div className="flex items-center gap-2">
                      <input
                        className="flex-1 min-w-0 h-7 bg-black/40 rounded px-2 text-[11.5px] font-mono text-zinc-200 border border-white/10 outline-none focus:border-primary/50"
                        placeholder="/local/path"
                        value={m.local}
                        onChange={(e) => {
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], local: e.target.value };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                      />
                      <ArrowLeftRight size={10} className="text-zinc-700 shrink-0 mx-1" />
                      <input
                        className="flex-1 min-w-0 h-7 bg-black/40 rounded px-2 text-[11.5px] font-mono text-zinc-200 border border-white/10 outline-none focus:border-primary/50"
                        placeholder="/remote/path"
                        value={m.remote}
                        onChange={(e) => {
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], remote: e.target.value };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                      />
                      <button
                        type="button"
                        onClick={() => setNewNode({ ...newNode, mirrors: newNode.mirrors.filter((_: any, i: number) => i !== idx) })}
                        className="opacity-0 group-hover:opacity-100 w-6 h-6 flex items-center justify-center text-red-500 hover:bg-red-500/10 rounded transition-all shrink-0"
                      >
                        <X size={12} />
                      </button>
                    </div>
                    <div className="flex items-center gap-3 pl-1">
                      <label className="flex items-center gap-1.5 text-[10.5px] text-zinc-400 select-none cursor-pointer">
                        <input
                          type="checkbox"
                          checked={m.soft_delete !== false}
                          onChange={(e) => {
                            const next = [...newNode.mirrors];
                            next[idx] = { ...next[idx], soft_delete: e.target.checked };
                            setNewNode({ ...newNode, mirrors: next });
                          }}
                          className="accent-primary"
                        />
                        <span>Soft delete</span>
                      </label>
                      <select
                        value={m.conflict_resolution || "local"}
                        onChange={(e) => {
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], conflict_resolution: e.target.value };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                        title="Initial-sync conflict resolution: which side overwrites when a file exists on both with different content. Files missing on one side always copy across."
                        className="h-6 px-1.5 bg-black/30 border border-white/10 rounded text-[10.5px] text-zinc-300 outline-none focus:border-primary/40"
                      >
                        <option value="local">Local wins</option>
                        <option value="remote">Remote wins</option>
                        <option value="newer">Newer wins</option>
                      </select>
                      <input
                        className="flex-1 min-w-0 h-6 bg-transparent rounded px-1 text-[10.5px] font-mono text-zinc-400 placeholder:text-zinc-700 outline-none"
                        placeholder="Excludes: .git, node_modules, *.swp"
                        value={(m.excludes || []).join(", ")}
                        onChange={(e) => {
                          const parts = e.target.value.split(",").map((s) => s.trim()).filter(Boolean);
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], excludes: parts };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                      />
                    </div>
                  </div>
                ))}
              </div>
            </div>

            {/* Autostart toggle — when on, the node opens a session and
                connects automatically right after the user unlocks the
                profile. Useful for the one or two servers you always have
                a shell on. */}
            <div className="flex items-center justify-between gap-3 pt-1">
              <div className="min-w-0">
                <div className="text-[11px] font-bold text-zinc-300">Autostart</div>
                <div className="text-[10.5px] text-zinc-500 leading-snug">
                  Open + connect this node automatically when the app launches.
                </div>
              </div>
              <button
                type="button"
                onClick={() => setNewNode({ ...newNode, autostart: !newNode.autostart })}
                role="switch"
                aria-checked={!!newNode.autostart}
                className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${
                  newNode.autostart ? "bg-primary" : "bg-white/10"
                }`}
              >
                <span
                  className={`absolute top-0.5 left-0.5 w-5 h-5 bg-zinc-100 rounded-full shadow transition-transform ${
                    newNode.autostart ? "translate-x-5" : "translate-x-0"
                  }`}
                />
              </button>
            </div>

            {/* Per-node notes — free-form, plain text. Lives on the server
                row (servers.notes column) and never crosses the network. The
                primary use case is small runbooks: which user owns root,
                where the deploy script lives, contact for the box, etc. */}
            <div className="space-y-1.5 pt-1">
              <label className="text-[11px] font-bold text-zinc-400 ml-1 flex items-center gap-2">
                <span>Notes</span>
                <span className="text-[10px] text-zinc-600 font-normal normal-case tracking-normal">
                  Description, runbook, contacts — only for this server.
                </span>
              </label>
              <textarea
                value={newNode.notes || ""}
                onChange={(e) => setNewNode({ ...newNode, notes: e.target.value })}
                placeholder="e.g. Prod web. Reboot windows Sun 04:00 UTC. Deploy via `make ship`. Pager: @ops"
                rows={6}
                className="w-full bg-[#1a1a1e] rounded-lg p-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner font-mono leading-relaxed resize-y"
              />
            </div>
          </div>
        </div>

        {/* Footer */}
        <div className="p-6 border-t border-white/5 shrink-0">
          <button onClick={onSave} className="w-full h-10 bg-primary text-black font-bold rounded-lg text-[13px] tracking-tight hover:bg-primary hover:shadow-[0_0_20px_rgba(var(--primary),0.3)] transition-all active:scale-[0.98] flex items-center justify-center gap-2">
            <Cpu size={14} /> {isEditMode ? "Save changes" : "Save server"}
          </button>
        </div>
      </div>
    </div>
  );
};

export default AddNodePanel;