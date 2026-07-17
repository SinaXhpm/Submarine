import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { DownloadCloud, Loader2, ChevronRight, ChevronDown } from "lucide-react";
import { useTextPrompt } from "../ui/confirm";

// Bring one of YOUR OWN personal cloud profiles down to this device. This is the
// new-device path that replaced the whole-vault blob download: the profile's
// data already lives in the per-entity /sync stream, and its key rides along
// sealed to your sharing identity, so all a fresh device needs is your cloud
// account + your sharing passphrase. You type the profile's name (you know it —
// it's yours); there's deliberately no server-side list of your profiles.

type Props = { onLocalProfilesChanged: () => void };

export default function RestoreSection({ onLocalProfilesChanged }: Props) {
  const textPrompt = useTextPrompt();
  const [open, setOpen] = useState(false);
  const [cloudName, setCloudName] = useState("");
  const [localName, setLocalName] = useState("");
  const [pw, setPw] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);

  const clean = (e: unknown) => String(e).replace(/^\[[A-Z_]+\]\s*/, "");

  const ensureIdentity = useCallback(async (): Promise<boolean> => {
    const s = await invoke<{ unlocked: boolean }>("identity_status");
    if (s.unlocked) return true;
    const pass = await textPrompt({
      title: "Unlock sharing",
      message:
        "Your profiles' keys are sealed to your sharing passphrase. Enter it to restore. It never leaves this device.",
      placeholder: "Sharing passphrase (min 8)",
      password: true,
      okLabel: "Continue",
      validate: (v) => (v.length < 8 ? "At least 8 characters" : null),
    });
    if (!pass) return false;
    await invoke("setup_identity", { encPassphrase: pass });
    return true;
  }, [textPrompt]);

  const restore = async () => {
    const cloud = cloudName.trim();
    const local = (localName.trim() || cloud);
    if (!cloud) { setErr("Enter the profile's cloud name."); return; }
    if (pw.length < 8) { setErr("Local password must be at least 8 characters."); return; }
    setBusy(true); setErr(null); setOk(null);
    try {
      if (!(await ensureIdentity())) return;
      const rep = await invoke<{ pushed: number; pulled: number }>("restore_personal_profile", {
        cloudProfile: cloud, localName: local, vaultPassword: pw,
      });
      setOk(`"${local}" restored — pulled ${rep.pulled} item(s). Open it from the profile list.`);
      setCloudName(""); setLocalName(""); setPw(""); setOpen(false);
      onLocalProfilesChanged();
    } catch (e) { setErr(clean(e)); } finally { setBusy(false); }
  };

  return (
    <div className="mt-4 pt-4 border-t border-white/5 space-y-2">
      <button
        onClick={() => setOpen((o) => !o)}
        className="w-full flex items-center gap-2 text-zinc-300 hover:text-zinc-100"
      >
        {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        <DownloadCloud size={14} className="text-primary" />
        <span className="text-[12.5px] font-bold uppercase tracking-wider">Restore a profile here</span>
      </button>

      {open && (
        <div className="bg-zinc-900/40 border border-white/5 rounded-lg p-3 space-y-2">
          <p className="text-[10.5px] text-zinc-500 leading-relaxed">
            Pull one of your own cloud profiles onto this device. Type its name, then set a password to
            protect the copy here (your own choice — unrelated to the cloud password).
          </p>
          {err && <div className="px-2.5 py-1.5 rounded bg-rose-500/10 border border-rose-500/30 text-rose-200 text-[11px]">{err}</div>}
          {ok && <div className="px-2.5 py-1.5 rounded bg-emerald-500/10 border border-emerald-500/30 text-emerald-200 text-[11px]">{ok}</div>}
          <input
            value={cloudName}
            onChange={(e) => setCloudName(e.target.value)}
            placeholder="Profile name in your cloud (e.g. main)"
            className="w-full h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
          />
          <input
            value={localName}
            onChange={(e) => setLocalName(e.target.value)}
            placeholder="Name on this device (blank = same)"
            className="w-full h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
          />
          <input
            type="password"
            value={pw}
            onChange={(e) => setPw(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && restore()}
            placeholder="Password for this copy (min 8)"
            className="w-full h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
          />
          <button
            onClick={restore}
            disabled={busy}
            className="h-8 px-3 rounded-lg bg-primary text-black text-[11.5px] font-bold disabled:opacity-40 flex items-center gap-1.5"
          >
            {busy ? <Loader2 size={12} className="animate-spin" /> : <DownloadCloud size={12} />} Restore
          </button>
        </div>
      )}
    </div>
  );
}
