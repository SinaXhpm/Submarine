import { useT } from "../i18n";

export const LogFooter = ({ logs }: { logs: any[] }) => {
  const t = useT();
  return (
  <footer className="h-32 bg-black/40 border-t border-white/5 p-3 font-mono text-[10px] flex flex-col shrink-0 overflow-hidden select-text">
    <div className="flex-1 overflow-y-auto space-y-1 custom-scrollbar">
      {logs.map((l, i) => (
        <div key={i} className={`flex gap-3 leading-tight ${l.type === 'error' ? 'text-red-400/90' : l.type === 'success' ? 'text-primary/90' : 'text-zinc-500'}`}>
          <span className="opacity-30 shrink-0 tabular-nums">
            [{new Date().toLocaleTimeString()}]
          </span>
          <span className="font-medium tracking-tight break-all">
            {l.msg}
          </span>
        </div>
      ))}
      {logs.length === 0 && (
        <div className="text-zinc-800 italic opacity-40">{t("System idle...")}</div>
      )}
    </div>
  </footer>
  );
};