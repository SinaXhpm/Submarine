// A copy of this profile in a folder the user chooses (#319): the profile's
// encrypted file, replaced after each save, for OneDrive, Google Drive or
// Dropbox to carry to the user's other computers, where Import opens it. The
// setting is this device's (folder_copy.rs); the page shows it on a computer
// only.
import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { FolderSync, Loader2 } from "lucide-react";
import { useT } from "../i18n";

type Status = { folder: string | null; copied_at: number | null; error: string | null };

export default function FolderCopyCard({ profile, card, label }: { profile: string; card: string; label: string }) {
  const t = useT();
  const [st, setSt] = useState<Status | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setSt(await invoke<Status>("folder_copy_status", { name: profile }));
    } catch (e) {
      setErr(String(e));
    }
  }, [profile]);
  useEffect(() => { void refresh(); }, [refresh]);
  // A copy follows each save: the time shown follows it.
  useEffect(() => {
    if (!st?.folder) return;
    const id = window.setInterval(() => void refresh(), 15000);
    return () => window.clearInterval(id);
  }, [st?.folder, refresh]);

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setErr(null);
    try {
      await fn();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };
  const choose = () => run(async () => {
    const chosen = await invoke<Status | null>("folder_copy_choose", { name: profile });
    if (chosen) setSt(chosen);
  });
  const copyNow = () => run(async () => setSt(await invoke<Status>("folder_copy_now", { name: profile })));
  const stop = () => run(async () => {
    await invoke("folder_copy_stop", { name: profile });
    await refresh();
  });

  const when = st?.copied_at
    ? new Date(st.copied_at * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" })
    : null;
  const button = "h-8 px-3 rounded-lg border border-white/10 bg-white/[0.04] text-zinc-200 text-[12px] font-semibold hover:bg-white/[0.08] disabled:opacity-40 flex items-center gap-1.5";

  return (
    <section className={card} data-folder-copy="">
      <div className={label}>{t("A copy in a folder")}</div>
      <p className="text-[11.5px] text-zinc-500 leading-relaxed">
        {t("Keeps a copy of this profile in a folder you choose and replaces it after each change. Put the folder in OneDrive, Google Drive or Dropbox to have the copy on your other computers, and open it there with Import.")}{" "}
        {t("The copy is the file Export makes, still locked with this profile's password. It's a copy, not sync: what changes in it elsewhere doesn't come back here.")}
      </p>
      {st?.folder ? (
        <>
          <div className="flex items-start gap-2 min-w-0">
            <FolderSync size={14} className="text-primary shrink-0 mt-0.5" />
            <div className="min-w-0">
              <div className="text-[12.5px] text-zinc-200 font-mono break-all" data-folder-copy-path="">{st.folder}</div>
              <div className="text-[11.5px] text-zinc-500 mt-0.5" data-folder-copy-when="">
                {when ? t("Last copy: {time}", { time: when }) : t("No copy yet")}
              </div>
            </div>
          </div>
          {st.error && (
            <p role="alert" className="text-[11.5px] text-rose-300 break-words">{t("The last copy failed: {error}", { error: st.error })}</p>
          )}
          <div className="flex flex-wrap gap-2">
            <button type="button" disabled={busy} onClick={copyNow} className={button}>
              {busy && <Loader2 size={12} className="animate-spin" />} {t("Copy now")}
            </button>
            <button type="button" disabled={busy} onClick={choose} className={button}>{t("Change folder…")}</button>
            <button type="button" disabled={busy} onClick={stop} title={t("The copy already in the folder stays.")} className={`${button} hover:text-rose-300`}>
              {t("Stop copying")}
            </button>
          </div>
        </>
      ) : (
        <div>
          <button type="button" disabled={busy || !st} onClick={choose} className={button}>
            <FolderSync size={13} className="text-primary" /> {t("Choose a folder…")}
          </button>
        </div>
      )}
      {err && <p role="alert" className="text-[11.5px] text-rose-300 break-words">{err}</p>}
    </section>
  );
}
