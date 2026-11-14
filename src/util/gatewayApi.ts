// The agent gateway's page side (src-tauri/src/gateway/hub.rs): AI agents on
// this computer ask for access to a server, the user grants it here, and
// every operation shows as a card. Desktop only.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { AiEvent, Answer, Preview } from "./aiApi";

export type GatewayOp =
  | "facts"
  | "commands.read"
  | "commands.run"
  | "files.browse"
  | "files.read"
  | "files.write"
  | "files.edit"
  | "files.delete"
  | "files.download"
  | "files.upload";

/** How a grant's operations run: a card for each, or some without one. */
export type GatewayMode = "ask_each" | "read_only_runs" | "within_grant";

/** One process of the chain that opened a connection, as the system says. */
export interface AgentProc {
  pid: number;
  /** Its program file; empty when the system won't say. */
  program: string;
}

export interface AgentConnection {
  id: number;
  /** Seconds since 1970. */
  since: number;
  /** What the agent says it is: its own claim. */
  name: string | null;
  version: string | null;
  /** The process that connected first, then the one that started it, and so on. */
  chain: AgentProc[];
  /** It came through `submarine mcp`. */
  bridged: boolean;
}

export interface AgentRequest {
  id: string;
  connection: number;
  /** The agent's own sentence. */
  purpose: string;
  ops: GatewayOp[];
  /** The folders it named for its file operations, as it wrote them. */
  paths: string[];
  /** The agent's hint at a server ("staging"): it picks nothing by itself. */
  server_hint: string | null;
  minutes: number | null;
  at: number;
}

export interface AgentGrant {
  id: string;
  connection: number;
  server_uuid: string;
  session: string;
  /** The server's name as the agent sees it. */
  label: string;
  ops: GatewayOp[];
  /** Where its file operations may go, as resolved on the server. */
  folders: string[];
  /** An FTP server: files only. */
  ftp: boolean;
  mode: GatewayMode;
  minutes_left: number;
}

/** An operation that waits for the user's answer. */
export interface AgentCard {
  call_id: string;
  grant: string;
  connection: number;
  server_uuid: string;
  tool: string;
  input: Record<string, unknown>;
  reason: string;
  red: string | null;
  /** A file operation's path as resolved, and what a change would write. */
  path: string | null;
  preview: Preview | null;
  at: number;
}

export interface GatewayOverview {
  enabled: boolean;
  listening: boolean;
  /** Why it isn't listening though it is on, or why the saved settings weren't used. */
  error: string | null;
  longest_minutes: number;
  profile_open: boolean;
  /** This program's own file: what an agent is told to start. */
  program: string;
  /** The operations this version can grant. */
  offered: GatewayOp[];
  connections: AgentConnection[];
  requests: AgentRequest[];
  grants: AgentGrant[];
  cards: AgentCard[];
}

/** A row of the agents' log: a connection, a request, an answer, an end. */
export interface AgentEventRow {
  id: number;
  at: number;
  kind: string;
  agent: string;
  connection: number;
  grant: string | null;
  server_uuid: string | null;
  detail: string;
}

/** An operation's progress: the assistant's own events, with whose they are. */
export interface GatewayCallEvent {
  grant: string;
  connection: number;
  server_uuid: string;
  session: string;
  event: AiEvent;
}

export const OPS: Record<GatewayOp, { label: string; changes: boolean }> = {
  facts: { label: "See what the server is (system, shell, user)", changes: false },
  "commands.read": { label: "Run commands that only read", changes: false },
  "commands.run": { label: "Run commands that change things", changes: true },
  "files.browse": { label: "List folders", changes: false },
  "files.read": { label: "Read files", changes: false },
  "files.write": { label: "Create files", changes: true },
  "files.edit": { label: "Change files", changes: true },
  "files.delete": { label: "Delete files", changes: true },
  "files.download": { label: "Download files to this computer", changes: false },
  "files.upload": { label: "Upload files to the server", changes: true },
};

/** It works on files, so it needs folders. */
export const isFileOp = (o: GatewayOp) => o.startsWith("files.");

/** It runs commands, which an FTP server can't. */
export const isCommandOp = (o: GatewayOp) => o.startsWith("commands.");

export const MODES: Record<GatewayMode, string> = {
  ask_each: "Ask me for each operation",
  read_only_runs: "Run read-only operations without asking",
  within_grant: "Run everything this access allows without asking",
};

/** The choices for how long one access lasts, in minutes. */
export const DURATIONS = [15, 60, 240, 480];

export const minutesText = (m: number) => (m < 60 ? `${m} min` : m % 60 === 0 ? `${m / 60} h` : `${Math.floor(m / 60)} h ${m % 60} min`);

/** The last part of a program's path: what most people know it by. */
export const programName = (p: string) => p.split(/[\\/]/).pop() || p;

/** The command that adds Submarine to Claude Code, and the block other agents take. */
export const setupCommands = (program: string) => ({
  claudeCode: `claude mcp add submarine -- "${program}" mcp`,
  json: JSON.stringify({ mcpServers: { submarine: { command: program, args: ["mcp"] } } }, null, 2),
});

export const gateway = {
  overview: () => invoke<GatewayOverview>("gateway_overview"),
  configure: (change: { enabled?: boolean; longestMinutes?: number }) =>
    invoke<GatewayOverview>("gateway_configure", { enabled: change.enabled ?? null, longestMinutes: change.longestMinutes ?? null }),
  /** `folders`: for the file operations, as the user typed them; the backend resolves them on the server. */
  approve: (request: string, serverUuid: string, ops: GatewayOp[], folders: string[], mode: GatewayMode, minutes: number, label: string | null) =>
    invoke<void>("gateway_approve", { request, serverUuid, ops, folders, mode, minutes, label }),
  deny: (request: string, note: string | null) => invoke<void>("gateway_deny", { request, note }),
  revoke: (access: string) => invoke<void>("gateway_revoke", { access }),
  disconnect: (connection: number) => invoke<void>("gateway_disconnect", { connection }),
  decide: (callId: string, answer: Answer) => invoke<void>("gateway_decide", { callId, answer }),
  events: () => invoke<AgentEventRow[]>("gateway_events"),
  /** Anything the panel shows changed: ask for the overview again. */
  onChanged: (f: () => void): Promise<UnlistenFn> => listen("gateway-changed", () => f()),
  onCall: (f: (e: GatewayCallEvent) => void): Promise<UnlistenFn> => listen<GatewayCallEvent>("gateway-call", (e) => f(e.payload)),
};
