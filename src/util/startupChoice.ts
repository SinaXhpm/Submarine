// "At startup" in the server form: what a server's tab does when the profile
// is unlocked. Kept as two fields of the server: `autostart` (open the tab and
// connect; SSH servers only, as before) and `auto_open` (open the tab without
// connecting; any kind of server). Autostart comes first where both are set.
export type StartupChoice = "off" | "open" | "connect";

/** The choice the form shows. A server that isn't SSH can't connect by itself:
 *  set to, it opens only. */
export function startupChoice(node: { autostart?: boolean; autoOpen?: boolean }, ssh: boolean): StartupChoice {
  if (ssh && node.autostart) return "connect";
  return node.autoOpen || node.autostart ? "open" : "off";
}

/** The form's two fields for a choice. */
export function startupFields(choice: StartupChoice): { autostart: boolean; autoOpen: boolean } {
  return { autostart: choice === "connect", autoOpen: choice === "open" };
}
