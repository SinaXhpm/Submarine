// Whole sentences one after another. English puts a space between them,
// Chinese none: the gap is whatever the language's translation of
// "{first} {second}" puts between the two.
import { t } from "../i18n";

export const sentenceGap = (): string => t("{first} {second}", { first: "", second: "" });

export function joinSentences(...parts: (string | false | null | undefined)[]): string {
  return parts.filter((p): p is string => !!p).join(sentenceGap());
}
