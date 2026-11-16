import { useRef, useState } from "react";
import { Eye, EyeOff } from "lucide-react";
import { useT } from "../i18n";

// Multi-line secret input with a show/hide eye toggle. When hidden the
// secret is shown as dots — one per character, like a password field and
// the same ****** display Termimus uses — so it is never readable on
// screen. The eye (or clicking the dots) reveals the real textarea for
// editing. Mirrors PasswordField, which is single-line only — a secret
// command can span lines, so this is a <textarea> instead.
//
// NOTE: the hidden state is a real masked display, not transparent text.
// An earlier version hid the text with `color: transparent`, which left
// it invisible but still plain text in the DOM (selectable, and the
// caller's text colour could override the hiding entirely). Dots are
// actual characters, so nothing can un-hide them by accident.
//
// `className` is forwarded verbatim to both the textarea and the masked
// display so the surrounding styling stays consistent with the
// neighbouring fields in each form. We just add `pr-10` to make room for
// the eye button.

interface Props {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
  rows?: number;
  autoFocus?: boolean;
  onKeyDown?: (e: any) => void;
  disabled?: boolean;
}

const SecretTextarea = ({
  value, onChange, placeholder, className, rows, autoFocus, onKeyDown, disabled,
}: Props) => {
  const t = useT();
  const [show, setShow] = useState(false);
  const textRef = useRef<HTMLTextAreaElement>(null);

  const reveal = () => {
    setShow(true);
    // Focus the textarea once it mounts so editing can start immediately.
    requestAnimationFrame(() => textRef.current?.focus());
  };

  return (
    <div className="relative">
      {show ? (
        <textarea
          ref={textRef}
          value={value}
          onChange={(e: any) => onChange(e.target.value)}
          placeholder={placeholder}
          rows={rows}
          autoFocus={autoFocus}
          onKeyDown={onKeyDown}
          disabled={disabled}
          spellCheck={false}
          className={`pr-10 ${className || ""}`}
        />
      ) : (
        <div
          onClick={reveal}
          title={t("Show secret")}
          className={`pr-10 cursor-text overflow-hidden ${className || ""}`}
          // Keep the masked display about the same height as the textarea
          // so the form doesn't jump when toggling (rows * line-height +
          // padding + border; border-box sizing, so minHeight covers all).
          style={rows ? { minHeight: `calc(${rows} * 1.625em + 1rem + 2px)` } : undefined}
        >
          {value === "" ? (
            <span className="text-zinc-600">{placeholder}</span>
          ) : (
            value.split("\n").map((line, i) => (
              <div key={i}>{line.length > 0 ? "•".repeat(line.length) : "\u00a0"}</div>
            ))
          )}
        </div>
      )}
      <button
        type="button"
        onClick={() => (show ? setShow(false) : reveal())}
        tabIndex={-1}
        title={show ? t("Hide secret") : t("Show secret")}
        className="absolute right-1.5 top-2 w-7 h-7 rounded-md text-zinc-400 hover:text-zinc-100 hover:bg-white/5 flex items-center justify-center"
      >
        {show ? <EyeOff size={14} /> : <Eye size={14} />}
      </button>
    </div>
  );
};

export default SecretTextarea;
