import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Cloud, CloudOff, RefreshCw, Share2, UserPlus, X, Loader2, Users,
  AlertTriangle, CheckCircle2, ShieldOff, Pencil, Eye, History, HardDrive,
} from "lucide-react";
import { useConfirm, useTextPrompt } from "../ui/confirm";
import { RoleBadge, roleBlurb } from "./shareRoles";

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
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
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
}

interface MemberInfo { user_id: number; email: string; role: string; status: string; }

type Props = {
  onSync: () => Promise<void> | void;
  syncing: boolean;
  lastSyncLabel: string | null;
  autoSync: boolean;
  onToggleAutoSync: () => void;
};

export default function ProfilePanel({ onSync, syncing, lastSyncLabel, autoSync, onToggleAutoSync }: Props) {
  const confirm = useConfirm();
  const textPrompt = useTextPrompt();

  const [st, setSt] = useState<ShareStatus | null>(null);
  const [members, setMembers] = useState<MemberInfo[] | null>(null);
  const [stats, setStats] = useState<SyncStats | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<"editor" | "user">("editor");

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
    } catch (e) { fail(e); }
  }, []);

  // Stats are separate from share status because the cloud diff inside them is a
  // network round-trip; we don't want it blocking the panel's first paint.
  const loadStats = useCallback(async () => {
    try { setStats(await invoke<SyncStats>("profile_sync_stats")); }
    catch { /* offline or no profile — panel still shows the rest */ }
  }, []);

  useEffect(() => { refresh(); loadStats(); }, [refresh, loadStats]);

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
      title: "Unlock sharing",
      message:
        "Sharing has its own passphrase — it protects the key that shared profiles are locked to. " +
        "Enter yours, or pick one now if this is your first share. It never leaves this device.",
      placeholder: "Sharing passphrase (min 8)",
      password: true,
      okLabel: "Continue",
      validate: (v) => (v.length < 8 ? "At least 8 characters" : null),
    });
    if (!pass) return false;
    await invoke("setup_identity", { encPassphrase: pass });
    await refresh();
    return true;
  }, [textPrompt, refresh]);

  const startSharing = async () => {
    setBusy(true); setErr(null);
    try {
      if (!(await ensureIdentity())) return;
      // The share is named after the profile — one less thing to invent. It's
      // what invitees see when the invitation shows up for them.
      await invoke<{ share_id: string }>("share_current_profile", { name: st?.profile ?? "profile" });
      await refresh();
      flash("This profile is shared. Invite people below.");
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const invite = async () => {
    const e = email.trim().toLowerCase();
    if (!e.includes("@")) { fail("Enter a valid email address."); return; }
    setBusy(true); setErr(null);
    try {
      if (!(await ensureIdentity())) return;
      await invoke("invite_to_share", { shareId: st!.share_id, email: e, role });
      setEmail("");
      await refresh();
      flash(`Invited ${e} as ${role === "user" ? "Viewer" : "Editor"}.`);
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
      title: "Remove access?",
      message: `${m.email} loses access to "${st?.profile}" immediately. Any copy already on their device stays there — this stops them receiving future changes.`,
      okLabel: "Remove",
      destructive: true,
    }))) return;
    setBusy(true);
    try {
      await invoke("share_revoke", { shareId: st!.share_id, memberUserId: m.user_id });
      await refresh();
      flash(`${m.email} removed.`);
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const stopSharing = async () => {
    const owner = st?.role === "owner";
    if (!(await confirm({
      title: owner ? "Stop sharing this profile?" : "Leave this shared profile?",
      message: owner
        ? `Everyone you invited loses access to "${st?.profile}" immediately. Your own copy stays on this device.`
        : `You stop receiving changes to "${st?.profile}". The copy on this device stays, and becomes yours alone.`,
      okLabel: owner ? "Stop sharing" : "Leave",
      destructive: true,
    }))) return;
    setBusy(true);
    try {
      await invoke("stop_sharing");
      await refresh();
      flash(owner ? "Sharing stopped — this profile is private again." : "You left the share.");
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
              ? isOwner ? "You share this profile. Manage who gets in, below." : "Shared with you by someone else."
              : "This profile lives only on your devices."}
          </p>
        </div>
      </header>

      {err && (
        <div className="px-3 py-2 rounded-lg bg-rose-500/10 border border-rose-500/30 text-rose-200 text-[12px] flex items-start gap-2">
          <AlertTriangle size={13} className="mt-0.5 shrink-0" />
          <span className="flex-1 break-words">{err}</span>
          <button onClick={() => setErr(null)} className="text-rose-300/70 hover:text-white"><X size={12} /></button>
        </div>
      )}
      {ok && !err && (
        <div className="px-3 py-2 rounded-lg bg-emerald-500/10 border border-emerald-500/30 text-emerald-200 text-[12px] flex items-center gap-2">
          <CheckCircle2 size={13} /> <span className="flex-1 break-words">{ok}</span>
        </div>
      )}

      {/* ---- Cloud + sync ---- */}
      <section className={card}>
        <div className={label}>Cloud</div>
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
                Not signed in — lock this profile and sign in from the Cloud button on the picker.
              </span>
            </>
          )}
        </div>
        {signedIn && (
          <>
            <div className="flex items-center gap-2 flex-wrap">
              <button
                onClick={doSync}
                disabled={syncing}
                className="h-9 px-3 rounded-lg bg-primary text-black text-[12.5px] font-bold disabled:opacity-40 flex items-center gap-2"
              >
                <RefreshCw size={13} className={syncing ? "animate-spin" : ""} />
                {syncing ? "Syncing…" : "Sync now"}
              </button>
              <span className="text-[11px] text-zinc-500">{lastSyncLabel ?? "Not synced yet this session."}</span>
            </div>
            <div className="flex items-center gap-2 flex-wrap text-[11.5px]">
              {autoSync ? (
                <>
                  <span className="flex items-center gap-1.5 text-emerald-300/90">
                    <span className="w-1.5 h-1.5 rounded-full bg-emerald-400 animate-pulse" />
                    Auto-sync is on — your changes are saved to the cloud automatically.
                  </span>
                  <button onClick={onToggleAutoSync} className="text-zinc-400 hover:text-zinc-200 underline underline-offset-2 shrink-0">Turn off</button>
                </>
              ) : (
                <>
                  <span className="flex items-center gap-1.5 text-zinc-400">
                    <span className="w-1.5 h-1.5 rounded-full bg-zinc-500" />
                    Auto-sync is off — changes sync only when you press Sync now.
                  </span>
                  <button onClick={onToggleAutoSync} className="text-primary hover:opacity-80 underline underline-offset-2 shrink-0">Turn on</button>
                </>
              )}
            </div>
            {stats?.diff && (() => {
              const d = stats.diff!;
              const pending = d.needs_push + d.needs_pull;
              if (pending === 0) {
                return (
                  <div className="flex items-center gap-1.5 text-[11.5px] text-emerald-300/90">
                    <CheckCircle2 size={12} /> Everything is in sync with the cloud.
                  </div>
                );
              }
              return (
                <div className="text-[11.5px] text-amber-200/90 space-y-1">
                  <div className="flex items-center gap-1.5">
                    <AlertTriangle size={12} />
                    {d.needs_push > 0 && <span>{d.needs_push} to send</span>}
                    {d.needs_push > 0 && d.needs_pull > 0 && <span className="text-zinc-500">·</span>}
                    {d.needs_pull > 0 && <span>{d.needs_pull} to receive</span>}
                    <span className="text-zinc-500">— run Sync now.</span>
                  </div>
                  {d.out_of_sync_nodes.length > 0 && (
                    <div className="text-[10.5px] text-zinc-500 pl-[18px]">
                      Nodes: {d.out_of_sync_nodes.slice(0, 8).join(", ")}
                      {d.out_of_sync_nodes.length > 8 ? ` +${d.out_of_sync_nodes.length - 8} more` : ""}
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
              <History size={12} /> Recent activity
            </div>
            <div className="flex items-center gap-1.5 text-[10.5px] text-zinc-500" title="Encrypted vault size on this device">
              <HardDrive size={11} />
              {fmtBytes(stats.vault_bytes)}
              <span className="text-zinc-600">·</span>
              {stats.total_records} items
            </div>
          </div>
          {stats.recent_edits.length === 0 ? (
            <p className="text-[11.5px] text-zinc-500 italic">No nodes yet.</p>
          ) : (
            <div className="space-y-1">
              {stats.recent_edits.map((e, i) => (
                <div key={i} className="flex items-center gap-2 py-1 text-[12px]">
                  <span className="text-zinc-200 truncate flex-1">{e.name}</span>
                  <span className="text-[10.5px] text-zinc-500 font-mono truncate max-w-[45%]">
                    {e.edited_by || "unattributed"}
                  </span>
                  <span className="text-[10.5px] text-zinc-600 shrink-0 w-16 text-right">{hlcAgo(e.updated_at)}</span>
                </div>
              ))}
            </div>
          )}
          {stats.vault_bytes > 5 * 1024 * 1024 && (
            <div className="flex items-start gap-1.5 text-[10.5px] text-amber-300/80 pt-1 border-t border-white/5">
              <AlertTriangle size={11} className="mt-0.5 shrink-0" />
              <span>This profile is getting large. Very heavy profiles sync and open more slowly — consider splitting rarely-used nodes into a separate profile.</span>
            </div>
          )}
        </section>
      )}

      {/* ---- Access ---- */}
      <section className={card}>
        <div className="flex items-center justify-between gap-2">
          <div className={label}>Access</div>
          <span className="text-[10px] text-zinc-500">end-to-end encrypted</span>
        </div>

        {!signedIn ? (
          <p className="text-[12px] text-zinc-500">Sharing needs a cloud account.</p>
        ) : !shared ? (
          <>
            <p className="text-[12px] text-zinc-400 leading-relaxed">
              Share this profile to let other people use its servers and logins. You choose what each
              person can do, and you can take access back at any time. The server never sees any of it.
            </p>
            <button
              onClick={startSharing}
              disabled={busy}
              className="h-9 px-3 rounded-lg bg-primary text-black text-[12.5px] font-bold disabled:opacity-40 flex items-center gap-2"
            >
              {busy ? <Loader2 size={13} className="animate-spin" /> : <Share2 size={13} />}
              Share this profile
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
                    placeholder="Invite by email…"
                    className="flex-1 min-w-[160px] h-9 px-3 bg-black/40 border border-white/10 rounded-lg text-[12.5px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
                  />
                  <div className="flex bg-black/30 border border-white/10 rounded-lg p-0.5">
                    <button
                      onClick={() => setRole("editor")}
                      className={`h-8 px-2.5 rounded-md text-[11px] font-bold flex items-center gap-1 ${role === "editor" ? "bg-sky-500/20 text-sky-200" : "text-zinc-400"}`}
                    >
                      <Pencil size={10} /> Editor
                    </button>
                    <button
                      onClick={() => setRole("user")}
                      className={`h-8 px-2.5 rounded-md text-[11px] font-bold flex items-center gap-1 ${role === "user" ? "bg-white/10 text-zinc-100" : "text-zinc-400"}`}
                    >
                      <Eye size={10} /> Viewer
                    </button>
                  </div>
                  <button
                    onClick={invite}
                    disabled={busy}
                    className="h-9 px-3 rounded-lg bg-primary text-black text-[12px] font-bold disabled:opacity-40 flex items-center gap-1.5"
                  >
                    {busy ? <Loader2 size={12} className="animate-spin" /> : <UserPlus size={12} />} Invite
                  </button>
                </div>
                <p className="text-[10.5px] text-zinc-500">{roleBlurb[role]}</p>
              </div>
            )}

            {/* Roster */}
            <div className="pt-1 space-y-1">
              {members === null ? (
                <div className="text-[11.5px] text-zinc-500 flex items-center gap-1.5">
                  <Loader2 size={11} className="animate-spin" /> Loading people…
                </div>
              ) : members.length === 0 ? (
                <div className="text-[11.5px] text-zinc-500 italic flex items-center gap-1.5">
                  <Users size={12} /> Nobody else yet.
                </div>
              ) : (
                members.map((m) => (
                  <div key={m.user_id} className="flex items-center gap-2 py-1 text-[12px]">
                    <span className="text-zinc-200 truncate flex-1 font-mono">
                      {m.email}
                      {m.email === st?.email && <span className="text-zinc-500 not-italic"> (you)</span>}
                    </span>
                    {m.status === "pending" && (
                      <span className="text-[9px] font-bold uppercase tracking-wider text-amber-300/80">invited</span>
                    )}
                    <RoleBadge role={m.role} />
                    {isOwner && m.role !== "owner" && (
                      <div className="flex items-center gap-1">
                        <button
                          onClick={() => changeRole(m.user_id, m.role === "user" ? "editor" : "user")}
                          disabled={busy}
                          title={m.role === "user" ? "Let them make changes" : "Make read-only"}
                          className="h-6 px-1.5 rounded bg-white/5 border border-white/10 text-zinc-400 hover:text-zinc-100 text-[10px] disabled:opacity-40"
                        >
                          {m.role === "user" ? "↑ Editor" : "↓ Viewer"}
                        </button>
                        <button
                          onClick={() => revoke(m)}
                          disabled={busy}
                          title="Remove access"
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

            <div className="pt-2 border-t border-white/5">
              <button
                onClick={stopSharing}
                disabled={busy}
                className="h-8 px-3 rounded-lg bg-white/5 border border-white/10 text-zinc-300 hover:text-rose-300 hover:bg-rose-500/10 text-[11.5px] font-bold flex items-center gap-1.5 disabled:opacity-40"
              >
                <ShieldOff size={12} /> {isOwner ? "Stop sharing" : "Leave this profile"}
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
