// Whether logins may use keys from an ssh-agent running on this computer
// (Pageant, the Windows OpenSSH agent, the agent in SSH_AUTH_SOCK; see
// src-tauri/src/agent_auth.rs). Per-device and on by default; read on each
// connect and passed to `initiate_connection` as `useSshAgent`. Monitoring
// gets it through `set_use_ssh_agent` and with each resume.
export const USE_SSH_AGENT_KEY = "submarine-use-ssh-agent";

export function readUseSshAgent(): boolean {
  try {
    return localStorage.getItem(USE_SSH_AGENT_KEY) !== "off";
  } catch {
    return true;
  }
}
