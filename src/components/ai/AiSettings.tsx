import { useCallback, useEffect, useState } from "react";
import { Sparkles, Plus, Star, KeyRound, Trash2, PlugZap, Pencil, MessagesSquare, Loader2 } from "lucide-react";
import { useConfirm } from "../../ui/confirm";
import {
  ai, errText, nearby, PRESETS, presetFor,
  type Kind, type ModelInfo, type Profile, type ProviderInput, type ProviderView, type Route,
} from "../../util/aiApi";

// Settings → AI: the providers the assistant talks to, each with its
// key. Every save goes through the backend; the key never comes back, only
// whether one is set.

const KIND_LABEL: Record<Kind, string> = { anthropic: "Anthropic", openai: "OpenAI", compat: "OpenAI-compatible" };
const PROFILE_LABEL: Record<Profile, string> = {
  open_router: "OpenRouter", deep_seek: "DeepSeek", groq: "Groq", gemini: "Gemini", ollama: "Ollama",
  lm_studio: "LM Studio", open_ai_chat: "OpenAI (Chat Completions)", custom: "Other",
};

/** The effort levels a provider's defaults can pick (the chat header asks the
 *  backend, which knows each Anthropic model's own list). */
export const effortChoices = (kind: Kind, profile: Profile | null): string[] => {
  if (kind === "anthropic") return ["low", "medium", "high", "xhigh", "max"];
  if (kind === "openai") return ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
  switch (profile) {
    case "deep_seek": return ["none", "high"];
    case "open_router": case "groq": case "gemini": case "ollama": case "lm_studio": return ["low", "medium", "high"];
    default: return [];
  }
};

export const effortLabel = (kind: Kind, profile: Profile | null, e: string) =>
  kind === "compat" && profile === "deep_seek" ? (e === "none" ? "thinking off" : "thinking on") : e;

const emptyInput = (): ProviderInput => ({
  id: null, name: "Anthropic", kind: "anthropic", profile: null, base_url: "https://api.anthropic.com",
  key: { action: "keep" }, model: "claude-opus-5-5",
  defaults: { effort: "medium", max_tokens: 64000, show_reasoning: true, fallback: true },
  context_tokens: null, allow_data_collection: false, route: { mode: "system" }, is_default: false,
});

const fromView = (p: ProviderView): ProviderInput => ({
  id: p.id, name: p.name, kind: p.kind, profile: p.kind === "compat" ? p.profile : null, base_url: p.base_url,
  key: { action: "keep" }, model: p.model, defaults: { ...p.defaults }, context_tokens: p.context_tokens,
  allow_data_collection: p.allow_data_collection, route: p.route, is_default: p.is_default,
});

/** The address as typed, once it is an http or https URL. */
const parseUrl = (s: string): URL | null => {
  try {
    const u = new URL(s.trim());
    return u.protocol === "http:" || u.protocol === "https:" ? u : null;
  } catch {
    return null;
  }
};

/** Where a saved provider's key goes. */
const keyPlace = (v: ProviderView) => ({ host: v.host, plain: v.plain_http, route: v.route });

/** Whether two connections are the same one. */
const sameRoute = (a: Route, b: Route): boolean => {
  if (a.mode === "manual") return b.mode === "manual" && a.url === b.url;
  if (a.mode === "server") return b.mode === "server" && a.server_uuid === b.server_uuid;
  return a.mode === b.mode;
};

const field = "w-full h-10 sm:h-9 px-3 bg-zinc-950 border border-white/10 rounded-lg text-[13px] text-zinc-100 focus:outline-none focus:border-primary/50";
const label = "text-[11px] font-black text-zinc-500 uppercase tracking-wider";
const smallBtn = "h-10 sm:h-8 px-3 rounded-lg text-[11px] font-bold uppercase border transition-all flex items-center gap-1.5 disabled:opacity-40";

const Switch = ({ on, onChange, label: aria }: { on: boolean; onChange: (v: boolean) => void; label: string }) => (
  <button
    type="button"
    role="switch"
    aria-label={aria}
    aria-checked={on}
    onClick={() => onChange(!on)}
    className={`relative shrink-0 w-11 h-6 rounded-full transition-colors before:absolute before:-inset-y-2 before:inset-x-0 before:content-[''] ${on ? "bg-primary" : "bg-zinc-700"}`}
  >
    <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${on ? "left-[22px]" : "left-0.5"}`} />
  </button>
);

interface Props {
  servers: any[];
  onOpenChats?: () => void;
}

export const AiSettings = ({ servers, onOpenChats }: Props) => {
  const confirm = useConfirm();
  const [providers, setProviders] = useState<ProviderView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<ProviderInput | null>(null);
  // The editor's identity: new each time it opens, and kept through a save, so
  // saving a new provider (which gives it an id) doesn't remount the editor.
  const [editorKey, setEditorKey] = useState(0);
  const openEditor = (input: ProviderInput) => {
    setEditing(input);
    setEditorKey((k) => k + 1);
  };

  const reload = useCallback(async () => {
    try {
      setProviders(await ai.providers());
      setError(null);
    } catch (e) {
      setError(errText(e));
    }
  }, []);

  useEffect(() => { void reload(); }, [reload]);

  const remove = async (p: ProviderView) => {
    const ok = await confirm({ title: `Delete ${p.name}?`, message: "Its API key is deleted from this device. Chats that use it stay; pick another provider in them.", okLabel: "Delete", destructive: true, safeDefault: true });
    if (!ok) return;
    try { await ai.deleteProvider(p.id); setEditing(null); await reload(); } catch (e) { setError(errText(e)); }
  };

  const withdraw = async (p: ProviderView) => {
    try { await ai.withdrawConsent(p.id); await reload(); } catch (e) { setError(errText(e)); }
  };

  return (
    <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
      <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
        <Sparkles size={14} /> AI Assistant
      </div>

      <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 space-y-4 shadow-xl">
        <p className="text-[12px] text-zinc-400 leading-relaxed">
          Connect the AI providers the assistant uses, with your own API keys or a model on your network. Requests go from this device straight to the provider; keys stay on this device, encrypted, and are never synced or exported.
        </p>

        {onOpenChats && (
          <button
            type="button"
            onClick={onOpenChats}
            className="px-4 h-10 sm:h-9 bg-primary/10 border border-primary/30 text-primary rounded-xl text-xs font-bold uppercase hover:bg-primary hover:text-zinc-950 transition-all w-full flex items-center justify-center gap-2"
          >
            <MessagesSquare size={14} /> Chats
          </button>
        )}

        {error && <p className="text-[12px] text-rose-300 bg-rose-500/10 border border-rose-500/20 rounded-lg px-3 py-2">{error}</p>}

        <div className="space-y-2">
          {providers.length === 0 && <p className="text-[12px] text-zinc-500">No provider yet.</p>}
          {providers.map((p) => (
            <div key={p.id} className="flex items-center gap-3 px-3 py-2.5 rounded-xl bg-zinc-950/60 border border-white/5">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-1.5 text-[13px] font-semibold text-zinc-100">
                  {p.is_default && <Star size={12} className="text-primary shrink-0" fill="currentColor" aria-label="Default" />}
                  <span className="truncate">{p.name}</span>
                </div>
                <div className="text-[11px] text-zinc-500 truncate">
                  {p.kind === "compat" ? PROFILE_LABEL[p.profile] : KIND_LABEL[p.kind]} · {p.model || "no model"} · {p.host}
                  {p.plain_http && <span className="text-amber-400"> · not encrypted</span>}
                </div>
                <div className="text-[11px] text-zinc-500 flex items-center gap-2 mt-0.5">
                  <span className={`flex items-center gap-1 ${p.has_key ? "text-emerald-400/80" : ""}`}><KeyRound size={11} /> {p.has_key ? "Key set" : "No key"}</span>
                  {p.consent_at && (
                    <span>
                      · consent given{" "}
                      <button type="button" onClick={() => void withdraw(p)} className="underline hover:text-zinc-300">withdraw</button>
                    </span>
                  )}
                </div>
              </div>
              <button type="button" aria-label={`Edit ${p.name}`} onClick={() => openEditor(fromView(p))} className="h-10 w-10 sm:h-auto sm:w-auto sm:p-2 flex items-center justify-center rounded-lg text-zinc-400 hover:text-zinc-100 hover:bg-white/5">
                <Pencil size={14} />
              </button>
            </div>
          ))}
        </div>

        {editing ? (
          <ProviderEditor
            key={editorKey}
            input={editing}
            view={providers.find((p) => p.id === editing.id) ?? null}
            servers={servers}
            onCancel={() => setEditing(null)}
            onSaved={async (v) => { setEditing((e) => (e ? { ...e, id: v.id } : e)); await reload(); }}
            onDelete={editing.id ? () => { const v = providers.find((p) => p.id === editing.id); if (v) void remove(v); } : undefined}
          />
        ) : (
          <button
            type="button"
            onClick={() => openEditor(emptyInput())}
            className="px-4 h-10 sm:h-9 bg-zinc-900 border border-white/5 text-zinc-300 rounded-xl text-xs font-bold uppercase hover:bg-white/5 transition-all w-full flex items-center justify-center gap-2"
          >
            <Plus size={14} /> Add provider
          </button>
        )}
      </div>
    </section>
  );
};

interface EditorProps {
  input: ProviderInput;
  view: ProviderView | null;
  servers: any[];
  onCancel: () => void;
  onSaved: (v: ProviderView) => void | Promise<void>;
  onDelete?: () => void;
}

const ProviderEditor = ({ input, view, servers, onCancel, onSaved, onDelete }: EditorProps) => {
  const [f, setF] = useState<ProviderInput>(input);
  const [newKey, setNewKey] = useState("");
  const [replacing, setReplacing] = useState(!view?.has_key);
  // Where the saved key goes (null: no key saved). It doesn't follow the
  // address to another host, from https to plain http, or a plain-http
  // address to another connection: there it is entered again, or removed,
  // and a save that would keep it is refused (KEY_HOST).
  const [keyFor, setKeyFor] = useState(view?.has_key ? keyPlace(view) : null);
  const [busy, setBusy] = useState<string | null>(null);
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [models, setModels] = useState<ModelInfo[] | null>(null);
  const set = (patch: Partial<ProviderInput>) => setF((cur) => ({ ...cur, ...patch }));
  const efforts = effortChoices(f.kind, f.profile);
  const url = parseUrl(f.base_url);
  const plain = url ? url.protocol === "http:" : /^http:\/\//i.test(f.base_url.trim());
  // Why the saved key can't stay as it is (null: it can).
  const keyMoved: "host" | "http" | "route" | null =
    keyFor === null || url === null ? null
    : url.host !== keyFor.host ? "host"
    : !plain ? null
    : !keyFor.plain ? "http"
    : !sameRoute(f.route, keyFor.route) ? "route"
    : null;
  // The key field: to replace the key, for a saved key that can't stay, and
  // once a key is typed (moving the address back doesn't hide it).
  const askKey = replacing || keyMoved !== null || newKey !== "";

  const applyPreset = (label: string) => {
    const p = PRESETS.find((x) => x.label === label);
    if (!p) return;
    set({
      name: p.label === "Other (OpenAI-compatible)" ? "My server" : p.label,
      kind: p.kind, profile: p.profile, base_url: p.base, model: p.model,
      context_tokens: p.context ?? null,
      defaults: { ...f.defaults, effort: p.kind === "anthropic" ? "medium" : null, max_tokens: p.kind === "anthropic" ? 64000 : null },
    });
    setModels(null);
  };

  const save = async (): Promise<ProviderView | null> => {
    setBusy("save");
    setMsg(null);
    try {
      const key = askKey && newKey.trim() ? { action: "set" as const, value: newKey } : f.key;
      const v = await ai.saveProvider({ ...f, key });
      // From now on this editor edits the saved provider: a second Save
      // updates it instead of adding another one.
      setF((cur) => ({ ...cur, id: v.id, key: { action: "keep" } }));
      setNewKey("");
      setReplacing(false);
      setKeyFor(v.has_key ? keyPlace(v) : null);
      await onSaved(v);
      setMsg({ ok: true, text: "Saved." });
      return v;
    } catch (e) {
      setMsg({ ok: false, text: errText(e) });
      return null;
    } finally {
      setBusy(null);
    }
  };

  const test = async () => {
    const v = await save();
    if (!v) return;
    setBusy("test");
    try { setMsg({ ok: true, text: await ai.testProvider(v.id) }); } catch (e) { setMsg({ ok: false, text: errText(e) }); } finally { setBusy(null); }
  };

  const loadModels = async () => {
    const v = await save();
    if (!v) return;
    setBusy("models");
    try { setModels(await ai.models(v.id)); } catch (e) { setMsg({ ok: false, text: errText(e) }); } finally { setBusy(null); }
  };

  const route = f.route;
  const routeMode = route.mode;
  const setRoute = (r: Route) => set({ route: r });
  const isLocalPreset = f.kind === "compat" && (f.profile === "ollama" || f.profile === "lm_studio");
  const viaServer = routeMode === "server" || routeMode === "chat_server";
  const near = plain && url ? nearby(url.hostname) : null;
  const withKey = newKey.trim() !== "" || (keyFor !== null && f.key.action !== "remove");

  return (
    <div className="space-y-3 border-t border-white/5 pt-4">
      {!f.id && (
        <div className="space-y-1">
          <div className={label}>Preset</div>
          <select className={field} defaultValue="Anthropic" onChange={(e) => applyPreset(e.target.value)}>
            {PRESETS.map((p) => <option key={p.label}>{p.label}</option>)}
          </select>
        </div>
      )}

      <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
        <label className="space-y-1 block">
          <div className={label}>Name</div>
          <input className={field} value={f.name} maxLength={80} onChange={(e) => set({ name: e.target.value })} />
        </label>
        <label className="space-y-1 block">
          <div className={label}>Kind</div>
          <select
            className={field}
            value={f.kind === "compat" ? `compat:${f.profile ?? "custom"}` : f.kind}
            onChange={(e) => {
              const v = e.target.value;
              if (v.startsWith("compat:")) set({ kind: "compat", profile: v.slice(7) as Profile });
              else set({ kind: v as Kind, profile: null });
            }}
          >
            <option value="anthropic">Anthropic</option>
            <option value="openai">OpenAI</option>
            {(Object.keys(PROFILE_LABEL) as Profile[]).map((p) => <option key={p} value={`compat:${p}`}>{PROFILE_LABEL[p]} (OpenAI-compatible)</option>)}
          </select>
        </label>
      </div>

      <label className="space-y-1 block">
        <div className={label}>Address</div>
        <input className={field} value={f.base_url} placeholder={presetFor(f.kind, f.profile)?.base || "https://…"} onChange={(e) => set({ base_url: e.target.value })} spellCheck={false} dir="ltr" />
        {plain && <p className="text-[11px] text-amber-400">Not encrypted: plain http is only allowed for a server on this machine or network.</p>}
        {near === "cgnat" && <p className="text-[11px] text-amber-400">Carriers use 100.64/10 addresses too, so plain http there is only safe inside Tailscale or a similar private network.</p>}
        {(near === "cgnat" || near === "network") && !viaServer && (
          <p className="text-[11px] text-amber-400">On another network, such as hotel Wi-Fi or mobile data, the same address can belong to someone else, and what is sent goes to them.</p>
        )}
      </label>

      <div className="space-y-1">
        <div className={label}>API key</div>
        {view?.has_key && !askKey ? (
          <div className="flex items-center gap-2">
            <span className="text-[12px] text-emerald-400/90 flex items-center gap-1"><KeyRound size={12} /> Key set</span>
            <button type="button" className={`${smallBtn} border-white/10 text-zinc-300 hover:bg-white/5`} onClick={() => setReplacing(true)}>Replace</button>
            <button type="button" className={`${smallBtn} border-white/10 text-zinc-300 hover:bg-white/5`} onClick={() => { set({ key: { action: "remove" } }); setReplacing(true); }}>Remove</button>
          </div>
        ) : (
          <input
            className={field}
            type="password"
            autoComplete="off"
            value={newKey}
            placeholder={f.kind === "compat" ? "Optional for local servers" : "Paste the key"}
            onChange={(e) => setNewKey(e.target.value)}
            dir="ltr"
          />
        )}
        {keyMoved && f.key.action === "keep" && !newKey.trim() && (
          <p className="text-[11px] text-amber-400">
            {keyMoved === "host"
              ? `The saved key was for ${keyFor?.host}. Enter the key for ${url?.host}, or `
              : keyMoved === "http"
                ? "The saved key was for https, and plain http isn't encrypted. Enter it again to send it this way, or "
                : "The saved key was for another connection, and plain http isn't encrypted. Enter it again for this one, or "}
            <button type="button" className="underline hover:text-amber-300" onClick={() => { set({ key: { action: "remove" } }); setReplacing(true); }}>remove it</button>.
          </p>
        )}
        {f.key.action === "remove" && <p className="text-[11px] text-amber-400">The key is removed when you save.</p>}
      </div>

      <div className="space-y-1">
        <div className={label}>Model</div>
        <div className="flex gap-2">
          <input className={field} value={f.model} onChange={(e) => set({ model: e.target.value })} spellCheck={false} dir="ltr" list={models ? "ai-models" : undefined} />
          <button type="button" className={`${smallBtn} border-white/10 text-zinc-300 hover:bg-white/5 shrink-0`} disabled={!!busy} onClick={() => void loadModels()}>
            {busy === "models" ? <Loader2 size={12} className="animate-spin" /> : null} Load models
          </button>
        </div>
        {models && (
          <>
            <datalist id="ai-models">{models.map((m) => <option key={m.id} value={m.id}>{m.name ?? m.id}</option>)}</datalist>
            <select className={field} value="" onChange={(e) => e.target.value && set({ model: e.target.value })}>
              <option value="">{models.length} models: pick one…</option>
              {models.map((m) => <option key={m.id} value={m.id}>{m.name ? `${m.name} (${m.id})` : m.id}</option>)}
            </select>
          </>
        )}
      </div>

      <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
        {efforts.length > 0 && (
          <label className="space-y-1 block">
            <div className={label}>Effort</div>
            <select className={field} value={f.defaults.effort ?? ""} onChange={(e) => set({ defaults: { ...f.defaults, effort: e.target.value || null } })}>
              <option value="">Provider's default</option>
              {efforts.map((e) => <option key={e} value={e}>{effortLabel(f.kind, f.profile, e)}</option>)}
            </select>
          </label>
        )}
        <label className="space-y-1 block">
          <div className={label}>Max output tokens</div>
          <input
            className={field}
            inputMode="numeric"
            value={f.defaults.max_tokens ?? ""}
            placeholder="Provider's default"
            onChange={(e) => {
              const n = parseInt(e.target.value.replace(/\D/g, ""), 10);
              set({ defaults: { ...f.defaults, max_tokens: Number.isFinite(n) && n > 0 ? n : null } });
            }}
          />
        </label>
        {f.kind === "compat" && (
          <label className="space-y-1 block">
            <div className={label}>Context size (tokens)</div>
            <input
              className={field}
              inputMode="numeric"
              value={f.context_tokens ?? ""}
              placeholder={isLocalPreset ? "4096" : "128000"}
              onChange={(e) => {
                const n = parseInt(e.target.value.replace(/\D/g, ""), 10);
                set({ context_tokens: Number.isFinite(n) && n > 0 ? n : null });
              }}
            />
            {f.profile === "ollama" && <p className="text-[11px] text-zinc-500 leading-snug">Ollama gives a model 4k of context on machines with less than 24 GB of graphics memory, and silently drops the start of a longer chat. Raise it in Ollama's settings or with OLLAMA_CONTEXT_LENGTH, then enter the new size here.</p>}
            {f.profile === "lm_studio" && <p className="text-[11px] text-zinc-500 leading-snug">The context length is set when the model is loaded in LM Studio.</p>}
          </label>
        )}
      </div>

      {f.kind === "anthropic" && (
        <div className="space-y-2">
          <div className="flex items-center justify-between gap-3">
            <span className="text-[12px] text-zinc-300">Show reasoning summaries</span>
            <Switch on={f.defaults.show_reasoning} label="Show reasoning summaries" onChange={(v) => set({ defaults: { ...f.defaults, show_reasoning: v } })} />
          </div>
          <div className="flex items-center justify-between gap-3">
            <span className="text-[12px] text-zinc-300">Retry on another model when declined</span>
            <Switch on={f.defaults.fallback} label="Retry on another model when declined" onChange={(v) => set({ defaults: { ...f.defaults, fallback: v } })} />
          </div>
        </div>
      )}

      {f.kind === "compat" && f.profile === "open_router" && (
        <div className="flex items-center justify-between gap-3">
          <span className="text-[12px] text-zinc-300">Allow upstream providers that may store or train on data</span>
          <Switch on={f.allow_data_collection} label="Allow providers that may store or train on data" onChange={(v) => set({ allow_data_collection: v })} />
        </div>
      )}

      <div className="space-y-1">
        <div className={label}>Connection</div>
        <select
          className={field}
          value={routeMode}
          onChange={(e) => {
            const m = e.target.value as Route["mode"];
            if (m === "manual") setRoute({ mode: "manual", url: route.mode === "manual" ? route.url : "" });
            else if (m === "server") setRoute({ mode: "server", server_uuid: route.mode === "server" ? route.server_uuid : (servers.find((s) => s.uuid)?.uuid ?? "") });
            else setRoute({ mode: m } as Route);
          }}
        >
          <option value="system">System proxy settings</option>
          <option value="direct">Direct, no proxy</option>
          <option value="manual">Through a proxy…</option>
          <option value="server">Through one of my servers…</option>
          <option value="chat_server">Through the chat's own server</option>
        </select>
        {route.mode === "manual" && (
          <input className={field} value={route.url} placeholder="socks5h://host:1080 or http://host:3128" onChange={(e) => setRoute({ mode: "manual", url: e.target.value })} spellCheck={false} dir="ltr" />
        )}
        {route.mode === "manual" && route.url.includes(":********@") && (
          <p className="mt-1 text-[11px] text-zinc-500">The saved password shows as ********; leave it to keep it. It goes only to this same proxy: for another address, type it again.</p>
        )}
        {route.mode === "server" && (
          <select className={field} value={route.server_uuid} onChange={(e) => setRoute({ mode: "server", server_uuid: e.target.value })}>
            {servers.filter((s) => s.uuid).map((s) => <option key={s.uuid} value={s.uuid}>{s.name}</option>)}
          </select>
        )}
        {viaServer && (
          <p className="text-[11px] text-zinc-500 leading-snug">
            Requests go out from that server, through its open SSH session, so the provider sees the server's address.{" "}
            {plain
              ? "Plain http has no TLS: that server, and its network when the address isn't on the server itself, see the key and the content."
              : "TLS still runs from this device to the provider: the server sees which host is contacted, never the key or the content."}
            {" "}The session must be open.
            {route.mode === "chat_server" && " Chats with no server can't use this route."}
          </p>
        )}
        {route.mode === "chat_server" && plain && withKey && (
          <p className="text-[11px] text-amber-400 leading-snug">With plain http, the API key goes unencrypted to each server you chat about.</p>
        )}
      </div>

      <div className="flex items-center justify-between gap-3">
        <span className="text-[12px] text-zinc-300">Default for new chats</span>
        <Switch on={f.is_default} label="Default for new chats" onChange={(v) => set({ is_default: v })} />
      </div>

      {msg && <p className={`text-[12px] rounded-lg px-3 py-2 border ${msg.ok ? "text-emerald-300 bg-emerald-500/10 border-emerald-500/20" : "text-rose-300 bg-rose-500/10 border-rose-500/20"}`}>{msg.text}</p>}

      <div className="flex flex-wrap gap-2">
        <button type="button" disabled={!!busy} onClick={() => void save()} className={`${smallBtn} bg-primary/10 border-primary/30 text-primary hover:bg-primary hover:text-zinc-950`}>
          {busy === "save" && <Loader2 size={12} className="animate-spin" />} Save
        </button>
        <button type="button" disabled={!!busy} onClick={() => void test()} className={`${smallBtn} border-white/10 text-zinc-300 hover:bg-white/5`}>
          {busy === "test" ? <Loader2 size={12} className="animate-spin" /> : <PlugZap size={12} />} Test
        </button>
        <button type="button" onClick={onCancel} className={`${smallBtn} border-white/10 text-zinc-400 hover:bg-white/5`}>Close</button>
        {onDelete && (
          <button type="button" onClick={onDelete} className={`${smallBtn} ml-auto border-rose-500/30 text-rose-300 hover:bg-rose-500/10`}>
            <Trash2 size={12} /> Delete
          </button>
        )}
      </div>
      <p className="text-[11px] text-zinc-500">Test sends one tiny request, which costs a few tokens.</p>
    </div>
  );
};
