import { useState } from "react";
import { Copy, Check } from "lucide-react";

// Touch-friendly copy affordance for value-heavy fields (IP addresses,
// ports, PIDs, unit names, etc). On desktop the icon is visible-on-hover
// so it doesn't clutter the tabular display; on touch there's no hover
// state, so we always render it at full opacity — the sm:opacity-40
// pair swaps this in at the small breakpoint.

interface Props {
  value: string;
  className?: string;
  children?: React.ReactNode;
}

export default function CopyValue({ value, className, children }: Props) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    navigator.clipboard.writeText(value).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };
  return (
    <span className={`inline-flex items-center gap-1 group ${className || ""}`}>
      <span>{children ?? value}</span>
      <button
        type="button"
        aria-label="Copy to clipboard"
        onClick={copy}
        className="opacity-100 sm:opacity-40 sm:group-hover:opacity-100 transition-opacity p-0.5 rounded hover:bg-white/10 shrink-0"
      >
        {copied ? <Check size={11} className="text-emerald-400" /> : <Copy size={11} />}
      </button>
    </span>
  );
}
