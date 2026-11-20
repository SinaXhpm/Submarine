// Whether the update notice on the profile list also tells about beta
// versions (pre-releases on GitHub). Per-device and off by default; read by
// ProfileSelectPage for each check and passed to `check_for_updates` as
// `includeBeta`. Builds that don't look for updates (the stores', Flatpak)
// don't show the setting.
export const UPDATE_BETA_KEY = "submarine-update-beta";

export function readUpdateBeta(): boolean {
  try {
    return localStorage.getItem(UPDATE_BETA_KEY) === "on";
  } catch {
    return false;
  }
}
