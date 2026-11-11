// Translations. English is the source text and the key: t("Delete server")
// looks the text up in the current language's dictionary (src/i18n/<id>.json)
// and falls back to the English itself, so a missing line never shows as a
// code. docs/TRANSLATING.md says how to add a language.
//
//   t("Delete {name}?", { name })          placeholders are {name}
//   tn(n, "{count} file", "{count} files")  one form or the other by count; a
//                                           language with more forms adds
//                                           them as "{count} files|few" lines
//   tx("Read the {link} first", { link: <a>…</a> })  a placeholder that is an
//                                           element, for JSX
//   tk("Saved")                             marks a key kept in a table and
//                                           translated later with t()
//   "Open|a verb"                           a context after the last bar when
//                                           one English text needs two
//                                           translations; the fallback drops it
//
// Components call `const t = useT()` so they render again when the language
// changes; plain code (handlers, util) calls t() directly. A language's file
// is fetched the first time it is needed; main.tsx waits for the device's
// language before the first screen, so it never shows English first.
import { Fragment, createElement, useSyncExternalStore } from "react";
import type { ReactNode } from "react";

export type LanguageId =
  | "en" | "de" | "es" | "fr" | "it" | "pl" | "pt-BR" | "tr"
  | "ru" | "uk" | "hi" | "ja" | "ko" | "zh-CN" | "zh-TW";
export type LanguageSetting = "system" | LanguageId;

export const LANGUAGE_KEY = "submarine-language";

// In the language's own name, so a user who can't read the current one still
// finds theirs.
export const LANGUAGES: { id: LanguageId; name: string }[] = [
  { id: "en", name: "English" },
  { id: "de", name: "Deutsch" },
  { id: "es", name: "Español" },
  { id: "fr", name: "Français" },
  { id: "it", name: "Italiano" },
  { id: "pl", name: "Polski" },
  { id: "pt-BR", name: "Português (Brasil)" },
  { id: "tr", name: "Türkçe" },
  { id: "ru", name: "Русский" },
  { id: "uk", name: "Українська" },
  { id: "hi", name: "हिन्दी" },
  { id: "ja", name: "日本語" },
  { id: "ko", name: "한국어" },
  { id: "zh-CN", name: "中文（简体）" },
  { id: "zh-TW", name: "中文（繁體）" },
];

type Dictionary = Record<string, string>;

// One chunk per language, fetched the first time it is used.
const LOADERS: Record<Exclude<LanguageId, "en">, () => Promise<{ default: Dictionary }>> = {
  de: () => import("./de.json"),
  es: () => import("./es.json"),
  fr: () => import("./fr.json"),
  it: () => import("./it.json"),
  pl: () => import("./pl.json"),
  "pt-BR": () => import("./pt-BR.json"),
  tr: () => import("./tr.json"),
  ru: () => import("./ru.json"),
  uk: () => import("./uk.json"),
  hi: () => import("./hi.json"),
  ja: () => import("./ja.json"),
  ko: () => import("./ko.json"),
  "zh-CN": () => import("./zh-CN.json"),
  "zh-TW": () => import("./zh-TW.json"),
};
const DICTS: Partial<Record<LanguageId, Dictionary>> = {};

const isLanguageId = (v: unknown): v is LanguageId => LANGUAGES.some((l) => l.id === v);

// A system language's first part to ours. Portuguese from Portugal gets the
// Brazilian text, the nearest there is.
const BY_BASE: Record<string, LanguageId> = {
  en: "en", de: "de", es: "es", fr: "fr", it: "it", pl: "pl", pt: "pt-BR", tr: "tr",
  ru: "ru", uk: "uk", hi: "hi", ja: "ja", ko: "ko",
};

// The system's language as one of ours, English when there is none: the first
// tag we can serve wins, in the order the system lists them. Chinese goes by
// its script: Traditional (zh-Hant, zh-TW, zh-HK, zh-MO) or Simplified.
export function systemLanguage(): LanguageId {
  if (typeof navigator === "undefined") return "en";
  const tags = navigator.languages?.length ? navigator.languages : [navigator.language];
  for (const tag of tags) {
    const lower = (tag || "").toLowerCase();
    const base = lower.split("-")[0];
    if (base === "zh") return /hant|-tw\b|-hk\b|-mo\b/.test(lower) ? "zh-TW" : "zh-CN";
    if (Object.prototype.hasOwnProperty.call(BY_BASE, base)) return BY_BASE[base];
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
// English until the chosen language's file is in.
let current: LanguageId = "en";
let version = 0;
let ticket = 0;
const listeners = new Set<() => void>();

const notify = () => {
  version++;
  listeners.forEach((l) => l());
};
const applyLang = () => {
  if (typeof document !== "undefined") document.documentElement.lang = current;
};
applyLang();

async function load(id: LanguageId): Promise<boolean> {
  if (id === "en" || DICTS[id]) return true;
  try {
    DICTS[id] = (await LOADERS[id]()).default;
    return true;
  } catch (e) {
    console.error(`[i18n] couldn't load ${id}:`, e);
    return false;
  }
}

// Switches to the language the setting asks for once its file is in. A later
// choice wins over one still loading.
async function follow(): Promise<void> {
  const mine = ++ticket;
  const id = resolve(setting);
  const ok = await load(id);
  if (mine !== ticket) return;
  current = ok ? id : "en";
  applyLang();
  notify();
}

// main.tsx: the device's language, before the first screen.
export function initI18n(): Promise<void> {
  return follow();
}

export function languageSetting(): LanguageSetting { return setting; }
export function currentLanguage(): LanguageId { return current; }

export function setLanguageSetting(next: LanguageSetting) {
  if (next === setting) return;
  setting = next;
  try { localStorage.setItem(LANGUAGE_KEY, next); } catch { /* per-device only */ }
  notify(); // the picker shows the choice at once; the texts follow when loaded
  void follow();
}

// The system language can change while the app runs (Android, a Mac).
if (typeof window !== "undefined") {
  window.addEventListener("languagechange", () => { if (setting === "system") void follow(); });
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

// One of two English forms by count; {count} is filled in. Exactly one takes
// the first. Other counts take the second, unless the language's own rules
// pick a form it adds as "<second key>|few" (Russian 2-4, say), "|many" or
// "|one" (Russian 21); a "one" count with no such line uses the first form
// when that one shows the number.
export function tn(count: number, one: string, other: string, params?: Params): string {
  const p = { count, ...params };
  if (count === 1) return t(one, p);
  const form = pluralForm(count);
  if (form !== "other") {
    const extra = DICTS[current]?.[`${other}|${form}`];
    if (extra) return fill(extra, p);
    if (form === "one" && one.includes("{count}")) return t(one, p);
  }
  return t(other, p);
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
