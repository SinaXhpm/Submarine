// Fonts installed on this device, read from the OS font folders by the
// backend (src-tauri/src/fonts.rs; Android lists none on purpose). Read once
// per run and shared by the font picker and the Settings "isn't installed"
// warning: it takes a moment on machines with many fonts.
import { invoke } from "@tauri-apps/api/core";

export type SystemFont = { family: string; monospace: boolean };

let installedFontsPromise: Promise<SystemFont[]> | null = null;

export const loadInstalledFonts = () => {
  if (!installedFontsPromise) {
    installedFontsPromise = invoke<SystemFont[]>("list_system_fonts").catch((e) => {
      installedFontsPromise = null; // let a later call retry
      throw e;
    });
  }
  return installedFontsPromise;
};
