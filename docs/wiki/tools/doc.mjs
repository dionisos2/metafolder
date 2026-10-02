// scripts/doc — read, search, edit and check the documentation wiki from the
// command line (docs/doc-wiki-proposal.md, "Outil scripts/doc"). The logic is
// in tid.mjs; this file only reads and writes files.

import fs from 'node:fs';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  parseTid,
  serializeTid,
  slugify,
  parseTitleList,
  stringifyTitleList,
  extractLinks,
  extractCodeRefs,
  checkWiki,
  renameInTiddler,
  renameInCode,
  scriptTiddler,
} from './tid.mjs';

/** @typedef {import('./tid.mjs').Tiddler} Tiddler */

export const WIKI = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const TIDDLERS = path.join(WIKI, 'tiddlers');
const REPO = path.resolve(WIKI, '..', '..');

/** Where the code that may cite the wiki lives, and what counts as code. */
const CODE_DIRS = ['crates', 'scripts'];
const CODE_EXT = new Set(['.rs', '.js', '.mjs', '.ts', '.svelte', '.sh', '.toml', '.html']);
const SKIP_DIRS = new Set(['node_modules', 'target', 'dist', 'coverage', '.git', 'gen']);

const USAGE = `usage: scripts/doc <command> [args]

  ls [--tag T]... [--kind K] [--audience A] [--status S] [--all]
                          list notes (title — summary); --all adds generated/system
  show NAME               print a note (NAME = title, alias or id)
  find TEXT               case-insensitive search, grouped by note
  links NAME              what a note links to, and what links to it
  refs NAME               where the code cites a note
  topic NAME              a topic's note followed by every note tagged with it
  new TITLE [--tags L] [--kind K] [--summary S] [--audience A] [--from-gen]
                          create a hand-written note at the right file name
  rename OLD NEW          rename a note and every mention of it (wiki and code)
  check                   every consistency check; exit 1 on any problem
  build [OUT]             render the help pages (default docs/wiki/dist/)
  gen                     rewrite tiddlers/generated/ from the code`;

/** @param {string} dir @returns {string[]} */
function walk(dir) {
  if (!fs.existsSync(dir)) return [];
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) return SKIP_DIRS.has(e.name) ? [] : walk(p);
    return [p];
  });
}

/** Every tiddler of the wiki. @param {string} [dir] @returns {Tiddler[]} */
export function loadWiki(dir = TIDDLERS) {
  return walk(dir)
    .filter((p) => p.endsWith('.tid'))
    .map((p) => {
      const file = path.relative(dir, p);
      const top = file.split(path.sep)[0];
      /** @type {Tiddler['area']} */
      const area = top === 'generated' ? 'generated' : top === 'system' ? 'system' : 'hand';
      return { file, area, ...parseTid(fs.readFileSync(p, 'utf8')) };
    });
}

/** Every `doc "…"` citation in the code. */
function scanCode() {
  return CODE_DIRS.flatMap((d) => walk(path.join(REPO, d)))
    .filter((p) => CODE_EXT.has(path.extname(p)) || path.basename(p) === 'doc')
    .flatMap((p) => {
      const file = path.relative(REPO, p);
      return extractCodeRefs(fs.readFileSync(p, 'utf8')).map((r) => ({ file, ...r }));
    });
}

/**
 * The shipped-script catalog: generated/shipped-script/ made to match
 * scripts/shipped/ (`update`), or the differences named (the Rust catalogs do
 * the same from their golden tests, metafolder_core::doc_gen).
 * @param {boolean} update @returns {string[]} problems
 */
function syncScripts(update) {
  const shipped = path.join(REPO, 'scripts', 'shipped');
  const dir = path.join(TIDDLERS, 'generated', 'shipped-script');
  /** @type {Map<string, string>} */
  const want = new Map(
    walk(shipped)
      .filter((p) => p.endsWith('.sh'))
      .map((p) => {
        const rel = path.relative(shipped, p);
        return [`${slugify(rel)}.tid`, serializeTid(scriptTiddler(rel, fs.readFileSync(p, 'utf8')))];
      }),
  );
  const have = new Map(
    walk(dir)
      .filter((p) => p.endsWith('.tid'))
      .map((p) => [path.basename(p), fs.readFileSync(p, 'utf8')]),
  );
  if (update) {
    fs.mkdirSync(dir, { recursive: true });
    for (const name of have.keys()) if (!want.has(name)) fs.rmSync(path.join(dir, name));
    for (const [name, src] of want) if (have.get(name) !== src) fs.writeFileSync(path.join(dir, name), src);
    return [];
  }
  const problems = [];
  for (const [name, src] of want) {
    if (!have.has(name)) problems.push(`missing ${name}`);
    else if (have.get(name) !== src) problems.push(`stale ${name}`);
  }
  for (const name of have.keys()) if (!want.has(name)) problems.push(`obsolete ${name}`);
  return problems.length === 0
    ? []
    : [`generated/shipped-script is out of date with scripts/shipped (${problems.join(', ')}); run scripts/doc gen`];
}

/** A note by title, alias or id. @param {Tiddler[]} wiki @param {string} name */
function resolve(wiki, name) {
  return (
    wiki.find((t) => t.fields.title === name) ??
    wiki.find((t) => parseTitleList(t.fields.aliases).includes(name)) ??
    wiki.find((t) => t.area === 'hand' && slugify(t.fields.title) === slugify(name))
  );
}

/** @param {Tiddler[]} wiki @param {string} name */
function mustResolve(wiki, name) {
  const t = resolve(wiki, name);
  if (!t) throw new UsageError(`no note named "${name}"`);
  return t;
}

class UsageError extends Error {}

/** @param {string[]} args */
function options(args) {
  /** @type {Record<string, string[]>} */
  const opts = {};
  /** @type {string[]} */
  const rest = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a.startsWith('--')) {
      const name = a.slice(2);
      const flag = name === 'all' || name === 'from-gen';
      (opts[name] ??= []).push(flag ? 'true' : (args[++i] ?? ''));
    } else rest.push(a);
  }
  return { opts, rest };
}

/** @param {Tiddler} t */
const line = (t) => `${t.fields.title}${t.fields.summary ? ` — ${t.fields.summary}` : ''}`;

/** @param {string[]} argv @returns {Promise<number>} exit status */
export async function main(argv) {
  const [command, ...args] = argv;
  const { opts, rest } = options(args);
  const wiki = loadWiki();
  switch (command) {
    case 'ls': {
      const tags = opts.tag ?? [];
      const picked = wiki.filter(
        (t) =>
          (opts.all || t.area === 'hand') &&
          tags.every((tag) => parseTitleList(t.fields.tags).includes(tag)) &&
          (!opts.kind || opts.kind.includes(t.fields.kind)) &&
          (!opts.audience || opts.audience.includes(t.fields.audience ?? 'user')) &&
          (!opts.status || opts.status.includes(t.fields.status ?? 'implemented')),
      );
      for (const t of picked.sort((a, b) => a.fields.title.localeCompare(b.fields.title))) {
        console.log(line(t));
      }
      return 0;
    }
    case 'show': {
      const t = mustResolve(wiki, rest.join(' '));
      console.log(`# ${path.join('docs/wiki/tiddlers', t.file)}`);
      process.stdout.write(serializeTid(t));
      return 0;
    }
    case 'find': {
      const needle = rest.join(' ').toLowerCase();
      if (!needle) throw new UsageError('find needs a text');
      for (const t of wiki) {
        const hits = serializeTid(t)
          .split('\n')
          .map((l, i) => ({ l, i }))
          .filter(({ l }) => l.toLowerCase().includes(needle));
        if (hits.length === 0) continue;
        console.log(`${t.fields.title} (${t.file})`);
        for (const { l, i } of hits) console.log(`  ${i + 1}: ${l.trim()}`);
      }
      return 0;
    }
    case 'links': {
      const t = mustResolve(wiki, rest.join(' '));
      const title = t.fields.title;
      console.log('out:');
      for (const l of extractLinks(t.text)) console.log(`  ${l.kind} ${l.target}`);
      for (const tag of parseTitleList(t.fields.tags)) console.log(`  tag ${tag}`);
      console.log('in:');
      for (const o of wiki) {
        const how = extractLinks(o.text)
          .filter((l) => l.target === title)
          .map((l) => l.kind);
        if (parseTitleList(o.fields.tags).includes(title)) how.push('tagged');
        if (how.length > 0) console.log(`  ${o.fields.title} (${[...new Set(how)].join(', ')})`);
      }
      return 0;
    }
    case 'refs': {
      const title = mustResolve(wiki, rest.join(' ')).fields.title;
      for (const r of scanCode().filter((r) => r.title === title)) console.log(`${r.file}:${r.line}`);
      return 0;
    }
    case 'topic': {
      const hub = mustResolve(wiki, rest.join(' '));
      const title = hub.fields.title;
      const order = parseTitleList(hub.fields.list);
      const rank = (/** @type {Tiddler} */ t) => {
        const i = order.indexOf(t.fields.title);
        return i === -1 ? order.length : i;
      };
      const members = wiki
        .filter((t) => parseTitleList(t.fields.tags).includes(title))
        .sort((a, b) => rank(a) - rank(b) || a.fields.title.localeCompare(b.fields.title));
      for (const t of [hub, ...members]) {
        console.log(`==== ${t.fields.title} (${t.file})`);
        process.stdout.write(serializeTid(t));
        console.log();
      }
      return 0;
    }
    case 'new': {
      const title = rest.join(' ');
      if (!title) throw new UsageError('new needs a title');
      if (resolve(wiki, title)?.fields.title === title) throw new UsageError(`"${title}" already exists`);
      /** @type {Record<string, string>} */
      const fields = { title };
      if (opts['from-gen']) {
        const g = wiki.find((t) => t.area === 'generated' && t.fields.target === title);
        if (!g) throw new UsageError(`nothing generated is named "${title}"`);
        fields.tags = stringifyTitleList([g.fields.catalog, ...parseTitleList(opts.tags?.[0])]);
        fields.kind = opts.kind?.[0] ?? 'reference';
        fields.summary = opts.summary?.[0] ?? g.fields.summary ?? '';
      } else {
        if (opts.tags) fields.tags = opts.tags[0];
        fields.kind = opts.kind?.[0] ?? 'reference';
        fields.summary = opts.summary?.[0] ?? '';
      }
      if (opts.audience) fields.audience = opts.audience[0];
      const file = path.join(TIDDLERS, `${slugify(title)}.tid`);
      if (fs.existsSync(file)) throw new UsageError(`${file} already exists`);
      fs.writeFileSync(file, serializeTid({ fields, text: '' }));
      console.log(path.relative(REPO, file));
      return 0;
    }
    case 'rename': {
      const [from, to] = rest;
      if (!from || !to) throw new UsageError('rename needs OLD and NEW');
      const old = wiki.find((t) => t.fields.title === from);
      if (!old) throw new UsageError(`no note titled "${from}"`);
      if (wiki.some((t) => t.fields.title === to)) throw new UsageError(`"${to}" already exists`);
      for (const t of wiki) {
        const r = renameInTiddler(t, from, to);
        const src = serializeTid(r);
        if (src !== serializeTid(t) || t === old) {
          const target = t === old && t.area === 'hand' ? `${slugify(to)}.tid` : t.file;
          if (target !== t.file) fs.rmSync(path.join(TIDDLERS, t.file));
          fs.writeFileSync(path.join(TIDDLERS, target), src);
        }
      }
      for (const file of new Set(scanCode().filter((r) => r.title === from).map((r) => r.file))) {
        const p = path.join(REPO, file);
        fs.writeFileSync(p, renameInCode(fs.readFileSync(p, 'utf8'), from, to));
        console.log(`updated ${file}`);
      }
      return 0;
    }
    case 'check': {
      const errors = [...checkWiki(wiki, scanCode()), ...syncScripts(false)];
      for (const e of errors) console.log(e);
      const hand = wiki.filter((t) => t.area === 'hand').length;
      console.log(errors.length === 0 ? `ok — ${hand} notes` : `${errors.length} problem(s)`);
      return errors.length === 0 ? 0 : 1;
    }
    case 'gen': {
      // The Rust catalogs rewrite themselves from their golden tests.
      for (const args of [
        ['test', '-q', '-p', 'metafolder-cli', '--bin', 'mf', 'doc_gen'],
        ['test', '-q', '-p', 'metafolder-gui', '--lib', 'doc_gen'],
        ['test', '-q', '-p', 'metafolder-daemon', '--lib', 'the_wiki_catalog_matches_the_code'],
      ]) {
        const run = spawnSync('cargo', args, {
          cwd: REPO,
          stdio: 'inherit',
          env: { ...process.env, MF_DOC_UPDATE: '1' },
        });
        if (run.status !== 0) return 1;
      }
      syncScripts(true);
      console.log('generated/ is up to date');
      return 0;
    }
    case 'build': {
      const { build } = await import('./build.mjs');
      const out = path.resolve(rest[0] ?? path.join(WIKI, 'dist'));
      console.log(`${await build(WIKI, out)} pages → ${path.relative(REPO, out) || out}`);
      return 0;
    }
    case undefined:
    case 'help':
    case '--help':
      console.log(USAGE);
      return 0;
    default:
      throw new UsageError(`unknown command "${command}"`);
  }
}

try {
  process.exitCode = await main(process.argv.slice(2));
} catch (error) {
  if (!(error instanceof UsageError)) throw error;
  console.error(`error: ${error.message}\n\n${USAGE}`);
  process.exitCode = 2;
}
