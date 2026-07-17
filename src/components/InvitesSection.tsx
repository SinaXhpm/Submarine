import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Users, Check, Loader2, Download, AlertTriangle, X } from "lucide-react";
import { useTextPrompt } from "../ui/confirm";
import { RoleBadge } from "./shareRoles";

// Profiles OTHER people shared with you, shown in the Cloud panel (which the
// profile picker owns). This is the only half of sharing that can't live inside
// a profile: taking one of these CREATES a local profile, so it belongs where
// profiles are born. Everything about a profile you already have — who can see
// it, their roles, revoking — is in that profile's own Profile panel.

interface ShareInfo { share_id: string; name: string; role: string; status: string; owner_email: string; }

type Props = { onLocalProfilesChanged: () => void };

export default function InvitesSection({ onLocalProfilesChanged }: Props) {
  const textPrompt = useTextPrompt();
  const [shares, setShares] = useState<ShareInfo[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);

  const clean = (e: unknown) => String(e).replace(/^\[[A-Z_]+\]\s*/, "");
  const fail = (e: unknown) => { setErr(clean(e)); setOk(null); };
  const flash = (m: string) => { setOk(m); setErr(null); setTimeout(() => setOk(null), 5000); };

  const load = useCallback(async () => {
    try {
      const all = await invoke<ShareInfo[]>("list_shares");
      setShares(all.filter((s) => s.role !== "owner"));
    } catch {
      // Identity not set up yet (or offline) — nothing to show, not an error.
      setShares([]);
    }
  }, []);

  useEffect(() => { load(); }, [load]);

  // Same on-demand rule as the Profile panel: ask for the sharing passphrase at
  // the moment it's needed, never as an upfront setup step.
  const ensureIdentity = useCallback(async (): Promise<boolean> => {
    const s = await invoke<{ unlocked: boolean; exists_on_server: boolean }>("identity_status");
    if (s.unlocked) return true;
    const pass = await textPrompt({
      title: "Unlock sharing",
      message:
        "Sharing has its own passphrase — it protects the key these profiles are locked to. " +
        "Enter yours, or pick one now if this is your first time. It never leaves this device.",
      placeholder: "Sharing passphrase (min 8)",
      password: true,
      okLabel: "Continue",
      validate: (v) => (v.length < 8 ? "At least 8 characters" : null),
    });
    if (!pass) return false;
    await invoke("setup_identity", { encPassphrase: pass });
    await load();
    return true;
  }, [textPrompt, load]);

  if (shares.length === 0 && !err && !ok) return null;

  return (
    <div className="mt-4 pt-4 border-t border-white/5 space-y-2">
      <div className="flex items-center gap-2 text-zinc-300">
        <Users size={14} className="text-indigo-300" />
        <span className="text-[12.5px] font-bold uppercase tracking-wider">Shared with you</span>
      </div>

      {err && (
        <div className="px-3 py-2 rounded bg-rose-500/10 border border-rose-500/30 text-rose-200 text-[11.5px] flex items-start gap-2">
          <AlertTriangle size={12} className="mt-0.5 shrink-0" />
          <span className="flex-1 break-words">{err}</span>
          <button onClick={() => setErr(null)} className="text-rose-300/70 hover:text-white"><X size={11} /></button>
        </div>
      )}
      {ok && !err && (
        <div className="px-3 py-2 rounded bg-emerald-500/10 border border-emerald-500/30 text-emerald-200 text-[11.5px]">{ok}</div>
      )}

      {shares.map((s) => (
        <InviteRow
          key={s.share_id}
          share={s}
          ensureIdentity={ensureIdentity}
          onDone={() => { load(); onLocalProfilesChanged(); }}
          onError={fail}
          onOk={flash}
        />
      ))}
    </div>
  );
}

// One invitation. "Accept" (still pending) and "Set up here" (already accepted
// on another device) are the same operation — take the shared profile and give
// it a home on THIS machine, under a local password of your own choosing.
function InviteRow({ share, ensureIdentity, onDone, onError, onOk }: {
  share: ShareInfo;
  ensureIdentity: () => Promise<boolean>;
  onDone: () => void;
  onError: (e: unknown) => void;
  onOk: (m: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(share.name);
  const [pw, setPw] = useState("");
  const [busy, setBusy] = useState(false);

  const pending = share.status === "pending";

  const take = async () => {
    if (pw.length < 8) { onError("Local password must be at least 8 characters."); return; }
    setBusy(true);
    try {
      if (!(await ensureIdentity())) return;
      if (pending) await invoke("accept_share", { shareId: share.share_id });
      const rep = await invoke<{ pushed: number; pulled: number }>("import_shared_profile", {
        shareId: share.share_id, localName: name.trim(), vaultPassword: pw,
      });
      onOk(`"${name.trim()}" is ready — pulled ${rep.pulled} item(s). Open it from the profile list.`);
      setOpen(false); setPw("");
      onDone();
    } catch (e) { onError(e); } finally { setBusy(false); }
  };

  return (
    <div className="bg-zinc-900/40 border border-white/5 rounded-lg p-2.5">
      <div className="flex items-center gap-2">
        <Users size={13} className="text-indigo-300 shrink-0" />
        <div className="min-w-0 flex-1">
          <div className="text-[12.5px] text-zinc-100 font-semibold truncate">{share.name}</div>
          <div className="text-[10.5px] text-zinc-500 truncate">from {share.owner_email}</div>
        </div>
        <RoleBadge role={share.role} />
        <button
          onClick={() => setOpen((o) => !o)}
          className="h-7 px-2.5 rounded-lg bg-primary text-black text-[11px] font-bold flex items-center gap-1"
        >
          {pending ? <><Check size={12} /> Accept</> : <><Download size={12} /> Set up here</>}
        </button>
      </div>
      {open && (
        <div className="mt-2 pt-2 border-t border-white/5 space-y-2">
          <p className="text-[10.5px] text-zinc-500 leading-relaxed">
            This makes a copy on this device. The password below is yours alone — it locks the copy on
            this machine and has nothing to do with {share.owner_email}'s password.
          </p>
          <input
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Name it on this device"
            className="w-full h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50"
          />
          <input
            type="password"
            value={pw}
            onChange={(e) => setPw(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && take()}
            placeholder="Password for this copy (min 8)"
            className="w-full h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
          />
          <div className="flex gap-2">
            <button
              onClick={take}
              disabled={busy}
              className="h-8 px-3 rounded-lg bg-primary text-black text-[11.5px] font-bold disabled:opacity-40 flex items-center gap-1.5"
            >
              {busy ? <Loader2 size={12} className="animate-spin" /> : <Check size={12} />} Set up
            </button>
            <button
              onClick={() => setOpen(false)}
              className="h-8 px-3 rounded-lg bg-white/5 border border-white/10 text-zinc-300 text-[11.5px] font-bold"
            >
              Cancel
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
