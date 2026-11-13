import { useState } from "react";
import { Zap, X, AlertTriangle, KeyRound, Lock, ShieldAlert } from "lucide-react";
import PasswordField from "./PasswordField";
import { useT } from "../i18n";
import { USUAL_PORTS, isFtp, parseServerLink, type FtpProtocol, type ServerProtocol } from "../util/serverProtocol";

// Quick Connect: the auth bundle lives in component state, gets passed to
// DesktopApp via onConnect, and the resulting session carries it through to
// `initiate_connection`'s `quick_auth` param on the backend (so the live
// connection uses the inline auth directly). DesktopApp ALSO persists the
// target as a reusable node in the root directory (see save_quick_connect_node)
// so it can be reconnected later without re-typing credentials.
//
// An FTP server (Type, or a pasted ftp:// / ftps:// / ftpes:// link) is saved
// first and then opened like any saved one: its tab, certificate check and
// password prompt are the saved server's (DesktopApp's openQuickConnect).

export interface QuickAuth {
  host: string;
  port: number;
  username: string;
  password?: string | null;
  private_key?: string | null;
  passphrase?: string | null;
  /** SSH when absent; an FTP kind for an FTP server (util/serverProtocol). */
  protocol?: "ssh" | FtpProtocol;
}

interface Props {
  isOpen: boolean;
  onClose: () => void;
  onConnect: (auth: QuickAuth) => void;
}

const QuickConnectModal = ({ isOpen, onClose, onConnect }: Props) => {
  const t = useT();
  const [protocol, setProtocolState] = useState<"ssh" | FtpProtocol>("ssh");
  const [host, setHost] = useState("");
  const [port, setPort] = useState<number>(22);
  const [username, setUsername] = useState("root");
  // Typed by the user: a change of kind leaves them as they are.
  const [portTyped, setPortTyped] = useState(false);
  const [userTyped, setUserTyped] = useState(false);
  const [authMode, setAuthMode] = useState<"password" | "key">("password");
  const [password, setPassword] = useState("");
  const [privateKey, setPrivateKey] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [err, setErr] = useState<string | null>(null);

  if (!isOpen) return null;

  const ftp = isFtp(protocol);
  // The usual port and login of the new kind, unless the user typed their own:
  // root for SSH (empty asks), empty for FTP (anonymous).
  const setProtocol = (next: "ssh" | FtpProtocol) => {
    setProtocolState(next);
    if (!portTyped) setPort(USUAL_PORTS[next as ServerProtocol]);
    if (!userTyped) setUsername(isFtp(next) ? "" : "root");
    if (isFtp(next)) setAuthMode("password");
  };
  // A pasted link fills the form: its kind, host, port and login.
  const onHostChange = (value: string) => {
    const link = value.includes("://") ? parseServerLink(value) : null;
    if (!link || (link.protocol !== "ssh" && !isFtp(link.protocol))) {
      setHost(value);
      return;
    }
    const kind = link.protocol as "ssh" | FtpProtocol;
    setProtocolState(kind);
    setHost(link.host);
    setPort(link.port);
    setPortTyped(true);
    if (link.username !== undefined) {
      setUsername(link.username);
      setUserTyped(true);
    } else if (!userTyped) {
      setUsername(isFtp(kind) ? "" : "root");
    }
    if (link.password !== undefined) {
      setAuthMode("password");
      setPassword(link.password);
    }
    if (isFtp(kind)) setAuthMode("password");
    setErr(null);
  };

  const reset = () => {
    setProtocolState("ssh"); setPortTyped(false); setUserTyped(false);
    setHost(""); setPort(22); setUsername("root");
    setAuthMode("password"); setPassword(""); setPrivateKey(""); setPassphrase("");
    setErr(null);
  };

  const close = () => { reset(); onClose(); };

  const connect = () => {
    setErr(null);
    if (!host.trim()) { setErr(t("Host is required")); return; }
    if (host.includes("://")) { setErr(t("Only ftp://, ftps://, ftpes://, sftp:// and ssh:// links work here")); return; }
    if (!port || port < 1 || port > 65535) { setErr(t("Port must be 1–65535")); return; }
    if (authMode === "key" && !privateKey.trim()) { setErr(t("Private key body is required")); return; }

    // A blank username (issue #54) and an empty password (issue #30) are both
    // asked for when connecting; an empty "Login as" answer means root.
    const auth: QuickAuth = {
      host: host.trim(),
      port,
      username: username.trim(),
      password: authMode === "password" && password ? password : null,
      private_key: authMode === "key" ? privateKey : null,
      passphrase: authMode === "key" && passphrase ? passphrase : null,
      protocol,
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
            <Zap size={13} className="text-primary" /> {t("Quick Connect")}
          </span>
          <button aria-label={t("Close")} onClick={close} className="text-zinc-400 hover:text-white"><X size={16} /></button>
        </div>

        <div className="flex-1 overflow-y-auto custom-scrollbar p-5 space-y-3">
          {err && (
            <div className="px-3 py-2 bg-rose-500/15 border border-rose-500/30 rounded text-rose-200 text-[11.5px] font-mono flex items-center gap-2">
              <AlertTriangle size={12} /> {err}
            </div>
          )}

          {/* SSH or FTP; an FTP server's encryption, as in the server form. */}
          <div className="grid grid-cols-2 gap-2">
            <div className="space-y-1">
              <label htmlFor="quick-type" className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Type")}</label>
              <select
                id="quick-type"
                value={ftp ? "ftps" : "ssh"}
                onChange={(e) => setProtocol(e.target.value === "ftps" ? "ftps" : "ssh")}
                className="w-full h-9 px-2 bg-zinc-900/60 border border-white/10 rounded-lg text-[12px] text-zinc-50 outline-none focus:border-primary/50"
              >
                <option value="ssh" className="bg-[#1a1a1e]">SSH</option>
                <option value="ftps" className="bg-[#1a1a1e]">{t("FTP or FTPS (files)")}</option>
              </select>
            </div>
            {ftp && (
              <div className="space-y-1">
                <label htmlFor="quick-ftp-security" className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Encryption")}</label>
                <select
                  id="quick-ftp-security"
                  value={protocol}
                  onChange={(e) => setProtocol(e.target.value as FtpProtocol)}
                  className="w-full h-9 px-2 bg-zinc-900/60 border border-white/10 rounded-lg text-[12px] text-zinc-50 outline-none focus:border-primary/50"
                >
                  <option value="ftps" className="bg-[#1a1a1e]">{t("FTPS: TLS after connecting (usual)")}</option>
                  <option value="ftps-implicit" className="bg-[#1a1a1e]">{t("FTPS: TLS from the start (port 990)")}</option>
                  <option value="ftp" className="bg-[#1a1a1e]">{t("None: plain FTP (not encrypted)")}</option>
                </select>
              </div>
            )}
          </div>
          {protocol === "ftp" && (
            <p role="note" className="flex gap-2 px-3 py-2 rounded-lg border border-amber-500/25 bg-amber-500/10 text-[11px] leading-snug text-amber-200/90">
              <ShieldAlert size={13} className="shrink-0 mt-px" aria-hidden="true" />
              <span>{t("Plain FTP isn't encrypted: the password and the files cross the network as they are.")}</span>
            </p>
          )}

          {/* Host + port */}
          <div className="grid grid-cols-4 gap-2">
            <div className="col-span-3 space-y-1">
              <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Host")}</label>
              <input
                value={host}
                onChange={(e) => onHostChange(e.target.value)}
                placeholder={t("192.168.1.1 or example.com")}
                aria-describedby="quick-host-hint"
                className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
                autoFocus
              />
            </div>
            <div className="space-y-1">
              <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Port")}</label>
              <input
                type="number"
                value={port}
                onChange={(e) => { setPort(parseInt(e.target.value) || USUAL_PORTS[protocol as ServerProtocol]); setPortTyped(true); }}
                className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
              />
            </div>
          </div>
          <p id="quick-host-hint" className="text-[10px] text-zinc-500 ml-0.5 -mt-1">{t("A link works too, such as ftp://user@host or sftp://user@host:22")}</p>

          {/* Username */}
          <div className="space-y-1">
            <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Username")}</label>
            <input
              value={username}
              onChange={(e) => { setUsername(e.target.value); setUserTyped(true); }}
              placeholder={ftp ? t("Empty: anonymous") : t("Ask when connecting")}
              className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
            />
          </div>

          {/* Auth mode toggle (SSH only: an FTP server takes a password) */}
          {!ftp && <div className="pt-1">
            <div className="flex items-center gap-1.5 bg-white/[0.03] border border-white/10 rounded-lg p-1">
              <button
                onClick={() => setAuthMode("password")}
                className={`flex-1 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider flex items-center justify-center gap-1.5 transition-colors ${
                  authMode === "password"
                    ? "bg-primary/20 text-primary"
                    : "text-zinc-400 hover:text-zinc-100"
                }`}
              >
                <Lock size={11} /> {t("Password")}
              </button>
              <button
                onClick={() => setAuthMode("key")}
                className={`flex-1 h-7 rounded text-[10.5px] font-bold uppercase tracking-wider flex items-center justify-center gap-1.5 transition-colors ${
                  authMode === "key"
                    ? "bg-primary/20 text-primary"
                    : "text-zinc-400 hover:text-zinc-100"
                }`}
              >
                <KeyRound size={11} /> {t("SSH Key")}
              </button>
            </div>
          </div>}

          {authMode === "password" ? (
            <div className="space-y-1 animate-in fade-in">
              <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Password")}</label>
              <PasswordField
                value={password}
                onChange={setPassword}
                placeholder="••••••"
                onKeyDown={(e: any) => e.key === "Enter" && connect()}
                className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-primary outline-none focus:border-primary/50"
              />
              <p className="text-[10px] text-zinc-500 ml-0.5">{t("Leave empty to be asked when connecting.")}</p>
            </div>
          ) : (
            <div className="space-y-2 animate-in fade-in">
              <div className="space-y-1">
                <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Private Key (PEM)")}</label>
                <textarea
                  value={privateKey}
                  onChange={(e) => setPrivateKey(e.target.value)}
                  rows={5}
                  placeholder="-----BEGIN OPENSSH PRIVATE KEY-----"
                  className="w-full px-3 py-2 bg-black/50 border border-white/10 rounded-lg text-[11.5px] font-mono text-zinc-200 outline-none focus:border-primary/50 resize-y"
                />
              </div>
              <div className="space-y-1">
                <label className="text-[10px] font-bold text-zinc-300 uppercase tracking-wider ml-0.5">{t("Passphrase (if any)")}</label>
                <PasswordField
                  value={passphrase}
                  onChange={setPassphrase}
                  placeholder={t("leave empty if unencrypted")}
                  className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] text-zinc-50 outline-none focus:border-primary/50"
                />
                <p className="text-[10px] text-zinc-500 ml-0.5">{t("For an encrypted key, leave empty to be asked when connecting.")}</p>
              </div>
            </div>
          )}

          <p className="text-[10.5px] text-zinc-400 leading-snug pt-1">
            {t("This host is saved as a node in your root directory for reuse. Edit or remove it anytime from the node grid.")}
          </p>
        </div>

        {/* Footer */}
        <div className="shrink-0 px-5 py-3 border-t border-white/5 flex items-center gap-2">
          <button
            onClick={connect}
            className="flex-1 h-10 rounded-lg text-[11.5px] font-bold uppercase tracking-wider bg-primary text-black hover:shadow-[0_0_20px_rgba(var(--primary),0.3)] flex items-center justify-center gap-2"
          >
            <Zap size={14} /> {t("Connect")}
          </button>
          <button onClick={close} className="h-10 px-4 rounded-lg text-[11.5px] font-bold uppercase tracking-wider bg-white/5 text-zinc-200 border border-white/10 hover:bg-white/10">
            {t("Cancel")}
          </button>
        </div>
      </div>
    </div>
  );
};

export default QuickConnectModal;
