// Translations. English is the source text and the key: t("Delete server")
// looks the text up in the current language's dictionary (src/i18n/<id>.json)
// and falls back to the English itself, so a missing line never shows as a
// code. docs/TRANSLATING.md says how to add a language.
//
//   t("Delete {name}?", { name })          placeholders are {name}
//   tn(n, "{count} file", "{count} files")  one form or the other, by the
//                                           language's plural rules
//   tx("Read the {link} first", { link: <a>…</a> })  a placeholder that is an
//                                           element, for JSX
//   tk("Saved")                             marks a key kept in a table and
//                                           translated later with t()
//   "Open|a verb"                           a context after the last bar when
//                                           one English text needs two
//                                           translations; the fallback drops it
//
// Components call `const t = useT()` so they render again when the language
// changes; plain code (handlers, util) calls t() directly.
import { Fragment, createElement, useSyncExternalStore } from "react";
import type { ReactNode } from "react";
import zhCN from "./zh-CN.json";

export type LanguageId = "en" | "zh-CN";
export type LanguageSetting = "system" | LanguageId;

export const LANGUAGE_KEY = "submarine-language";

// In the language's own name, so a user who can't read the current one still
// finds theirs.
export const LANGUAGES: { id: LanguageId; name: string }[] = [
  { id: "en", name: "English" },
  { id: "zh-CN", name: "中文（简体）" },
];

type Dictionary = Record<string, string>;
const DICTS: Partial<Record<LanguageId, Dictionary>> = { "zh-CN": zhCN as Dictionary };

const isLanguageId = (v: unknown): v is LanguageId => LANGUAGES.some((l) => l.id === v);

// The system's language as one of ours: Simplified Chinese (zh, zh-CN,
// zh-Hans, zh-SG) → zh-CN, anything else → English. Traditional Chinese
// (zh-TW, zh-HK, zh-MO, zh-Hant) waits for its own translation. The first
// tag we can serve wins, in the order the system lists them.
export function systemLanguage(): LanguageId {
  if (typeof navigator === "undefined") return "en";
  const tags = navigator.languages?.length ? navigator.languages : [navigator.language];
  for (const tag of tags) {
    const lower = (tag || "").toLowerCase();
    if (lower === "en" || lower.startsWith("en-")) return "en";
    if (lower === "zh" || lower.startsWith("zh-")) {
      if (/hant|-tw|-hk|-mo/.test(lower)) continue;
      return "zh-CN";
    }
  }
  return "en";
}

export function readLanguageSetting(): LanguageSetting {
  try {
    const v = localStorage.getItem(LANGUAGE_KEY);
    if (v === "system" || isLanguageId(v)) return v;
  } catch { /* storage unavailable: follow the system */ }
  return "system";
}

const resolve = (s: LanguageSetting): LanguageId => (s === "system" ? systemLanguage() : s);

let setting: LanguageSetting = readLanguageSetting();
let current: LanguageId = resolve(setting);
let version = 0;
const listeners = new Set<() => void>();

const applyLang = () => {
  if (typeof document !== "undefined") document.documentElement.lang = current;
};
applyLang();

function changed() {
  current = resolve(setting);
  version++;
  applyLang();
  listeners.forEach((l) => l());
}

export function languageSetting(): LanguageSetting { return setting; }
export function currentLanguage(): LanguageId { return current; }

export function setLanguageSetting(next: LanguageSetting) {
  if (next === setting) return;
  setting = next;
  try { localStorage.setItem(LANGUAGE_KEY, next); } catch { /* per-device only */ }
  changed();
}

// The system language can change while the app runs (Android, a Mac).
if (typeof window !== "undefined") {
  window.addEventListener("languagechange", () => { if (setting === "system") changed(); });
}

type Params = Record<string, string | number>;

const PLACEHOLDER = /\{(\w+)\}/g;

const fill = (text: string, params?: Params) =>
  params ? text.replace(PLACEHOLDER, (m, name) => (name in params ? String(params[name]) : m)) : text;

// "Open|a verb" → "Open". Only a lowercase context after the last bar counts,
// so an English text with a bar in it stays whole.
const CONTEXT = /\|[a-z][a-z ]*$/;
const stripContext = (key: string) => key.replace(CONTEXT, "");

export function t(key: string, params?: Params): string {
  const line = DICTS[current]?.[key];
  return fill(line === undefined || line === "" ? stripContext(key) : line, params);
}

// Marks a string that is a key but is translated later (a label kept in a
// table, rendered with t(LABELS[kind])). Returns it unchanged.
export const tk = (key: string): string => key;

const rules = new Map<LanguageId, Intl.PluralRules>();
function pluralForm(n: number): Intl.LDMLPluralRule {
  let r = rules.get(current);
  if (!r) {
    try { r = new Intl.PluralRules(current); } catch { r = new Intl.PluralRules("en"); }
    rules.set(current, r);
  }
  return r.select(n);
}

// One of two English forms by the current language's rules; {count} is filled
// in. Chinese has one form: both keys are translated alike.
export function tn(count: number, one: string, other: string, params?: Params): string {
  return t(pluralForm(count) === "one" ? one : other, { count, ...params });
}

// t() for JSX: a placeholder whose part is an element is put in as that
// element, so a link or a bold word can sit inside a translated sentence.
export function tx(key: string, parts: Record<string, ReactNode>): ReactNode {
  const text = t(key);
  const nodes: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(PLACEHOLDER)) {
    const name = m[1];
    if (!(name in parts)) continue;
    if (m.index! > last) nodes.push(text.slice(last, m.index));
    const part = parts[name];
    nodes.push(typeof part === "string" || typeof part === "number" ? String(part) : part);
    last = m.index! + m[0].length;
  }
  if (last < text.length) nodes.push(text.slice(last));
  return createElement(Fragment, null, ...nodes);
}

const subscribe = (l: () => void) => { listeners.add(l); return () => { listeners.delete(l); }; };
const snapshot = () => version;

// Subscribes the component to language changes and hands back t.
export function useT(): typeof t {
  useSyncExternalStore(subscribe, snapshot, snapshot);
  return t;
}

export function useLanguage(): LanguageId {
  useSyncExternalStore(subscribe, snapshot, snapshot);
  return current;
}
