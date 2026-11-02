// The `[panel-defaults.*]` tables of the shipped `gui/config.toml`, the only
// source of these values (doc "No runtime fallback"), as `metafolder.defaults`
// exposes them: camelCased keys, all panel types together (no key is shared).

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

function camelCase(key: string): string {
  return key.replace(/-([a-z])/g, (_, c: string) => c.toUpperCase());
}

/** Enough TOML for those tables: `key = value`, where a value is JSON once its
 *  array spans are joined. */
function tables(prefix: string): Record<string, any> {
  const file = resolve(__dirname, '../../../default-config/config.toml');
  const lines = readFileSync(file, 'utf8').split('\n');
  const out: Record<string, any> = {};
  let inside = false;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i].trim();
    if (line.startsWith('[') && !/^\S+\s*=/.test(line)) {
      inside = line.startsWith(prefix);
      continue;
    }
    if (!inside || line === '' || line.startsWith('#')) continue;
    const eq = line.indexOf('=');
    const key = line.slice(0, eq).trim();
    let value = line.slice(eq + 1).split(' #')[0].trim();
    while (value.startsWith('[') && !value.endsWith(']')) {
      i += 1;
      value += lines[i].trim();
    }
    out[camelCase(key)] = JSON.parse(value.replace(/,\s*\]$/, ']'));
  }
  return out;
}

export function shippedDefaults(): Record<string, any> {
  return tables('[panel-defaults.');
}

/** The `[panels]` table, as `metafolder.settings` exposes it. */
export function shippedSettings(): Record<string, any> {
  return tables('[panels]');
}
