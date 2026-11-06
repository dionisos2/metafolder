// The `file:` commands (doc "File commands"): what is done to files, from any
// panel. They are generic panel commands (doc "Generic panel commands"): the
// panel named first designates the files — `paths`, the checked ones or the one
// under its cursor — or the folder it shows — `directory` — and the command
// does the rest, so renaming a file is the same command in the file manager,
// the list and the viewer. `clipboard:paste` lives here with them: it works on
// the file or folder under the panel's cursor — `pasteTarget`.
//
// Pure apart from `FileOps`, what the shell hands in: the filesystem, the trash,
// the questions and the status bar.

import {
  baseName,
  dedupeName,
  entryName,
  isWithin,
  joinPath,
  parentDir,
  pasteDir,
  trashedMessage,
} from '../../../panel-shim/file-actions.js';
import { pathText, samePath } from '../../../panel-shim/ui.js';

/** A path in its wire form (doc "Tree names"): a string while it is text,
 *  `{text, bytes}` otherwise. Shown by its text, acted on as it is. */
type Path = Metafolder.WirePath;

/** What the `file:` commands do their work with. */
export interface FileOps {
  readDir(dir: Path): Promise<{ name: string }[]>;
  move(from: Path, to: Path): Promise<void>;
  copy(from: Path, to: Path): Promise<void>;
  mkdir(path: Path): Promise<void>;
  createFile(path: Path): Promise<void>;
  /** Sends `path` to the active repository's trash, its metarecords with it,
   *  or to the desktop's when no repository holds it; true for the desktop's. */
  trash(path: Path): Promise<boolean>;
  /** Gives `path` its metarecord; false when the user gave up. */
  record(path: Path): Promise<boolean>;
  /** Asks before a trashing; true to go ahead. One file is asked about
   *  unless the user switched the question off, `many` always (doc
   *  "Confirming a trashing"). */
  confirm(question: string, many: boolean): Promise<boolean>;
  /** Asks for a text in the command input; null when given up. */
  ask(question: string, initial?: string): Promise<string | null>;
  /** The file clipboard (doc "Global GUI variables"). */
  clipboard(): Promise<{ paths: Path[]; mode: 'copy' | 'cut' } | null>;
  clearClipboard(): Promise<void>;
  status(text: string): Promise<void>;
  /** Tells the other panels the files changed (`metarecords:dirty`). */
  changed(): Promise<void>;
}

/** The functions of the panel a `file:` command runs on. */
export interface FileFunctions {
  paths?: () => Path[] | Promise<Path[]>;
  /** What `file:record` takes, when it is not `paths`: those of them without
   *  a metarecord. */
  recordable?: () => Path[] | Promise<Path[]>;
  directory?: () => Path | null | Promise<Path | null>;
  /** The file or folder `clipboard:paste` lands at. */
  pasteTarget?: () => Path | null | Promise<Path | null>;
  refresh?: () => unknown;
  /** Puts the cursor on the entry `name` of the folder shown. */
  reveal?: (name: string) => unknown;
}

/** What each `file:` command needs of its panel. */
export const FILE_NEEDS: Record<string, 'paths' | 'directory' | 'pasteTarget'> = {
  'file:trash': 'paths',
  'file:rename': 'paths',
  'file:record': 'paths',
  'file:new': 'directory',
  'clipboard:paste': 'pasteTarget',
};

export const FILE_COMMANDS = Object.keys(FILE_NEEDS);
export const NEW_KINDS = ['folder', 'file'];

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;

async function designated(fns: FileFunctions): Promise<Path[]> {
  const paths = (await fns.paths?.()) ?? [];
  if (paths.length === 0) throw new Error('no file here');
  return paths;
}

async function folder(fns: FileFunctions): Promise<Path> {
  const dir = (await fns.directory?.()) ?? null;
  if (dir === null) throw new Error('no folder here');
  return dir;
}

/** The names in `dir` as listed: their text. */
async function takenNames(ops: FileOps, dir: Path): Promise<Set<string>> {
  return new Set((await ops.readDir(dir)).map((entry) => entry.name));
}

/** The panel reads again, the cursor on `name` when the command made one. */
async function after(ops: FileOps, fns: FileFunctions, name: string | null): Promise<void> {
  await ops.changed();
  await fns.refresh?.();
  if (name !== null) await fns.reveal?.(name);
}

/** Runs the `file:` command `name` on a panel's functions, `rest` being its
 *  arguments after the panel. */
export async function runFileCommand(
  name: string,
  fns: FileFunctions,
  rest: string[],
  ops: FileOps,
): Promise<void> {
  switch (name) {
    case 'file:trash': {
      const paths = await designated(fns);
      const named =
        paths.length === 1 ? `"${pathText(baseName(paths[0]))}"` : plural(paths.length, 'file');
      if (!(await ops.confirm(`Send ${named} to the trash?`, paths.length > 1))) return;
      const desktop = [];
      for (const path of paths) desktop.push(await ops.trash(path));
      const what = paths.length === 1 ? pathText(baseName(paths[0])) : plural(paths.length, 'file');
      await ops.status(trashedMessage(what, paths.length > 1, desktop));
      await after(ops, fns, null);
      return;
    }
    case 'file:rename': {
      const paths = await designated(fns);
      if (paths.length > 1) throw new Error('rename one file at a time');
      const [path] = paths;
      // The text is what is edited: what is typed is a new name.
      const old = pathText(baseName(path));
      // Renaming edits the name there is: it is offered (doc "Interactive
      // command arguments").
      const typed = rest.length > 0 ? rest.join(' ') : await ops.ask('Rename to:', old);
      const next = (typed ?? '').trim();
      if (!next || next === old) return;
      entryName(next);
      await ops.move(path, joinPath(parentDir(path), next));
      await ops.status(`Renamed to ${next}`);
      await after(ops, fns, next);
      return;
    }
    case 'file:record': {
      const paths = await designated(fns.recordable ? { paths: fns.recordable } : fns);
      let recorded = 0;
      for (const path of paths) if (await ops.record(path)) recorded += 1;
      if (recorded === 0) return;
      await ops.status(
        `Recorded ${recorded === 1 ? pathText(baseName(paths[0])) : plural(recorded, 'file')} (mf_watch = false)`,
      );
      await after(ops, fns, null);
      return;
    }
    case 'file:new': {
      const [kind, ...words] = rest;
      if (!NEW_KINDS.includes(kind ?? '')) {
        throw new Error(`create what? "${kind ?? ''}" (${NEW_KINDS.join(' / ')})`);
      }
      const dir = await folder(fns);
      const typed = words.length > 0 ? words.join(' ') : await ops.ask(`New ${kind} name:`);
      const newName = (typed ?? '').trim();
      if (!newName) return;
      entryName(newName);
      const path = joinPath(dir, newName);
      if (kind === 'folder') await ops.mkdir(path);
      else await ops.createFile(path);
      await ops.status(`Created ${kind} ${newName}`);
      await after(ops, fns, newName);
      return;
    }
    case 'clipboard:paste': {
      const clip = await ops.clipboard();
      if (!clip || clip.paths.length === 0) throw new Error('clipboard is empty');
      const at = (await fns.pasteTarget?.()) ?? null;
      if (at === null) throw new Error('no folder here');
      const dir = await pasteDir(ops, at, undefined, clip);
      const taken = await takenNames(ops, dir);
      let pasted = 0;
      let last: string | null = null;
      for (const src of clip.paths) {
        // Never a folder into itself or one of its descendants.
        if (isWithin(dir, src)) {
          await ops.status(`cannot paste ${pathText(baseName(src))} into itself`);
          continue;
        }
        const target = dedupeName(baseName(src), taken);
        if (clip.mode === 'cut') await ops.move(src, joinPath(dir, target));
        else await ops.copy(src, joinPath(dir, target));
        taken.add(pathText(target));
        pasted += 1;
        last = pathText(target);
      }
      // What was cut has moved: there is nothing left to paste.
      if (clip.mode === 'cut') await ops.clearClipboard();
      if (pasted === 0) return;
      await ops.status(`Pasted ${plural(pasted, 'item')}`);
      // The cursor can only go on what was made in the folder the panel shows.
      const showing = (await fns.directory?.()) ?? null;
      const shown = showing !== null && samePath(showing, dir);
      await after(ops, fns, shown && pasted === 1 ? last : null);
      return;
    }
  }
  throw new Error(`not a file command: ${name}`);
}
