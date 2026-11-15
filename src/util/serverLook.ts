// How a saved server looks wherever it is listed: the Servers page's cards and
// rows, the + menu after the tabs, and the tabs themselves.
import type { CSSProperties } from "react";
import { Cable, FolderSync, Monitor, ScreenShare, Terminal } from "lucide-react";

// The kind of connection as a server's avatar shows it, and the port each one
// uses by default: another port is shown after the host.
export const PROTOCOL_ICON: Record<string, typeof Terminal> = {
  ssh: Terminal, telnet: Cable, rdp: Monitor, vnc: ScreenShare,
  ftp: FolderSync, ftps: FolderSync, "ftps-implicit": FolderSync,
};
export const USUAL_PORT: Record<string, number> = { ssh: 22, telnet: 23, rdp: 3389, vnc: 5900, ftp: 21, ftps: 21, "ftps-implicit": 990 };
export const protocolOf = (s: any): string => (s?.protocol && PROTOCOL_ICON[s.protocol] ? s.protocol : "ssh");

// A server's or folder's own colour as its avatar's fill, edge and glyph, the
// glyph a little lighter so it reads on the dark card; the accent without one.
export const hexRgb = (c?: string | null): [number, number, number] | null => {
  const m = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec((c || "").trim());
  if (!m) return null;
  const h = m[1].length === 3 ? m[1].replace(/./g, (x) => x + x) : m[1];
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16)) as [number, number, number];
};
export const avatarStyle = (color?: string | null): CSSProperties => {
  const rgb = hexRgb(color);
  if (!rgb) return { background: "rgb(var(--primary) / 0.16)", borderColor: "rgb(var(--primary) / 0.4)", color: "rgb(var(--primary))" };
  const [r, g, b] = rgb;
  const lift = (v: number) => Math.round(v + (255 - v) * 0.35);
  return { background: `rgba(${r}, ${g}, ${b}, 0.16)`, borderColor: `rgba(${r}, ${g}, ${b}, 0.4)`, color: `rgb(${lift(r)}, ${lift(g)}, ${lift(b)})` };
};
// The server's own colour as a solid line (the open tab's top edge); the accent without one.
export const accentOf = (color?: string | null): string => (hexRgb(color) ? (color as string).trim() : "rgb(var(--primary))");

// The dot of a server whose tab is open, in the colours of the tab's own dot.
export const TAB_DOT: Record<string, string> = {
  waiting: "bg-primary animate-pulse-brief",
  connected: "bg-emerald-400",
  connecting: "bg-amber-400 animate-pulse-brief",
  failed: "bg-rose-500",
  disconnected: "bg-rose-500",
};
