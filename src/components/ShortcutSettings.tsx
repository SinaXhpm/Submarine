// Settings → Keyboard shortcuts (util/shortcuts): the keys for the app's own
// actions, in a table. The + beside an action takes the next keys pressed for
// it, an × drops a key, and the arrow puts a row's keys back to the default.
// A key can belong to one action only. Per device, like the rest of Settings.
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { Keyboard, Plus, RotateCcw, X } from "lucide-react";
import { tk, useT } from "../i18n";
import { useConfirm } from "../ui/confirm";
import {
  APPLE_KEYS, APP_SHORTCUTS, SHORTCUT_ACTIONS, TERMINAL_SHORTCUTS, comboLabel, comboOf, comboText, comboTextLabel,
  modifiersLabel, reachesTerminal, refreshKeyboardLayout, resetShortcuts, setShortcuts, shortcutChanged,
  shortcutOwner, usableCombo, useShortcuts, type ShortcutAction,
} from "../util/shortcuts";

const LABELS: Record<ShortcutAction, string> = {
  openServer: tk("Open a server"),
  nextTab: tk("Next tab"),
  previousTab: tk("Previous tab"),
  newTerminal: tk("New terminal"),
  closeTerminal: tk("Close terminal"),
  find: tk("Find"),
  history: tk("Command history"),
  clear: tk("Clear terminal"),
  copy: tk("Copy"),
  paste: tk("Paste"),
  selectAll: tk("Select all"),
};

// A line under an action's name, where the name alone says too little.
const NOTES: Partial<Record<ShortcutAction, string>> = {
  openServer: tk("The + after the last tab: find a saved server and open it."),
  newTerminal: tk("Another terminal on the same server."),
  copy: tk("With nothing selected, a key the terminal uses, such as {keys}, still goes to it."),
};

const GROUPS: { title: string; actions: readonly ShortcutAction[] }[] = [
  { title: tk("Anywhere in the app"), actions: APP_SHORTCUTS },
  { title: tk("In a terminal"), actions: TERMINAL_SHORTCUTS },
];

// Keys the system or the app's menu takes before the page sees them: a Mac's
// menu quits, hides or closes the window on these, and Windows and Linux
// close the window on Alt+F4.
const SYSTEM_KEYS = APPLE_KEYS
  ? ["Meta+Q", "Meta+W", "Meta+H", "Meta+M", "Alt+Meta+H", "Ctrl+Meta+F"]
  : ["Alt+F4"];

type Message = { action: ShortcutAction; tone: "error" | "note"; text: string };

export function ShortcutSettings() {
  const t = useT();
  const confirm = useConfirm();
  const table = useShortcuts();
  const [recording, setRecording] = useState<ShortcutAction | null>(null);
  // The modifiers held while keys are taken, as they are shown ("Ctrl+Shift+").
  const [held, setHeld] = useState("");
  const [message, setMessage] = useState<Message | null>(null);
  const addButtons = useRef<Partial<Record<ShortcutAction, HTMLButtonElement | null>>>({});
  const [focusAdd, setFocusAdd] = useState<ShortcutAction | null>(null);

  // What the keys known by their place print on this keyboard (util/shortcuts).
  useEffect(() => { refreshKeyboardLayout(); }, []);
  // Back on the row's + once its keys are taken, for the keyboard's sake.
  useEffect(() => {
    if (!focusAdd) return;
    addButtons.current[focusAdd]?.focus();
    setFocusAdd(null);
  }, [focusAdd]);

  const start = (action: ShortcutAction) => {
    setMessage(null);
    setHeld("");
    setRecording(action);
  };
  const stop = (refocus: boolean) => {
    if (refocus && recording) setFocusAdd(recording);
    setRecording(null);
    setHeld("");
  };

  const onRecordKey = (e: KeyboardEvent<HTMLInputElement>) => {
    const ev = e.nativeEvent;
    const action = recording;
    if (!action || ev.isComposing || ev.keyCode === 229) return;
    const plain = !e.ctrlKey && !e.altKey && !e.metaKey;
    // Tab moves on, as anywhere; leaving the field ends the taking of keys.
    if (e.key === "Tab" && plain) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape" && plain && !e.shiftKey) {
      stop(true);
      return;
    }
    const combo = comboOf(ev);
    if (!combo) {
      setHeld(modifiersLabel({ ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, meta: e.metaKey }));
      return;
    }
    setHeld("");
    const keys = comboLabel(combo);
    if (!usableCombo(combo)) {
      setMessage({
        action,
        tone: "error",
        text: APPLE_KEYS
          ? t("Hold ⌘, ⌃ or ⌥ with the key. F1 to F24 work without them.")
          : t("Hold Ctrl or Alt with the key. F1 to F24 work without them."),
      });
      return;
    }
    if (SYSTEM_KEYS.includes(comboText(combo))) {
      setMessage({ action, tone: "error", text: t("{keys} belongs to the system, so it can't be a shortcut here.", { keys }) });
      return;
    }
    const owner = shortcutOwner(combo);
    if (owner && owner !== action) {
      setMessage({ action, tone: "error", text: t("{keys} is already the shortcut for “{action}”. Remove it there first.", { keys, action: t(LABELS[owner]) }) });
      return;
    }
    if (!owner) {
      setShortcuts(action, [...table[action], combo]);
      // Copy says it in its note: with nothing selected the key goes on.
      if (action !== "copy" && reachesTerminal(combo)) {
        setMessage({ action, tone: "note", text: t("Programs in a terminal no longer get {keys}.", { keys }) });
      }
    }
    stop(true);
  };
  const onRecordKeyUp = (e: KeyboardEvent<HTMLInputElement>) => {
    setHeld(modifiersLabel({ ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, meta: e.metaKey }));
  };

  const resetAll = async () => {
    const ok = await confirm({
      title: t("Reset all shortcuts?"),
      message: t("Every action gets its default keys back."),
      // Not "Reset": that one is the error screen's retry button.
      okLabel: t("Reset|to defaults"),
    });
    if (!ok) return;
    setMessage(null);
    resetShortcuts();
  };

  const anyChanged = SHORTCUT_ACTIONS.some(shortcutChanged);

  return (
    <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
      <div className="flex items-center gap-2 text-zinc-400 font-semibold text-xs">
        <Keyboard size={14} /> {t("Keyboard shortcuts")}
      </div>
      <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 shadow-xl space-y-3" data-shortcut-table="">
        <p className="text-[12px] text-zinc-400 leading-relaxed">
          {t("Click + beside an action, then press the keys you want for it. An action can have several keys; × removes one.")}
        </p>
        <table className="w-full border-collapse text-left">
          {GROUPS.map((group) => (
            <tbody key={group.title}>
              <tr>
                <th colSpan={3} scope="colgroup" className="pt-3 pb-1 text-[11px] font-semibold text-zinc-500">
                  {t(group.title)}
                </th>
              </tr>
              {group.actions.map((action) => {
                const keys = table[action];
                const taking = recording === action;
                const note = NOTES[action];
                const name = t(LABELS[action]);
                const said = message?.action === action ? message : null;
                return [
                  <tr key={action} data-shortcut={action} className="border-t border-white/5 align-top">
                    <th scope="row" className="py-2 pr-3 font-normal">
                      <div className="text-[13px] text-zinc-100">{name}</div>
                      {note && (
                        <div className="text-[11px] text-zinc-500 leading-snug mt-0.5">
                          {t(note, { keys: comboTextLabel("Ctrl+C") })}
                        </div>
                      )}
                    </th>
                    <td className="py-2">
                      <div className="flex flex-wrap items-center justify-end gap-1.5">
                        {keys.length === 0 && !taking && <span className="text-[12px] text-zinc-600">{t("None")}</span>}
                        {keys.map((combo) => {
                          const label = comboLabel(combo);
                          return (
                            <span
                              key={comboText(combo)}
                              data-keys={comboText(combo)}
                              className="inline-flex items-center h-7 rounded-md border border-white/10 bg-black/40 pl-2 pr-0.5"
                            >
                              <kbd className="font-mono text-[11.5px] text-zinc-200 whitespace-nowrap">{label}</kbd>
                              <button
                                type="button"
                                onClick={() => {
                                  setMessage(null);
                                  setShortcuts(action, keys.filter((k) => k !== combo));
                                }}
                                aria-label={t("Remove {keys}", { keys: label })}
                                title={t("Remove {keys}", { keys: label })}
                                className="ml-0.5 w-6 h-6 flex items-center justify-center rounded text-zinc-500 hover:text-rose-300 hover:bg-white/5"
                              >
                                <X size={12} />
                              </button>
                            </span>
                          );
                        })}
                        {taking && (
                          <input
                            autoFocus
                            readOnly
                            data-shortcut-recorder=""
                            value={held}
                            placeholder={t("Press the keys…")}
                            aria-label={t("New keys for {action}", { action: name })}
                            onKeyDown={onRecordKey}
                            onKeyUp={onRecordKeyUp}
                            onBlur={() => stop(false)}
                            className="w-36 h-7 rounded-md bg-black border border-primary/60 px-2 text-[11.5px] font-mono text-zinc-100 placeholder:text-zinc-500 outline-none"
                          />
                        )}
                      </div>
                    </td>
                    {/* Its own column, so the buttons line up whatever the keys take. */}
                    <td className="py-2 pl-1.5 w-16">
                      <div className="flex items-center justify-end gap-1">
                        {shortcutChanged(action) && (
                          <button
                            type="button"
                            onClick={() => {
                              setMessage(null);
                              resetShortcuts(action);
                            }}
                            aria-label={t("Back to the default keys for {action}", { action: name })}
                            title={t("Back to the default keys")}
                            className="w-7 h-7 flex items-center justify-center rounded-md text-zinc-500 hover:text-white hover:bg-white/5"
                          >
                            <RotateCcw size={13} />
                          </button>
                        )}
                        <button
                          type="button"
                          ref={(el) => { addButtons.current[action] = el; }}
                          onClick={() => start(action)}
                          aria-label={t("Add a shortcut for {action}", { action: name })}
                          aria-pressed={taking}
                          title={t("Add a shortcut")}
                          className={`w-7 h-7 flex items-center justify-center rounded-md border ${taking ? "border-primary/60 text-white" : "border-white/10 text-zinc-400 hover:text-white hover:border-primary/40"}`}
                        >
                          <Plus size={14} />
                        </button>
                      </div>
                    </td>
                  </tr>,
                  said && (
                    <tr key={`${action}-said`}>
                      <td colSpan={3} className="pb-2">
                        <p
                          role={said.tone === "error" ? "alert" : "status"}
                          className={`text-[11.5px] leading-snug ${said.tone === "error" ? "text-rose-300" : "text-amber-300/90"}`}
                        >
                          {said.text}
                        </p>
                      </td>
                    </tr>
                  ),
                ];
              })}
            </tbody>
          ))}
        </table>
        <p className="text-[11px] text-zinc-500 leading-relaxed">
          {t("In a remote desktop tab, every key goes to the remote computer.")}
        </p>
        {anyChanged && (
          <button
            type="button"
            onClick={() => void resetAll()}
            className="px-4 h-9 bg-zinc-900 border border-white/5 text-zinc-300 rounded-xl text-xs font-semibold hover:bg-white/5 transition-all w-full"
          >
            {t("Reset all shortcuts")}
          </button>
        )}
      </div>
    </section>
  );
}
