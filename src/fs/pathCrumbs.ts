// Splits a directory path into the segments the FilePanel path bar shows.
// Each crumb carries the full path up to and including its segment. Accepts
// "/" and "\" so Windows local paths work next to POSIX and SFTP ones.

export interface PathCrumb {
  label: string;
  path: string;
  /**
   * False for the server of a UNC path (`\\srv`): it is not a directory and
   * cannot be listed, so the path bar shows it as plain text.
   */
  navigable: boolean;
}

export function pathCrumbs(path: string, sep: "/" | "\\"): PathCrumb[] {
  const out: PathCrumb[] = [];
  // Windows verbatim (`\\?\`, `\\?\UNC\`) or device (`\\.\`) prefix, as
  // std::fs::canonicalize returns it. Kept in every crumb path, never shown.
  const verbatim = path.match(/^\\\\[?.]\\(UNC\\)?/i)?.[0] ?? "";
  const rest = path.slice(verbatim.length);
  const unc = verbatim ? /unc\\$/i.test(verbatim) : /^[\\/]{2}/.test(rest);
  const lead = rest.match(/^[\\/]+/)?.[0] ?? "";
  let acc = verbatim + lead;
  if (!verbatim && lead.length === 1) out.push({ label: lead, path: lead, navigable: true });
  let n = 0;
  for (const m of rest.slice(lead.length).matchAll(/([^\\/]+)([\\/]*)/g)) {
    acc += m[1];
    const after = m[2] ? m[2][0] : "";
    // A drive crumb targets the drive root ("C:\"): bare "C:" is the drive's
    // current directory.
    const drive = n === 0 && !unc && !lead && /^[a-zA-Z]:$/.test(m[1]);
    out.push({ label: m[1], path: drive ? acc + (after || sep) : acc, navigable: !(unc && n === 0) });
    acc += after;
    n++;
  }
  return out;
}
