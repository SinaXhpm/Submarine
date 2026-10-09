// Local paths as text: where a path's root ends, the folder above it, and a
// name inside it. Nothing here touches the disk.
//
// A path that starts with "/" is a POSIX one: only "/" separates there, and
// "\" is an ordinary character in a name. Any other path is read as a Windows
// one, where both separate (a Windows path may be typed with either).

// The root of a path is the part "Up" never goes above:
//   "/"                         POSIX
//   "C:\"                       a drive
//   "\\server\share\"           a share
//   "\\?\C:\"                   the same two in Windows' long form, which the
//   "\\?\UNC\server\share\"     backend keeps for the few paths that need it
//   "\\?\Volume{…}\"            any other long-form or device (`\\.\`) name
// `root` ends with the separator, or is "" for a path that has none (a
// relative one). `below` is the rest, without separators at either end.
const splitRoot = (path: string): { root: string; below: string; posix: boolean } => {
  if (path.startsWith("/")) {
    return { root: "/", below: path.replace(/^\/+|\/+$/g, ""), posix: true };
  }
  const m =
    /^\\\\\?\\UNC\\[^\\]+\\[^\\]+/i.exec(path) ??
    /^\\\\\?\\[a-zA-Z]:/.exec(path) ??
    /^\\\\[?.]\\[^\\]+/.exec(path) ??
    /^\\\\[^\\?.][^\\]*\\[^\\]+/.exec(path) ??
    /^[a-zA-Z]:/.exec(path);
  const root = m ? m[0] + "\\" : "";
  const below = path.slice(m ? m[0].length : 0).replace(/^[\\/]+|[\\/]+$/g, "");
  return { root, below, posix: false };
};

export const localRoot = (path: string): string => splitRoot(path).root;

// The folder above `path`. A root is its own parent, so "Up" stops there.
export const localParent = (path: string): string => {
  const { root, below, posix } = splitRoot(path);
  const cut = posix ? below.lastIndexOf("/") : Math.max(below.lastIndexOf("\\"), below.lastIndexOf("/"));
  if (cut < 0) return root || path;
  return root + below.slice(0, cut);
};

// `path` as the name of a folder: no separator at the end, except the one a
// root ends with ("C:\" stays, and "C:" becomes it: without the separator
// Windows reads a drive letter as "the current folder on that drive").
export const localDir = (path: string): string => {
  const { root, below, posix } = splitRoot(path);
  if (!below) return root || path;
  return path.replace(posix ? /\/+$/ : /[\\/]+$/, "");
};

// `name` inside the folder `dir`.
export const localJoin = (dir: string, name: string): string => {
  const folder = localDir(dir);
  const sep = splitRoot(folder).posix ? "/" : "\\";
  return folder.endsWith(sep) ? folder + name : folder + sep + name;
};
