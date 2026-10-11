import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Server, AlertTriangle, ChevronRight } from "lucide-react";
import { useConfirm } from "../ui/confirm";

// Self-hosting: which server cloud sync uses, and an optional proxy. Shown at
// the end of the Cloud panel, one line until "Change". The backend checks and
// saves the settings (`cloud_config.rs`). Another server signs this device
// out, since a sign-in only counts on the server that issued it, and every
// profile stays with the server it syncs with.

export type ProxyMode = "system" | "direct" | "manual";
export interface ProxyConfig { mode: ProxyMode; url: string; }
export interface CloudConfigView {
  /** The server in use. */
  server: string;
  default_server: string;
  is_custom: boolean;
  proxy: ProxyConfig;
  /** Why the saved settings couldn't be used, when they couldn't. */
  load_error: string | null;
}

// "https://sync.example.com:8443" → "sync.example.com:8443".
export const hostOf = (url: string) => {
  try { return new URL(url).host || url; } catch { return url; }
};

interface Props {
  view: CloudConfigView;
  signedIn: boolean;
  onChanged: (view: CloudConfigView, serverChanged: boolean) => void;
}

const PROXY_MODES: { mode: ProxyMode; label: string }[] = [
  { mode: "system", label: "System" },
  { mode: "direct", label: "None" },
  { mode: "manual", label: "Manual" },
];

const SyncServerSetting = ({ view, signedIn, onChanged }: Props) => {
  const confirm = useConfirm();
  const [editing, setEditing] = useState(false);
  const [server, setServer] = useState("");
  const [proxy, setProxy] = useState<ProxyConfig>(view.proxy);
  const [proxyOpen, setProxyOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const startEdit = () => {
    setServer(view.is_custom ? view.server : "");
    setProxy(view.proxy);
    setProxyOpen(view.proxy.mode !== "system");
    setError(null);
    setEditing(true);
  };

  // A self-hosted server has to answer before it is saved, and then the user
  // confirms where their password will go. Back to the default only asks when
  // it signs this device out.
  const save = async (input: string) => {
    setError(null);
    const custom = input.trim();
    const changing = custom
      ? custom.replace(/\/+$/, "").toLowerCase() !== view.server.toLowerCase()
      : view.is_custom;
    if (custom) {
      setBusy(true);
      try {
        await invoke<number>("cloud_test_connection", { server: custom, proxy });
      } catch (e) {
        setError(`${hostOf(custom)} didn't answer: ${String(e)}`);
        setBusy(false);
        return;
      }
      setBusy(false);
    }
    const signOut = changing && signedIn ? "\n\nThis signs you out on this device. Your profiles keep syncing with the server they sync with now." : "";
    if (changing && custom) {
      if (!(await confirm({
        title: `Use ${hostOf(custom)}?`,
        message: `Your email and password go to ${hostOf(custom)} when you sign in, and your profiles' encrypted data when they sync. Only use a server you trust.${signOut}`,
        okLabel: "Use this server",
      }))) return;
    } else if (signOut && !(await confirm({
      title: "Use the default server?",
      message: signOut.trim(),
      okLabel: "Use the default",
    }))) return;
    setBusy(true);
    try {
      const saved = await invoke<CloudConfigView>("cloud_set_config", { server: custom, proxy });
      setEditing(false);
      onChanged(saved, saved.server !== view.server);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const input =
    "w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] font-mono text-zinc-50 placeholder:text-zinc-600 outline-none focus:border-primary/50 disabled:opacity-50";
  const proxyLabel = proxy.mode === "manual" ? (proxy.url ? hostOf(proxy.url) : "Manual") : PROXY_MODES.find((m) => m.mode === proxy.mode)!.label;

  return (
    <div className="mt-2 pt-3 border-t border-white/5 text-[11.5px] text-zinc-500 space-y-2">
      {view.load_error && (
        <p className="text-amber-300/90 flex items-start gap-1.5">
          <AlertTriangle size={12} className="mt-0.5 shrink-0" />
          <span className="break-words">{view.load_error}</span>
        </p>
      )}
      {!editing ? (
        <div className="flex items-center gap-2 min-w-0">
          <Server size={12} className="shrink-0" />
          <span className="shrink-0">Sync server:</span>
          <span className={`font-mono truncate ${view.is_custom ? "text-primary" : "text-zinc-400"}`} title={view.server}>
            {hostOf(view.server)}
          </span>
          {view.is_custom && <span className="text-zinc-600 shrink-0">(custom)</span>}
          <button onClick={startEdit} className="ml-auto shrink-0 hover:text-primary">
            Change
          </button>
        </div>
      ) : (
        <div className="space-y-2">
          <input
            value={server}
            onChange={(e) => setServer(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && !busy && save(server)}
            placeholder={`${hostOf(view.default_server)} (default)`}
            disabled={busy}
            className={input}
            aria-label="Sync server URL"
            autoFocus
          />
          <p className="text-zinc-600">Empty for the default. Use https:// (http:// only for localhost).</p>

          <button
            type="button"
            onClick={() => setProxyOpen(!proxyOpen)}
            aria-expanded={proxyOpen}
            className="flex items-center gap-1 hover:text-zinc-300"
          >
            <ChevronRight size={12} className={`transition-transform ${proxyOpen ? "rotate-90" : ""}`} />
            Proxy: {proxyLabel}
          </button>
          {proxyOpen && (
            <div className="pl-4 space-y-2">
              <div role="radiogroup" aria-label="Proxy" className="grid grid-cols-3 gap-1">
                {PROXY_MODES.map((m) => (
                  <button
                    key={m.mode}
                    type="button"
                    role="radio"
                    aria-checked={proxy.mode === m.mode}
                    disabled={busy}
                    onClick={() => setProxy({ ...proxy, mode: m.mode })}
                    className={`h-7 rounded border transition-colors disabled:opacity-50 ${
                      proxy.mode === m.mode
                        ? "bg-primary/15 border-primary/40 text-zinc-100"
                        : "bg-white/[0.04] border-white/10 text-zinc-400 hover:bg-white/[0.08]"
                    }`}
                  >
                    {m.label}
                  </button>
                ))}
              </div>
              {proxy.mode === "manual" && (
                <input
                  value={proxy.url}
                  onChange={(e) => setProxy({ ...proxy, url: e.target.value })}
                  placeholder="socks5h://127.0.0.1:1080 or http://user:pw@proxy:3128"
                  disabled={busy}
                  className={input}
                  aria-label="Proxy address"
                />
              )}
            </div>
          )}

          {error && <p className="text-rose-300 break-words">{error}</p>}
          <div className="flex items-center gap-2 justify-end">
            {view.is_custom && (
              <button onClick={() => save("")} disabled={busy} className="mr-auto hover:text-primary disabled:opacity-50">
                Reset to default
              </button>
            )}
            <button
              onClick={() => { setEditing(false); setError(null); }}
              disabled={busy}
              className="h-7 px-3 rounded bg-white/5 border border-white/10 text-zinc-300 hover:bg-white/10 disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              onClick={() => save(server)}
              disabled={busy}
              className="h-7 px-3 rounded bg-primary text-black font-bold disabled:opacity-50"
            >
              {busy ? "Checking…" : "Save"}
            </button>
          </div>
        </div>
      )}
    </div>
  );
};

export default SyncServerSetting;
