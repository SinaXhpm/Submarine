import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Server, AlertTriangle } from "lucide-react";

// Self-hosting: lets the user point cloud sync at their own server. Kept in its
// own component (mounted once from CloudPanel) so the fork's diff against
// upstream stays small. The backend owns validation + persistence; changing the
// server signs this device out, since a token only works on the server that
// issued it.

interface ServerInfo { url: string; default_url: string; is_custom: boolean; }

interface Props {
  signedIn: boolean;
  onChanged: () => void;
}

const SyncServerSetting = ({ signedIn, onChanged }: Props) => {
  const [server, setServer] = useState<ServerInfo | null>(null);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<ServerInfo>("cloud_get_server").then(setServer).catch((e) => setError(String(e)));
  }, []);

  if (!server) return null;

  const apply = async (url: string) => {
    setError(null);
    setBusy(true);
    try {
      const next = await invoke<ServerInfo>("cloud_set_server", { url });
      setServer(next);
      setEditing(false);
      onChanged();
    } catch (e: any) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const startEdit = () => {
    setDraft(server.is_custom ? server.url : "");
    setError(null);
    setEditing(true);
  };

  return (
    <div className="mt-2 pt-3 border-t border-white/5 text-[11.5px] text-zinc-500">
      {!editing ? (
        <div className="flex items-center gap-2">
          <Server size={12} className="shrink-0" />
          <span>Sync server:</span>
          <span className={`font-mono truncate ${server.is_custom ? "text-primary" : "text-zinc-400"}`}>
            {server.url}
          </span>
          {server.is_custom && <span className="text-zinc-600">(custom)</span>}
          <button onClick={startEdit} className="ml-auto shrink-0 hover:text-primary">
            Change
          </button>
        </div>
      ) : (
        <div className="space-y-2">
          <div className="flex items-center gap-2 text-zinc-400">
            <Server size={12} /> Sync server URL
          </div>
          <input
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && !busy && apply(draft)}
            placeholder={server.default_url}
            className="w-full h-9 px-3 bg-zinc-900/60 border border-white/10 rounded-lg text-[12.5px] font-mono text-zinc-50 placeholder:text-zinc-600 outline-none focus:border-primary/50"
            autoFocus
          />
          <p className="text-zinc-600 leading-relaxed">
            Leave blank to use the default. Must be https:// (http:// is allowed for localhost only).
          </p>
          {signedIn && (
            <p className="text-amber-300/80 flex items-start gap-1.5">
              <AlertTriangle size={12} className="mt-0.5 shrink-0" />
              Changing the server signs you out on this device. Your local profiles are not affected.
            </p>
          )}
          {error && <p className="text-rose-300 break-words">{error}</p>}
          <div className="flex items-center gap-2 justify-end">
            {server.is_custom && (
              <button
                onClick={() => apply("")}
                disabled={busy}
                className="mr-auto hover:text-primary disabled:opacity-50"
              >
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
              onClick={() => apply(draft)}
              disabled={busy}
              className="h-7 px-3 rounded bg-primary text-black font-bold disabled:opacity-50"
            >
              {busy ? "…" : "Save"}
            </button>
          </div>
        </div>
      )}
    </div>
  );
};

export default SyncServerSetting;
