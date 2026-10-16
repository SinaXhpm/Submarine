import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Fingerprint } from "lucide-react";
import { useTextPrompt } from "../ui/confirm";

// Quick unlock (#166, quick_unlock.rs): this profile opens with Windows Hello
// or the fingerprint on this device. Off until turned on here, which takes
// the profile's password once. Shown only where the platform offers it.
export default function QuickUnlockCard({ profile, card, label }: { profile: string; card: string; label: string }) {
  const textPrompt = useTextPrompt();
  const [support, setSupport] = useState<{ method: string | null; ready: boolean } | null>(null);
  const [on, setOn] = useState(false);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ text: string; tone: "ok" | "err" } | null>(null);

  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const s = await invoke<{ method: string | null; ready: boolean }>("quick_unlock_support");
        if (!live) return;
        setSupport(s);
        if (s.method) setOn(await invoke<boolean>("quick_unlock_status", { name: profile }));
      } catch { /* not offered */ }
    })();
    return () => { live = false; };
  }, [profile]);

  if (!support?.method) return null;
  const hello = support.method === "windows-hello";
  const what = hello ? "Windows Hello" : "fingerprint";

  const turnOn = async () => {
    const password = await textPrompt({
      title: `Unlock with ${what}`,
      message: hello
        ? "Enter this profile's password once. Then it opens on this computer with your Windows Hello PIN, face or finger, and Windows asks you to confirm now."
        : "Enter this profile's password once. Then it opens on this phone with a fingerprint, and the phone asks for one now.",
      placeholder: "Profile password",
      password: true,
      keepWhitespace: true,
      okLabel: "Continue",
    });
    if (!password) return;
    setBusy(true); setMsg(null);
    try {
      await invoke("quick_unlock_enable", { password });
      setOn(true);
      setMsg({ text: `This profile now opens with ${what} on this device.`, tone: "ok" });
    } catch (e) {
      const raw = String(e);
      if (raw.includes("WRONG_PASSWORD")) setMsg({ text: "That isn't this profile's password.", tone: "err" });
      else if (!raw.includes("CANCELLED")) {
        // "[QUICK_UNLOCK] UNAVAILABLE: <why>" -> "<why>".
        const why = raw.replace(/^.*?\[QUICK_UNLOCK\]\s*[A-Z_]+:?\s*/, "").trim();
        setMsg({ text: `Couldn't turn it on${why ? `: ${why}` : `. ${what} isn't available on this device right now.`}`, tone: "err" });
      }
    } finally {
      setBusy(false);
    }
  };
  const turnOff = async () => {
    setBusy(true); setMsg(null);
    try {
      await invoke("quick_unlock_disable", { name: profile });
      setOn(false);
    } catch (e) {
      setMsg({ text: String(e), tone: "err" });
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className={card}>
      <div className={label}>On this device</div>
      <div className="flex items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-1.5 text-[13px] font-semibold text-zinc-100">
            <Fingerprint size={14} className="text-primary shrink-0" /> Unlock with {what}
          </div>
          <p className="text-[11.5px] text-zinc-500 leading-relaxed mt-1">
            {support.ready
              ? `Open this profile without typing its password, on this ${hello ? "computer" : "phone"} only. The password still works, and is still needed on other devices.`
              : hello
                ? "Set up Windows Hello first (a PIN, face or fingerprint, in Windows Settings → Accounts → Sign-in options)."
                : "Add a fingerprint in the phone's settings first."}
          </p>
        </div>
        <button
          role="switch"
          aria-checked={on}
          aria-label={`Unlock with ${what}`}
          disabled={busy || (!on && !support.ready)}
          onClick={() => void (on ? turnOff() : turnOn())}
          className={`relative shrink-0 w-11 h-6 rounded-full transition-colors disabled:opacity-40 ${on ? "bg-primary" : "bg-zinc-700"}`}
        >
          <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${on ? "left-[22px]" : "left-0.5"}`} />
        </button>
      </div>
      {msg && <p role="status" className={`text-[11.5px] ${msg.tone === "err" ? "text-rose-300" : "text-emerald-300"}`}>{msg.text}</p>}
    </section>
  );
}
