import { useState } from "react";
import { Eye, EyeOff } from "lucide-react";
import { useT } from "../i18n";

// Password input with a built-in show/hide eye toggle. Used wherever the
// user might need to *inspect* a stored secret (vault credential password,
// SSH key passphrase, node inline password) — masking is still the default
// so anyone behind them doesn't see the value just from focusing the field.
//
// `className` is forwarded verbatim to the input so the surrounding
// styling (height, bg, border) stays consistent with the neighbouring
// fields in each form. We just add `pr-10` to make room for the eye button,
// and `no-native-reveal` so Windows doesn't add an eye of its own.

interface Props {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  className?: string;
  autoFocus?: boolean;
  onKeyDown?: (e: any) => void;
  disabled?: boolean;
}

const PasswordField = ({
  value, onChange, placeholder, className, autoFocus, onKeyDown, disabled,
}: Props) => {
  const t = useT();
  const [show, setShow] = useState(false);
  return (
    <div className="relative">
      <input
        type={show ? "text" : "password"}
        value={value}
        onChange={(e: any) => onChange(e.target.value)}
        placeholder={placeholder}
        autoFocus={autoFocus}
        onKeyDown={onKeyDown}
        disabled={disabled}
        className={`pr-10 no-native-reveal ${className || ""}`}
      />
      <button
        type="button"
        onClick={() => setShow((s) => !s)}
        tabIndex={-1}
        title={show ? t("Hide password") : t("Show password")}
        className="absolute right-1.5 top-1/2 -translate-y-1/2 w-7 h-7 rounded-md text-zinc-400 hover:text-zinc-100 hover:bg-white/5 flex items-center justify-center"
      >
        {show ? <EyeOff size={14} /> : <Eye size={14} />}
      </button>
    </div>
  );
};

export default PasswordField;
