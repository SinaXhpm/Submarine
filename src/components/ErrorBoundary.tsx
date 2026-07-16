import { Component, type CSSProperties, type ErrorInfo, type ReactNode } from "react";

interface Props { children: ReactNode; }
interface State { error: Error | null; info: ErrorInfo | null; }

// Top-level error boundary. A render-time throw anywhere in the tree would
// otherwise unmount the whole app and leave a blank white window with no way
// out. Here we catch it, show a recoverable screen with the details, and offer
// a reload — so a bug in one panel never bricks the entire session UI.
export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, info: null };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    this.setState({ info });
    // Surface to the console (and any global handler) with the component stack.
    console.error("[ui] uncaught render error:", error, info?.componentStack);
  }

  private reset = () => this.setState({ error: null, info: null });
  private reload = () => window.location.reload();

  render() {
    const { error, info } = this.state;
    if (!error) return this.props.children;

    const detail = [
      error.stack || String(error),
      info?.componentStack ? `\nComponent stack:${info.componentStack}` : "",
    ].join("");

    return (
      <div style={S.wrap} role="alert">
        <div style={S.card}>
          <div style={S.badge}>Something broke in the interface</div>
          <h1 style={S.h1}>The window hit an unexpected error</h1>
          <p style={S.lede}>
            Your connections and saved profiles are untouched — this is only the on-screen
            interface. Reloading almost always clears it. If it keeps happening, copy the
            details below when reporting the bug.
          </p>
          <div style={S.row}>
            <button style={{ ...S.btn, ...S.btnPrimary }} onClick={this.reload}>Reload the app</button>
            <button style={S.btn} onClick={this.reset}>Try to continue</button>
            <button
              style={S.btn}
              onClick={() => { navigator.clipboard?.writeText(`${error.message}\n\n${detail}`).catch(() => {}); }}
            >
              Copy details
            </button>
          </div>
          <pre style={S.pre}>{error.message}{"\n\n"}{detail}</pre>
        </div>
      </div>
    );
  }
}

const S: Record<string, CSSProperties> = {
  wrap: {
    position: "fixed", inset: 0, display: "grid", placeItems: "center",
    background: "#0a0a0c", color: "#e7eef6", padding: 24,
    fontFamily: "system-ui, -apple-system, 'Segoe UI', sans-serif", overflow: "auto",
  },
  card: { maxWidth: 640, width: "100%" },
  badge: {
    fontFamily: "ui-monospace, 'Cascadia Code', monospace", fontSize: 11, letterSpacing: ".12em",
    textTransform: "uppercase", color: "#ff8a8a", marginBottom: 14,
  },
  h1: { fontSize: 24, margin: "0 0 12px", letterSpacing: "-.01em", fontWeight: 700 },
  lede: { color: "#9fb2c7", fontSize: 14.5, lineHeight: 1.6, margin: "0 0 20px", maxWidth: "58ch" },
  row: { display: "flex", flexWrap: "wrap", gap: 10, marginBottom: 20 },
  btn: {
    fontSize: 13, fontWeight: 600, padding: "9px 16px", borderRadius: 10, cursor: "pointer",
    background: "rgba(255,255,255,.05)", color: "#e7eef6", border: "1px solid rgba(255,255,255,.12)",
  },
  btnPrimary: { background: "#3b82f6", borderColor: "#3b82f6", color: "#fff" },
  pre: {
    fontFamily: "ui-monospace, 'Cascadia Code', monospace", fontSize: 11.5, lineHeight: 1.5,
    color: "#7f97ad", background: "rgba(255,255,255,.03)", border: "1px solid rgba(255,255,255,.08)",
    borderRadius: 10, padding: 14, overflow: "auto", maxHeight: 260, whiteSpace: "pre-wrap", margin: 0,
  },
};
