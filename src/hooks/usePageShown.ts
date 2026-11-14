import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

/** Whether the page is on screen: not minimised or behind another window's
 *  tab, and on a phone, the app not in the background. */
export function usePageShown(): boolean {
  const [pageShown, setPageShown] = useState(() => document.visibilityState !== "hidden");
  useEffect(() => {
    const onChange = () => setPageShown(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", onChange);
    // A phone's app in the background: its WebView still says it is visible,
    // so the backend tells (lib.rs, from Android's onPause and onResume).
    const off = listen<boolean>("app-shown", (e) => setPageShown(!!e.payload));
    return () => {
      document.removeEventListener("visibilitychange", onChange);
      off.then((f) => f());
    };
  }, []);
  return pageShown;
}
