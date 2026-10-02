// Archive names for the file panels: which files count as archives, and what
// to call a new one. Pure, so the naming can be checked without a UI.

export type ArchiveFormat = "zip" | "tar" | "tar.gz";

export const ARCHIVE_FORMATS: { format: ArchiveFormat; ext: string }[] = [
  { format: "zip", ext: ".zip" },
  { format: "tar.gz", ext: ".tar.gz" },
  { format: "tar", ext: ".tar" },
];

// Longest first, so "a.tar.gz" is not read as a ".gz" of "a.tar".
const KNOWN_EXTS: { ext: string; format: ArchiveFormat }[] = [
  { ext: ".tar.gz", format: "tar.gz" },
  { ext: ".tgz", format: "tar.gz" },
  { ext: ".tar", format: "tar" },
  { ext: ".zip", format: "zip" },
];

const matchExt = (name: string) => {
  const lower = name.toLowerCase();
  // A bare ".zip" is a dotfile, not an archive with an empty name.
  return KNOWN_EXTS.find((k) => lower.endsWith(k.ext) && lower.length > k.ext.length);
};

/** Format of an archive we can extract, by file name; null for anything else. */
export function archiveKind(name: string): ArchiveFormat | null {
  return matchExt(name)?.format ?? null;
}

/** "site.tar.gz" → "site": the folder an archive extracts into. */
export function archiveBaseName(name: string): string {
  const hit = matchExt(name);
  return hit ? name.slice(0, name.length - hit.ext.length) : name;
}

/**
 * Folder for "Extract to folder", or null when the archive's base name is not
 * one plain path component: "...zip" would give "..", and a name a server
 * reported as "../x.zip" would give "../x". Either one is the parent
 * directory, not a folder in this one. A base that starts or ends with
 * whitespace or ends with a dot is refused as well: Windows drops trailing
 * spaces and dots when it creates a folder, so ".. " and "..." would become
 * the parent or the current one. Mirrors `is_plain_folder_name` in archive.rs.
 */
export function extractFolderName(name: string): string | null {
  if (archiveKind(name) === null) return null;
  const base = archiveBaseName(name);
  if (/[\\/\0]/.test(base) || base === "" || base !== base.trim() || base.endsWith(".")) return null;
  return base;
}

/** Adds the format's extension unless the name already ends with it. */
export function archiveFileName(base: string, format: ArchiveFormat): string {
  const ext = ARCHIVE_FORMATS.find((f) => f.format === format)!.ext;
  const name = base.trim();
  return name.toLowerCase().endsWith(ext) ? name : `${name}${ext}`;
}

// "report.txt" → "report". A leading dot is part of the name (".bashrc").
const stripExtension = (name: string) => {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(0, dot) : name;
};

/**
 * Name (without extension) offered for a new archive: the item's own name for
 * a single item, the folder's name for several. A numeric suffix is added
 * until no format's file name is taken, so switching the format in the dialog
 * never lands on an existing file.
 */
export function suggestArchiveBase(
  items: { name: string; isDir: boolean }[],
  dirName: string,
  existing: string[],
  /** Names differing only by case are the same entry (local side). */
  foldCase: boolean,
): string {
  const only = items.length === 1 ? items[0] : null;
  const stem = only
    ? (only.isDir ? only.name : stripExtension(archiveBaseName(only.name)))
    : dirName;
  const base = stem || "archive";
  const key = (n: string) => (foldCase ? n.toLowerCase() : n);
  const have = new Set(existing.map(key));
  const free = (candidate: string) =>
    ARCHIVE_FORMATS.every((f) => !have.has(key(archiveFileName(candidate, f.format))));
  if (free(base)) return base;
  for (let n = 2; ; n++) {
    if (free(`${base}-${n}`)) return `${base}-${n}`;
  }
}
