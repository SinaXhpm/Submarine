import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { Keyboard, Loader2 } from "lucide-react";
import { IS_ANDROID } from "../util/platform";
import { scancodeFor } from "../util/scancodes";

// The screen of a remote desktop in its tab (VNC, vnc_client.rs) and its
// mouse and keyboard. The backend sends frames over a channel: [1, w, h] when
// the desktop's size changes, [2, x, y, w, h, RGBA rows] for a part that
// changed (little-endian u16s). Input goes back in small batches, one per
// animation frame, through `inputCommand`; the size the tab wants through
// `resizeCommand`. A key is sent with both what it means (KeyboardEvent.key)
// and which key it is (its scan code): each protocol takes what it needs.

export type RemoteScreenHandle = {
  /** A new frame channel, drawn on this canvas (one per connect). */
  newChannel: () => Channel<ArrayBuffer>;
  /** The size the desktop should have for the tab (larger on a phone). */
  wantedSize: () => { width: number; height: number };
};

// A phone gets a desktop larger than its screen: at least 720 on its short
// side (as far as its pixel density goes, so it stays sharp), drawn to fit
// and zoomed with two fingers. A phone-sized desktop is too small for most
// programs. Elsewhere the desktop is the tab's size.
const PHONE_SHORT_SIDE = 720;
const MAX_ZOOM = 5;
function desktopFor(w: number, h: number): { width: number; height: number } {
  let k = 1;
  if (IS_ANDROID) {
    const short = Math.max(1, Math.min(w, h));
    k = Math.min(Math.max(1, PHONE_SHORT_SIDE / short), Math.max(1, window.devicePixelRatio || 1));
  }
  return { width: Math.floor(w * k), height: Math.floor(h * k) };
}

type Ev =
  | { t: "key"; code: number; key: string; down: boolean }
  | { t: "text"; ch: string; down: boolean }
  | { t: "move"; x: number; y: number }
  | { t: "button"; b: number; down: boolean }
  | { t: "wheel"; vertical: boolean; units: number }
  | { t: "releaseall" };

type Props = {
  sessionId: string;
  active: boolean;
  connected: boolean;
  /** The command that takes a batch of input events. */
  inputCommand: string;
  /** The command that takes the size the tab wants. */
  resizeCommand: string;
};

const RemoteScreen = forwardRef<RemoteScreenHandle, Props>(
  ({ sessionId, active, connected, inputCommand, resizeCommand }, ref) => {
    const boxRef = useRef<HTMLDivElement | null>(null);
    const canvasRef = useRef<HTMLCanvasElement | null>(null);
    const imeRef = useRef<HTMLTextAreaElement | null>(null);
    const [desk, setDesk] = useState<{ w: number; h: number } | null>(null);
    // Nothing drawn yet on this connection: a slow server can take a while
    // to send its first picture.
    const [painted, setPainted] = useState(false);
    const paintedRef = useRef(false);
    // The tab's size: the desktop is drawn as large as fits, keeping its shape.
    const [box, setBox] = useState<{ w: number; h: number }>({ w: 0, h: 0 });
    const queue = useRef<Ev[]>([]);
    const flushing = useRef(false);
    // A phone's keyboard is up (it takes part of the tab while it is).
    const typing = useRef(false);
    // The tab size the desktop was last sized for.
    const sizedFor = useRef<{ w: number; h: number } | null>(null);
    // Two-finger zoom (1 = the whole desktop fits) and the pan, in CSS pixels
    // from the centre.
    const [view, setView] = useState({ z: 1, x: 0, y: 0 });
    const viewRef = useRef(view);
    viewRef.current = view;
    // The desktop's size on screen at zoom 1.
    const fit = desk && box.w > 0 ? (() => { const k = Math.min(box.w / desk.w, box.h / desk.h); return { w: Math.floor(desk.w * k), h: Math.floor(desk.h * k) }; })() : null;
    const fitRef = useRef(fit);
    fitRef.current = fit;
    // A zoomed desktop still covers the tab: it can't be panned off it.
    const clampView = (z: number, x: number, y: number) => {
      const f = fitRef.current;
      const b = boxRef.current?.getBoundingClientRect();
      if (!f || !b) return { z: 1, x: 0, y: 0 };
      const mx = Math.max(0, (f.w * z - b.width) / 2);
      const my = Math.max(0, (f.h * z - b.height) / 2);
      return { z, x: Math.max(-mx, Math.min(mx, x)), y: Math.max(-my, Math.min(my, y)) };
    };
    // The tab changed size (the keyboard, the phone turned): a zoomed desktop
    // still covers it.
    useEffect(() => {
      setView((v) => {
        if (v.z === 1) return v;
        const c = clampView(v.z, v.x, v.y);
        return c.x === v.x && c.y === v.y ? v : c;
      });
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [fit?.w, fit?.h, box.w, box.h]);

    const send = useCallback(
      (ev: Ev) => {
        queue.current.push(ev);
        if (flushing.current) return;
        flushing.current = true;
        requestAnimationFrame(() => {
          flushing.current = false;
          const events = queue.current.splice(0, queue.current.length);
          if (events.length) invoke(inputCommand, { sessionId, events }).catch(() => {});
        });
      },
      [sessionId, inputCommand],
    );

    useImperativeHandle(ref, () => ({
      newChannel: () => {
        paintedRef.current = false;
        setPainted(false);
        const ch = new Channel<ArrayBuffer>();
        ch.onmessage = (raw: ArrayBuffer | number[]) => {
          // Bytes, or a list of numbers when Tauri had to fall back from its
          // binary IPC (slower, still drawn).
          const buf = raw instanceof ArrayBuffer ? raw : Array.isArray(raw) ? new Uint8Array(raw).buffer : null;
          const canvas = canvasRef.current;
          if (!canvas || !buf || buf.byteLength < 5) return;
          const v = new DataView(buf);
          const kind = v.getUint8(0);
          if (kind === 1) {
            const w = v.getUint16(1, true);
            const h = v.getUint16(3, true);
            canvas.width = w;
            canvas.height = h;
            setDesk({ w, h });
            setView({ z: 1, x: 0, y: 0 });
          } else if (kind === 2 && buf.byteLength >= 9) {
            const x = v.getUint16(1, true), y = v.getUint16(3, true);
            const w = v.getUint16(5, true), h = v.getUint16(7, true);
            if (w === 0 || h === 0 || buf.byteLength < 9 + w * h * 4) return;
            const ctx = canvas.getContext("2d");
            ctx?.putImageData(new ImageData(new Uint8ClampedArray(buf, 9, w * h * 4), w, h), x, y);
            if (!paintedRef.current) {
              paintedRef.current = true;
              setPainted(true);
            }
          }
        };
        return ch;
      },
      wantedSize: () => {
        const r = boxRef.current?.getBoundingClientRect();
        const w = Math.max(200, r?.width ?? 1024), h = Math.max(200, r?.height ?? 768);
        sizedFor.current = { w, h };
        return desktopFor(w, h);
      },
    }));

    useEffect(() => {
      const el = boxRef.current;
      if (!el) return;
      const ro = new ResizeObserver(() => { const r = el.getBoundingClientRect(); setBox({ w: r.width, h: r.height }); });
      ro.observe(el);
      return () => ro.disconnect();
    }, []);

    // The desktop follows the tab's size (a moment after it settles).
    useEffect(() => {
      const box = boxRef.current;
      if (!box || !connected) return;
      let timer = 0;
      const ro = new ResizeObserver(() => {
        window.clearTimeout(timer);
        timer = window.setTimeout(() => {
          const r = box.getBoundingClientRect();
          if (r.width < 50 || r.height < 50) return;
          // The keyboard coming up or going: the desktop keeps its size and is
          // drawn smaller for the moment, not laid out again each time. A new
          // width (the phone turned) still counts.
          const last = sizedFor.current;
          if (typing.current && last && Math.abs(r.width - last.w) <= 4) return;
          sizedFor.current = { w: r.width, h: r.height };
          const d = desktopFor(r.width, r.height);
          invoke(resizeCommand, { sessionId, width: d.width, height: d.height }).catch(() => {});
        }, 400);
      });
      ro.observe(box);
      return () => {
        ro.disconnect();
        window.clearTimeout(timer);
      };
    }, [sessionId, connected, resizeCommand]);

    // Keys focus here when the tab is shown.
    useEffect(() => {
      if (active && connected && !IS_ANDROID) canvasRef.current?.focus();
    }, [active, connected]);

    // A point on the canvas as a point of the remote desktop.
    const toDesk = (clientX: number, clientY: number) => {
      const c = canvasRef.current;
      if (!c) return null;
      const r = c.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) return null;
      const x = Math.round(((clientX - r.left) / r.width) * c.width);
      const y = Math.round(((clientY - r.top) / r.height) * c.height);
      return { x: Math.max(0, Math.min(c.width - 1, x)), y: Math.max(0, Math.min(c.height - 1, y)) };
    };
    const move = (clientX: number, clientY: number) => {
      const p = toDesk(clientX, clientY);
      if (p) send({ t: "move", ...p });
    };

    // ---- mouse ----
    // A phone's browser follows a touch with mouse events of its own: they
    // aren't clicks of their own (a tap would be a double click).
    const lastTouch = useRef(0);
    const fromTouch = () => Date.now() - lastTouch.current < 800;
    const onMouseMove = (e: React.MouseEvent) => {
      if (!fromTouch()) move(e.clientX, e.clientY);
    };
    const onMouseDown = (e: React.MouseEvent) => {
      e.preventDefault();
      if (fromTouch()) return;
      canvasRef.current?.focus();
      move(e.clientX, e.clientY);
      send({ t: "button", b: e.button, down: true });
    };
    const onMouseUp = (e: React.MouseEvent) => {
      e.preventDefault();
      if (fromTouch()) return;
      move(e.clientX, e.clientY);
      send({ t: "button", b: e.button, down: false });
    };
    const onWheel = (e: React.WheelEvent) => {
      const vertical = Math.abs(e.deltaY) >= Math.abs(e.deltaX);
      const delta = vertical ? e.deltaY : e.deltaX;
      if (delta === 0) return;
      // A notch is 120; away from the user is positive.
      const units = Math.max(-255, Math.min(255, Math.round(-delta * (e.deltaMode === 1 ? 40 : 1.2))));
      if (units !== 0) send({ t: "wheel", vertical, units });
    };

    // ---- keyboard ----
    const onKey = (down: boolean) => (e: React.KeyboardEvent) => {
      if (e.nativeEvent.isComposing) return;
      const code = scancodeFor(e.code);
      if (code !== null) {
        send({ t: "key", code, key: e.key, down });
        e.preventDefault();
      } else if (down && e.key.length === 1) {
        send({ t: "text", ch: e.key, down: true });
        send({ t: "text", ch: e.key, down: false });
        e.preventDefault();
      }
    };
    const onBlur = () => send({ t: "releaseall" });

    // ---- touch (a phone): tap is a click, a long press a right click,
    // dragging moves the pointer with the left button down; two fingers zoom
    // (pinch) and pan. ----
    const touch = useRef<{ x: number; y: number; at: number; moved: boolean; long: number } | null>(null);
    const pinch = useRef<{ d0: number; z0: number; x0: number; y0: number; mx0: number; my0: number } | null>(null);
    const twoFingers = (e: React.TouchEvent) => {
      const a = e.touches[0], b = e.touches[1];
      return { d: Math.max(1, Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY)), mx: (a.clientX + b.clientX) / 2, my: (a.clientY + b.clientY) / 2 };
    };
    const onTouchStart = (e: React.TouchEvent) => {
      lastTouch.current = Date.now();
      if (e.touches.length >= 2) {
        // A second finger: no click or drag from the first one.
        const s = touch.current;
        touch.current = null;
        if (s) {
          window.clearTimeout(s.long);
          if (s.moved) send({ t: "button", b: 0, down: false });
        }
        if (e.touches.length === 2) {
          const g = twoFingers(e);
          const v = viewRef.current;
          pinch.current = { d0: g.d, z0: v.z, x0: v.x, y0: v.y, mx0: g.mx, my0: g.my };
        }
        return;
      }
      if (pinch.current) return;
      const t = e.touches[0];
      move(t.clientX, t.clientY);
      const long = window.setTimeout(() => {
        if (touch.current && !touch.current.moved) {
          send({ t: "button", b: 2, down: true });
          send({ t: "button", b: 2, down: false });
          touch.current = null;
        }
      }, 550);
      touch.current = { x: t.clientX, y: t.clientY, at: Date.now(), moved: false, long };
    };
    const onTouchMove = (e: React.TouchEvent) => {
      lastTouch.current = Date.now();
      const p = pinch.current;
      if (p) {
        if (e.touches.length !== 2) return;
        const g = twoFingers(e);
        const b = boxRef.current?.getBoundingClientRect();
        if (!b) return;
        // The point under the fingers stays under them while zooming.
        const z = Math.max(1, Math.min(MAX_ZOOM, (p.z0 * g.d) / p.d0));
        const cx = b.left + b.width / 2, cy = b.top + b.height / 2;
        const x = g.mx - cx - (z / p.z0) * (p.mx0 - cx - p.x0);
        const y = g.my - cy - (z / p.z0) * (p.my0 - cy - p.y0);
        setView(clampView(z, x, y));
        return;
      }
      const s = touch.current;
      if (!s || e.touches.length !== 1) return;
      const t = e.touches[0];
      if (!s.moved && Math.hypot(t.clientX - s.x, t.clientY - s.y) > 8) {
        s.moved = true;
        window.clearTimeout(s.long);
        send({ t: "button", b: 0, down: true });
      }
      if (s.moved) move(t.clientX, t.clientY);
    };
    const onTouchEnd = (e: React.TouchEvent) => {
      lastTouch.current = Date.now();
      // No mouse events after it (see fromTouch).
      if (e.cancelable) e.preventDefault();
      if (pinch.current) {
        // Done when the last finger is up; the one left isn't a click.
        if (e.touches.length === 0) pinch.current = null;
        return;
      }
      const s = touch.current;
      touch.current = null;
      if (!s) return;
      window.clearTimeout(s.long);
      if (s.moved) {
        send({ t: "button", b: 0, down: false });
      } else {
        send({ t: "button", b: 0, down: true });
        send({ t: "button", b: 0, down: false });
      }
    };
    // The system took the touch over (a gesture): nothing stays pressed.
    const onTouchCancel = () => {
      lastTouch.current = Date.now();
      pinch.current = null;
      const s = touch.current;
      touch.current = null;
      if (!s) return;
      window.clearTimeout(s.long);
      if (s.moved) send({ t: "button", b: 0, down: false });
    };

    // A phone's keyboard types into a hidden box; what it inserts is sent
    // as text, and its delete / enter as those keys.
    const onImeInput = (e: React.FormEvent<HTMLTextAreaElement>) => {
      const native = e.nativeEvent as InputEvent;
      const tap = (key: "Backspace" | "Enter") => {
        const code = scancodeFor(key) ?? 0;
        send({ t: "key", code, key, down: true });
        send({ t: "key", code, key, down: false });
      };
      if (native.inputType === "deleteContentBackward") tap("Backspace");
      else if (native.inputType === "insertLineBreak") tap("Enter");
      else if (native.data) {
        for (const ch of native.data) {
          if (ch === "\n") tap("Enter");
          else {
            send({ t: "text", ch, down: true });
            send({ t: "text", ch, down: false });
          }
        }
      }
      // Kept with one character in it, so the next delete has something to remove.
      e.currentTarget.value = " ";
    };

    return (
      <div ref={boxRef} className="relative flex-1 min-h-0 w-full flex items-center justify-center bg-black overflow-hidden">
        <canvas
          ref={canvasRef}
          tabIndex={0}
          aria-label="Remote desktop"
          className="outline-none touch-none"
          style={fit ? { width: fit.w, height: fit.h, transform: view.z === 1 ? undefined : `translate(${view.x}px, ${view.y}px) scale(${view.z})`, transformOrigin: "center" } : { display: "none" }}
          onMouseMove={onMouseMove}
          onMouseDown={onMouseDown}
          onMouseUp={onMouseUp}
          onWheel={onWheel}
          onKeyDown={onKey(true)}
          onKeyUp={onKey(false)}
          onBlur={onBlur}
          onContextMenu={(e) => e.preventDefault()}
          onTouchStart={onTouchStart}
          onTouchMove={onTouchMove}
          onTouchEnd={onTouchEnd}
          onTouchCancel={onTouchCancel}
        />
        {connected && !painted && (
          <div className="absolute inset-0 flex items-center justify-center gap-2 text-[12px] text-zinc-500 pointer-events-none" role="status">
            <Loader2 size={14} className="animate-spin" />
            Waiting for the desktop…
          </div>
        )}
        {IS_ANDROID && connected && (
          <>
            <textarea
              ref={imeRef}
              aria-label="Type on the remote desktop"
              defaultValue=" "
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              onInput={onImeInput}
              onFocus={() => { typing.current = true; }}
              onBlur={() => { typing.current = false; }}
              className="absolute opacity-0 w-px h-px -left-10 top-0"
            />
            <button
              type="button"
              onClick={() => {
                // Closed with Back, the box keeps its focus: focusing it again
                // is what brings the keyboard back.
                const t = imeRef.current;
                if (!t) return;
                if (document.activeElement !== t) t.focus();
                else {
                  // At once, the keyboard would go and not come back.
                  t.blur();
                  window.setTimeout(() => t.focus(), 60);
                }
              }}
              aria-label="Show the keyboard"
              className="absolute right-3 bottom-3 w-11 h-11 rounded-full bg-zinc-800/90 border border-white/10 text-zinc-200 flex items-center justify-center shadow-lg"
            >
              <Keyboard size={18} />
            </button>
          </>
        )}
      </div>
    );
  },
);

export default RemoteScreen;
