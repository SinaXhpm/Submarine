import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Users, Share2, KeyRound, ShieldCheck, UserPlus, Crown, Pencil, Eye,
  Check, X, Trash2, LogOut, ChevronDown, ChevronRight, Loader2,
} from "lucide-react";

// E2E profile sharing, folded into the Cloud panel (signed-in only). Three
// blocks: unlock your sharing identity, shares shared WITH you (accept + pull
// into a local profile), and shares you OWN (invite by email with a per-invite
// role choice, manage members). All crypto happens in Rust; this is pure UI.

interface IdentityStatus { exists_on_server: boolean; unlocked: boolean; public_key: string | null; }
interface ShareInfo { share_id: string; name: string; role: string; status: string; owner_email: string; }
interface MemberInfo { user_id: number; email: string; role: string; status: string; }

type Props = { onLocalProfilesChanged: () => void };

const roleMeta: Record<string, { label: string; icon: any; cls: string }> = {
  owner:  { label: "Owner",  icon: Crown,  cls: "text-amber-300 bg-amber-500/10 border-amber-500/30" },
  editor: { label: "Editor", icon: Pencil, cls: "text-sky-300 bg-sky-500/10 border-sky-500/30" },
  user:   { label: "Viewer", icon: Eye,    cls: "text-zinc-300 bg-white/5 border-white/10" },
};

function RoleBadge({ role }: { role: string }) {
  const m = roleMeta[role] ?? roleMeta.user;
  const I = m.icon;
  return (
    <span className={`inline-flex items-center gap-1 h-5 px-1.5 rounded-full border text-[9px] font-bold uppercase tracking-wider ${m.cls}`}>
      <I size={9} /> {m.label}
    </span>
  );
}

export default function SharingSection({ onLocalProfilesChanged }: Props) {
  const [identity, setIdentity] = useState<IdentityStatus | null>(null);
  const [shares, setShares] = useState<ShareInfo[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [ok, setOk] = useState<string | null>(null);

  const [passphrase, setPassphrase] = useState("");
  const [membersOpen, setMembersOpen] = useState<Record<string, MemberInfo[] | "loading">>({});

  const flash = (m: string) => { setOk(m); setErr(null); setTimeout(() => setOk(null), 4000); };
  const fail = (e: unknown) => setErr(String(e).replace(/^\[[A-Z_]+\]\s*/, ""));

  const loadIdentity = useCallback(async () => {
    try { setIdentity(await invoke<IdentityStatus>("identity_status")); }
    catch (e) { fail(e); }
  }, []);

  const loadShares = useCallback(async () => {
    try { setShares(await invoke<ShareInfo[]>("list_shares")); }
    catch (e) { /* not fatal if identity not set up */ }
  }, []);

  useEffect(() => { loadIdentity(); loadShares(); }, [loadIdentity, loadShares]);

  const unlock = async () => {
    if (passphrase.length < 8) { setErr("Encryption passphrase must be at least 8 characters."); return; }
    setBusy(true); setErr(null);
    try {
      const st = await invoke<IdentityStatus>("setup_identity", { encPassphrase: passphrase });
      setIdentity(st);
      setPassphrase("");
      await loadShares();
      flash(st.exists_on_server ? "Sharing unlocked." : "Sharing set up.");
    } catch (e) { fail(e); } finally { setBusy(false); }
  };

  const fetchMembers = async (shareId: string) => {
    setMembersOpen((m) => ({ ...m, [shareId]: "loading" }));
    try {
      const list = await invoke<MemberInfo[]>("share_member_list", { shareId });
      setMembersOpen((m) => ({ ...m, [shareId]: list }));
    } catch (e) { fail(e); setMembersOpen((m) => { const c = { ...m }; delete c[shareId]; return c; }); }
  };

  const toggleMembers = async (shareId: string) => {
    if (membersOpen[shareId]) {
      setMembersOpen((m) => { const c = { ...m }; delete c[shareId]; return c; });
      return;
    }
    await fetchMembers(shareId);
  };

  // After an invite/role/revoke, refresh members only if that row is expanded.
  const reloadMembers = (shareId: string) => {
    if (membersOpen[shareId]) fetchMembers(shareId);
  };

  const mine = shares.filter((s) => s.role === "owner");
  const withMe = shares.filter((s) => s.role !== "owner");

  const unlocked = identity?.unlocked === true;

  return (
    <div className="mt-4 pt-4 border-t border-white/5 space-y-3">
      <div className="flex items-center gap-2 text-zinc-300">
        <Share2 size={14} className="text-primary" />
        <span className="text-[12.5px] font-bold uppercase tracking-wider">Profile Sharing</span>
        <span className="text-[10px] text-zinc-500">end-to-end encrypted</span>
      </div>

      {err && <div className="px-3 py-2 rounded bg-rose-500/10 border border-rose-500/30 text-rose-200 text-[11.5px]">{err}</div>}
      {ok && <div className="px-3 py-2 rounded bg-emerald-500/10 border border-emerald-500/30 text-emerald-200 text-[11.5px]">{ok}</div>}

      {/* ---- Identity gate ---- */}
      {!unlocked ? (
        <div className="bg-zinc-900/40 border border-white/5 rounded-lg p-3 space-y-2">
          <div className="flex items-center gap-2 text-[12px] text-zinc-300">
            <KeyRound size={13} className="text-primary" />
            {identity?.exists_on_server
              ? "Enter your encryption passphrase to unlock sharing."
              : "Set an encryption passphrase to enable sharing on this account."}
          </div>
          <p className="text-[10.5px] text-zinc-500 leading-relaxed">
            This passphrase never leaves your device — it protects the private key that lets you read shared profiles.
            {identity?.exists_on_server ? "" : " Keep it safe: if you forget it you'll have to reset your identity and re-share."}
          </p>
          <div className="flex gap-2">
            <input
              type="password"
              value={passphrase}
              onChange={(e) => setPassphrase(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && unlock()}
              placeholder="Encryption passphrase (min 8)"
              className="flex-1 h-9 px-3 bg-black/40 border border-white/10 rounded-lg text-[12.5px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600"
            />
            <button onClick={unlock} disabled={busy}
              className="h-9 px-3 rounded-lg bg-primary text-black text-[12px] font-bold disabled:opacity-40 flex items-center gap-1.5">
              {busy ? <Loader2 size={13} className="animate-spin" /> : <ShieldCheck size={13} />}
              {identity?.exists_on_server ? "Unlock" : "Set up"}
            </button>
          </div>
        </div>
      ) : (
        <div className="flex items-center gap-2 text-[11px] text-emerald-300/80">
          <ShieldCheck size={12} /> Sharing identity unlocked for this session.
        </div>
      )}

      {/* ---- Shared with me ---- */}
      {unlocked && withMe.length > 0 && (
        <div className="space-y-2">
          <div className="text-[11px] font-bold uppercase tracking-wider text-zinc-500">Shared with you</div>
          {withMe.map((s) => (
            <SharedWithMeRow key={s.share_id} share={s} onImported={() => { loadShares(); onLocalProfilesChanged(); }} onError={fail} onOk={flash} />
          ))}
        </div>
      )}

      {/* ---- My shares ---- */}
      {unlocked && (
        <div className="space-y-2">
          <div className="text-[11px] font-bold uppercase tracking-wider text-zinc-500">Shares you own</div>
          {mine.length === 0 ? (
            <p className="text-[11.5px] text-zinc-500 italic">
              None yet. Open a profile and use <span className="text-zinc-300 font-semibold">Share this profile</span> to create one.
            </p>
          ) : (
            mine.map((s) => (
              <OwnedShareRow
                key={s.share_id}
                share={s}
                members={membersOpen[s.share_id]}
                onToggleMembers={() => toggleMembers(s.share_id)}
                onChanged={() => { loadShares(); reloadMembers(s.share_id); }}
                onError={fail}
                onOk={flash}
              />
            ))
          )}
        </div>
      )}
    </div>
  );
}

// ---- A share someone else shared with me: accept + pull into a local profile.
function SharedWithMeRow({ share, onImported, onError, onOk }: {
  share: ShareInfo; onImported: () => void; onError: (e: unknown) => void; onOk: (m: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(share.name);
  const [pw, setPw] = useState("");
  const [busy, setBusy] = useState(false);

  const accept = async () => {
    if (pw.length < 8) { onError("Local vault password must be at least 8 characters."); return; }
    setBusy(true);
    try {
      await invoke("accept_share", { shareId: share.share_id });
      const rep = await invoke<{ pushed: number; pulled: number }>("import_shared_profile", {
        shareId: share.share_id, localName: name.trim(), vaultPassword: pw,
      });
      onOk(`Imported "${name.trim()}" — pulled ${rep.pulled} item(s).`);
      setOpen(false); setPw("");
      onImported();
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
        {share.status === "pending" && (
          <span className="text-[9px] font-bold uppercase tracking-wider text-amber-300/80">pending</span>
        )}
        <button onClick={() => setOpen((o) => !o)}
          className="h-7 px-2.5 rounded-lg bg-primary text-black text-[11px] font-bold flex items-center gap-1">
          <Check size={12} /> Accept
        </button>
      </div>
      {open && (
        <div className="mt-2 pt-2 border-t border-white/5 space-y-2">
          <p className="text-[10.5px] text-zinc-500">Set up a local copy. The vault password protects it on THIS device (your own choice).</p>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Local profile name"
            className="w-full h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50" />
          <input type="password" value={pw} onChange={(e) => setPw(e.target.value)} placeholder="Local vault password (min 8)"
            className="w-full h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600" />
          <div className="flex gap-2">
            <button onClick={accept} disabled={busy}
              className="h-8 px-3 rounded-lg bg-primary text-black text-[11.5px] font-bold disabled:opacity-40 flex items-center gap-1.5">
              {busy ? <Loader2 size={12} className="animate-spin" /> : <Check size={12} />} Accept & set up
            </button>
            <button onClick={() => setOpen(false)} className="h-8 px-3 rounded-lg bg-white/5 border border-white/10 text-zinc-300 text-[11.5px] font-bold">Cancel</button>
          </div>
        </div>
      )}
    </div>
  );
}

// ---- A share I own: invite (role chosen per invite), manage members, delete.
function OwnedShareRow({ share, members, onToggleMembers, onChanged, onError, onOk }: {
  share: ShareInfo; members: MemberInfo[] | "loading" | undefined;
  onToggleMembers: () => void; onChanged: () => void; onError: (e: unknown) => void; onOk: (m: string) => void;
}) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<"editor" | "user">("editor");
  const [busy, setBusy] = useState(false);

  const invite = async () => {
    const e = email.trim().toLowerCase();
    if (!e || !e.includes("@")) { onError("Enter a valid email."); return; }
    setBusy(true);
    try {
      await invoke("invite_to_share", { shareId: share.share_id, email: e, role });
      onOk(`Invited ${e} as ${role === "user" ? "Viewer" : "Editor"}.`);
      setEmail("");
      onChanged();
    } catch (err) { onError(err); } finally { setBusy(false); }
  };

  const setMemberRole = async (uid: number, r: string) => {
    try { await invoke("share_set_role", { shareId: share.share_id, memberUserId: uid, role: r }); onChanged(); }
    catch (e) { onError(e); }
  };
  const revoke = async (uid: number) => {
    try { await invoke("share_revoke", { shareId: share.share_id, memberUserId: uid }); onChanged(); onOk("Member removed."); }
    catch (e) { onError(e); }
  };
  const del = async () => {
    if (!confirm(`Delete shared profile "${share.name}" for everyone? This can't be undone.`)) return;
    try { await invoke("share_delete", { shareId: share.share_id }); onChanged(); onOk("Share deleted."); }
    catch (e) { onError(e); }
  };

  const expanded = members !== undefined;

  return (
    <div className="bg-zinc-900/40 border border-white/5 rounded-lg p-2.5">
      <div className="flex items-center gap-2">
        <button onClick={onToggleMembers} className="text-zinc-500 hover:text-zinc-300 shrink-0">
          {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
        </button>
        <Share2 size={13} className="text-primary shrink-0" />
        <div className="text-[12.5px] text-zinc-100 font-semibold truncate flex-1">{share.name}</div>
        <RoleBadge role="owner" />
        <button onClick={del} title="Delete share"
          className="h-7 w-7 grid place-items-center rounded-lg bg-white/5 border border-white/10 text-zinc-400 hover:text-rose-300 hover:bg-rose-500/10">
          <Trash2 size={12} />
        </button>
      </div>

      {/* Invite row — role is chosen here, every time. */}
      <div className="mt-2 flex flex-wrap items-center gap-1.5">
        <input value={email} onChange={(e) => setEmail(e.target.value)} onKeyDown={(e) => e.key === "Enter" && invite()}
          placeholder="Invite by email…"
          className="flex-1 min-w-[140px] h-8 px-2.5 bg-black/40 border border-white/10 rounded-lg text-[12px] text-zinc-100 outline-none focus:border-primary/50 placeholder:text-zinc-600" />
        <div className="flex bg-black/30 border border-white/10 rounded-lg p-0.5">
          <button onClick={() => setRole("editor")}
            className={`h-7 px-2 rounded-md text-[10.5px] font-bold flex items-center gap-1 ${role === "editor" ? "bg-sky-500/20 text-sky-200" : "text-zinc-400"}`}>
            <Pencil size={10} /> Editor
          </button>
          <button onClick={() => setRole("user")}
            className={`h-7 px-2 rounded-md text-[10.5px] font-bold flex items-center gap-1 ${role === "user" ? "bg-white/10 text-zinc-100" : "text-zinc-400"}`}>
            <Eye size={10} /> Viewer
          </button>
        </div>
        <button onClick={invite} disabled={busy}
          className="h-8 px-2.5 rounded-lg bg-primary text-black text-[11.5px] font-bold disabled:opacity-40 flex items-center gap-1">
          {busy ? <Loader2 size={12} className="animate-spin" /> : <UserPlus size={12} />} Invite
        </button>
      </div>

      {/* Members */}
      {expanded && (
        <div className="mt-2 pt-2 border-t border-white/5 space-y-1">
          {members === "loading" ? (
            <div className="text-[11px] text-zinc-500 flex items-center gap-1.5"><Loader2 size={11} className="animate-spin" /> Loading members…</div>
          ) : members && members.length > 0 ? (
            members.map((m) => (
              <div key={m.user_id} className="flex items-center gap-2 text-[11.5px]">
                <span className="text-zinc-200 truncate flex-1">{m.email}</span>
                {m.status === "pending" && <span className="text-[9px] font-bold uppercase text-amber-300/80">pending</span>}
                <RoleBadge role={m.role} />
                {m.role !== "owner" && (
                  <div className="flex items-center gap-1">
                    <button onClick={() => setMemberRole(m.user_id, m.role === "user" ? "editor" : "user")}
                      title={m.role === "user" ? "Make Editor" : "Make Viewer"}
                      className="h-6 px-1.5 rounded bg-white/5 border border-white/10 text-zinc-400 hover:text-zinc-100 text-[10px]">
                      {m.role === "user" ? "↑ Editor" : "↓ Viewer"}
                    </button>
                    <button onClick={() => revoke(m.user_id)} title="Remove"
                      className="h-6 w-6 grid place-items-center rounded bg-white/5 border border-white/10 text-zinc-400 hover:text-rose-300 hover:bg-rose-500/10">
                      <X size={11} />
                    </button>
                  </div>
                )}
              </div>
            ))
          ) : (
            <div className="text-[11px] text-zinc-500 italic">No members yet — invite someone above.</div>
          )}
        </div>
      )}
    </div>
  );
}
