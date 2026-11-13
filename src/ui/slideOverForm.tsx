import { useCallback, useEffect, useRef } from "react";
import { useUnsavedPrompt, type UnsavedPromptOptions } from "./confirm";
import { t, tk } from "../i18n";

// The question for a form closed with changes in it. A new item is offered for
// saving as a whole; one that exists is named as it was saved. Whole sentences
// per kind, so a translation can give each noun its own grammar.
export type UnsavedKind = "server" | "command" | "note" | "login" | "key";

const NEW_TITLE: Record<UnsavedKind, string> = {
  server: tk("Save this server?"),
  command: tk("Save this command?"),
  note: tk("Save this note?"),
  login: tk("Save this login?"),
  key: tk("Save this SSH key?"),
};
const NEW_MESSAGE: Record<UnsavedKind, string> = {
  server: tk("This new server isn't saved yet. Save it or discard it?"),
  command: tk("This new command isn't saved yet. Save it or discard it?"),
  note: tk("This new note isn't saved yet. Save it or discard it?"),
  login: tk("This new login isn't saved yet. Save it or discard it?"),
  key: tk("This new SSH key isn't saved yet. Save it or discard it?"),
};
const CHANGED_MESSAGE: Record<UnsavedKind, string> = {
  server: tk("You changed this server. Save the changes or discard them?"),
  command: tk("You changed this command. Save the changes or discard them?"),
  note: tk("You changed this note. Save the changes or discard them?"),
  login: tk("You changed this login. Save the changes or discard them?"),
  key: tk("You changed this SSH key. Save the changes or discard them?"),
};

export const unsavedQuestion = (kind: UnsavedKind, isNew: boolean, savedName?: string | null): UnsavedPromptOptions => {
  if (isNew) return { title: t(NEW_TITLE[kind]), message: t(NEW_MESSAGE[kind]) };
  const name = String(savedName ?? "").trim();
  return {
    title: t("Save changes?"),
    message: name
      ? t("You changed \"{name}\". Save the changes or discard them?", { name })
      : t(CHANGED_MESSAGE[kind]),
  };
};

interface UnsavedGuardOptions<T> {
  open: boolean;
  /** The form's data. */
  value: T;
  /** Closes the form, throwing away what is in it. */
  close: () => void;
  /** Saves the form; it closes the form itself when the save works. */
  save: () => unknown;
  /** The question, from the data as the form opened with it. */
  question: (opened: T) => UnsavedPromptOptions;
  /** The data as compared (JSON), when part of it must stay out. */
  snapshot?: (value: T) => string;
  /** A save of the form's own is running. */
  busy?: () => boolean;
}

// A form in a slide-over (it stays mounted, and slides out when closed). A
// close, by a click beside it or by its X, goes through at once when nothing
// changed since the form opened. With changes, it asks: save them, discard
// them or keep editing.
//  - What the form opened with is kept (as JSON) while it is open, and dropped
//    when it closes.
//  - A click beside the form must start there too: a text selection dragged
//    out of a field ends in a click on the backdrop as well. That press leaves
//    the focus in the field, so "Keep editing" goes back to it. Clicks from a
//    portal inside the form (they bubble here through React) don't count.
//  - Saves run one at a time; the Save button uses the returned `save` too. A
//    close while one runs does nothing.
export function useUnsavedGuard<T>(options: UnsavedGuardOptions<T>) {
  const prompt = useUnsavedPrompt();
  const latest = useRef(options);
  latest.current = options;
  const snapshot = (v: T) => (latest.current.snapshot ?? JSON.stringify)(v);
  const openedWithRef = useRef("");
  useEffect(() => {
    openedWithRef.current = options.open ? snapshot(options.value) : "";
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [options.open]);

  const savingRef = useRef(false);
  const save = useCallback(async () => {
    if (savingRef.current) return;
    savingRef.current = true;
    try {
      await latest.current.save();
    } finally {
      savingRef.current = false;
    }
  }, []);

  const askingRef = useRef(false);
  const requestClose = useCallback(async () => {
    const o = latest.current;
    if (!o.open || askingRef.current || savingRef.current || o.busy?.()) return;
    const openedWith = openedWithRef.current;
    if (snapshot(o.value) === openedWith) {
      o.close();
      return;
    }
    askingRef.current = true;
    try {
      let opened = o.value;
      try { opened = JSON.parse(openedWith); } catch { /* the data as it is */ }
      const choice = await prompt(o.question(opened));
      // The form was closed some other way while the question was up.
      if (!latest.current.open) return;
      if (choice === "save") await save();
      else if (choice === "discard") latest.current.close();
    } finally {
      askingRef.current = false;
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [prompt, save]);

  // The press is seen as a pointer event (mouse, touch and pen alike); its
  // mouse event only keeps the focus where it was. A click made by a script
  // (the lab tests close forms that way) has no press, and counts.
  const pressedBesideRef = useRef(false);
  const backdropProps = {
    onPointerDown: (e: React.PointerEvent<HTMLElement>) => {
      pressedBesideRef.current = e.target === e.currentTarget;
    },
    onMouseDown: (e: React.MouseEvent<HTMLElement>) => {
      if (e.target === e.currentTarget) e.preventDefault();
    },
    onClick: (e: React.MouseEvent<HTMLElement>) => {
      const beside = pressedBesideRef.current || !e.nativeEvent.isTrusted;
      pressedBesideRef.current = false;
      if (e.target === e.currentTarget && beside) void requestClose();
    },
  };

  return { requestClose, backdropProps, save };
}

// A form's error line, at the top of its scrolling body. It comes into view
// when it appears: the form may be scrolled down (Save at the bottom, or Save
// in the question). Only for the open form: the forms share one message.
export const FormError = ({ message, active }: { message: string; active: boolean }) => {
  const ref = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    if (active && message) ref.current?.scrollIntoView({ block: "nearest" });
  }, [active, message]);
  if (!message) return null;
  return (
    <div ref={ref} role="alert" className="p-3 bg-red-500/10 border border-red-500/20 rounded-xl text-red-500 text-[12px] font-bold">
      {message}
    </div>
  );
};
