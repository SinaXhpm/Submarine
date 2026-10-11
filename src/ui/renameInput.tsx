import { useState } from "react";

// The inline name editor in the session-tab and terminal-tab menus: it opens
// focused with the name selected; Enter keeps the name, Esc goes back to the
// menu. What an empty name means is up to `onCommit` (both callers go back
// to the default name).
export const RenameInput = ({ initialValue, placeholder, onCommit, onCancel }: {
  initialValue: string;
  placeholder?: string;
  onCommit: (value: string) => void;
  onCancel: () => void;
}) => {
  const [draft, setDraft] = useState(initialValue);
  return (
    <div className="px-2 py-1.5 flex items-center gap-1.5">
      <input
        autoFocus
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onFocus={(e) => e.target.select()}
        onKeyDown={(e) => {
          // Enter that ends an input-method composition isn't a commit.
          if (e.nativeEvent.isComposing) return;
          if (e.key === "Enter") onCommit(draft);
          else if (e.key === "Escape") onCancel();
        }}
        placeholder={placeholder ?? "Name…"}
        maxLength={60}
        aria-label="New name"
        className="flex-1 min-w-0 bg-black/40 border border-primary/40 rounded px-2 py-1 text-[11px] text-zinc-100 outline-none"
      />
    </div>
  );
};
