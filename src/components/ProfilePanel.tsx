import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Cloud, CloudOff, RefreshCw, Share2, UserPlus, X, Loader2, Users,
  AlertTriangle, CheckCircle2, ShieldOff, Pencil, Eye, History, HardDrive, KeyRound,
} from "lucide-react";
import { useConfirm, useTextPrompt } from "../ui/confirm";
import QuickUnlockCard from "./QuickUnlockCard";
import { RoleBadge, roleBlurb } from "./shareRoles";
import { hostOf } from "./SyncServerSetting";
import { t, tk, tn, tx, useT } from "../i18n";
import { joinSentences } from "../util/sentences";

interface RecentEdit { name: string; edited_by: string; updated_at: string; }
interface SyncDiff { in_sync: number; needs_push: number; needs_pull: number; out_of_sync_nodes: string[]; }
interface SyncStats {
  total_records: number;
  vault_bytes: number;
  recent_edits: RecentEdit[];
  diff: SyncDiff | null;
}

// The leading 15 digits of an HLC stamp are wall-clock milliseconds.
function hlcAgo(stamp: string): string {
  const ms = parseInt(stamp.slice(0, 15), 10);
  if (!Number.isFinite(ms) || ms <= 0) return "";
  const s = Math.floor((Date.now() - ms) / 1000);
  if (s < 60) return t("just now");
  if (s < 3600) return t("{count}m ago", { count: Math.floor(s / 60) });
  if (s < 86400) return t("{count}h ago", { count: Math.floor(s / 3600) });
  return t("{count}d ago", { count: Math.floor(s / 86400) });
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

// Everything about the OPEN profile, in one screen: its cloud sync, and who has
// access to it.
//
// This exists because sharing used to be split in half — you created a share
// from the sidebar, but could only manage it (invite, roles, revoke) from the
// Cloud panel, which is mounted by the profile PICKER. Managing access to a
// profile therefore meant logging out of the profile you were managing. The one
// piece that legitimately can't live here is an invite someone else sent you:
// accepting it CREATES a local profile, so it belongs where profiles are born.

interface ShareStatus {
  profile: string | null;
  share_id: string | null;
  role: string | null;
  identity_unlocked: boolean;
  signed_in: boolean;
  email: string | null;
  /** The server this profile syncs with; null until its first sync. */
  cloud_server: string | null;
  /** The sync server in use. */
  current_server: string;
  /** Whether that is a self-hosted one, not the default. */
  custom_server: boolean;
  /** Deleted from the cloud on this device: it isn't uploaded until the user says so. */
  cloud_deleted: boolean;
}

interface MemberInfo { user_id: number; email: string; role: string; status: string; }
// A member who wasn't given a share's new key after a removal, and why.
interface LeftOut { email: string; reason: string; }
// What came of replacing a share's key. `first_seen`: members who got the new
// key with no key of theirs on record here to check against.
interface KeyResult { left_out: LeftOut[]; first_seen: string[]; }
const LEFT_OUT_WHY: Record<string, string> = {
  key_changed: tk("their sharing key changed since you invited them"),
  other_account: tk("the server answered for another account"),
  no_key: tk("they have no sharing key on the server"),
  weak_key: tk("their sharing key can't be used"),
  upload_failed: tk("the upload failed"),
};
const leftOutWhy = (reason: string) => { const why = LEFT_OUT_WHY[reason]; return why ? t(why) : reason; };
const leftOutText = (l: LeftOut[]) =>
  t("These people didn't get the new key, so they can't see changes from now on until you invite them again: {people}.", {
    people: l.map((x) => t("{email} ({why})", { email: x.email, why: leftOutWhy(x.reason) })).join(", "),
  });
// The member list comes from the server, so a name it added would get the key
// too. This device can't tell, but the owner can.
const firstSeenText = (emails: string[]) =>
  t("The new key also went to {emails}, with no earlier key on record here to check. If you don't know someone on that list, remove them.", { emails: emails.join(", ") });
const keyResultText = (r: KeyResult) =>
  joinSentences(r.left_out.length > 0 && leftOutText(r.left_out), r.first_seen.length > 0 && firstSeenText(r.first_seen));

type Props = {
  onSync: () => Promise<void> | void;
  syncing: boolean;
  lastSyncLabel: string | null;
  autoSync: boolean;
  syncIntervalMin: number;
  onSetInterval: (m: number) => void;
  onToggleAutoSync: () => void;
};

export default function ProfilePanel({ onSync, syncing, lastSyncLabel, autoSync, syncIntervalMin, onSetInterval, onToggleAutoSync }: Props) {
  const t = useT();
  const confirm = useConfirm();
  const textPrompt = useTextPrompt();

  const [st, setSt] = useState<ShareStatus | null>(null);
  const [members, setMembers] = useState<MemberInfo[] | null>(null);
  // The share key's fingerprint, and a new key's while it waits for an OK.
  const [keys, setKeys] = useState<{ current: string | null; pending: string | null } | null>(null);
  // A removal went through but replacing the key didn't: Replace the key shows.
  const [keyRetry, setKeyRetry] = useState(false);
  const [stats, setStats] = useState<SyncStats | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<"editor" | "user">("editor");
  // Set once a sync completes and items to receive are STILL outstanding. That
  // second observation is what separates "mid-sync, be patient" from "syncing
  // can't fix this" — offering a destructive reset for the former would be
  // handing someone a sledgehammer for a door that was about to open anyway.
  const [stuck, setStuck] = useState(false);
  const prevPull = useRef<number | null>(null);

  const clean = (e: unknown) => String(e).replace(/^\[[A-Z_]+\]\s*/, "");
  const flash = (m: string) => { setOk(m); setErr(null); setTimeout(() => setOk(null), 4000); };
  const fail = (e: unknown) => { setErr(clean(e)); setOk(null); };

  const refresh = useCallback(async () => {
    try {
      const s = await invoke<ShareStatus>("profile_share_status");
      setSt(s);
      if (s.share_id) {
        try { setMembers(await invoke<MemberInfo[]>("share_member_list", { shareId: s.share_id })); }
        catch { setMembers(null); }
      } else {
        setMembers(null);
      }
      try { setKeys(await invoke("share_key_state")); }
      catch { setKeys(null); }
    } catch (e) { fail(e); }
  }, []);

  // Stats are separate from share status because the cloud diff inside them is a
  // network round-trip; we don't want it blocking the panel's first paint.
  const loadStats = useCallback(async () => {
    try {
      const s = await invoke<SyncStats>("profile_sync_stats");
      setStats(s);
      // Same count still waiting after another round trip ⇒ these aren't in
      // flight, they're stuck. One repeat is enough to be sure without making
      // the user click Sync five times before the app admits there's a problem.
      const pull = s.diff?.needs_pull ?? 0;
      if (pull > 0 && prevPull.current === pull) setStuck(true);
      if (pull === 0) setStuck(false);
      prevPull.current = pull;
    }
    catch { /* offline or no profile — panel still shows the rest */ }
  }, []);

  useEffect(() => { refresh(); loadStats(); }, [refresh, loadStats]);

  // Last resort for a partition that can't converge. Destructive to the CLOUD
  // copy only, so the confirm spells out exactly what goes and what stays.
  const forcePush = async () => {
    if (!(await confirm({
      title: t("Replace the cloud copy?"),
      message:
        t("Everything currently in the cloud for this profile is deleted, then this device uploads its own copy in full.\n\nNothing on this device changes. But any item that exists ONLY in the cloud — including the ones this device can't read — is gone for good, and other devices will take this copy as the truth the next time they sync."),
      okLabel: t("Replace cloud copy"),
      destructive: true,
    }))) return;
    setBusy(true); setErr(null);
    try {
      const rep = await invoke<{ pushed: number; pulled: number }>("force_push_profile");
      setStuck(false);
      prevPull.current = null;
      flash(tn(rep.pushed, "Cloud copy replaced — uploaded {count} item.", "Cloud copy replaced — uploaded {count} items."));
      await loadStats();
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  // The sharing identity protects the private key that shared profiles are
  // sealed to. It's deliberately a different secret from the vault password, so
  // rather than making everyone walk through an "identity setup" step they don't
  // understand yet, we ask for it at the exact moment it's first needed. People
  // who never share never see it.
  const ensureIdentity = useCallback(async (): Promise<boolean> => {
    const cur = await invoke<ShareStatus>("profile_share_status");
    setSt(cur);
    if (cur.identity_unlocked) return true;
    const pass = await textPrompt({
      title: t("Unlock sharing"),
      message:
        t("Sharing has its own passphrase — it protects the key that shared profiles are locked to. Enter yours, or pick one now if this is your first share. It never leaves this device."),
      placeholder: t("Sharing passphrase (min 8)"),
      password: true,
      okLabel: t("Continue"),
      validate: (v) => (v.length < 8 ? t("At least 8 characters") : null),
    });
    if (!pass) return false;
    await invoke("setup_identity", { encPassphrase: pass });
    await refresh();
    return true;
  }, [textPrompt, refresh, t]);

  const startSharing = async () => {
    setBusy(true); setErr(null);
    try {
      if (!(await ensureIdentity())) return;
      // The share is named after the profile — one less thing to invent. It's
      // what invitees see when the invitation shows up for them.
      await invoke<{ share_id: string }>("share_current_profile", { name: st?.profile ?? "profile" });
      await refresh();
      flash(t("This profile is shared. Invite people below."));
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const invite = async () => {
    const e = email.trim().toLowerCase();
    if (!e.includes("@")) { fail(t("Enter a valid email address.")); return; }
    setBusy(true); setErr(null);
    try {
      if (!(await ensureIdentity())) return;
      await invoke("invite_to_share", { shareId: st!.share_id, email: e, role });
      setEmail("");
      await refresh();
      flash(role === "user" ? t("Invited {email} as Viewer.", { email: e }) : t("Invited {email} as Editor.", { email: e }));
    } catch (er) { fail(er); } finally { setBusy(false); }
  };

  const changeRole = async (uid: number, next: string) => {
    setBusy(true);
    try {
      await invoke("share_set_role", { shareId: st!.share_id, memberUserId: uid, role: next });
      await refresh();
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const revoke = async (m: MemberInfo) => {
    if (!(await confirm({
      title: t("Remove access?"),
      message: t("{email} loses access to \"{name}\" immediately. Any copy already on their device stays there — this stops them receiving future changes.", { email: m.email, name: st?.profile ?? "" }),
      okLabel: t("Remove"),
      destructive: true,
    }))) return;
    setBusy(true);
    try {
      // Removing someone replaces the key, which needs the sharing identity.
      if (!(await ensureIdentity())) return;
      const r = await invoke<KeyResult>("share_revoke", { shareId: st!.share_id, memberUserId: m.user_id });
      setKeyRetry(false);
      await refresh();
      loadStats(); // the removal synced
      if (keyResultText(r)) fail(joinSentences(t("{email} removed.", { email: m.email }), keyResultText(r)));
      else flash(t("{email} removed.", { email: m.email }));
    } catch (e) {
      // Out of the share, but the key wasn't replaced: offer to finish it.
      if (String(e).includes("REVOKED_BUT_KEY_NOT_ROTATED")) setKeyRetry(true);
      await refresh();
      loadStats();
      fail(e);
    } finally { setBusy(false); }
  };

  const replaceKey = async () => {
    setBusy(true); setErr(null);
    try {
      if (!(await ensureIdentity())) return;
      const r = await invoke<KeyResult>("share_replace_key");
      setKeyRetry(false);
      await refresh();
      loadStats();
      if (keyResultText(r)) fail(joinSentences(t("Key replaced."), keyResultText(r)));
      else flash(t("Key replaced."));
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const stopSharing = async () => {
    const owner = st?.role === "owner";
    if (!(await confirm({
      title: owner ? t("Stop sharing this profile?") : t("Leave this shared profile?"),
      message: owner
        ? t("Everyone you invited loses access to \"{name}\" immediately. Your own copy stays on this device.", { name: st?.profile ?? "" })
        : t("You stop receiving changes to \"{name}\". The copy on this device stays, and becomes yours alone.", { name: st?.profile ?? "" }),
      okLabel: owner ? t("Stop sharing") : t("Leave"),
      destructive: true,
    }))) return;
    setBusy(true);
    try {
      await invoke("stop_sharing");
      await refresh();
      flash(owner ? t("Sharing stopped — this profile is private again.") : t("You left the share."));
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const doSync = async () => {
    await onSync();
    refresh();
    loadStats();
  };

  const shared = !!st?.share_id;
  const isOwner = st?.role === "owner";
  const signedIn = st?.signed_in === true;
  // A profile linked to another server than the one in use doesn't sync: its
  // data and keys only ever go to the server it syncs with.
  const otherServer = st?.cloud_server && st.cloud_server !== st.current_server ? st.cloud_server : null;

  const moveHere = async () => {
    if (!st || !otherServer) return;
    const from = hostOf(otherServer), to = hostOf(st.current_server);
    if (!(await confirm({
      title: t("Sync this profile with {to}?", { to }),
      message:
        t("It syncs with {from} now. From now on it syncs with {to} instead, and the next sync uploads it there in full.\n\nIts copy on {from} isn't changed or deleted.", { from, to }),
      okLabel: t("Sync with {to}", { to }),
    }))) return;
    setBusy(true); setErr(null);
    try {
      await invoke("move_profile_to_current_server");
      await refresh();
      flash(t("This profile now syncs with {to}.", { to }));
      if (signedIn) await doSync();
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  // Deleted from the cloud on this device: the user's word to upload it again.
  const uploadAgain = async () => {
    setBusy(true); setErr(null);
    try {
      await invoke("cloud_resume_profile");
      await refresh();
      await doSync();
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const card = "bg-zinc-900/40 border border-white/5 rounded-xl p-4 space-y-3";
  const label = "text-[11px] font-bold uppercase tracking-wider text-zinc-500";

  return (
    <div className="flex-1 flex flex-col p-4 sm:p-8 space-y-5 animate-in overflow-y-auto custom-scrollbar">
      <header className="flex flex-col sm:flex-row sm:justify-between sm:items-center gap-3 border-b border-zinc-700 pb-5 sm:pb-6 shrink-0">
        <div className="min-w-0">
          <div className="flex items-center gap-2 min-w-0">
            <h2 className="text-[18px] sm:text-[22px] font-bold text-white tracking-tight truncate">
              {st?.profile ?? "—"}
            </h2>
            {shared && <RoleBadge role={st?.role ?? "user"} />}
          </div>
          <p className="hidden sm:block text-[13px] text-zinc-400">
            {shared
              ? isOwner ? t("You share this profile. Manage who gets in, below.") : t("Shared with you by someone else.")
              : t("This profile lives only on your devices.")}
          </p>
        </div>
      </header>

      {err && (
        <div className="px-3 py-2 rounded-lg bg-rose-500/10 border border-rose-500/30 text-rose-200 text-[12px] flex items-start gap-2">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          <span className="flex-1 break-words">{err}</span>
          <button aria-label={t("Dismiss")} onClick={() => setErr(null)} className="text-rose-300/70 hover:text-white"><X size={12} /></button>
        </div>
      )}
      {ok && !err && (
        <div className="px-3 py-2 rounded-lg bg-emerald-500/10 border border-emerald-500/30 text-emerald-200 text-[12px] flex items-center gap-2">
          <CheckCircle2 size={13} /> <span className="flex-1 break-words">{ok}</span>
        </div>
      )}

      {/* ---- Quick unlock on this device (#166) ---- */}
      {st?.profile && <QuickUnlockCard profile={st.profile} card={card} label={label} />}

      {/* ---- Cloud + sync ---- */}
      <section className={card}>
        <div className={label}>{t("Cloud")}</div>
        <div className="flex items-center gap-2 flex-wrap">
          {signedIn ? (
            <>
              <Cloud size={14} className="text-primary shrink-0" />
              <span className="text-[12.5px] text-zinc-200 font-mono truncate">{st?.email}</span>
            </>
          ) : (
            <>
              <CloudOff size={14} className="text-zinc-500 shrink-0" />
              <span className="text-[12.5px] text-zinc-400">
                {t("Not signed in — lock this profile and sign in from the Cloud button on the picker.")}
              </span>
            </>
          )}
        </div>
        {st?.custom_server && !otherServer && (
          <div className="text-[11px] text-zinc-500">
            {tx("Sync server: {host}", { host: <span className="font-mono text-zinc-400">{hostOf(st.current_server)}</span> })}
          </div>
        )}
        {otherServer && st && (
          <div className="px-3 py-2 rounded-lg bg-amber-500/10 border border-amber-500/25 text-amber-100 text-[12px] space-y-2">
            <div className="flex items-start gap-2">
              <AlertTriangle size={13} className="mt-0.5 shrink-0 text-amber-300" />
              <span className="flex-1 break-words">
                {tx("This profile syncs with {other}, not with {current}, so it doesn't sync now.", {
                  other: <span className="font-mono">{hostOf(otherServer)}</span>,
                  current: <span className="font-mono">{hostOf(st.current_server)}</span>,
                })}{" "}
                {shared
                  ? t("A shared profile stays on the server it was shared on: switch the sync server back to sync it.")
                  : t("Switch the sync server back, or move this profile to the server in use.")}
              </span>
            </div>
            {!shared && (
              <button
                onClick={moveHere}
                disabled={busy}
                className="h-8 px-3 rounded-lg bg-amber-500/20 border border-amber-400/30 text-amber-100 text-[12px] font-bold hover:bg-amber-500/30 disabled:opacity-40"
              >
                {t("Sync with {to}", { to: hostOf(st.current_server) })}
              </button>
            )}
          </div>
        )}
        {st?.cloud_deleted && (
          <div className="px-3 py-2 rounded-lg bg-amber-500/10 border border-amber-500/25 text-amber-100 text-[12px] space-y-2">
            <div className="flex items-start gap-2">
              <AlertTriangle size={13} className="mt-0.5 shrink-0 text-amber-300" />
              <span className="flex-1 break-words">
                {t("You deleted this profile from the cloud on this device, so it doesn't sync now. Upload it again if you want it back there.")}
              </span>
            </div>
            {signedIn && (
              <button
                onClick={uploadAgain}
                disabled={busy}
                className="h-8 px-3 rounded-lg bg-amber-500/20 border border-amber-400/30 text-amber-100 text-[12px] font-bold hover:bg-amber-500/30 disabled:opacity-40"
              >
                {t("Upload it again")}
              </button>
            )}
          </div>
        )}
        {signedIn && (
          <>
            <div className="flex items-center gap-2 flex-wrap">
              <button
                onClick={doSync}
                disabled={syncing}
                className="h-9 px-3 rounded-lg bg-primary text-black text-[12.5px] font-bold disabled:opacity-40 flex items-center gap-2"
              >
                <RefreshCw size={13} className={syncing ? "animate-spin" : ""} />
                {syncing ? t("Syncing…") : t("Sync now")}
              </button>
              <span className="text-[11px] text-zinc-500">{lastSyncLabel ?? t("Not synced yet this session.")}</span>
            </div>
            <div className="flex items-center gap-2 flex-wrap text-[11.5px]">
              {autoSync ? (
                <>
                  <span className="flex items-center gap-1.5 text-emerald-300/90">
                    <span className="w-1.5 h-1.5 rounded-full bg-emerald-400 animate-pulse-brief" />
                    {t("Auto-sync is on — your changes are saved to the cloud automatically.")}
                  </span>
                  <button onClick={onToggleAutoSync} className="text-zinc-400 hover:text-zinc-200 underline underline-offset-2 shrink-0">{t("Turn off")}</button>
                </>
              ) : (
                <>
                  <span className="flex items-center gap-1.5 text-zinc-400">
                    <span className="w-1.5 h-1.5 rounded-full bg-zinc-500" />
                    {t("Auto-sync is off — changes sync only when you press Sync now.")}
                  </span>
                  <button onClick={onToggleAutoSync} className="text-primary hover:opacity-80 underline underline-offset-2 shrink-0">{t("Turn on")}</button>
                </>
              )}
            </div>
            {/* The cadence lives here rather than in Settings: it only means
                anything next to the Sync-now button and the on/off state it
                modifies, and it's the answer to "is this thing even running?" */}
            {autoSync && (
              <div className="border-t border-white/5 pt-3 flex items-center gap-2 flex-wrap">
                <span className="text-[11.5px] text-zinc-400">{t("Check for others' changes every")}</span>
                <div className="flex items-center gap-1">
                  {[1, 2, 5, 10, 15, 30].map((m) => (
                    <button
                      key={m}
                      onClick={() => onSetInterval(m)}
                      className={`h-7 px-2.5 rounded-md text-[11.5px] font-medium transition-colors ${
                        syncIntervalMin === m
                          ? "bg-primary text-black"
                          : "bg-zinc-900/60 text-zinc-400 hover:text-zinc-200 border border-white/5"
                      }`}
                    >
                      {t("{count}m", { count: m })}
                    </button>
                  ))}
                </div>
              </div>
            )}
            {stats?.diff && (() => {
              const d = stats.diff!;
              const pending = d.needs_push + d.needs_pull;
              if (pending === 0) {
                return (
                  <div className="flex items-center gap-1.5 text-[11.5px] text-emerald-300/90">
                    <CheckCircle2 size={12} /> {t("Everything is in sync with the cloud.")}
                  </div>
                );
              }
              return (
                <div className="text-[11.5px] text-amber-200/90 space-y-1">
                  <div className="flex items-center gap-1.5">
                    <AlertTriangle size={12} />
                    {d.needs_push > 0 && <span>{t("{count} to send", { count: d.needs_push })}</span>}
                    {d.needs_push > 0 && d.needs_pull > 0 && <span className="text-zinc-500">·</span>}
                    {d.needs_pull > 0 && <span>{t("{count} to receive", { count: d.needs_pull })}</span>}
                    <span className="text-zinc-500">{t("— run Sync now.")}</span>
                  </div>
                  {d.out_of_sync_nodes.length > 0 && (
                    <div className="text-[10.5px] text-zinc-500 pl-[18px]">
                      {t("Nodes: {list}", { list: d.out_of_sync_nodes.slice(0, 8).join(", ") })}
                      {d.out_of_sync_nodes.length > 8 ? " " + t("+{count} more", { count: d.out_of_sync_nodes.length - 8 }) : ""}
                    </div>
                  )}
                  {/* Only offered once syncing has demonstrably stopped helping.
                      Items that won't come down are almost always unreadable to
                      this device (pushed under a different key by a stray copy of
                      the vault), and no amount of syncing can ever clear them. */}
                  {stuck && (
                    <div className="pl-[18px] pt-1 space-y-1">
                      <div className="text-[10.5px] text-zinc-500 leading-relaxed">
                        {t("Still {count} to receive after syncing. These are almost certainly items this device can't read — pushed by another copy of this profile that had its own key. Syncing will never clear them.", { count: d.needs_pull })}
                      </div>
                      <button
                        onClick={forcePush}
                        disabled={busy || syncing}
                        className="text-[11px] text-amber-300 hover:text-amber-200 underline underline-offset-2 disabled:opacity-40"
                      >
                        {t("Replace the cloud copy with this device")}
                      </button>
                    </div>
                  )}
                </div>
              );
            })()}
          </>
        )}
      </section>

      {/* ---- Activity (recent edits + heaviness) ---- */}
      {stats && (stats.recent_edits.length > 0 || stats.total_records > 0) && (
        <section className={card}>
          <div className="flex items-center justify-between gap-2">
            <div className={`${label} flex items-center gap-1.5`}>
              <History size={12} /> {t("Recent activity")}
            </div>
            <div className="flex items-center gap-1.5 text-[10.5px] text-zinc-500" title={t("Encrypted vault size on this device")}>
              <HardDrive size={11} />
              {fmtBytes(stats.vault_bytes)}
              <span className="text-zinc-600">·</span>
              {t("{count} items", { count: stats.total_records })}
            </div>
          </div>
          {stats.recent_edits.length === 0 ? (
            <p className="text-[11.5px] text-zinc-500 italic">{t("No nodes yet.")}</p>
          ) : (
            <div className="space-y-1">
              {stats.recent_edits.map((e, i) => (
                <div key={i} className="flex items-center gap-2 py-1 text-[12px]">
                  <span className="text-zinc-200 truncate flex-1">{e.name}</span>
                  <span className="text-[10.5px] text-zinc-500 font-mono truncate max-w-[45%]">
                    {e.edited_by || t("unattributed")}
                  </span>
                  <span className="text-[10.5px] text-zinc-600 shrink-0 w-16 text-right">{hlcAgo(e.updated_at)}</span>
                </div>
              ))}
            </div>
          )}
          {stats.vault_bytes > 5 * 1024 * 1024 && (
            <div className="flex items-start gap-1.5 text-[10.5px] text-amber-300/80 pt-1 border-t border-white/5">
              <AlertTriangle size={11} className="mt-0.5 shrink-0" />
              <span>{t("This profile is getting large. Very heavy profiles sync and open more slowly — consider splitting rarely-used nodes into a separate profile.")}</span>
            </div>
          )}
        </section>
      )}

      {/* ---- Access ---- */}
      <section className={card}>
        <div className="flex items-center justify-between gap-2">
          <div className={label}>{t("Access")}</div>
          <span className="text-[10px] text-zinc-500">{t("end-to-end encrypted")}</span>
        </div>

        {!signedIn ? (
          <p className="text-[12px] text-zinc-500">{t("Sharing needs a cloud account.")}</p>
        ) : !shared ? (
          <>
            <p className="text-[12px] text-zinc-400 leading-relaxed">
              {t("Share this profile to let other people use its servers and logins. You choose what each person can do, and you can take access back at any time. The server can't read the servers and logins in it; it sees the profile's name and who it is shared with.")}
            </p>
            <button
              onClick={startSharing}
              disabled={busy}
              className="h-9 px-3 rounded-lg bg-primary text-black text-[12.5px] font-bold disabled:opacity-40 flex items-center gap-2"
            >
              {busy ? <Loader2 size={13} className="animate-spin" /> : <Share2 size={13} />}
              {t("Share this profile")}
            </button>
          </>
        ) : (
          <>
            {/* Invite — owners only; the server rejects the rest anyway. */}
            {isOwner && (
              <div className="space-y-1.5">
                <div className="flex flex-wrap items-center gap-1.5">
                  <input
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    onKeyDown={(e) => e.key === "Enter" && invite()}
                    placeholder={t("Invite by email…")}
                    className="flex-1 min-w-[160px] h-9 px-3 bg-black/40 border border-white/10 rounded-lg text-[12.5px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
                  />
                  <div className="flex bg-black/30 border border-white/10 rounded-lg p-0.5">
                    <button
                      onClick={() => setRole("editor")}
                      className={`h-8 px-2.5 rounded-md text-[11px] font-bold flex items-center gap-1 ${role === "editor" ? "bg-sky-500/20 text-sky-200" : "text-zinc-400"}`}
                    >
                      <Pencil size={10} /> {t("Editor")}
                    </button>
                    <button
                      onClick={() => setRole("user")}
                      className={`h-8 px-2.5 rounded-md text-[11px] font-bold flex items-center gap-1 ${role === "user" ? "bg-white/10 text-zinc-100" : "text-zinc-400"}`}
                    >
                      <Eye size={10} /> {t("Viewer")}
                    </button>
                  </div>
                  <button
                    onClick={invite}
                    disabled={busy}
                    className="h-9 px-3 rounded-lg bg-primary text-black text-[12px] font-bold disabled:opacity-40 flex items-center gap-1.5"
                  >
                    {busy ? <Loader2 size={12} className="animate-spin" /> : <UserPlus size={12} />} {t("Invite")}
                  </button>
                </div>
                <p className="text-[10.5px] text-zinc-500">{t(roleBlurb[role])}</p>
              </div>
            )}

            {/* Roster */}
            <div className="pt-1 space-y-1">
              {members === null ? (
                <div className="text-[11.5px] text-zinc-500 flex items-center gap-1.5">
                  <Loader2 size={11} className="animate-spin" /> {t("Loading people…")}
                </div>
              ) : members.length === 0 ? (
                <div className="text-[11.5px] text-zinc-500 italic flex items-center gap-1.5">
                  <Users size={12} /> {t("Nobody else yet.")}
                </div>
              ) : (
                members.map((m) => (
                  <div key={m.user_id} className="flex items-center gap-2 py-1 text-[12px]">
                    <span className="text-zinc-200 truncate flex-1 font-mono">
                      {m.email}
                      {m.email === st?.email && <span className="text-zinc-500 not-italic"> {t("(you)")}</span>}
                    </span>
                    {m.status === "pending" && (
                      <span className="text-[9px] font-bold uppercase tracking-wider text-amber-300/80">{t("invited")}</span>
                    )}
                    <RoleBadge role={m.role} />
                    {isOwner && m.role !== "owner" && (
                      <div className="flex items-center gap-1">
                        <button
                          onClick={() => changeRole(m.user_id, m.role === "user" ? "editor" : "user")}
                          disabled={busy}
                          title={m.role === "user" ? t("Let them make changes") : t("Make read-only")}
                          className="h-6 px-1.5 rounded bg-white/5 border border-white/10 text-zinc-400 hover:text-zinc-100 text-[10px] disabled:opacity-40"
                        >
                          {m.role === "user" ? t("↑ Editor") : t("↓ Viewer")}
                        </button>
                        <button
                          onClick={() => revoke(m)}
                          disabled={busy}
                          title={t("Remove access")}
                          className="h-6 w-6 grid place-items-center rounded bg-white/5 border border-white/10 text-zinc-400 hover:text-rose-300 hover:bg-rose-500/10 disabled:opacity-40"
                        >
                          <X size={11} />
                        </button>
                      </div>
                    )}
                  </div>
                ))
              )}
            </div>

            {keys?.current && (
              <p className="text-[10.5px] text-zinc-500 leading-relaxed">
                {tx("Key fingerprint {key}.", { key: <span className="font-mono text-zinc-300">{keys.current}</span> })}{" "}
                {isOwner
                  ? t("When you remove someone, the key is replaced, and members compare the new one with this.")
                  : t("If the key changes, compare the new one with the owner's.")}
              </p>
            )}
            {keys?.pending && (
              <p className="text-[11px] text-amber-200/90 leading-relaxed">
                {tx("A new key ({key}) is waiting for your OK in Servers. Until you accept it, your changes aren't uploaded.", { key: <span className="font-mono">{keys.pending}</span> })}
              </p>
            )}
            {isOwner && keyRetry && (
              <div className="flex items-center gap-2 text-[11px] text-amber-200/90">
                <span className="flex-1">{t("The key wasn't replaced, so whoever you removed still holds it.")}</span>
                <button
                  onClick={replaceKey}
                  disabled={busy}
                  className="h-7 px-2.5 rounded bg-amber-400/10 border border-amber-400/30 text-amber-200 hover:bg-amber-400/20 text-[11px] font-bold disabled:opacity-40 flex items-center gap-1.5"
                >
                  {busy ? <Loader2 size={11} className="animate-spin" /> : <KeyRound size={11} />} {t("Replace the key")}
                </button>
              </div>
            )}

            <div className="pt-2 border-t border-white/5">
              <button
                onClick={stopSharing}
                disabled={busy}
                className="h-8 px-3 rounded-lg bg-white/5 border border-white/10 text-zinc-300 hover:text-rose-300 hover:bg-rose-500/10 text-[11.5px] font-bold flex items-center gap-1.5 disabled:opacity-40"
              >
                <ShieldOff size={12} /> {isOwner ? t("Stop sharing") : t("Leave this profile")}
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
