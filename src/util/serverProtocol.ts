// What a saved server is, from its `protocol` (set_server_protocol in lib.rs):
// SSH (NULL in the table), RDP, telnet, VNC, or FTP in one of three forms.
// "ftps" is FTPS with TLS after connecting (explicit, the usual one),
// "ftps-implicit" TLS from the first byte (port 990), "ftp" plain FTP, not
// encrypted (src-tauri/src/ftp.rs).

export type FtpProtocol = "ftps" | "ftps-implicit" | "ftp";
export type ServerProtocol = "ssh" | "rdp" | "telnet" | "vnc" | FtpProtocol;

export const FTP_PROTOCOLS: readonly FtpProtocol[] = ["ftps", "ftps-implicit", "ftp"];
const KNOWN: readonly string[] = ["rdp", "telnet", "vnc", ...FTP_PROTOCOLS];

export function isFtp(protocol: unknown): protocol is FtpProtocol {
  return typeof protocol === "string" && (FTP_PROTOCOLS as readonly string[]).includes(protocol);
}

/** A server's protocol as the page uses it: anything unknown is SSH. */
export function serverProtocol(protocol: unknown): ServerProtocol {
  return typeof protocol === "string" && KNOWN.includes(protocol) ? (protocol as ServerProtocol) : "ssh";
}

/** The port each kind usually listens on. */
export const USUAL_PORTS: Record<ServerProtocol, number> = {
  ssh: 22, rdp: 3389, telnet: 23, vnc: 5900, ftps: 21, "ftps-implicit": 990, ftp: 21,
};

/** What a pasted link names (Quick Connect): `ftp://user:pass@host:21/path`. */
export interface ServerLink {
  protocol: ServerProtocol;
  host: string;
  port: number;
  username?: string;
  password?: string;
}

// The kinds of link Quick Connect takes. ftp:// is FTPS with TLS after
// connecting here: the app never sends a password in the clear unless plain
// FTP is picked on purpose (the form's Encryption). ftps:// is TLS from the
// start, as curl and FileZilla read it; ftpes:// is TLS after connecting.
const LINK_KINDS: Record<string, ServerProtocol> = {
  ftp: "ftps", ftpes: "ftps", ftps: "ftps-implicit", sftp: "ssh", ssh: "ssh",
};

const decoded = (s: string) => {
  try { return decodeURIComponent(s); } catch { return s; }
};

/**
 * The server a link names, or null for text that isn't one of LINK_KINDS'
 * links. Only the part before the first `/`, `?` or `#` counts; a login with
 * an `@` in it (common on hosting: `ftp://me@example.com@ftp.example.com`)
 * reads as the login, up to the last `@`.
 */
export function parseServerLink(text: string): ServerLink | null {
  const m = /^\s*([a-z][a-z0-9+.-]*):\/\/(\S+?)\s*$/i.exec(text);
  if (!m) return null;
  const protocol = LINK_KINDS[m[1].toLowerCase()];
  if (!protocol) return null;
  let rest = m[2].split(/[/?#]/)[0];
  let username: string | undefined;
  let password: string | undefined;
  const at = rest.lastIndexOf("@");
  if (at >= 0) {
    const login = rest.slice(0, at);
    rest = rest.slice(at + 1);
    const colon = login.indexOf(":");
    username = decoded(colon >= 0 ? login.slice(0, colon) : login);
    if (colon >= 0) password = decoded(login.slice(colon + 1));
  }
  let host = rest;
  let port: number | undefined;
  const v6 = /^\[([^\]]+)\](?::(\d+))?$/.exec(rest);
  if (v6) {
    host = v6[1];
    port = v6[2] ? Number(v6[2]) : undefined;
  } else {
    const colon = rest.lastIndexOf(":");
    if (colon >= 0 && rest.indexOf(":") === colon && /^\d+$/.test(rest.slice(colon + 1))) {
      host = rest.slice(0, colon);
      port = Number(rest.slice(colon + 1));
    }
  }
  host = decoded(host).trim();
  // What is left is a name or an address (an IPv6 one came in brackets);
  // anything else is a link this can't read, such as an unencoded `/` in a
  // password.
  if (!host || (!v6 && /[\s/@:]/.test(host))) return null;
  if (port !== undefined && (port < 1 || port > 65535)) return null;
  return { protocol, host, port: port ?? USUAL_PORTS[protocol], username, password };
}
