import { memo, useMemo, type Ref } from "react";
import { AlertTriangle } from "lucide-react";
import { inspect, segments, type Inspection, type Segment } from "../../util/hiddenChars";

// Text exactly as it will run or be written: monospace, left to right,
// never Markdown. Hidden characters and long runs of blanks or of blank lines
// show as escapes such as ⟨U+202E⟩, ⟨40 spaces⟩ and ⟨40 empty lines⟩
// (util/hiddenChars), set off in amber so that no plain text can pass for one.

export interface ExactTextProps {
  text: string;
  /** What the warning calls the text: "This command contains hidden characters". */
  what?: "command" | "text";
  /** Runs of 16 or more blanks as ⟨40 spaces⟩ and of two or more blank lines as ⟨40 empty lines⟩ (the default). Off for a file, where they are mostly indentation and spacing. */
  blankRuns?: boolean;
  /** Scrolls past this height. */
  maxHeight?: number | string;
  /** The box the text is drawn in, the one that scrolls. */
  boxRef?: Ref<HTMLPreElement>;
  className?: string;
}

// Escapes drawn at most. Only a text made to be odd gets near it (a
// 20,000-line file with CR LF line ends has 20,000); past it, the rest isn't
// drawn and a note says so.
const MAX_ESCAPES = 30_000;

const ESCAPE = "rounded-[3px] bg-amber-500/15 px-[3px] text-amber-300";

/** The pieces of a text (util/hiddenChars `segments`), escapes set off; for inside a `<pre>` or a cell. */
export const ExactSegments = ({ segs }: { segs: Segment[] }) => (
  <>{segs.map((s, i) => (s.kind === "text" ? s.text : <span key={i} className={ESCAPE}>{s.text}</span>))}</>
);

/** `segs` up to MAX_ESCAPES escapes, and how many characters are left after them. */
export function capEscapes(segs: Segment[]): { shown: Segment[]; rest: number } {
  let escapes = 0;
  for (let i = 0; i < segs.length; i++) {
    if (segs[i].kind !== "text" && ++escapes > MAX_ESCAPES) {
      let rest = 0;
      for (const s of segs.slice(i)) rest += s.kind === "text" ? s.text.length : s.raw.length;
      return { shown: segs.slice(0, i), rest };
    }
  }
  return { shown: segs, rest: 0 };
}

/** The notes under a text: hidden characters, blank lines in a row, letters outside ASCII. `what` is the text's name: "command", "change". */
export const HiddenWarnings = ({ found, what, className = "mt-1.5" }: { found: Inspection; what: string; className?: string }) => {
  if (!found.hidden && !found.blankLines && !found.nonAsciiLetters) return null;
  return (
    <div className={`${className} space-y-1 text-[11px] leading-snug`}>
      {found.hidden && (
        <p className="flex items-start gap-1.5 text-amber-300">
          <AlertTriangle size={12} aria-hidden="true" className="mt-px shrink-0" />
          This {what} contains hidden characters
        </p>
      )}
      {found.blankLines && (
        <p className="flex items-start gap-1.5 text-amber-300">
          <AlertTriangle size={12} aria-hidden="true" className="mt-px shrink-0" />
          This {what} contains blank lines in a row, each run shown here as one line
        </p>
      )}
      {found.nonAsciiLetters && (
        <p className="flex items-start gap-1.5 text-amber-400/80">
          <AlertTriangle size={12} aria-hidden="true" className="mt-px shrink-0" />
          Contains letters outside ASCII, which can look like ASCII ones
        </p>
      )}
    </div>
  );
};

export const ExactText = memo(function ExactText({ text, what = "command", blankRuns = true, maxHeight, boxRef, className = "" }: ExactTextProps) {
  const segs = useMemo(() => segments(text, { blankRuns }), [text, blankRuns]);
  const found = useMemo(() => inspect(text, { blankLines: blankRuns }), [text, blankRuns]);
  // Where the drawing stops (MAX_ESCAPES), and how much is left after it.
  const { shown, rest } = useMemo(() => capEscapes(segs), [segs]);

  return (
    <div className={className}>
      <pre
        ref={boxRef}
        dir="ltr"
        style={maxHeight !== undefined ? { maxHeight } : undefined}
        className="m-0 overflow-auto custom-scrollbar whitespace-pre-wrap break-words [overflow-wrap:anywhere] select-text rounded-lg border border-white/5 bg-black/40 px-3 py-2 font-mono text-[12px] leading-relaxed text-zinc-200"
      >
        <code><ExactSegments segs={shown} /></code>
      </pre>
      {rest > 0 && (
        <p className="mt-1.5 text-[11px] text-amber-400/90">
          … {rest.toLocaleString()} more characters not shown: too many hidden characters to draw.
        </p>
      )}
      <HiddenWarnings found={found} what={what} />
    </div>
  );
});
