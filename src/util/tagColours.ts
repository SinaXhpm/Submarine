// The colours a server can be tagged with (the swatches in the server form).
// A tag is saved as its hex value, so the first seven are the ones that were
// always there (same values); the rest are the same family of shades (Tailwind's
// 500s) in hue order, and the form also takes any other colour from the
// system's colour picker.
export type TagColour = { name: string; value: string | null };

export const TAG_COLOURS: TagColour[] = [
  { name: "Default", value: null },
  { name: "Red", value: "#ef4444" },
  { name: "Rose", value: "#f43f5e" },
  { name: "Pink", value: "#ec4899" },
  { name: "Fuchsia", value: "#d946ef" },
  { name: "Purple", value: "#a855f7" },
  { name: "Violet", value: "#8b5cf6" },
  { name: "Indigo", value: "#6366f1" },
  { name: "Blue", value: "#3b82f6" },
  { name: "Sky", value: "#0ea5e9" },
  { name: "Cyan", value: "#06b6d4" },
  { name: "Teal", value: "#14b8a6" },
  { name: "Emerald", value: "#10b981" },
  { name: "Green", value: "#22c55e" },
  { name: "Lime", value: "#84cc16" },
  { name: "Yellow", value: "#eab308" },
  { name: "Amber", value: "#f59e0b" },
  { name: "Orange", value: "#f97316" },
  { name: "Brown", value: "#92400e" },
  { name: "Slate", value: "#64748b" },
  { name: "Silver", value: "#d4d4d8" },
];

/** What the picker of another colour starts from when none was chosen. */
export const CUSTOM_COLOUR_START = "#6366f1";

/** The colour of a tag that isn't one of the swatches (one the picker made, or one a synced row carries), else null. */
export function customTagColour(value: string | null | undefined): string | null {
  if (!value) return null;
  return TAG_COLOURS.some((c) => c.value === value) ? null : value;
}

/** `value` as the colour input wants it (#rrggbb), or its starting colour for anything else. */
export function pickerValue(value: string | null): string {
  return value && /^#[0-9a-f]{6}$/i.test(value) ? value.toLowerCase() : CUSTOM_COLOUR_START;
}
