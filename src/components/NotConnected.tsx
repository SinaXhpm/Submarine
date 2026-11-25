import { Plug, X } from "lucide-react";
import { useT } from "../i18n";

// What a tab shows while it waits to be connected: it opened when the profile
// was unlocked (its server's "At startup: Open the tab"), and connects when
// Connect is pressed here or in the tab's menu (hooks/useWaitToConnect).
export default function NotConnected({ name, onConnect, onClose }: { name: string; onConnect: () => void; onClose: () => void }) {
  const t = useT();
  return (
    <div className="relative flex-1 flex flex-col items-center justify-center gap-5 p-6 bg-shade-0a0a0c text-white text-center" data-not-connected>
      <div className="w-12 h-12 rounded-2xl bg-white/[0.04] border border-white/10 flex items-center justify-center text-zinc-400" aria-hidden="true">
        <Plug size={20} />
      </div>
      <div className="min-w-0 max-w-full">
        <h2 className="text-sm font-semibold text-zinc-100 truncate" title={name}>{name}</h2>
        <p className="text-[12px] text-zinc-500 mt-1">{t("Not connected. This tab opened with the profile.")}</p>
      </div>
      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={onConnect}
          className="h-10 px-5 rounded-xl bg-primary text-zinc-950 hover:brightness-110 flex items-center gap-2 text-sm font-semibold transition-all outline-none focus-visible:ring-2 focus-visible:ring-primary/60 focus-visible:ring-offset-2 focus-visible:ring-offset-shade-0a0a0c"
        >
          <Plug size={15} /> {t("Connect")}
        </button>
        <button
          type="button"
          onClick={onClose}
          className="h-10 px-4 rounded-xl bg-white/5 text-zinc-300 hover:bg-red-500/15 hover:text-red-300 flex items-center gap-2 text-sm font-semibold transition-colors outline-none focus-visible:ring-2 focus-visible:ring-white/30"
        >
          <X size={15} /> {t("Close")}
        </button>
      </div>
    </div>
  );
}
