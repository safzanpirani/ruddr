import { readdir, readFile, writeFile } from "node:fs/promises";
import { basename, join } from "node:path";

type ThemeValue = string | { dark: string; light: string };
type ThemeFile = {
  defs?: Record<string, string>;
  theme: Record<string, ThemeValue>;
};

// Refreshes the OpenCode palettes in the theme list the TUI and web
// dashboard share. Ruddr intentionally uses each theme's dark variant.
// Source: https://github.com/anomalyco/opencode/tree/b72b50006b24666da9f2088dbce907d6b24b6901/packages/tui/src/theme/assets
const themesFile = new URL("../crates/ruddr-core/src/themes.json", import.meta.url).pathname;
const [assetsDirectory] = Bun.argv.slice(2);
if (!assetsDirectory) {
  throw new Error("usage: bun scripts/sync-opencode-themes.ts OPENCODE_THEME_ASSETS_DIRECTORY");
}

const mappings = {
  background: "background",
  panel: "backgroundPanel",
  border: "border",
  text: "text",
  dim: "textMuted",
  accent: "primary",
  selected: "backgroundElement",
  danger: "error",
  success: "success",
  warning: "warning",
} as const;

function resolveColor(file: ThemeFile, value: ThemeValue): string {
  let current = typeof value === "string" ? value : value.dark;
  if (current === "transparent") return "#00000000";
  const seen = new Set<string>();
  while (!current.startsWith("#")) {
    if (seen.has(current)) throw new Error(`circular color reference ${current}`);
    seen.add(current);
    const next = file.defs?.[current];
    if (!next) throw new Error(`unresolved color reference ${current}`);
    current = next;
    if (current === "transparent") return "#00000000";
  }
  if (/^#[0-9a-fA-F]{3,4}$/.test(current))
    current = `#${[...current.slice(1)].map((digit) => digit.repeat(2)).join("")}`;
  if (!/^#[0-9a-fA-F]{6}([0-9a-fA-F]{2})?$/.test(current))
    throw new Error(`unsupported color ${current}`);
  return current;
}

const files = (await readdir(assetsDirectory))
  .filter((file) => file.endsWith(".json"))
  .sort();
const themes: Record<string, Record<string, string>> = {};
for (const fileName of files) {
  const file = JSON.parse(
    await readFile(join(assetsDirectory, fileName), "utf8"),
  ) as ThemeFile;
  const name = basename(fileName, ".json");
  themes[name] = Object.fromEntries(
    Object.entries(mappings).map(([ruddrKey, openCodeKey]) => {
      const value = file.theme[openCodeKey];
      if (!value) throw new Error(`${name} is missing ${openCodeKey}`);
      return [ruddrKey, resolveColor(file, value)];
    }),
  );
}

type Theme = { name: string; label: string; source: string; palette: Record<string, string> };
const existing = JSON.parse(await readFile(themesFile, "utf8")) as Theme[];
const labels = new Map(existing.map((theme) => [theme.name, theme.label]));
const titleCase = (name: string) =>
  name.split("-").map((word) => `${word[0]?.toUpperCase() ?? ""}${word.slice(1)}`).join(" ");
const updated: Theme[] = [
  ...existing.filter((theme) => theme.source !== "OpenCode"),
  ...Object.entries(themes).map(([name, palette]) => ({
    name,
    label: labels.get(name) ?? titleCase(name),
    source: "OpenCode",
    palette,
  })),
];
await writeFile(themesFile, JSON.stringify(updated));
console.log(`wrote ${updated.length} themes to ${themesFile}`);
