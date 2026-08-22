import { useState } from "react";
import { Zap, X, AlertTriangle, KeyRound, Lock } from "lucide-react";
import PasswordField from "./PasswordField";

// Quick Connect: the auth bundle lives in component state, gets passed to
// DesktopApp via onConnect, and the resulting session carries it through to
// `initiate_connection`'s `quick_auth` param on the backend (so the live
// connection uses the inline auth directly). DesktopApp ALSO persists the
// target as a reusable node in the root directory (see save_quick_connect_node)
// so it can be reconnected later without re-typing credentials.

export interface QuickAuth {
  host: string;
  port: number;
  username: string;
  password?: string | null;
  private_key?: string | null;
  passphrase?: string | null;
}

interface Props {
  isOpen: boolean;
  onClose: () => void;
  onConnect: (auth: QuickAuth) => void;
}

const QuickConnectModal = ({ isOpen, onClose, onConnect }: Props) => {
  const [host, setHost] = useState("");
  const [port, setPort] = useState<number>(22);
  const [username, setUsername] = useState("root");
  const [authMode, setAuthMode] = useState<"password" | "key">("password");
  const [password, setPassword] = useState("");
  const [privateKey, setPrivateKey] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [err, setErr] = useState<string | null>(null);

  if (!isOpen) return null;

  const reset = () => {
    setHost(""); setPort(22); setUsername("root");
    setAuthMode("password"); setPassword(""); setPrivateKey(""); setPassphrase("");
    setErr(null);
  };

  const close = () => { reset(); onClose(); };

  const connect = () => {
    setErr(null);
    if (!host.trim()) { setErr("Host is required"); return; }
    if (!username.trim()) { setErr("Username is required"); return; }
    if (!port || port < 1 || port > 65535) { setErr("Port must be 1–65535"); return; }
    if (authMode === "password" && !password) { setErr("Password is required"); return; }
    if (authMode === "key" && !privateKey.trim()) { setErr("Private key body is required"); return; }

    const auth: QuickAuth = {
      host: host.trim(),
      port,
      username: username.trim(),
      password: authMode === "password" ? password : null,
      private_key: authMode === "key" ? privateKey : null,
      passphrase: authMode === "key" && passphrase ? passphrase : null,
    };
    reset();
    onConnect(auth);
  };

  return (
    <div className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm flex items-center justify-center p-3" onClick={close}>
      <div className="w-full max-w-md max-h-[90vh] flex flex-col bg-[#121214] border border-white/10 rounded-xl shadow-2xl" onClick={(e) => e.stopPropagation()}>
        {/* Header */}
        <div className="shrink-0 px-4 py-3 border-b border-white/5 flex items-center justify-between">
          <span className="text-[12px] font-bold uppercase tracking-widest text-white flex items-center gap-2">
            <Zap size={13} className="text-primary" /> Quick Connect
          </span>
          <button onClick={close} className="text-zinc-400 hover:text-white"><X size={16} /></button>
        </div>

        <div className="flex-1 overflow-y-auto custom-scrollbar p-5 space-y-3">
          {err && (
            <div className="px-3 py-2 bg-rose-500/15 border border-rose-500/30 rounded text-rose-200 text-[11.5px] font-mono flex items-center gap-2">
              <AlertTriangle size={12} /> {err}
            </div>
          )}

          {/* Host + port */}
          <div className="grid grid-cols-4 gap-2">
            <div className="col-span-3 space-y-1">
              <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">Host</label>
              <input
                value={host}
                onChange={(e) => setHost(e.target.value)}
                placeholder="192.168.1.1 or example.com"
                className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
                autoFocus
              />
            </div>
            <div className="space-y-1">
              <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">Port</label>
              <input
                type="number"
                value={port}
                onChange={(e) => setPort(parseInt(e.target.value) || 22)}
                className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
              />
            </div>
          </div>

          {/* Username */}
          <div className="space-y-1">
            <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">Username</label>
            <input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              placeholder="root"
              className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
            />
          </div>

          {/* Auth mode toggle */}
          <div className="pt-1">
            <div className="flex items-center gap-1.5 bg-white/[0.03] border border-white/10 rounded-lg p-1">
              <button
                onClick={() => setAuthMode("password")}
                className={`flex-1 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider flex items-center justify-center gap-1.5 transition-colors ${
                  authMode === "password"
                    ? "bg-primary/20 text-primary"
                    : "text-zinc-400 hover:text-zinc-100"
                }`}
              >
                <Lock size={11} /> Password
              </button>
              <button
                onClick={() => setAuthMode("key")}
                className={`flex-1 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider flex items-center justify-center gap-1.5 transition-colors ${
                  authMode === "key"
                    ? "bg-primary/20 text-primary"
                    : "text-zinc-400 hover:text-zinc-100"
                }`}
              >
                <KeyRound size={11} /> SSH Key
              </button>
            </div>
          </div>

          {authMode === "password" ? (
            <div className="space-y-1 animate-in fade-in">
              <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">Password</label>
              <PasswordField
                value={password}
                onChange={setPassword}
                placeholder="••••••"
                onKeyDown={(e: any) => e.key === "Enter" && connect()}
                className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-primary outline-none focus:border-primary/50"
              />
            </div>
          ) : (
            <div className="space-y-2 animate-in fade-in">
              <div className="space-y-1">
                <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">Private Key (PEM)</label>
                <textarea
                  value={privateKey}
                  onChange={(e) => setPrivateKey(e.target.value)}
                  rows={5}
                  placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
                  className="w-full px-3 py-2 bg-black/50 border border-white/10 rounded-lg text-[11.5px] font-mono text-zinc-200 outline-none focus:border-primary/50 resize-y"
                />
              </div>
              <div className="space-y-1">
                <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">Passphrase (if any)</label>
                <PasswordField
                  value={passphrase}
                  onChange={setPassphrase}
                  placeholder="leave empty if unencrypted"
                  className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
                />
              </div>
            </div>
          )}

          <p className="text-[10.5px] text-zinc-400 leading-snug pt-1">
            This host is saved as a node in your root directory for reuse. Edit or remove it anytime from the node grid.
          </p>
        </div>

        {/* Footer */}
        <div className="shrink-0 px-5 py-3 border-t border-white/5 flex items-center gap-2">
          <button
            onClick={connect}
            className="flex-1 h-10 rounded-lg text-[11.5px] font-bold uppercase tracking-wider bg-primary text-black hover:shadow-[0_0_20px_rgba(var(--primary),0.3)] flex items-center justify-center gap-2"
          >
            <Zap size={14} /> Connect
          </button>
          <button onClick={close} className="h-10 px-4 rounded-lg text-[11.5px] font-bold uppercase tracking-wider bg-white/5 text-zinc-200 border border-white/10 hover:bg-white/10">
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
};

export default QuickConnectModal;
