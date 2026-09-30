import { archiveBaseName, archiveFileName, archiveKind, extractFolderName, suggestArchiveBase } from "./archive.ts";

function eq(actual: unknown, expected: unknown, label: string) {
  if (actual !== expected) {
    throw new Error(`${label}: ${String(actual)} !== ${String(expected)}`);
  }
}

// `foldCase`: exact names over SFTP, case-insensitive on the local side.
const remote = false;
const win = true;

const file = (name: string) => ({ name, isDir: false });
const dir = (name: string) => ({ name, isDir: true });

// Recognition.
eq(archiveKind("site.zip"), "zip", "zip");
eq(archiveKind("SITE.ZIP"), "zip", "zip, upper case");
eq(archiveKind("site.tar"), "tar", "tar");
eq(archiveKind("site.tar.gz"), "tar.gz", "tar.gz");
eq(archiveKind("site.tgz"), "tar.gz", "tgz");
eq(archiveKind("site.gz"), null, "plain gzip is not an archive");
eq(archiveKind("site.rar"), null, "rar");
eq(archiveKind(".zip"), null, "dotfile named .zip");
eq(archiveKind("tar"), null, "no extension");

eq(archiveBaseName("site.tar.gz"), "site", "base of tar.gz");
eq(archiveBaseName("site.v2.zip"), "site.v2", "base keeps inner dots");
eq(archiveBaseName("notes.txt"), "notes.txt", "base of a non-archive");

// Folder for "Extract to folder": never the directory itself or its parent.
eq(extractFolderName("site.tar.gz"), "site", "folder of tar.gz");
eq(archiveBaseName("...zip"), "..", "base of ...zip is the parent");
eq(extractFolderName("...zip"), null, "no folder for ...zip");
eq(extractFolderName("..tar"), null, "no folder for ..tar");
eq(extractFolderName("...tar.gz"), null, "no folder for ...tar.gz");
eq(extractFolderName(" .zip"), null, "blank base");
eq(extractFolderName("....zip"), null, "three dots: the current folder once Windows drops them");
eq(extractFolderName(".. .zip"), null, "dot dot space: the parent after a trim");
eq(extractFolderName(". .zip"), null, "dot space");
eq(extractFolderName(" x.zip"), null, "leading space");
eq(extractFolderName("x .zip"), null, "trailing space");
eq(extractFolderName("v1..zip"), null, "trailing dot");
eq(extractFolderName(".hidden.zip"), ".hidden", "leading dot is fine");
eq(extractFolderName("site.v2.tar"), "site.v2", "inner dot is fine");
eq(extractFolderName("notes.txt"), null, "not an archive");
eq(extractFolderName("../x.zip"), null, "name with ../");
eq(extractFolderName("foo/../../tmp/x.zip"), null, "name with slashes");
eq(extractFolderName("/etc/x.tar.gz"), null, "absolute name");
eq(extractFolderName("a\\b.zip"), null, "name with a backslash");

// File name from the dialog.
eq(archiveFileName("site", "zip"), "site.zip", "adds extension");
eq(archiveFileName(" site ", "tar.gz"), "site.tar.gz", "trims");
eq(archiveFileName("site.zip", "zip"), "site.zip", "extension already typed");
eq(archiveFileName("site.ZIP", "zip"), "site.ZIP", "typed extension, other case");
eq(archiveFileName("site.zip", "tar"), "site.zip.tar", "other format's extension is part of the name");

// Suggested name.
eq(suggestArchiveBase([file("report.txt")], "docs", [], remote), "report", "single file drops its extension");
eq(suggestArchiveBase([file(".bashrc")], "home", [], remote), ".bashrc", "dotfile keeps its name");
eq(suggestArchiveBase([dir("www.old")], "var", [], remote), "www.old", "single folder keeps its dots");
eq(suggestArchiveBase([file("data.tar.gz")], "srv", ["data.tar.gz"], remote), "data-2", "re-archiving an archive");
eq(suggestArchiveBase([file("a"), file("b")], "project", [], remote), "project", "several items take the folder name");
eq(suggestArchiveBase([file("a"), file("b")], "", [], remote), "archive", "several items at the root");
eq(suggestArchiveBase([dir("www")], "var", ["www.zip"], remote), "www-2", "taken in one format");
eq(suggestArchiveBase([dir("www")], "var", ["www.tar", "www-2.tar.gz"], remote), "www-3", "taken twice");
eq(suggestArchiveBase([dir("www")], "var", ["WWW.zip"], remote), "www", "remote: case differs");
eq(suggestArchiveBase([dir("www")], "var", ["WWW.zip"], win), "www-2", "windows: case differs");

console.log("archive ok");
