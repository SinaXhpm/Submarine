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
