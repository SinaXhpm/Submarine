import { memo, useMemo, useState } from "react";
import { diffRows, lineDiff, type DiffLine } from "../../util/lineDiff";
import { inspect, segments } from "../../util/hiddenChars";
import { ExactSegments, ExactText, HiddenWarnings } from "./ExactText";

// The line diff of a write or edit card: unified, 3 lines of context,
// the unchanged lines between changes folded, line numbers on both sides and
// hidden characters shown as in ExactText. When the texts are too large or
// too different to compare (util/lineDiff), the new content is shown whole.

export interface LineDiffProps {
  /** Null: a new file, every line added. */
  oldText: string | null;
  newText: string;
  /** Scrolls past this height. */
  maxHeight?: number | string;
  className?: string;
}

// Rows drawn at first, and added by each "Show more": a 20,000-line file
// drawn at once would hold the page up.
const ROWS_STEP = 1000;

const ROW: Record<DiffLine["kind"], string> = {
  same: "text-zinc-400",
  add: "bg-emerald-500/10 text-emerald-100",
  del: "bg-rose-500/10 text-rose-100",
};
const SIGN: Record<DiffLine["kind"], string> = { same: " ", add: "+", del: "-" };
const SIGN_COLOUR: Record<DiffLine["kind"], string> = { same: "", add: "text-emerald-400", del: "text-rose-400" };
const NUMBER = "select-none whitespace-nowrap ps-2 pe-1.5 text-right align-top tabular-nums text-zinc-600";

export const LineDiff = memo(function LineDiff({ oldText, newText, maxHeight, className = "" }: LineDiffProps) {
  const diff = useMemo(() => lineDiff(oldText, newText), [oldText, newText]);
  const rows = useMemo(() => (diff.status === "ok" ? diffRows(diff) : []), [diff]);
  // What the change brings in, for the notes under it.
  const found = useMemo(() => {
    if (diff.status !== "ok") return null;
    const added: string[] = [];
    for (const h of diff.hunks) for (const line of h.lines) if (line.kind === "add") added.push(line.text);
    return inspect(added.join("\n"));
  }, [diff]);
  const [limit, setLimit] = useState(ROWS_STEP);

  if (diff.status !== "ok") {
    return (
      <div className={className}>
        <p className="mb-1.5 text-[11px] text-zinc-500">
          {diff.status === "too_large" ? "Too large to diff" : "Too different to diff"}. The new content:
        </p>
        <ExactText text={newText} what="text" blankRuns={false} maxHeight={maxHeight} />
      </div>
    );
  }

  const shown = rows.length > limit ? rows.slice(0, limit) : rows;
  const left = rows.length - shown.length;
  return (
    <div className={className}>
      <div className="mb-1 flex items-center gap-2 font-mono text-[11px]">
        <span className="text-emerald-400">+{diff.added}</span>
        <span className="text-rose-400">−{diff.removed}</span>
        {oldText === null && <span className="text-zinc-500">new file</span>}
      </div>
      {rows.length === 0 ? (
        <p className="rounded-lg border border-white/5 bg-black/40 px-3 py-2 text-[12px] text-zinc-500">
          {oldText === null ? "Empty file" : "No changes"}
        </p>
      ) : (
        <div
          style={maxHeight !== undefined ? { maxHeight } : undefined}
          className="overflow-auto custom-scrollbar rounded-lg border border-white/5 bg-black/40"
        >
          <table dir="ltr" className="w-full border-collapse font-mono text-[12px] leading-relaxed">
            <tbody>
              {shown.map((r, i) =>
                r.kind === "fold" ? (
                  <tr key={i} className="bg-white/[0.02]">
                    <td colSpan={4} className="select-none px-3 py-0.5 text-[11px] text-zinc-500">
                      … {r.count.toLocaleString()} unchanged lines
                    </td>
                  </tr>
                ) : (
                  <tr key={i} className={ROW[r.kind]}>
                    <td className={NUMBER}>{r.oldNo ?? ""}</td>
                    <td className={NUMBER}>{r.newNo ?? ""}</td>
                    <td className={`select-none whitespace-pre pe-1.5 align-top ${SIGN_COLOUR[r.kind]}`}>{SIGN[r.kind]}</td>
                    <td className="w-full whitespace-pre-wrap break-words [overflow-wrap:anywhere] select-text pe-3 align-top">
                      <ExactSegments segs={segments(r.text, { blankRuns: false })} />
                      {r.noEol && <span className="ms-2 select-none text-[10.5px] text-zinc-500">no line break at end</span>}
                    </td>
                  </tr>
                ),
              )}
            </tbody>
          </table>
          {left > 0 && (
            <button
              type="button"
              onClick={() => setLimit((n) => n + ROWS_STEP)}
              className="w-full border-t border-white/5 py-3 sm:py-1.5 text-[11px] font-bold text-zinc-400 hover:bg-white/5 hover:text-zinc-200"
            >
              Show more ({left.toLocaleString()} lines left)
            </button>
          )}
        </div>
      )}
      {found && <HiddenWarnings found={found} what="change" />}
    </div>
  );
});
