// Whether logins may use keys from an ssh-agent running on this computer
// (Pageant, the Windows OpenSSH agent, the agent in SSH_AUTH_SOCK; see
// src-tauri/src/agent_auth.rs). Per-device and on by default; read on each
// connect and passed to `initiate_connection` as `useSshAgent`. Monitoring
// gets it through `set_use_ssh_agent` and with each resume.
import { IS_MOBILE } from "./platform";

export const USE_SSH_AGENT_KEY = "submarine-use-ssh-agent";

export function readUseSshAgent(): boolean {
  try {
    return localStorage.getItem(USE_SSH_AGENT_KEY) !== "off";
  } catch {
    return true;
  }
}

// The setting for monitoring: always off on a phone or tablet, which has no
// ssh-agent (the setting is hidden there, but on by default), so a node with
// nothing saved isn't told to load its key into one. Logins the user starts
// keep readUseSshAgent(): there, finding no agent is said plainly, while
// "turned off" would point to a setting a phone doesn't show.
export function readUseSshAgentForMonitor(): boolean {
  return !IS_MOBILE && readUseSshAgent();
}
