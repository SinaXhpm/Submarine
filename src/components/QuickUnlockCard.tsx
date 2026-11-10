import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Fingerprint, Unlock } from "lucide-react";
import { useTextPrompt } from "../ui/confirm";

// Quick unlock (#166, quick_unlock.rs): this profile opens with Windows Hello
// or the fingerprint on this device. Off until turned on here, which takes
// the profile's password once. Shown only where the platform offers it.
// Windows also offers opening with no prompt at all (#275): a choice of its
// own, off by default, in place of Windows Hello when it's on.
const AUTO = "windows-auto";
const AUTO_TITLE = "Open automatically on this Windows account";

export default function QuickUnlockCard({ profile, card, label }: { profile: string; card: string; label: string }) {
  const textPrompt = useTextPrompt();
  const [support, setSupport] = useState<{ method: string | null; ready: boolean; auto: boolean } | null>(null);
  const [on, setOn] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<{ text: string; tone: "ok" | "err" } | null>(null);

  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const s = await invoke<{ method: string | null; ready: boolean; auto: boolean }>("quick_unlock_support");
        if (!live) return;
        setSupport(s);
        if (s.method) setOn((await invoke<{ method: string | null }>("quick_unlock_status", { name: profile })).method);
      } catch { /* not offered */ }
    })();
    return () => { live = false; };
  }, [profile]);

  if (!support?.method) return null;
  const method = support.method;
  const hello = method === "windows-hello";
  const what = hello ? "Windows Hello" : "fingerprint";

  const turnOn = async (auto: boolean) => {
    const password = await textPrompt({
      title: auto ? AUTO_TITLE : `Unlock with ${what}`,
      message: auto
        ? `Enter this profile's password once. Then it opens by itself when Submarine starts, with no password, PIN, face or fingerprint. Anyone or any program using this Windows account can open it, so use this only on a computer that's yours alone. Its key stays encrypted for this account. After Lock it takes the password again until Submarine restarts.${on === method ? " This replaces Windows Hello for this profile." : ""}`
        : hello
          ? `Enter this profile's password once. Then it opens on this computer with your Windows Hello PIN, face or finger, and Windows asks you to confirm now.${on === AUTO ? " This replaces opening automatically." : ""}`
          : "Enter this profile's password once. Then it opens on this phone with a fingerprint, and the phone asks for one now.",
      placeholder: "Profile password",
      password: true,
      keepWhitespace: true,
      okLabel: auto ? "Turn on" : "Continue",
    });
    if (!password) return;
    setBusy(true); setMsg(null);
    try {
      await invoke("quick_unlock_enable", { password, auto });
      setOn(auto ? AUTO : method);
      setMsg({ text: auto ? "This profile now opens by itself when Submarine starts on this Windows account." : `This profile now opens with ${what} on this device.`, tone: "ok" });
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
      setOn(null);
    } catch (e) {
      setMsg({ text: String(e), tone: "err" });
    } finally {
      setBusy(false);
    }
  };

  const toggle = (title: string, checked: boolean, disabled: boolean, onClick: () => void) => (
    <button
      role="switch"
      aria-checked={checked}
      aria-label={title}
      disabled={disabled}
      onClick={onClick}
      className={`relative shrink-0 w-11 h-6 rounded-full transition-colors disabled:opacity-40 ${checked ? "bg-primary" : "bg-zinc-700"}`}
    >
      <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${checked ? "left-[22px]" : "left-0.5"}`} />
    </button>
  );

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
        {toggle(`Unlock with ${what}`, on === method, busy || (on !== method && !support.ready), () => void (on === method ? turnOff() : turnOn(false)))}
      </div>
      {support.auto && (
        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="flex items-center gap-1.5 text-[13px] font-semibold text-zinc-100">
              <Unlock size={14} className="text-amber-300 shrink-0" /> {AUTO_TITLE}
            </div>
            <p className="text-[11.5px] text-zinc-500 leading-relaxed mt-1">
              Less safe: no password or Windows Hello when Submarine starts. Anyone or any program using this Windows account can open this profile. After Lock it takes the password until Submarine restarts.
            </p>
          </div>
          {toggle(AUTO_TITLE, on === AUTO, busy, () => void (on === AUTO ? turnOff() : turnOn(true)))}
        </div>
      )}
      {msg && <p role="status" className={`text-[11.5px] ${msg.tone === "err" ? "text-rose-300" : "text-emerald-300"}`}>{msg.text}</p>}
    </section>
  );
}
