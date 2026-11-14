// The assistant's commands and types (backend: src-tauri/src/ai/). The page
// never holds an API key: providers come back with `has_key` only, and a key
// goes one way, into `ai_provider_save`.

import { invoke, Channel } from "@tauri-apps/api/core";
import { segments } from "./hiddenChars";

export type Kind = "anthropic" | "openai" | "compat";
export type Profile = "open_router" | "deep_seek" | "groq" | "gemini" | "ollama" | "lm_studio" | "open_ai_chat" | "custom";

/** How a provider's traffic leaves this device. */
export type Route =
  | { mode: "system" }
  | { mode: "direct" }
  | { mode: "manual"; url: string }
  | { mode: "server"; server_uuid: string }
  | { mode: "chat_server" };

export interface ProviderDefaults {
  effort: string | null;
  max_tokens: number | null;
  show_reasoning: boolean;
  fallback: boolean;
}

export interface ProviderView {
  id: number;
  name: string;
  kind: Kind;
  profile: Profile;
  base_url: string;
  has_key: boolean;
  model: string;
  defaults: ProviderDefaults;
  context_tokens: number | null;
  allow_data_collection: boolean;
  route: Route;
  consent_at: number | null;
  is_default: boolean;
  host: string;
  plain_http: boolean;
  local: boolean;
}

export type KeyChange = { action: "keep" } | { action: "set"; value: string } | { action: "remove" };

export interface ProviderInput {
  id: number | null;
  name: string;
  kind: Kind;
  profile: Profile | null;
  base_url: string;
  key: KeyChange;
  model: string;
  defaults: ProviderDefaults;
  context_tokens: number | null;
  allow_data_collection: boolean;
  route: Route;
  is_default: boolean;
}

export interface ChatSettings {
  provider_id: number | null;
  model: string;
  effort: string | null;
  max_tokens: number | null;
  show_reasoning: boolean;
  fallback: boolean;
}

export interface ChatSummary {
  id: number;
  title: string;
  server_uuid: string | null;
  server_label: string | null;
  provider_name: string | null;
  model: string;
  last_activity: number;
  turns: number;
  state: "ok" | "failed" | "running";
}

export interface Usage {
  input: number;
  cache_read: number;
  cache_write: number;
  output: number;
  cost?: number | null;
}

/** A server's permission level. */
export type Level = "suggest" | "ask_all" | "ask" | "auto" | "full";

export const LEVELS: { id: Level; label: string; note: string }[] = [
  { id: "suggest", label: "Suggest only", note: "The assistant can't reach the server. It writes commands for you to run." },
  { id: "ask_all", label: "Ask every command", note: "Every command and every file change waits for your approval, even commands on the always-run list. Reading files (except ones that may hold secrets) and listing folders still run by themselves." },
  { id: "ask", label: "Ask", note: "Read-only commands run by themselves. Anything else waits for your approval." },
  { id: "auto", label: "Auto", note: "Commands and file changes run by themselves. Ones that look risky still ask: removal (any rm), sudo, the network and package installs, accounts, files that run at login or boot, and the like. But a command can hide what it does, so anything may run." },
  { id: "full", label: "Full access", note: "Everything runs without asking. For this session only: it ends when the tab closes, on Lock and when the app quits." },
];

/** The notes for a files-only server (FTP), where no command runs. */
const FILES_ONLY_NOTES: Partial<Record<Level, string>> = {
  suggest: "The assistant can't reach the server. It tells you what to change, and you make the changes.",
  ask_all: "Every file change waits for your approval. Reading files (except ones that may hold secrets) and listing folders run by themselves. This server runs no commands, so this is the same as Ask.",
  ask: "Reading files (except ones that may hold secrets) and listing folders run by themselves. Every file change waits for your approval.",
  auto: "File changes run by themselves. Ones that look risky still ask: files that may hold secrets, and files that run at login, at boot or on a schedule.",
};

/** What a level does; on a files-only server (FTP) it speaks of files only. */
export const levelNote = (id: Level, filesOnly?: boolean | null): string | undefined =>
  (filesOnly ? FILES_ONLY_NOTES[id] : undefined) ?? LEVELS.find((l) => l.id === id)?.note;

/** A server's level as the AI tab shows it. */
export interface LevelInfo {
  /** What calls are decided at now: Full access while it is on in this session. */
  level: Level;
  /** The level saved for the server (Full access is never saved). */
  saved: Level;
  /** Whether the server answered with a POSIX shell when a chat about it last
   *  asked; null when no chat has asked yet. Auto isn't offered without one. */
  posix: boolean | null;
  /** Files only (an FTP server): Auto works there without a POSIX shell. */
  files_only?: boolean | null;
  /** The server's name now: what turning on Full access asks to be typed. */
  name: string | null;
}

/** A server whose level isn't the default, for the server cards. */
export interface ServerLevel {
  server_uuid: string;
  level: Level;
}

/** Sent on `window` after a server's level changed, so every view of it follows. */
export const AI_LEVEL_EVENT = "submarine-ai-level";
export type AiLevelDetail = { serverUuid: string };

/** Sent on `window` to show the AI page, opened on a chat when one is given. */
export const AI_OPEN_EVENT = "submarine-ai-open";
export type AiOpenDetail = { chatId?: number };

/** Sent on `window` to open (or show) a saved server's session, then its AI
 *  tab on the chat given: "Open the server" on the AI page. */
export const AI_OPEN_SERVER_EVENT = "submarine-ai-open-server";
export type AiOpenServerDetail = { serverUuid: string; chatId: number | null };

/** Sent on `window` after chats were deleted (`all`: every one), so a view
 *  still showing one lets it go. */
export const AI_CHATS_DELETED_EVENT = "submarine-ai-chats-deleted";
export type AiChatsDeletedDetail = { ids?: number[]; all?: boolean };

/** A write or an edit, shown as a diff before it runs. `old` is null for a new file. */
export interface Preview {
  old: string | null;
  new: string;
}

/** One tool call as the chat shows it (backend: act.rs CallRecord). */
export interface CallRecord {
  id: string;
  tool: string;
  input: Record<string, unknown>;
  path?: string;
  /** The command that ran, when the user changed it. */
  command?: string;
  how?: string;
  state: "pending" | "done" | "failed" | "denied" | "refused" | "skipped" | "stopped" | "interrupted";
  summary: string;
  result: string;
  out?: string;
  err?: string;
  exit?: number;
  action_id?: number;
  preview?: Preview;
  red?: string;
  level?: string;
}

/** The user's answer to a card that waits. */
export type Answer =
  | { decision: "allow"; always: boolean }
  | { decision: "deny"; note: string | null }
  | { decision: "edit"; command: string };

export interface ChatItem {
  kind: "user" | "assistant" | "note";
  seq?: number;
  text?: string;
  /** An assistant turn's tool calls. */
  calls?: CallRecord[];
  /** A user turn that only carries tool results: not shown as a message. */
  results?: boolean;
  thinking?: string;
  model?: string | null;
  usage?: Usage;
  answered_by?: string | null;
  stop?: string;
  failed?: { error: string; partial: string; stopped: boolean };
  note?: string;
}

export interface ChatView {
  id: number;
  title: string;
  server_uuid: string | null;
  server_label: string | null;
  settings: ChatSettings;
  /** What the facts probe found out about the server. */
  facts: string | null;
  /** Whether those facts went to the model (found when the chat started). */
  facts_sent: boolean;
  items: ChatItem[];
  /** How full the model's context was at the last request: input tokens, cache
   *  included, and the model's limit when it is known. */
  context: ContextUse | null;
  /** Text for the composer: the summary a "Continue in a new chat" wrote. */
  draft: string | null;
  /** The activity entries of this chat whose change was undone. */
  undone: number[];
}

export interface ContextUse {
  used: number;
  limit: number | null;
}

/** From this share of the context on, the chat offers a new chat with a summary. */
export const CONTEXT_WARN = 0.8;

export interface ModelInfo {
  id: string;
  name: string | null;
  context: number | null;
  max_output: number | null;
  efforts: string[];
}

export type AiEvent =
  | { type: "run_started"; run_id: number; chat_id: number; model: string }
  | { type: "model"; model: string }
  | { type: "text_delta"; text: string }
  | { type: "thinking_delta"; text: string }
  | { type: "notice"; kind: string; text: string }
  | { type: "usage"; usage: Usage }
  | { type: "context"; used: number; limit: number | null }
  | { type: "tool_call"; call_id: string; tool: string; input: Record<string, unknown>; needs: "none" | "approval"; reason: string | null; red: string | null; path: string | null; preview: Preview | null }
  | { type: "tool_running"; call_id: string; how: string; command: string | null }
  | { type: "tool_output"; call_id: string; stream: "out" | "err"; text: string }
  | { type: "tool_result"; call_id: string; record: CallRecord }
  | { type: "run_finished"; stop: string; error: string | null };

/** An entry of a server's activity log. */
export interface ActionView {
  id: number;
  chat_id: number | null;
  server_uuid: string | null;
  at: number;
  tool: string;
  summary: string;
  decision: string;
  level: string;
  exit_code: number | null;
  can_undo: boolean;
  /** The change was undone (it can't be undone again). */
  undone: boolean;
  /** The file a write or an edit changed. */
  path: string | null;
  /** The outside agent the call came from (the agent gateway); null: the assistant's own. */
  agent?: string | null;
}

export interface Preset {
  label: string;
  kind: Kind;
  profile: Profile | null;
  base: string;
  model: string;
  context?: number;
}

export const PRESETS: Preset[] = [
  { label: "Anthropic", kind: "anthropic", profile: null, base: "https://api.anthropic.com", model: "claude-opus-5-5" },
  { label: "OpenAI", kind: "openai", profile: null, base: "https://api.openai.com/v1", model: "" },
  { label: "OpenRouter", kind: "compat", profile: "open_router", base: "https://openrouter.ai/api/v1", model: "" },
  { label: "DeepSeek", kind: "compat", profile: "deep_seek", base: "https://api.deepseek.com", model: "deepseek-v4-pro" },
  { label: "Groq", kind: "compat", profile: "groq", base: "https://api.groq.com/openai/v1", model: "" },
  { label: "Gemini", kind: "compat", profile: "gemini", base: "https://generativelanguage.googleapis.com/v1beta/openai", model: "" },
  { label: "Ollama", kind: "compat", profile: "ollama", base: "http://localhost:11434/v1", model: "", context: 4096 },
  { label: "LM Studio", kind: "compat", profile: "lm_studio", base: "http://localhost:1234/v1", model: "", context: 4096 },
  { label: "Other (OpenAI-compatible)", kind: "compat", profile: "custom", base: "", model: "" },
];

export const presetFor = (kind: Kind, profile: Profile | null): Preset | undefined =>
  PRESETS.find((p) => p.kind === kind && (kind !== "compat" || p.profile === profile));

/** Where a plain-http provider's host is: this machine, the 100.64/10 range
 *  (Tailscale's, which carriers use too), or another nearby address or a
 *  name (allowed when it resolves to one). */
export type Nearby = "loopback" | "cgnat" | "network";

export const nearby = (hostname: string): Nearby => {
  const h = hostname.toLowerCase().replace(/^\[(.*)\]$/, "$1");
  if (h === "localhost" || h === "::1" || /^127\.\d+\.\d+\.\d+$/.test(h)) return "loopback";
  const m = /^100\.(\d+)\.\d+\.\d+$/.exec(h);
  return m && (Number(m[1]) & 0xc0) === 0x40 ? "cgnat" : "network";
};

// Plain http has no TLS, so the note says what the data crosses on its way.
const plainNote = (p: ProviderView): string => {
  let name = p.host;
  try { name = new URL(p.base_url).hostname; } catch { /* the host as saved */ }
  const here = nearby(name) === "loopback";
  // Through a server, the address is reached from that server's side.
  if (p.route.mode === "server" || p.route.mode === "chat_server") {
    const from = p.route.mode === "server" ? "one of your servers" : "the server each chat is about";
    return here
      ? `This address is reached from ${from}, through its SSH session, so the data goes to that server.`
      : `This address is reached from ${from}, through its SSH session, then with plain http, so the data crosses that server's network unencrypted.`;
  }
  return here
    ? "This server is on this machine, so the data stays here."
    : "This server is on your network and is reached with plain http, so the data crosses that network unencrypted.";
};

/** The provider's own data rules, where they matter: by its preset, and by
 *  its address for one added as "Other". Plain http first: whatever the
 *  provider, the data then goes unencrypted. */
export const consentNote = (p: ProviderView): string | null => {
  if (p.plain_http) return plainNote(p);
  if (p.kind !== "compat") return null;
  const host = p.host.toLowerCase();
  const at = (domain: string) => host === domain || host.endsWith(`.${domain}`);
  if (p.profile === "gemini" || at("generativelanguage.googleapis.com")) {
    return "On Google's free tier, Google may use this data to improve its products, and people may read it. Google says not to send sensitive data on the free tier.";
  }
  if (p.profile === "deep_seek" || at("deepseek.com")) return "DeepSeek stores this data in China.";
  if (p.profile === "open_router") {
    return p.allow_data_collection
      ? "OpenRouter passes requests on to other providers, and this provider is set to allow ones that may keep or train on data."
      : "OpenRouter passes requests on to other providers. Submarine asks it to use only providers that don't keep or train on data; this can be changed in the provider's settings.";
  }
  // Only the OpenRouter preset asks it to keep away from such providers.
  if (at("openrouter.ai")) return "OpenRouter passes requests on to other providers, and some may keep or train on data. Add it with the OpenRouter preset to have Submarine ask for providers that don't.";
  return null;
};

/** An error from a command, without its `[AI] CODE:` tag. */
export const errText = (e: unknown): string => {
  const s = typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
  return s.replace(/^\[[A-Z]+\]\s*[A-Z_]+(?::[^:\s]*)?:\s*/, "").replace(/^\[[A-Z]+\]\s*/, "");
};

/** The provider id a send needs consent for, if that was the error. Only the
 *  backend's own tag, whole: other errors can quote a provider's text, and
 *  that text could carry one. */
export const consentNeeded = (e: unknown): number | null => {
  const m = /^\[AI\] CONSENT_NEEDED:(\d+)$/.exec(String(e).trim());
  return m ? Number(m[1]) : null;
};

/** What Undo asks when the file changed since the assistant wrote it. The
 *  path shows hidden characters as escapes, as the cards do. */
/** `byAgent`: an outside agent made the change (the agent gateway), not the assistant. */
export const undoChangedQuestion = (path: string, byAgent = false) => ({
  title: "The file changed since",
  message: `${segments(path, { blankRuns: false }).map((s) => s.text).join("")}\n\nIt isn't what ${byAgent ? "the agent" : "the assistant"} wrote any more. Put the earlier content back anyway? What is in it now is lost.`,
  okLabel: "Put it back",
  destructive: true,
  safeDefault: true,
});

export const ai = {
  notice: () => invoke<string | null>("ai_store_notice"),
  providers: () => invoke<ProviderView[]>("ai_providers_list"),
  saveProvider: (input: ProviderInput) => invoke<ProviderView>("ai_provider_save", { input }),
  deleteProvider: (id: number) => invoke<void>("ai_provider_delete", { id }),
  testProvider: (id: number) => invoke<string>("ai_provider_test", { id }),
  models: (id: number) => invoke<ModelInfo[]>("ai_models", { id }),
  effortLevels: (providerId: number, model: string) => invoke<string[]>("ai_effort_levels", { providerId, model }),
  /** The yes to the consent dialog, for the host it named: refused when the
   *  provider's address has changed since (CONSENT_HOST). */
  consent: (providerId: number, host: string) => invoke<void>("ai_consent", { providerId, host }),
  withdrawConsent: (providerId: number) => invoke<void>("ai_consent_withdraw", { providerId }),
  chats: (filter: { kind: "all" } | { kind: "plain" } | { kind: "server"; server_uuid: string }, search: string) =>
    invoke<ChatSummary[]>("ai_chats_list", { filter, search }),
  createChat: (providerId: number | null, model: string | null, server?: { serverUuid: string; sessionId: string | null }) =>
    invoke<ChatSummary>("ai_chat_create", { input: { provider_id: providerId, model, server_uuid: server?.serverUuid ?? null, session_id: server?.sessionId ?? null } }),
  lastChat: (serverUuid: string) => invoke<number | null>("ai_last_chat", { serverUuid }),
  loadChat: (chatId: number) => invoke<ChatView>("ai_chat_load", { chatId }),
  setChatSettings: (chatId: number, settings: ChatSettings) => invoke<ChatSettings>("ai_chat_settings", { chatId, settings }),
  renameChat: (chatId: number, title: string) => invoke<void>("ai_chat_rename", { chatId, title }),
  /** Deletes a chat; every view showing it lets it go (AI_CHATS_DELETED_EVENT). */
  deleteChat: async (chatId: number, clearActivity = false) => {
    await invoke<void>("ai_chat_delete", { chatId, clearActivity });
    window.dispatchEvent(new CustomEvent<AiChatsDeletedDetail>(AI_CHATS_DELETED_EVENT, { detail: { ids: [chatId] } }));
  },
  deleteAllChats: async (clearActivity = false) => {
    await invoke<void>("ai_chats_delete_all", { clearActivity });
    window.dispatchEvent(new CustomEvent<AiChatsDeletedDetail>(AI_CHATS_DELETED_EVENT, { detail: { all: true } }));
  },
  /** `sessionId`: the open session of the chat's server, for its tools. */
  send: (chatId: number, text: string, onEvent: (e: AiEvent) => void, sessionId: string | null = null) => {
    const channel = new Channel<AiEvent>();
    channel.onmessage = onEvent;
    return invoke<number>("ai_send", { input: { chat_id: chatId, text, session_id: sessionId }, onEvent: channel });
  },
  retry: (chatId: number, onEvent: (e: AiEvent) => void, sessionId: string | null = null) => {
    const channel = new Channel<AiEvent>();
    channel.onmessage = onEvent;
    return invoke<number>("ai_retry", { chatId, sessionId, onEvent: channel });
  },
  stop: (runId: number) => invoke<void>("ai_stop", { runId }),
  /** Stops every run that acts through this session, from the AI tab or the AI page: the AI terminal's Ctrl+C. */
  stopSession: (sessionId: string) => invoke<void>("ai_stop_session", { sessionId }),
  decide: (runId: number, callId: string, answer: Answer) => invoke<void>("ai_decide", { runId, callId, answer }),
  /** The level calls are decided at: with `sessionId`, Full access when it is on there. */
  level: (serverUuid: string, sessionId: string | null = null) => invoke<Level>("ai_level_get", { serverUuid, sessionId }),
  levelInfo: (serverUuid: string, sessionId: string | null) => invoke<LevelInfo>("ai_level_info", { serverUuid, sessionId }),
  /**
   * Saves a server's level, or turns on Full access in the open session
   * `sessionId`, which needs the server's name typed (`confirmName`). Any
   * other level ends Full access on that server.
   */
  setLevel: async (serverUuid: string, level: Level, opts: { sessionId?: string | null; confirmName?: string } = {}) => {
    const l = await invoke<Level>("ai_level_set", { serverUuid, level, sessionId: opts.sessionId ?? null, confirmName: opts.confirmName ?? null });
    window.dispatchEvent(new CustomEvent<AiLevelDetail>(AI_LEVEL_EVENT, { detail: { serverUuid } }));
    return l;
  },
  /** Servers whose level isn't the default (Auto and Full access get a red mark). */
  serverLevels: () => invoke<ServerLevel[]>("ai_server_levels"),
  alwaysList: (serverUuid: string) => invoke<string[]>("ai_always_list", { serverUuid }),
  alwaysRemove: (serverUuid: string, command: string) => invoke<void>("ai_always_remove", { serverUuid, command }),
  activity: (serverUuid: string | null) => invoke<ActionView[]>("ai_activity", { serverUuid }),
  /** Clears the activity log, of one server or of all. Undo goes with it. */
  clearActivity: (serverUuid: string | null) => invoke<void>("ai_activity_clear", { serverUuid }),
  terminalReplay: (chatId: number) => invoke<CallRecord[]>("ai_terminal_replay", { chatId }),
  /**
   * Puts back what a change replaced. `path` is the file the card or the log
   * row shows, and `chatId` the chat whose card it is (null from the log): an
   * entry that isn't that change is refused (NO_ACTION).
   */
  undo: (actionId: number, sessionId: string, force: boolean, path: string, chatId: number | null) =>
    invoke<string>("ai_undo", { actionId, sessionId, force, path, chatId }),
  /**
   * "Continue in a new chat": one request without tools writes a summary of
   * the chat; a new chat on the same server and settings gets it as a draft.
   * The old chat stays.
   */
  continueChat: (chatId: number, sessionId: string | null) =>
    invoke<{ chat: ChatSummary; draft: string }>("ai_chat_continue", { chatId, sessionId }),
  /** Stops whatever runs for this chat (the summary of continueChat included). */
  stopChat: (chatId: number) => invoke<void>("ai_chat_stop", { chatId }),
  /** Saves the open profile's assistant data now, for Lock to ask when it can't. */
  persist: () => invoke<void>("ai_persist"),
};
