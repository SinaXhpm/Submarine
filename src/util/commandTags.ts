import { t } from "../i18n";

// A Library command's tags, as typed in its form: a comma-separated list.
// The backend keeps them in one form (normalize_command_tags in lib.rs):
// trimmed, empty ones dropped, a repeat dropped (A-Z compared without case),
// at most 20 tags of at most 32 characters each. This mirrors it, so that a
// form says what is wrong in the user's language before it saves.

const MAX_TAGS = 20;
const MAX_LENGTH = 32;

/** One tag as the backend tells tags apart: only A-Z without case, as
 *  Rust's eq_ignore_ascii_case does. */
export const tagKey = (tag: string) => tag.replace(/[A-Z]/g, (c) => c.toLowerCase());

/** The tags in a comma-separated list: trimmed, without empty ones or repeats. */
export function splitTags(text: string | null | undefined): string[] {
  const seen = new Set<string>();
  const tags: string[] = [];
  for (const part of (text || "").split(",")) {
    const tag = part.trim();
    if (!tag || seen.has(tagKey(tag))) continue;
    seen.add(tagKey(tag));
    tags.push(tag);
  }
  return tags;
}

/** What the backend would refuse in these tags, in words for the user;
 *  null when they are fine. */
export function tagsProblem(text: string | null | undefined): string | null {
  const tags = splitTags(text);
  // Characters as Rust counts them (code points), not UTF-16 units.
  if (tags.some((tag) => Array.from(tag).length > MAX_LENGTH)) return t("A tag can be at most 32 characters long.");
  if (tags.length > MAX_TAGS) return t("A command can have at most 20 tags.");
  return null;
}
