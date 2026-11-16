import { useState } from "react";
import { Eye, EyeOff } from "lucide-react";
import { useT } from "../i18n";

// Multi-line secret input with a show/hide eye toggle. The secret is masked
// by default (dots) so anyone behind the user doesn't see it; the eye reveals
// it for editing. Mirrors PasswordField, which is single-line only — a
// secret command can span lines, so this is a <textarea> instead.
//
// `className` is forwarded verbatim to the textarea so the surrounding
// styling stays consistent with the neighbouring fields in each form. We
// just add `pr-10` to make room for the eye button.

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
  return (
    <div className="relative">
      <textarea
        value={value}
        onChange={(e: any) => onChange(e.target.value)}
        placeholder={placeholder}
        rows={rows}
        autoFocus={autoFocus}
        onKeyDown={onKeyDown}
        disabled={disabled}
        // Masked: the textarea can't show dots per character the way a
        // password input does, so hide the text entirely until revealed.
        // The field keeps its height so the form doesn't jump.
        className={`pr-10 ${show ? "" : "text-transparent caret-white selection:text-transparent selection:bg-white/20"} ${className || ""}`}
      />
      <button
        type="button"
        onClick={() => setShow((s) => !s)}
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
