import { Crown, Pencil, Eye } from "lucide-react";

// The three sharing roles, in one place — the Profile panel (manage access) and
// the Cloud panel (incoming invites) both render them, and a role reading
// "Editor" in one screen and "editor" in the other is exactly the kind of drift
// that made sharing feel like two unrelated features.

export const roleMeta: Record<string, { label: string; icon: any; cls: string }> = {
  owner:  { label: "Owner",  icon: Crown,  cls: "text-amber-300 bg-amber-500/10 border-amber-500/30" },
  editor: { label: "Editor", icon: Pencil, cls: "text-sky-300 bg-sky-500/10 border-sky-500/30" },
  user:   { label: "Viewer", icon: Eye,    cls: "text-zinc-300 bg-white/5 border-white/10" },
};

export function RoleBadge({ role }: { role: string }) {
  const m = roleMeta[role] ?? roleMeta.user;
  const I = m.icon;
  return (
    <span className={`inline-flex items-center gap-1 h-5 px-1.5 rounded-full border text-[9px] font-bold uppercase tracking-wider ${m.cls}`}>
      <I size={9} /> {m.label}
    </span>
  );
}

// What each role can actually do, in the user's words — shown next to the role
// picker so "Editor vs Viewer" doesn't require reading the docs.
export const roleBlurb: Record<string, string> = {
  editor: "Can use and change this profile.",
  user: "Can use it, but can't change anything.",
};
