import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Fingerprint, Unlock } from "lucide-react";
import { useTextPrompt } from "../ui/confirm";

type Support = { method: string | null; ready: boolean };
export default function QuickUnlockCard({ profile, card, label }: { profile: string; card: string; label: string }) {
  const textPrompt = useTextPrompt();
  const [support, setSupport] = useState<Support | null>(null);
  const [on, setOn] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ text: string; tone: "ok" | "err" } | null>(null);
  useEffect(() => {
    let live = true;
    setSupport(null); setOn(false); setMsg(null);
    (async () => {
      try {
        const s = await invoke<Support>("quick_unlock_support");
        const enabled = s.method ? await invoke<boolean>("quick_unlock_status", { name: profile }) : false;
        if (live) { setSupport(s); setOn(enabled); }
      } catch { /* Password unlocking remains available. */ }
    })();
    return () => { live = false; };
  }, [profile]);
  if (!support?.method) return null;
  const windows = support.method === "windows-hello";
  const title = windows ? "Open automatically on this Windows account" : "Unlock with fingerprint";
  const turnOn = async () => {
    const password = await textPrompt({
      title,
      message: windows
        ? "Enter this profile's password once to enable automatic opening. Future launches open the last-used profile without a password, PIN, face or fingerprint prompt. Anyone or any program using this Windows account can access it. The vault stays encrypted; its unlock key is protected by Windows for this account. Keep the password for other devices and recovery."
        : "Enter this profile's password once. Then it opens on this phone with a fingerprint, and the phone asks for one now.",
      placeholder: "Profile password", password: true, keepWhitespace: true, okLabel: "Enable",
    });
    if (!password) return;
    setBusy(true); setMsg(null);
    try {
      await invoke("quick_unlock_enable", { password });
      setOn(true);
      setMsg({ text: windows ? "Automatic opening enabled. Close and reopen Submarine to use it." : "Fingerprint unlock enabled.", tone: "ok" });
    } catch (e) {
      const raw = String(e);
      if (raw.includes("WRONG_PASSWORD")) setMsg({ text: "That isn't this profile's password.", tone: "err" });
      else if (!raw.includes("CANCELLED")) setMsg({ text: raw.replace(/^\[QUICK_UNLOCK\]\s*/, ""), tone: "err" });
    } finally { setBusy(false); }
  };
  const turnOff = async () => {
    setBusy(true); setMsg(null);
    try {
      await invoke("quick_unlock_disable", { name: profile });
      setOn(false);
      setMsg({ text: "Disabled. The profile password will be required next time.", tone: "ok" });
    } catch (e) { setMsg({ text: String(e), tone: "err" }); }
    finally { setBusy(false); }
  };
  const Icon = windows ? Unlock : Fingerprint;
  return (
    <section className={card}>
      <div className={label}>On this device</div>
      <div className="flex items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-1.5 text-[13px] font-semibold text-zinc-100">
            <Icon size={14} className="text-primary shrink-0" /> {title}
          </div>
          <p className="text-[11.5px] text-zinc-500 leading-relaxed mt-1">
            {windows
              ? "No password or Windows Hello prompt after enabling. This trusts your signed-in Windows account. The password still works and is needed on other devices. Turn off to forget the saved unlock key."
              : support.ready ? "Open this profile with a fingerprint on this phone only. The password still works."
                : "Add a fingerprint in the phone's settings first."}
          </p>
        </div>
        <button role="switch" aria-checked={on} aria-label={title}
          disabled={busy || (!on && !support.ready)} onClick={() => void (on ? turnOff() : turnOn())}
          className={`relative shrink-0 w-11 h-6 rounded-full transition-colors disabled:opacity-40 ${on ? "bg-primary" : "bg-zinc-700"}`}>
          <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${on ? "left-[22px]" : "left-0.5"}`} />
        </button>
      </div>
      {msg && <p role="status" className={`text-[11.5px] ${msg.tone === "err" ? "text-rose-300" : "text-emerald-300"}`}>{msg.text}</p>}
    </section>
  );
}
