// Whether the user's last action was a finger's touch, anywhere in the window.
//
// A PC with a touch screen still has its keyboard and mouse, so the screen
// alone says too little: with one, Windows can make the webview report
// "(pointer: coarse)" although the user works with the mouse and keys. What
// matters is what the user just used. A tap sets it, and a click, a pen or a
// key clears it.
let lastWasTouch = false;

if (typeof window !== "undefined") {
  const opts = { capture: true, passive: true } as const;
  window.addEventListener("pointerdown", (e) => { lastWasTouch = e.pointerType === "touch"; }, opts);
  window.addEventListener("keydown", () => { lastWasTouch = false; }, opts);
}

export const lastInputWasTouch = (): boolean => lastWasTouch;
