import { canMoveInto, pathKey, rulesFor, takenNames } from "./moveRules.ts";

function eq(actual: unknown, expected: unknown, label: string) {
  if (actual !== expected) {
    throw new Error(`${label}: ${String(actual)} !== ${String(expected)}`);
  }
}

const remote = rulesFor("remote", "/");
const win = rulesFor("local", "\\");
const mac = rulesFor("local", "/");

const file = (path: string) => ({ path, isDir: false });
const dir = (path: string) => ({ path, isDir: true });

eq(pathKey("/", remote), "/", "posix root");
eq(pathKey("/var/www/", remote), "/var/www", "trailing slash");
eq(pathKey("C:\\", win), "c:", "drive root");
eq(pathKey("C:/Work/Proj/", win), "c:\\work\\proj", "mixed slashes and case");
eq(pathKey("/Users/Me", mac), "/users/me", "local posix folds case");
eq(pathKey("/srv/a\\b", remote), "/srv/a\\b", "backslash is a name char on posix");

// Already in that folder.
eq(canMoveInto(file("/var/a.txt"), "/var", remote), false, "same folder");
eq(canMoveInto(file("/var/a.txt"), "/var/", remote), false, "same folder, trailing slash");
eq(canMoveInto(file("/a.txt"), "/", remote), false, "same folder at root");
eq(canMoveInto(file("C:\\work\\a.txt"), "C:/Work", win), false, "same folder, other slash and case");
eq(canMoveInto(file("C:\\a.txt"), "C:\\", win), false, "same folder at drive root");

// Plain moves.
eq(canMoveInto(file("/var/a.txt"), "/var/www", remote), true, "file into subfolder");
eq(canMoveInto(file("/var/www/a.txt"), "/var", remote), true, "file into parent");
eq(canMoveInto(file("/var/www/a.txt"), "/", remote), true, "file into root");
eq(canMoveInto(dir("/var/www"), "/srv", remote), true, "folder elsewhere");
eq(canMoveInto(dir("/var/ww"), "/var/www", remote), true, "sibling sharing a name prefix");
eq(canMoveInto(dir("/var/Www"), "/var/www", remote), true, "remote: case differs, other folder");

// A folder into itself or its own subtree.
eq(canMoveInto(dir("/var/www"), "/var/www", remote), false, "folder into itself");
eq(canMoveInto(dir("/var/www"), "/var/www/html/css", remote), false, "folder into its subtree");
eq(canMoveInto(dir("C:\\work\\proj"), "C:/work/proj", win), false, "itself, other slash");
eq(canMoveInto(dir("C:\\work\\proj"), "c:\\WORK\\PROJ\\src", win), false, "subtree, other case");
eq(canMoveInto(dir("/Users/me/Proj"), "/users/me/proj/src", mac), false, "local posix subtree, other case");

// Name collisions in the target.
const taken = (names: string[], existing: string[], rules = remote) =>
  [...takenNames(names, existing, rules)].join(",");
eq(taken(["a.txt", "b.txt"], ["b.txt", "c.txt"]), "b.txt", "exact match");
eq(taken(["notes.txt"], ["Notes.txt"]), "", "remote: case differs");
eq(taken(["notes.txt"], ["Notes.txt"], win), "notes.txt", "windows: case differs");
eq(taken(["notes.txt"], ["Notes.txt"], mac), "notes.txt", "macOS: case differs");
eq(taken(["a.txt"], []), "", "empty target");

console.log("moveRules ok");
