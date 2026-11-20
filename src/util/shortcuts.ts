// Keyboard shortcuts for the app's own actions (#367). Settings → Keyboard
// shortcuts changes them: an action can have several keys, or none. Per
// device, like the other settings: what was changed is kept in localStorage,
// the defaults are here. A Mac, an iPad and an iPhone take ⌘ where the others
// take Ctrl.
//
// A key combination is kept as text, its modifiers in a fixed order and then
// the key: "Ctrl+Shift+T", "Shift+Meta+W". The key is a letter or a digit, as
// xterm reads it (the layout's own letter: Dvorak's, AZERTY's; on a layout
// without Latin letters, the one at that place on a US keyboard), "Plus",
// F1–F24, a named key ("Tab", "Insert", "ArrowUp"…), or for the other keys,
// whose character changes with Shift and with the layout, the key's place
// ("Minus", "BracketLeft"…, as KeyboardEvent.code says).
import { useSyncExternalStore } from "react";
import { IS_ANDROID, IS_IOS, IS_LINUX, IS_MAC, IS_WINDOWS } from "./platform";

/** An Apple keyboard: ⌘ is the shortcut key, and keys are written ⌃⌥⇧⌘. */
export const APPLE_KEYS = IS_MAC || IS_IOS;

export type ShortcutAction =
  | "openServer"
  | "nextTab"
  | "previousTab"
  | "newTerminal"
  | "closeTerminal"
  | "find"
  | "history"
  | "clear"
  | "copy"
  | "paste"
  | "selectAll";

/** Shortcuts that work anywhere in the window (DesktopApp). */
export const APP_SHORTCUTS: readonly ShortcutAction[] = ["openServer", "nextTab", "previousTab"];
/** Shortcuts that work in the terminal that has the keyboard (TerminalView). */
export const TERMINAL_SHORTCUTS: readonly ShortcutAction[] = [
  "newTerminal", "closeTerminal", "find", "history", "clear", "copy", "paste", "selectAll",
];
export const SHORTCUT_ACTIONS: readonly ShortcutAction[] = [...APP_SHORTCUTS, ...TERMINAL_SHORTCUTS];

export interface Combo {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
  key: string;
}

// With ⌘ the Ctrl version stays too, as it worked before this table.
const ctrlShift = (key: string) => (APPLE_KEYS ? [`Shift+Meta+${key}`, `Ctrl+Shift+${key}`] : [`Ctrl+Shift+${key}`]);
const DEFAULTS: Record<ShortcutAction, string[]> = {
  // As in Windows Terminal (⌘T as in a Mac's own apps). Plain Ctrl+T stays
  // the shell's: it swaps two characters there.
  openServer: [APPLE_KEYS ? "Meta+T" : "Ctrl+Shift+T"],
  nextTab: ["Ctrl+Tab"],
  previousTab: ["Ctrl+Shift+Tab"],
  newTerminal: ctrlShift("Plus"),
  closeTerminal: ctrlShift("W"),
  find: ctrlShift("F"),
  history: ctrlShift("R"),
  clear: ctrlShift("K"),
  copy: APPLE_KEYS ? ["Meta+C", "Ctrl+Shift+C"] : ["Ctrl+Shift+C", "Ctrl+Insert"],
  paste: APPLE_KEYS ? ["Meta+V"] : ["Ctrl+Shift+V", "Shift+Insert"],
  selectAll: APPLE_KEYS ? ["Meta+A", "Ctrl+Shift+A"] : ["Ctrl+Shift+A"],
};

// Keys the webview copies, pastes or selects with by itself in a terminal. Set
// for that action, those in PASS_THROUGH are left to it, as before: a Mac's ⌘C
// goes through its Edit menu, and a paste key's paste event is checked like any
// paste (TerminalView). Set for nothing, all of them are held back, so they do
// nothing there, as the table says. Linux reads the clipboard through the
// backend (#89): its paste keys are never left to the webview.
const WEBVIEW_KEYS = APPLE_KEYS ? ["Meta+C", "Meta+V", "Meta+A"] : ["Ctrl+Insert", "Ctrl+Shift+V", "Shift+Insert"];
const PASS_THROUGH: Partial<Record<ShortcutAction, string[]>> = APPLE_KEYS
  ? { copy: ["Meta+C"], paste: ["Meta+V"], selectAll: ["Meta+A"] }
  : IS_LINUX ? {} : { paste: ["Ctrl+Shift+V", "Shift+Insert"] };

// ── Keys from key events ─────────────────────────────────────────────────

// Keys that are held with another key: no shortcut by themselves.
const MODIFIER_KEYS = new Set([
  "Control", "Shift", "Alt", "AltGraph", "Meta", "OS", "Super", "Hyper",
  "Fn", "FnLock", "CapsLock", "NumLock", "ScrollLock", "Symbol", "SymbolLock",
]);
// Keys known by their name, whatever the layout. Escape isn't one: it closes
// things, and it ends the taking of a new key in Settings.
const NAMED_KEYS = new Set([
  "Tab", "Enter", "Backspace", "Delete", "Insert", "Home", "End", "PageUp", "PageDown",
  "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Pause", "ContextMenu",
]);
// The other keys, by their place.
const PLACE_KEYS = new Set([
  "Minus", "Equal", "BracketLeft", "BracketRight", "Backslash", "Semicolon", "Quote",
  "Comma", "Period", "Slash", "Backquote", "IntlBackslash", "IntlRo", "IntlYen",
  "NumpadSubtract", "NumpadMultiply", "NumpadDivide", "NumpadDecimal",
  ...Array.from({ length: 10 }, (_, i) => `Numpad${i}`),
]);

// The plus key, by its character: "+" itself, the keypad's plus, the US "=/+"
// key unshifted, and the German plus key with Shift, which gives "*". A key in
// the same place on another layout ("]" or "}" on a US keyboard) isn't it.
export const isPlusKey = (ev: KeyboardEvent): boolean =>
  ev.key === "+" ||
  ev.code === "NumpadAdd" ||
  (ev.code === "Equal" && ev.key === "=") ||
  (ev.code === "BracketRight" && ev.key === "*");

/** The key of a key event (see the top of this file), or null for a modifier
 *  alone, a key an input method is using, or a key that can't be a shortcut. */
export function keyOf(ev: KeyboardEvent): string | null {
  if (ev.isComposing || ev.keyCode === 229 || MODIFIER_KEYS.has(ev.key)) return null;
  if (isPlusKey(ev)) return "Plus";
  // xterm's way, by key code: the layout's letter, or on a layout without
  // Latin letters the US one, which Windows and macOS give such keys.
  if (ev.keyCode >= 65 && ev.keyCode <= 90) return String.fromCharCode(ev.keyCode);
  if (/^Key[A-Z]$/.test(ev.code)) return ev.code.slice(3);
  if (/^Digit[0-9]$/.test(ev.code)) return ev.code.slice(5);
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(ev.key)) return ev.key;
  if (ev.key === " " || ev.code === "Space") return "Space";
  if (NAMED_KEYS.has(ev.key)) return ev.key;
  if (PLACE_KEYS.has(ev.code)) return ev.code;
  return null;
}

/** The keys of a key event, or null when it is no shortcut. A character typed
 *  with AltGr (Ctrl+Alt on Windows) is text, not a shortcut. */
export function comboOf(ev: KeyboardEvent): Combo | null {
  if (ev.getModifierState?.("AltGraph")) return null;
  const key = keyOf(ev);
  return key ? { ctrl: ev.ctrlKey, alt: ev.altKey, shift: ev.shiftKey, meta: ev.metaKey, key } : null;
}

// ── Text ─────────────────────────────────────────────────────────────────

/** The combination as it is kept: "Ctrl+Alt+Shift+Meta+Key". */
export function comboText(c: Combo): string {
  return [c.ctrl && "Ctrl", c.alt && "Alt", c.shift && "Shift", c.meta && "Meta", c.key].filter(Boolean).join("+");
}

const KNOWN_KEY = (key: string) =>
  /^[A-Z0-9]$/.test(key) || key === "Plus" || key === "Space" || /^F([1-9]|1[0-9]|2[0-4])$/.test(key) ||
  NAMED_KEYS.has(key) || PLACE_KEYS.has(key);

export function parseCombo(text: unknown): Combo | null {
  if (typeof text !== "string") return null;
  const parts = text.split("+");
  const key = parts.pop() ?? "";
  if (!KNOWN_KEY(key)) return null;
  const c: Combo = { ctrl: false, alt: false, shift: false, meta: false, key };
  for (const p of parts) {
    if (p === "Ctrl") c.ctrl = true;
    else if (p === "Alt") c.alt = true;
    else if (p === "Shift") c.shift = true;
    else if (p === "Meta") c.meta = true;
    else return null;
  }
  return c;
}

const sameCombo = (a: Combo, b: Combo) =>
  a.key === b.key && a.ctrl === b.ctrl && a.alt === b.alt && a.shift === b.shift && a.meta === b.meta;

// ── What is shown ────────────────────────────────────────────────────────

// What the keys known by their place print on this keyboard, where the webview
// can tell (Chromium: Windows and Android). Asked again each time Settings
// shows the table, since the layout can change while the app runs.
let layoutMap: Map<string, string> | null = null;
export function refreshKeyboardLayout(): void {
  try {
    const kb = (navigator as any).keyboard;
    kb?.getLayoutMap?.().then((m: Map<string, string>) => {
      layoutMap = m;
      notify();
    }, () => {});
  } catch { /* not this webview */ }
}

const KEY_LABELS: Record<string, string> = {
  Plus: APPLE_KEYS ? "+" : "Plus",
  Space: "Space",
  ArrowUp: "↑", ArrowDown: "↓", ArrowLeft: "←", ArrowRight: "→",
  PageUp: "Page Up", PageDown: "Page Down",
  Backspace: APPLE_KEYS ? "⌫" : "Backspace",
  Delete: APPLE_KEYS ? "⌦" : "Delete",
  ContextMenu: "Menu",
  Minus: "-", Equal: "=", BracketLeft: "[", BracketRight: "]", Backslash: "\\",
  Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/", Backquote: "`",
  IntlBackslash: "\\", IntlRo: "\\", IntlYen: "¥",
  NumpadSubtract: "Num -", NumpadMultiply: "Num *", NumpadDivide: "Num /", NumpadDecimal: "Num .",
};

export function keyLabel(key: string): string {
  if (/^Numpad[0-9]$/.test(key)) return `Num ${key.slice(6)}`;
  if (PLACE_KEYS.has(key)) {
    const printed = layoutMap?.get(key);
    if (printed && printed.trim()) return printed.toUpperCase();
  }
  return KEY_LABELS[key] ?? key;
}

const META_NAME = IS_WINDOWS ? "Win" : IS_ANDROID ? "Meta" : "Super";

/** The modifiers held so far, as they are shown: "Ctrl+Shift+" or "⌃⇧". */
export function modifiersLabel(m: { ctrl: boolean; alt: boolean; shift: boolean; meta: boolean }): string {
  if (APPLE_KEYS) return `${m.ctrl ? "⌃" : ""}${m.alt ? "⌥" : ""}${m.shift ? "⇧" : ""}${m.meta ? "⌘" : ""}`;
  return [m.ctrl && "Ctrl", m.alt && "Alt", m.shift && "Shift", m.meta && META_NAME]
    .filter(Boolean)
    .map((name) => `${name}+`)
    .join("");
}

/** The combination as this system writes it: "Ctrl+Shift+T", or "⇧⌘T" on a Mac. */
export function comboLabel(c: Combo): string {
  return modifiersLabel(c) + keyLabel(c.key);
}

/** The same, from its text (for a combination named in a sentence). */
export const comboTextLabel = (text: string): string => {
  const c = parseCombo(text);
  return c ? comboLabel(c) : text;
};

// ── Rules ────────────────────────────────────────────────────────────────

/** Whether the keys can be a shortcut. A key that types, alone or with Shift,
 *  is needed for typing: a shortcut takes Ctrl, Alt or ⌘, except F1–F24 and
 *  Shift+Insert (a paste key on Windows and Linux). */
export function usableCombo(c: Combo): boolean {
  if (c.ctrl || c.alt || c.meta) return true;
  return /^F\d+$/.test(c.key) || (c.key === "Insert" && c.shift);
}

/** Whether a terminal would send these keys on to the program running in it
 *  (as xterm 5.3 does): a shortcut on them takes them from it. Ctrl+Shift and
 *  a letter, ⌘ (or the Windows key) and anything give the program nothing. */
export function reachesTerminal(c: Combo): boolean {
  if (c.meta) return false;
  if (/^F\d+$/.test(c.key)) return true;
  if (c.key === "Insert") return !c.ctrl && !c.shift;
  if (NAMED_KEYS.has(c.key) || c.key === "Space") return true;
  if (c.alt) return true;
  if (c.ctrl) return !c.shift;
  return true;
}

/** Whether the webview leaves the action to itself on these keys, as before
 *  this table: a Mac's ⌘C, ⌘V and ⌘A, the paste keys elsewhere but on Linux. */
export const leftToWebview = (action: ShortcutAction, c: Combo): boolean =>
  !!PASS_THROUGH[action]?.includes(comboText(c));

/** Whether the webview would copy, paste or select all on these keys in a
 *  terminal by itself. */
export const isWebviewEditKey = (c: Combo): boolean => WEBVIEW_KEYS.includes(comboText(c));

// ── The table ────────────────────────────────────────────────────────────

export const SHORTCUTS_KEY = "submarine-shortcuts";

export type ShortcutTable = Readonly<Record<ShortcutAction, readonly Combo[]>>;

// The actions changed in Settings, by action: their keys as text.
function readChanges(): Partial<Record<ShortcutAction, string[]>> {
  try {
    const raw = localStorage.getItem(SHORTCUTS_KEY);
    const v = raw ? JSON.parse(raw) : null;
    return v && typeof v === "object" && !Array.isArray(v) ? v : {};
  } catch {
    return {};
  }
}

let cache: ShortcutTable | null = null;
const listeners = new Set<() => void>();
function notify() {
  cache = null;
  for (const l of listeners) l();
}

function writeChanges(changes: Partial<Record<ShortcutAction, string[]>>) {
  try {
    if (Object.keys(changes).length === 0) localStorage.removeItem(SHORTCUTS_KEY);
    else localStorage.setItem(SHORTCUTS_KEY, JSON.stringify(changes));
  } catch {
    // Best-effort, same as the other per-device prefs.
  }
  notify();
}

/** Every action's keys. The changed ones come first: a key given to one
 *  action is no longer another's by default. A key kept twice counts once. */
export function readShortcuts(): ShortcutTable {
  if (cache) return cache;
  const changes = readChanges();
  const taken = new Set<string>();
  const table = {} as Record<ShortcutAction, Combo[]>;
  const take = (list: unknown[]): Combo[] => {
    const out: Combo[] = [];
    for (const text of list) {
      const c = parseCombo(text);
      if (!c || !usableCombo(c) || taken.has(comboText(c))) continue;
      taken.add(comboText(c));
      out.push(c);
    }
    return out;
  };
  for (const a of SHORTCUT_ACTIONS) {
    const list = changes[a];
    if (Array.isArray(list)) table[a] = take(list);
  }
  for (const a of SHORTCUT_ACTIONS) {
    if (!table[a]) table[a] = take(DEFAULTS[a]);
  }
  cache = table;
  return table;
}

export const defaultShortcuts = (action: ShortcutAction): Combo[] =>
  DEFAULTS[action].map(parseCombo).filter((c): c is Combo => !!c);

/** Whether the action has keys of its own choosing. */
export const shortcutChanged = (action: ShortcutAction): boolean => Array.isArray(readChanges()[action]);

/** Sets the action's keys. Its defaults again count as unchanged. */
export function setShortcuts(action: ShortcutAction, combos: readonly Combo[]): void {
  const changes = readChanges();
  const defaults = defaultShortcuts(action);
  const asDefault = combos.length === defaults.length && combos.every((c, i) => sameCombo(c, defaults[i]));
  if (asDefault) delete changes[action];
  else changes[action] = combos.map(comboText);
  writeChanges(changes);
}

export function resetShortcuts(action?: ShortcutAction): void {
  if (!action) {
    writeChanges({});
    return;
  }
  const changes = readChanges();
  delete changes[action];
  writeChanges(changes);
}

/** The action that has these keys, if any. */
export function shortcutOwner(c: Combo): ShortcutAction | null {
  const table = readShortcuts();
  for (const a of SHORTCUT_ACTIONS) if (table[a].some((k) => sameCombo(k, c))) return a;
  return null;
}

/** The action among `actions` that the keys are set for. In `withoutShift`
 *  (a terminal whose shell has ended: close it, open another) keys set with
 *  Shift work without it too, unless those keys are another action's. */
export function shortcutFor(
  c: Combo,
  actions: readonly ShortcutAction[],
  withoutShift: readonly ShortcutAction[] = [],
): ShortcutAction | null {
  const table = readShortcuts();
  for (const a of actions) if (table[a].some((k) => sameCombo(k, c))) return a;
  if (c.shift || withoutShift.length === 0 || shortcutOwner(c)) return null;
  for (const a of withoutShift) if (table[a].some((k) => k.shift && sameCombo({ ...k, shift: false }, c))) return a;
  return null;
}

/** The action among `actions` that a key event is a shortcut for, if any. */
export function matchShortcut(ev: KeyboardEvent, actions: readonly ShortcutAction[]): ShortcutAction | null {
  const c = comboOf(ev);
  return c ? shortcutFor(c, actions) : null;
}

const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => { listeners.delete(l); };
};

/** The table, kept up to date in a component. */
export function useShortcuts(): ShortcutTable {
  return useSyncExternalStore(subscribe, readShortcuts, readShortcuts);
}
