import { invoke } from "@tauri-apps/api/core";
import { FileEntry, ListResult, LocalFileProvider } from "./types";

// Local filesystem provider. All paths are OS-native (backslashes on Windows,
// forward slashes elsewhere). Backed by the small set of `local_*` Tauri
// commands declared in main.rs.

type RawLocalEntry = {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  modified?: number;
};

const isWindowsLike = (p: string) => /^[a-zA-Z]:[\\/]/.test(p) || p.includes("\\");

// Parent of a local path. POSIX "/var" has its only slash at index 0; that
// is the root, so the parent is "/", not "/var" again. "/" trims to "" and
// takes the same branch. A path with no separator ("foo") stays put.
export function parentPathOf(path: string, sep: "/" | "\\"): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const idx = trimmed.lastIndexOf(sep);
  if (sep === "/" && (idx === 0 || trimmed === "")) return "/";
  if (idx <= 0) return trimmed;
  // Preserve drive root on Windows ("C:\").
  if (sep === "\\" && idx === 2 && /^[a-zA-Z]:$/.test(trimmed.slice(0, 2))) {
    return trimmed.slice(0, 3);
  }
  return trimmed.slice(0, idx);
}

// The backend lists the canonicalized directory, so on Windows entry paths
// come back with the verbatim prefix (`\\?\C:\Users`, `\\?\UNC\srv\share`).
// Drop it so the path bar and every later call use the ordinary form.
export function stripVerbatimPrefix(path: string): string {
  const unc = path.match(/^\\\\\?\\UNC\\(.*)$/i);
  if (unc) return `\\\\${unc[1]}`;
  return path.replace(/^\\\\\?\\(?=[a-zA-Z]:)/, "");
}

export function createLocalProvider(): LocalFileProvider {
  // Sep is computed lazily from the first real path we see (after `homePath`
  // resolves). Defaulting to `\` on Windows is fine because the renderer is
  // bundled per-OS anyway, but probing keeps it honest if someone runs the
  // binary under WSL etc.
  let cachedSep: "/" | "\\" | null = null;

  const inferSep = (sample: string) => {
    if (cachedSep) return cachedSep;
    cachedSep = isWindowsLike(sample) ? "\\" : "/";
    return cachedSep;
  };

  return {
    id: "local",
    label: "Local",
    get pathSep() {
      return cachedSep ?? "\\";
    },

    async homePath() {
      // On Android there is no Desktop and, under scoped storage, most
      // shared paths (Downloads, DCIM, ...) aren't guaranteed to be
      // writable from a direct filesystem call. `android_default_local_dir`
      // returns the first entry from `android_quick_dirs` that passed a
      // touch-probe, falling back to the app-scoped internal storage. If
      // even that fails, we let it error and the panel just stays empty.
      if (/Android/i.test(navigator.userAgent)) {
        const dir = await invoke<string>("android_default_local_dir");
        if (dir) {
          inferSep(dir);
          return dir;
        }
      }
      // Desktop: default to Desktop — that's where most users keep
      // transient work and is the natural landing zone for drag-out from
      // the remote pane. The Rust side falls back to the home dir if
      // Desktop is unset.
      const dir = await invoke<string>("local_desktop_dir");
      inferSep(dir);
      return dir;
    },

    async list(path: string): Promise<ListResult> {
      const raw = await invoke<RawLocalEntry[]>("local_list_dir", { path });
      const entries: FileEntry[] = raw.map((r) => ({
        name: r.name,
        path: stripVerbatimPrefix(r.path),
        isDir: r.is_dir,
        size: r.size,
        modified: r.modified,
      }));
      if (entries.length > 0) inferSep(entries[0].path);
      return { currentPath: stripVerbatimPrefix(path), entries };
    },

    joinPath(dir: string, name: string) {
      const sep = inferSep(dir);
      return dir.endsWith(sep) ? `${dir}${name}` : `${dir}${sep}${name}`;
    },

    parentPath(path: string) {
      const sep = inferSep(path);
      return parentPathOf(path, sep);
    },

    async mkdir(path: string) {
      await invoke("local_create_dir", { path });
    },

    async remove(path: string, isDir: boolean) {
      await invoke("local_remove", { path, isDir });
    },

    async rename(from: string, to: string) {
      await invoke("local_rename", { from, to });
    },

    async archive(dir, names, dest, format, overwrite) {
      await invoke("local_archive", { dir, names, dest, format, overwrite });
    },

    async extract(dir: string, name: string, folder: string | null) {
      await invoke("local_extract", { dir, name, folder });
    },
  };
}
