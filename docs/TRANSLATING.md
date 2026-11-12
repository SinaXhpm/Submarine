# Translating Submarine

The app's text lives in the code in English. A translation is one file,
`src/i18n/<language>.json`, that maps each English text to its translation.
English itself needs no file: a text with no line in the chosen language shows
in English.

Messages that come from a server, from the SSH connection or from the
operating system are not translated; they are shown as they arrive.

## Fixing a word

The translations were made with AI, so a native speaker will find words to
make better. Open an issue that says where the text is, what it says now and
what it should say: Settings → Language → Suggest a fix opens one with the
language in its title. Or change the line in `src/i18n/<language>.json` and
send a pull request.

## Adding a language

1. Run `npm run i18n`. It writes `src/i18n/en.json`, the list of every text,
   English to English. Copy it to `src/i18n/<tag>.json`, with the language's
   BCP 47 tag (`zh-CN`, `de`, `pt-BR`), and translate the values. Leave the
   keys as they are. A value you can't translate yet can stay empty: it shows
   in English.
2. Add the language to `LanguageId`, `LANGUAGES` and `LOADERS` in
   `src/i18n/index.ts`, and to `BY_BASE` there so a system set to it picks it
   by itself. The name in `LANGUAGES` is in the language itself (`Deutsch`, not
   `German`). Each language's file is loaded only when it is used.
3. Run `npm run i18n` again. It reports what is missing, lines the code no
   longer uses, and placeholders that differ. `npm run i18n -- --strict` also
   fails while a text is missing. Then `npm run build`.

## The lines

- A placeholder is `{name}` and is filled at run time: `"Delete {name}?"`. Keep
  every placeholder, in whichever order the language needs.
- Some keys end in a context after a bar, `"Open|a verb"`, where one English
  text needs two translations. Translate the text before the bar; the context
  only tells you which meaning it is.
- Counts come in two English forms, `"{count} file"` and `"{count} files"`.
  The first is used for exactly one. A language with one form (Chinese,
  Japanese, Korean) translates both the same way. A language with more forms
  translates the second with its form for 5 and adds a line for each other
  form, the English key followed by `|few`, `|many`, `|one` or `|two`:

  ```json
  "{count} files": "{count} файлов",
  "{count} files|few": "{count} файла",
  "{count} files|one": "{count} файл"
  ```

  Which forms a language has comes from `Intl.PluralRules`; `npm run i18n`
  lists every `few` and `two` line that is missing. A `|one` line is for the
  counts other than 1 that take the singular (21 in Russian), when the English
  singular has no `{count}`.
- Keep product and protocol names (Submarine, SSH, SFTP, RDP, VNC, Docker,
  ssh-agent), file paths such as `~/.ssh/config`, key names (`Ctrl+Shift+C`)
  and command names as they are.
- `\n` is a line break inside a longer text.

## For code

- Every text a person sees goes through `t("…")` from `src/i18n`: labels,
  buttons, titles, placeholders, `aria-label`s, toasts, dialog texts, error
  texts the app itself writes.
- In a component, take `const t = useT();` at the top; it renders the
  component again when the language changes. Outside components (event
  handlers, `util/`), import `t` and call it directly.
- Keep the English text as the key, whole sentences rather than fragments
  glued together: `t("{count} files selected")`, not `t("files") + " selected"`.
  Variables go in as placeholders: `t("Delete {name}?", { name })`. A
  sentence that holds an element uses `tx`:
  `tx("Read the {link} first", { link: <a …>{t("privacy policy")}</a> })`.
- One of two forms by count: `tn(n, "{count} file", "{count} files")`.
- A text kept in a table is marked with `tk("…")` so the list finds it, and
  translated where it is shown: `t(LABELS[kind])`.
- Not translated: values that are stored, compared or sent to the backend,
  element ids and `data-*` values, CSS classes, log lines for the console,
  command text for a server, URLs, and names of things (fonts, colour schemes
  by their own names, languages).
- Run `npm run i18n` after changing texts, and commit `src/i18n/en.json`
  with them. A new text shows in English in the other languages until it is
  translated; the script lists those. It fails on a line the code no longer
  uses (remove it from the language files) and on placeholders that differ.
