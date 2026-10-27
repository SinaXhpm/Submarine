// Builds the product websites into website/dist/<site>/: one static page per
// language and page, from the strings in sites/<site>/i18n/<lang>.json
// (English is the source; a missing string falls back to it, with a warning).
// No dependencies beyond Node 20+ and git.
//
//   node website/build.mjs [--site submarine|echoes|all] [--out website/dist]
//                          [--commit <sha>] [--date 2026-10-27]
//
// --commit is the Submarine main-branch commit its security page's code links
// point to (default: origin/main). It must exist on GitHub.
import path from "node:path";
import { HERE } from "./engine.mjs";

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : fallback;
};
const SITES = ["submarine", "echoes"];
const which = arg("site", "all");
const out = path.resolve(arg("out", path.join(HERE, "dist")));
const date = arg("date", new Date().toISOString().slice(0, 10));
const commit = arg("commit", "");

let ok = true;
for (const id of which === "all" ? SITES : [which]) {
  if (!SITES.includes(id)) throw new Error(`unknown site ${id}`);
  const { default: build } = await import(`./sites/${id}/site.mjs`);
  ok = build({ out: path.join(out, id), commit, date }) && ok;
}
process.exitCode = ok ? 0 : 1;
