import { pathCrumbs } from "./pathCrumbs.ts";

function eq(actual: unknown, expected: unknown, label: string) {
  if (actual !== expected) {
    throw new Error(`${label}: ${String(actual)} !== ${String(expected)}`);
  }
}

// "label=path" per crumb, "!" marks one that cannot be opened.
const show = (path: string, sep: "/" | "\\") =>
  pathCrumbs(path, sep).map((c) => `${c.label}=${c.path}${c.navigable ? "" : "!"}`).join(" | ");

eq(show("", "/"), "", "empty");
eq(show("/", "/"), "/=/", "posix root");
eq(show("/home/user", "/"), "/=/ | home=/home | user=/home/user", "posix");
eq(show("/home/user/", "/"), "/=/ | home=/home | user=/home/user", "trailing slash");
eq(show("C:\\", "\\"), "C:=C:\\", "drive root");
eq(show("C:", "\\"), "C:=C:\\", "bare drive");
eq(show("C:\\Users\\me", "\\"), "C:=C:\\ | Users=C:\\Users | me=C:\\Users\\me", "drive path");
eq(show("C:/Users", "\\"), "C:=C:/ | Users=C:/Users", "forward slashes on windows");
eq(
  show("\\\\?\\C:\\Users\\me", "\\"),
  "C:=\\\\?\\C:\\ | Users=\\\\?\\C:\\Users | me=\\\\?\\C:\\Users\\me",
  "verbatim drive path",
);
eq(
  show("\\\\srv\\share\\dir", "\\"),
  "srv=\\\\srv! | share=\\\\srv\\share | dir=\\\\srv\\share\\dir",
  "unc",
);
eq(
  show("\\\\?\\UNC\\srv\\share\\dir", "\\"),
  "srv=\\\\?\\UNC\\srv! | share=\\\\?\\UNC\\srv\\share | dir=\\\\?\\UNC\\srv\\share\\dir",
  "verbatim unc",
);

console.log("pathCrumbs ok");
