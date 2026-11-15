import { Fragment, memo, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Check, Copy, Image as ImageIcon, TerminalSquare } from "lucide-react";
import { inspect, segments, stripHidden } from "../../util/hiddenChars";
import { ExactSegments, HiddenWarnings, capEscapes } from "./ExactText";
import {
  Block, Chunk, Inline, LinkDef, ListBlock, ListItem, RefMap, Split, TableBlock,
  inlineDir, inlineText, parseChunk, parseInline, splitBlocks, textDir,
} from "./markdownParse";

// An AI reply, drawn from its Markdown (parsed in markdownParse.ts) with React
// elements only: nothing here sets HTML, so markup in a reply shows as text.
// Links and images are buttons that hand their http(s) URL to `onOpenLink`,
// which asks before anything opens; any other URL stays plain text, and no
// image is ever loaded.
//
// Replies stream in and this redraws many times a second. The text is cut into
// its top-level blocks, and a block whose text hasn't changed keeps its parse
// and skips its redraw, so an update costs about as much as the block still
// being written, not the whole reply.

export interface MarkdownProps {
  text: string;                          // the reply so far
  streaming?: boolean;                   // true while text is still arriving: the last block may be unfinished
  onOpenLink: (url: string) => void;     // the parent asks the user, then opens it
  onInsert?: (code: string) => void;     // when given, sh/bash/shell code blocks get an "Insert into terminal" button
}

// Code blocks in these languages can be put into the terminal.
const SHELL_LANGS = new Set(["sh", "bash", "shell"]);
// Code blocks in these languages hold commands, which get pasted into a
// terminal: Copy leaves out the hidden characters, as Insert does, and a note
// tells of letters outside ASCII.
const COMMAND_LANGS = new Set([...SHELL_LANGS, "zsh", "console"]);
// These hold nothing but commands, so long runs of blanks are drawn as escapes
// too, as a command card draws them (util/hiddenChars). In other blocks such
// runs are mostly indentation or the columns of a table, and stay as they are.
const BLANK_RUN_LANGS = new Set([...SHELL_LANGS, "zsh"]);
// A code block shows this much at most (about 200 KB). A reply can hold a much
// bigger one, which would make the page slow to lay out; Copy still takes all.
const CODE_SHOWN = 200_000;
const NO_REFS: RefMap = new Map();

// What blocks need from the Markdown drawing them. The same object from one
// render to the next, so that finished blocks can skip theirs.
interface Ctx {
  open: (url: string) => void;
  insert: ((code: string) => void) | null;
}

// One block's render. Its inline text is parsed once, though a quote, list or
// table also reads its first text to find its direction.
interface Env {
  ctx: Ctx;
  inlines: (src: string) => Inline[];
}

type Dir = "rtl" | "ltr";

export function Markdown({ text, streaming = false, onOpenLink, onInsert }: MarkdownProps): JSX.Element {
  // The parent may pass new functions on every render. Finished blocks mustn't
  // redraw for that, so they get wrappers that never change and call the
  // latest ones.
  const openRef = useRef(onOpenLink);
  openRef.current = onOpenLink;
  const insertRef = useRef(onInsert);
  insertRef.current = onInsert;
  const canInsert = onInsert !== undefined;
  const ctx = useMemo<Ctx>(
    () => ({
      open: (url) => openRef.current(url),
      insert: canInsert ? (code) => insertRef.current?.(code) : null,
    }),
    [canInsert],
  );

  // The last render's split, which the next one starts from while the text
  // only grows, and its parsed blocks by their text: a block that reads the
  // same is taken from there, the others are parsed, and those gone dropped.
  const split = useRef<Split | null>(null);
  const parsed = useRef<Map<string, Chunk> | null>(null);
  const chunks = useMemo(() => {
    split.current = splitBlocks(text, split.current);
    const srcs = split.current.srcs;
    const known = parsed.current;
    const next = new Map<string, Chunk>();
    const out = srcs.map((src, i) => {
      // The last block of a streaming reply may be unfinished (a code block
      // still waiting for its closing fence), so it isn't the same as that
      // text once the reply is complete.
      const open = streaming && i === srcs.length - 1;
      const key = open ? "\0" + src : src;
      const chunk = next.get(key) ?? known?.get(key) ?? parseChunk(src, open);
      next.set(key, chunk);
      return chunk;
    });
    parsed.current = next;
    return out;
  }, [text, streaming]);

  // Link definitions ("[1]: https://…") count wherever they are in the reply.
  // Not those of a block still arriving: its last URL may be cut short, and
  // each new letter would redraw every block. The map stays the same object
  // while the definitions do, so finished blocks keep skipping their redraw.
  const lastRefs = useRef<{ key: string; map: RefMap }>({ key: "", map: NO_REFS });
  const refs = useMemo(() => {
    const defs: LinkDef[] = [];
    chunks.forEach((chunk, i) => {
      if (!(streaming && i === chunks.length - 1)) for (const d of chunk.defs) defs.push(d);
    });
    const key = defs.map((d) => `${d.label}\0${d.url}`).join("\n");
    if (key !== lastRefs.current.key) {
      const map = new Map<string, string>();
      for (const d of defs) if (!map.has(d.label)) map.set(d.label, d.url);
      lastRefs.current = { key, map };
    }
    return lastRefs.current.map;
  }, [chunks, streaming]);

  return (
    <div className="min-w-0 space-y-2 break-words text-start text-[13px] leading-relaxed text-zinc-200">
      {chunks.map((chunk, i) => (
        <ChunkView key={i} chunk={chunk} refs={refs} ctx={ctx} />
      ))}
    </div>
  );
}

// One top-level block: redrawn only when its text was parsed again, or when
// the link definitions or the Insert button came or went.
const ChunkView = memo(function ChunkView({ chunk, refs, ctx }: { chunk: Chunk; refs: RefMap; ctx: Ctx }) {
  const seen = new Map<string, Inline[]>();
  const env: Env = {
    ctx,
    inlines: (src) => {
      let r = seen.get(src);
      if (!r) seen.set(src, (r = parseInline(src, refs)));
      return r;
    },
  };
  return <>{chunk.blocks.map((b, i) => renderBlock(b, i, env, 0))}</>;
});

// Sizes stay close to the text's: this sits in a chat bubble.
const HEADING = [
  "",
  "text-[16px] font-bold text-zinc-50",
  "text-[15px] font-bold text-zinc-50",
  "text-[14px] font-semibold text-zinc-100",
  "text-[13px] font-semibold text-zinc-100",
  "text-[13px] font-semibold text-zinc-300",
  "text-[12px] font-semibold uppercase tracking-wide text-zinc-400",
];
const BULLETS = ["list-disc", "list-[circle]", "list-[square]"];

// `lists`: how many lists are around this block, for the bullet shape.
function renderBlock(b: Block, key: number, env: Env, lists: number): JSX.Element {
  switch (b.t) {
    case "p":
      return <p key={key} dir="auto">{renderInlines(env.inlines(b.text), env.ctx)}</p>;
    case "h": {
      const H = `h${b.level}` as "h1" | "h2" | "h3" | "h4" | "h5" | "h6";
      return (
        <H key={key} dir="auto" className={`pt-1 leading-snug ${HEADING[b.level]}`}>
          {renderInlines(env.inlines(b.text), env.ctx)}
        </H>
      );
    }
    case "code":
      return <CodeBlock key={key} lang={b.lang} code={b.code} open={b.open} insert={env.ctx.insert} />;
    case "hr":
      return <hr key={key} className="border-white/10" />;
    case "quote":
      return (
        <blockquote key={key} dir={blocksDir(b.c, env) ?? "auto"} className="space-y-2 border-s-2 border-white/15 ps-3 text-zinc-400">
          {b.c.map((c, i) => renderBlock(c, i, env, lists))}
        </blockquote>
      );
    case "list":
      return renderList(b, key, env, lists);
    case "table":
      return renderTable(b, key, env);
    case "plain":
      return <p key={key} dir="auto" className="whitespace-pre-wrap">{b.text}</p>;
  }
}

// The direction of the first letter in these blocks, as dir="auto" would find
// it. A quote, list or table can't take dir="auto" itself: that skips text in
// children with a dir of their own, which all of theirs have, so a Persian
// quote would come out left to right with its bar on the wrong side.
function blocksDir(blocks: Block[], env: Env): Dir | null {
  for (const b of blocks) {
    const d =
      b.t === "p" || b.t === "h" ? inlineDir(env.inlines(b.text))
      : b.t === "plain" ? textDir(b.text)
      : b.t === "quote" ? blocksDir(b.c, env)
      : b.t === "list" ? listDir(b, env)
      : b.t === "table" ? tableDir(b, env)
      : null;
    if (d) return d;
  }
  return null;
}

function listDir(b: ListBlock, env: Env): Dir | null {
  for (const item of b.items) {
    const d = blocksDir(item.c, env);
    if (d) return d;
  }
  return null;
}

function tableDir(b: TableBlock, env: Env): Dir | null {
  for (const row of [b.head, ...b.rows]) {
    for (const cell of row) {
      const d = inlineDir(env.inlines(cell));
      if (d) return d;
    }
  }
  return null;
}

function renderList(b: ListBlock, key: number, env: Env, lists: number): JSX.Element {
  const items = b.items.map((item, i) => (
    <li key={i} dir="auto" className={`${b.tight ? "space-y-1" : "space-y-2"}${item.task === null ? "" : " list-none"}`}>
      {renderItem(item, b.tight, env, lists + 1)}
    </li>
  ));
  const dir = listDir(b, env) ?? "auto";
  const gap = b.tight ? "space-y-0.5" : "space-y-2";
  if (!b.ordered) {
    return (
      <ul key={key} dir={dir} className={`${BULLETS[lists % BULLETS.length]} ${gap} ps-5 marker:text-zinc-500`}>
        {items}
      </ul>
    );
  }
  // Room for the widest number: a list may start at 2024.
  const digits = String(b.start + b.items.length - 1).length;
  return (
    <ol key={key} dir={dir} start={b.start === 1 ? undefined : b.start} className={`list-decimal ${gap} marker:text-zinc-500`} style={{ paddingInlineStart: `${0.6 * digits + 1}em` }}>
      {items}
    </ol>
  );
}

function renderItem(item: ListItem, tight: boolean, env: Env, lists: number): JSX.Element[] {
  const box = item.task === null ? null : (
    <span role="img" aria-label={item.task ? "Done" : "Not done"} className="me-1.5 select-none text-zinc-400">
      {item.task ? "☑" : "☐"}
    </span>
  );
  const out: JSX.Element[] = [];
  if (box && item.c[0]?.t !== "p") out.push(<Fragment key="box">{box}</Fragment>);
  item.c.forEach((b, i) => {
    if (b.t !== "p") {
      out.push(renderBlock(b, i, env, lists));
      return;
    }
    const lead = i === 0 ? box : null;
    const text = renderInlines(env.inlines(b.text), env.ctx);
    // A tight list's text sits in the item itself, without paragraph spacing.
    out.push(
      tight ? <span key={i}>{lead}{text}</span> : <p key={i} dir="auto">{lead}{text}</p>,
    );
  });
  return out;
}

function renderTable(b: TableBlock, key: number, env: Env): JSX.Element {
  // A column's own alignment wins; the others follow the cell's direction.
  const align = (i: number) => ({ textAlign: b.align[i] ?? undefined });
  return (
    <div key={key} dir={tableDir(b, env) ?? "auto"} className="max-w-full overflow-x-auto custom-scrollbar rounded-lg border border-white/5">
      <table className="border-collapse text-[12px]">
        <thead className="bg-white/[0.04]">
          <tr>
            {b.head.map((cell, i) => (
              <th key={i} dir="auto" style={align(i)} className="text-start align-bottom px-2.5 py-1.5 border-b border-white/10 font-semibold text-zinc-100">
                {renderInlines(env.inlines(cell), env.ctx)}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {b.rows.map((row, r) => (
            <tr key={r} className="border-t border-white/5 first:border-t-0">
              {row.map((cell, i) => (
                <td key={i} dir="auto" style={align(i)} className="text-start align-top px-2.5 py-1.5">
                  {renderInlines(env.inlines(cell), env.ctx)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {b.hidden > 0 && (
        <p className="px-2.5 py-1.5 border-t border-white/5 text-[11px] text-zinc-500">… {b.hidden} more rows not shown</p>
      )}
    </div>
  );
}

function renderInlines(nodes: Inline[], ctx: Ctx): (JSX.Element | string)[] {
  return nodes.map((n, i) => {
    switch (n.t) {
      case "text":
        return n.v;
      case "br":
        return <br key={i} />;
      case "code":
        return (
          <code key={i} dir="ltr" className="rounded border border-white/[0.06] bg-white/[0.06] px-1 py-px font-mono text-[0.92em] text-zinc-100">
            {n.v}
          </code>
        );
      case "em":
        return <em key={i}>{renderInlines(n.c, ctx)}</em>;
      case "strong":
        return <strong key={i} className="font-semibold text-zinc-100">{renderInlines(n.c, ctx)}</strong>;
      case "del":
        return <del key={i} className="text-zinc-500">{renderInlines(n.c, ctx)}</del>;
      case "link":
        return (
          <LinkButton key={i} url={n.url} label={inlineText(n.c)} onOpen={ctx.open}>
            {renderInlines(n.c, ctx)}
          </LinkButton>
        );
      case "img":
        return <LinkButton key={i} url={n.url} label={n.alt} image onOpen={ctx.open} />;
    }
  });
}

// A link as a button: the parent shows the whole URL and asks before opening
// it, so a label can't pass for somewhere else. The title shows it on hover.
function LinkButton({ url, label, image = false, onOpen, children }: { url: string; label: string; image?: boolean; onOpen: (url: string) => void; children?: React.ReactNode }) {
  const name = label.trim();
  const what = image ? "Image at" : "Link to";
  // A bare link's text is its URL already: said once is enough.
  const named = name !== "" && name !== url && name + "/" !== url;
  return (
    <button
      type="button"
      title={url}
      aria-label={named ? `${name}, ${what.toLowerCase()} ${url}` : `${what} ${url}`}
      onClick={() => onOpen(url)}
      className="inline rounded-sm text-start text-primary underline decoration-primary/40 underline-offset-2 transition-colors hover:decoration-primary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-primary/60"
    >
      {image && <ImageIcon size={12} aria-hidden="true" className="me-1 inline-block -mt-0.5" />}
      {children ?? (name || url)}
    </button>
  );
}

// The part of a code block that is drawn, and how much more there is.
function cutCode(code: string): { text: string; rest: number } {
  if (code.length <= CODE_SHOWN) return { text: code, rest: 0 };
  // At a line break, when there is one near the limit.
  let end = code.lastIndexOf("\n", CODE_SHOWN);
  if (end < CODE_SHOWN * 0.9) {
    end = CODE_SHOWN;
    // not between the two halves of an emoji
    const c = code.charCodeAt(end - 1);
    if (c >= 0xd800 && c <= 0xdbff) end--;
  }
  return { text: code.slice(0, end), rest: code.length - end };
}

// Without the line feeds at its end. A loop, not /\n+$/: that pattern
// backtracks over a long run of them that isn't at the end.
// A run of 16 or more blanks between two parts of one line, which can push the
// rest of a command out of view. Indentation doesn't count. A loop too.
function hasGap(s: string): boolean {
  let run = 0;
  let text = false;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c === 0x20 || c === 0x09) {
      run++;
    } else if (c === 0x0a) {
      run = 0;
      text = false;
    } else {
      if (text && run >= 16) return true;
      run = 0;
      text = true;
    }
  }
  return false;
}

function trimFinalNewlines(s: string): string {
  let end = s.length;
  while (end > 0 && s.charCodeAt(end - 1) === 0x0a) end--;
  return s.slice(0, end);
}

function CodeBlock({ lang, code, open, insert }: { lang: string; code: string; open: boolean; insert: ((code: string) => void) | null }) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const shown = useMemo(() => cutCode(code), [code]);
  const kind = lang.toLowerCase();
  const onInsert = insert && SHELL_LANGS.has(kind) ? insert : null;
  // The reply picks the label, so whatever it is, hidden characters show as
  // escapes, a note tells of a long run of blanks drawn as it is, and Copy
  // leaves out the line feeds at the end: one there would run the command as
  // it lands. Commands, and blocks with no language (often commands too), are
  // also copied without their hidden characters.
  const command = COMMAND_LANGS.has(kind);
  const exact = command || kind === "";
  const blankRuns = BLANK_RUN_LANGS.has(kind);
  // The block's escapes, at most as many as a card draws.
  const drawn = useMemo(() => capEscapes(segments(shown.text, { blankRuns })), [blankRuns, shown.text]);
  // Its notes go by the whole block, which Copy takes; while it streams in,
  // by what is drawn, so that each update costs no more than the drawing.
  const found = useMemo(() => {
    const f = inspect(open ? shown.text : code);
    return command ? f : { ...f, nonAsciiLetters: false };
  }, [command, open, shown.text, code]);
  const gap = useMemo(() => !blankRuns && hasGap(open ? shown.text : code), [blankRuns, open, shown.text, code]);

  // writeText is called within the click itself: WebKit (the iOS app) turns
  // down a clipboard write that comes later.
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(trimFinalNewlines(exact ? stripHidden(code) : code));
      setCopied(true);
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1500);
    } catch (e) {
      console.error("Clipboard write failed:", e);
    }
  };

  // Not while the block is still arriving (half a command), nor when it is cut
  // short on screen: what goes into the terminal must be what was shown.
  const insertBlocked = open ? "Wait until the block is complete" : shown.rest > 0 || drawn.rest > 0 ? "Too long to insert here; use Copy" : null;
  const rest = shown.rest < 1024 ? `${shown.rest} characters` : `about ${Math.round(shown.rest / 1024)} KB`;

  return (
    <div dir="ltr" className="overflow-hidden rounded-lg border border-white/5 bg-black/40">
      <div className="flex items-center gap-1.5 py-1 pe-1 ps-2.5 border-b border-white/5 bg-white/[0.03]">
        <span className="flex-1 min-w-0 truncate text-[10px] font-bold uppercase tracking-wider text-zinc-500">{lang || "code"}</span>
        {open && <span className="shrink-0 text-[10px] text-zinc-500">writing…</span>}
        {onInsert && (
          <button
            type="button"
            onClick={() => onInsert(code)}
            disabled={insertBlocked !== null}
            aria-label="Insert into terminal"
            title={insertBlocked ?? "Insert into terminal"}
            className="shrink-0 h-10 sm:h-6 px-2 rounded-md bg-primary/10 text-primary border border-primary/20 hover:bg-primary hover:text-zinc-950 hover:border-primary flex items-center gap-1 text-[10px] font-bold transition-all disabled:opacity-40 disabled:cursor-not-allowed"
          >
            <TerminalSquare size={11} /> Insert into terminal
          </button>
        )}
        <button
          type="button"
          onClick={copy}
          aria-label={copied ? "Copied" : "Copy code"}
          title="Copy to clipboard"
          className="shrink-0 h-10 sm:h-6 px-2 rounded-md bg-white/[0.04] border border-white/10 hover:bg-white/10 hover:border-white/20 text-zinc-300 hover:text-white flex items-center gap-1 text-[10px] font-bold transition-all"
        >
          {copied ? <Check size={11} className="text-emerald-400" /> : <Copy size={11} />}
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <pre dir="ltr" className="m-0 overflow-x-auto custom-scrollbar whitespace-pre px-3 py-2.5 font-mono text-[12px] leading-relaxed text-zinc-200">
        <code><ExactSegments segs={drawn.shown} /></code>
      </pre>
      <HiddenWarnings found={found} what={command ? "command" : "code"} className="px-3 py-1.5 border-t border-white/5" />
      {gap && (
        <p className="flex items-start gap-1.5 px-3 py-1.5 border-t border-white/5 text-[11px] leading-snug text-amber-400/80">
          <AlertTriangle size={12} aria-hidden="true" className="mt-px shrink-0" />
          A line here has a long run of blanks: scroll sideways to see all of it.
        </p>
      )}
      {drawn.rest > 0 && (
        <p className="px-3 py-1.5 border-t border-white/5 text-[11px] text-amber-400/90">
          … {drawn.rest.toLocaleString()} more characters not shown: too many hidden characters to draw.
        </p>
      )}
      {shown.rest > 0 && (
        <p className="px-3 py-1.5 border-t border-white/5 text-[11px] text-amber-400/90">
          … cut: {rest} more not shown. Copy takes the whole block.
        </p>
      )}
    </div>
  );
}
