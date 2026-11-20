import { createElement, type ComponentType } from "react";

// Loads the unlocked app's chunk (workspace.ts) once, and stands in for its
// components until then. A stand-in renders the real component directly once
// the chunk is in, so nothing waits after the first load; before that it
// suspends (the nearest <Suspense> shows its fallback) and starts the load.
// React.lazy would suspend once more even with the chunk loaded, and cost
// the first render after unlock a second pass.
type Workspace = typeof import("./workspace");

let loaded: Workspace | null = null;
let loading: Promise<Workspace> | null = null;
// A load that failed (the chunk is in the app's own files, so this is for a
// broken install). Every stand-in rendered right after it throws it, so each
// screen's error boundary shows its "crashed" panel at once; one rendered
// later, as when a panel's Reset is pressed, starts a new load instead. Had
// each stand-in started its own load, they would take turns suspending the
// whole app, and no panel would ever show.
let failure: { error: unknown; at: number } | null = null;
const FAILURE_SHOWN_MS = 1000;

/** Starts loading the chunk (once at a time); resolves with it. */
export function loadWorkspace(): Promise<Workspace> {
  loading ??= import("./workspace").then(
    (m) => {
      loaded = m;
      failure = null;
      return m;
    },
    (e) => {
      failure = { error: e, at: Date.now() };
      loading = null;
      throw e;
    },
  );
  return loading;
}

/** The chunk, if it is in already. */
export const workspaceNow = (): Workspace | null => loaded;

/** A stand-in for one of the chunk's components, used like the component. */
export function fromWorkspace<K extends keyof Workspace>(key: K): Workspace[K] {
  const Part = (props: object) => {
    if (loaded) return createElement(loaded[key] as ComponentType<object>, props);
    if (failure && Date.now() - failure.at < FAILURE_SHOWN_MS) throw failure.error;
    failure = null;
    throw loadWorkspace();
  };
  Part.displayName = `Workspace.${String(key)}`;
  return Part as unknown as Workspace[K];
}
