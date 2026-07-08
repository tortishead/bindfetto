// Resolving the `bindfetto.catalogPath` setting to a single catalog JSON string.
// Kept free of the `vscode` module so it can be unit-tested under plain Node.

import * as fs from "fs";
import * as path from "path";

/**
 * The setting may point to one catalog file, or a directory — in which case every
 * `*.json` under it (recursively) is merged into one catalog. On an interface both
 * define, their code→method maps are merged; later files (sorted by path) win per code.
 */
export function loadCatalogJson(catalogPath: string): string {
  if (!fs.statSync(catalogPath).isDirectory()) {
    return fs.readFileSync(catalogPath, "utf8");
  }
  const files = collectJsonFiles(catalogPath).sort();
  if (files.length === 0) {
    throw new Error(`no .json catalog files found under ${catalogPath}`);
  }
  const merged: Record<string, Record<string, string>> = {};
  for (const file of files) {
    let obj: unknown;
    try {
      obj = JSON.parse(fs.readFileSync(file, "utf8"));
    } catch (err) {
      throw new Error(`invalid catalog ${file}: ${err instanceof Error ? err.message : err}`);
    }
    for (const [iface, methods] of Object.entries(obj as Record<string, Record<string, string>>)) {
      merged[iface] = { ...merged[iface], ...methods };
    }
  }
  return JSON.stringify(merged);
}

/** All `*.json` files under `dir`, recursively. */
function collectJsonFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...collectJsonFiles(full));
    } else if (entry.name.endsWith(".json")) {
      out.push(full);
    }
  }
  return out;
}
