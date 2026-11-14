/** Where the privacy policy is published: the website's page. The same text is
 *  PRIVACY.md in the repository, whose address keeps working. */
export const PRIVACY_URL = "https://submarine.sinaxhpm.com/privacy/";

/** A new GitHub issue, for when the feedback inbox is full for the day. */
export const REPO_ISSUES_URL = "https://github.com/SinaXhpm/Submarine/issues/new";

/** A new GitHub issue to fix a translation, its title naming the language
 *  ("Translation (de): "), with a short form to fill in. */
export function translationIssueUrl(language: string): string {
  const title = `Translation (${language}): `;
  const body = "Where in the app:\n\nIt says now:\n\nBetter:\n\nWhy (optional):\n";
  return `https://github.com/SinaXhpm/Submarine/issues/new?title=${encodeURIComponent(title)}&body=${encodeURIComponent(body)}`;
}
