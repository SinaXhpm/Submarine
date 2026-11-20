import { invoke } from "@tauri-apps/api/core";
import { IS_ANDROID } from "./platform";

/** Whether Android has a hardware keyboard in use: a keyboard cover, or a USB
 *  or Bluetooth keyboard (the app's Android plugin reads the system's own
 *  configuration). Asked each time, since a cover comes and goes. False on
 *  other systems, and when Android can't say. */
export async function hardwareKeyboardAttached(): Promise<boolean> {
  if (!IS_ANDROID) return false;
  try {
    return await invoke<boolean>("hardware_keyboard_attached");
  } catch {
    return false;
  }
}
