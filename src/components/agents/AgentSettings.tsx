// Settings → AI agents (desktop): the agent gateway's switch, the longest time
// one access may last, and how to add Submarine to an agent. These settings
// belong to this device, not to a profile, and aren't synced.
import { useEffect, useState } from "react";
import { Bot, Copy, Check } from "lucide-react";
import { errText } from "../../util/aiApi";
import { gateway, DURATIONS, minutesText, setupCommands, type GatewayOverview } from "../../util/gatewayApi";

export const AgentSettings = ({ onOpenPanel }: { onOpenPanel?: () => void }) => {
  const [ov, setOv] = useState<GatewayOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState<string | null>(null);

  useEffect(() => {
    const load = () => gateway.overview().then(setOv, (e) => setError(errText(e)));
    void load();
    const off = gateway.onChanged(() => void load());
    return () => void off.then((f) => f());
  }, []);

  const change = async (c: { enabled?: boolean; longestMinutes?: number }) => {
    setBusy(true);
    try {
      setOv(await gateway.configure(c));
      setError(null);
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };

  const copy = (what: string, text: string) => {
    void navigator.clipboard.writeText(text).then(() => {
      setCopied(what);
      setTimeout(() => setCopied((c) => (c === what ? null : c)), 1500);
    });
  };

  if (!ov) return error ? <section className="text-[12px] text-red-300">{error}</section> : null;
  const cmds = setupCommands(ov.program);

  return (
    <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
      <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
        <Bot size={14} /> AI agents
      </div>

      <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 shadow-xl space-y-4">
        <div className="flex items-center justify-between gap-4">
          <div className="min-w-0">
            <div className="text-sm font-semibold text-zinc-100">Let AI agents on this computer ask for access to my servers</div>
            <p className="text-[12px] text-zinc-400 leading-relaxed mt-1">
              An agent (Claude Code, Cursor and the like) can only ask. You choose the server and what it may do, here, every
              time. It never gets your passwords, keys or the server's address.
            </p>
          </div>
          <button
            role="switch"
            aria-checked={ov.enabled}
            aria-label="Let AI agents ask for access"
            disabled={busy}
            onClick={() => void change({ enabled: !ov.enabled })}
            className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${ov.enabled ? "bg-primary" : "bg-zinc-700"} disabled:opacity-60`}
          >
            <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${ov.enabled ? "left-[22px]" : "left-0.5"}`} />
          </button>
        </div>

        {ov.enabled && (
          <div className="text-[12px] text-zinc-400">
            {ov.listening ? "Listening for agents on this computer." : ov.profile_open ? "Not listening." : "Agents can ask once a profile is open."}
            {ov.error && <span className="text-amber-300"> {ov.error}</span>}
            {onOpenPanel && (
              <button type="button" onClick={onOpenPanel} className="ml-2 text-primary hover:underline">Open the Agents page</button>
            )}
          </div>
        )}
        {error && <div className="text-[12px] text-red-300">{error}</div>}

        <label className="flex items-center justify-between gap-4">
          <span className="text-[12px] text-zinc-300">Longest time for one access</span>
          <select
            value={ov.longest_minutes}
            disabled={busy}
            onChange={(e) => void change({ longestMinutes: Number(e.target.value) })}
            className="h-8 px-2 bg-zinc-950 border border-white/10 rounded-lg text-[12px] text-zinc-200"
          >
            {DURATIONS.map((d) => (
              <option key={d} value={d}>{minutesText(d)}</option>
            ))}
          </select>
        </label>

        <div className="space-y-2">
          <div className="text-[12px] text-zinc-300">Add Submarine to an agent</div>
          <Snippet label="Claude Code" text={cmds.claudeCode} copied={copied === "cc"} onCopy={() => copy("cc", cmds.claudeCode)} />
          <Snippet label="Claude Desktop, Cursor, VS Code and others (their MCP settings)" text={cmds.json} copied={copied === "json"} onCopy={() => copy("json", cmds.json)} />
          <p className="text-[11px] text-zinc-500 leading-relaxed">
            What you allow an agent to see goes to the agent, and from there to the AI service it uses.
          </p>
        </div>
      </div>
    </section>
  );
};

const Snippet = ({ label, text, copied, onCopy }: { label: string; text: string; copied: boolean; onCopy: () => void }) => (
  <div>
    <div className="text-[11px] text-zinc-500 mb-1">{label}</div>
    <div className="flex items-start gap-2">
      <pre className="flex-1 min-w-0 overflow-x-auto bg-zinc-950 border border-white/10 rounded-lg p-2 text-[11px] text-zinc-300 whitespace-pre">{text}</pre>
      <button type="button" aria-label={`Copy (${label})`} onClick={onCopy} className="h-8 w-8 shrink-0 rounded-lg border border-white/10 text-zinc-400 hover:text-zinc-100 hover:bg-white/5 flex items-center justify-center">
        {copied ? <Check size={13} /> : <Copy size={13} />}
      </button>
    </div>
  </div>
);
