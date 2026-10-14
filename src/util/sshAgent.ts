// Whether logins may use keys from an ssh-agent running on this computer
// (Pageant, the Windows OpenSSH agent, the agent in SSH_AUTH_SOCK; see
// src-tauri/src/agent_auth.rs). Per-device and on by default; read on each
// connect and passed to `initiate_connection` as `useSshAgent`.
export const USE_SSH_AGENT_KEY = "submarine-use-ssh-agent";

export function readUseSshAgent(): boolean {
  try {
    return localStorage.getItem(USE_SSH_AGENT_KEY) !== "off";
  } catch {
    return true;
  }
}
