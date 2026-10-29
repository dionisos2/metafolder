// The `file:` commands (doc "File commands"): what is done to files, from any
// panel. They are generic panel commands (doc "Generic panel commands"): the
// panel named first designates the files — `paths`, the checked ones or the one
// under its cursor — or the folder it shows — `directory` — and the command
// does the rest, so renaming a file is the same command in the file manager,
// the list and the viewer.
//
// Pure apart from `FileOps`, what the shell hands in: the filesystem, the trash,
// the questions and the status bar.

import {
  baseName,
  dedupeName,
  isWithin,
  joinPath,
  parentDir,
  trashedMessage,
} from '../../../panel-shim/file-actions.js';

/** What the `file:` commands do their work with. */
export interface FileOps {
  readDir(dir: string): Promise<{ name: string }[]>;
  move(from: string, to: string): Promise<void>;
  copy(from: string, to: string): Promise<void>;
  mkdir(path: string): Promise<void>;
  createFile(path: string): Promise<void>;
  /** Sends `path` to the active repository's trash, its metarecords with it,
   *  or to the desktop's when no repository holds it; true for the desktop's. */
  trash(path: string): Promise<boolean>;
  /** Gives `path` its metarecord; false when the user gave up. */
  record(path: string): Promise<boolean>;
  /** Asks before a trashing; true to go ahead. One file is asked about
   *  unless the user switched the question off, `many` always (doc
   *  "Confirming a trashing"). */
  confirm(question: string, many: boolean): Promise<boolean>;
  /** Asks for a text in the command input; null when given up. */
  ask(question: string, initial?: string): Promise<string | null>;
  /** The file clipboard (doc "Global GUI variables"). */
  clipboard(): Promise<{ paths: string[]; mode: 'copy' | 'cut' } | null>;
  clearClipboard(): Promise<void>;
  status(text: string): Promise<void>;
  /** Tells the other panels the files changed (`metarecords:dirty`). */
  changed(): Promise<void>;
}

/** The functions of the panel a `file:` command runs on. */
export interface FileFunctions {
  paths?: () => string[] | Promise<string[]>;
  /** What `file:record` takes, when it is not `paths`: those of them without
   *  a metarecord. */
  recordable?: () => string[] | Promise<string[]>;
  directory?: () => string | null | Promise<string | null>;
  refresh?: () => unknown;
  /** Puts the cursor on the entry `name` of the folder shown. */
  reveal?: (name: string) => unknown;
}

/** What each `file:` command needs of its panel. */
export const FILE_NEEDS: Record<string, 'paths' | 'directory'> = {
  'file:trash': 'paths',
  'file:rename': 'paths',
  'file:duplicate': 'paths',
  'file:record': 'paths',
  'file:new': 'directory',
  'file:paste': 'directory',
};

export const FILE_COMMANDS = Object.keys(FILE_NEEDS);
export const NEW_KINDS = ['folder', 'file'];

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`;

async function designated(fns: FileFunctions): Promise<string[]> {
  const paths = (await fns.paths?.()) ?? [];
  if (paths.length === 0) throw new Error('no file here');
  return paths;
}

async function folder(fns: FileFunctions): Promise<string> {
  const dir = (await fns.directory?.()) ?? null;
  if (dir === null) throw new Error('no folder here');
  return dir;
}

async function takenNames(ops: FileOps, dir: string): Promise<Set<string>> {
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
      const named = paths.length === 1 ? `"${baseName(paths[0])}"` : plural(paths.length, 'file');
      if (!(await ops.confirm(`Send ${named} to the trash?`, paths.length > 1))) return;
      const desktop = [];
      for (const path of paths) desktop.push(await ops.trash(path));
      const what = paths.length === 1 ? baseName(paths[0]) : plural(paths.length, 'file');
      await ops.status(trashedMessage(what, paths.length > 1, desktop));
      await after(ops, fns, null);
      return;
    }
    case 'file:rename': {
      const paths = await designated(fns);
      if (paths.length > 1) throw new Error('rename one file at a time');
      const [path] = paths;
      const old = baseName(path);
      // Renaming edits the name there is: it is offered (doc "Interactive
      // command arguments").
      const typed = rest.length > 0 ? rest.join(' ') : await ops.ask('Rename to:', old);
      const next = (typed ?? '').trim();
      if (!next || next === old) return;
      await ops.move(path, joinPath(parentDir(path), next));
      await ops.status(`Renamed to ${next}`);
      await after(ops, fns, next);
      return;
    }
    case 'file:duplicate': {
      const paths = await designated(fns);
      let last: string | null = null;
      for (const path of paths) {
        const dir = parentDir(path);
        last = dedupeName(baseName(path), await takenNames(ops, dir));
        await ops.copy(path, joinPath(dir, last));
      }
      await ops.status(paths.length === 1 ? `Duplicated to ${last}` : `Duplicated ${plural(paths.length, 'file')}`);
      await after(ops, fns, paths.length === 1 ? last : null);
      return;
    }
    case 'file:record': {
      const paths = await designated(fns.recordable ? { paths: fns.recordable } : fns);
      let recorded = 0;
      for (const path of paths) if (await ops.record(path)) recorded += 1;
      if (recorded === 0) return;
      await ops.status(`Recorded ${recorded === 1 ? baseName(paths[0]) : plural(recorded, 'file')} (mf_watch = false)`);
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
      const path = joinPath(dir, newName);
      if (kind === 'folder') await ops.mkdir(path);
      else await ops.createFile(path);
      await ops.status(`Created ${kind} ${newName}`);
      await after(ops, fns, newName);
      return;
    }
    case 'file:paste': {
      const dir = await folder(fns);
      const clip = await ops.clipboard();
      if (!clip || clip.paths.length === 0) throw new Error('clipboard is empty');
      const taken = await takenNames(ops, dir);
      let pasted = 0;
      for (const src of clip.paths) {
        // Never a folder into itself or one of its descendants.
        if (isWithin(dir, src)) {
          await ops.status(`cannot paste ${baseName(src)} into itself`);
          continue;
        }
        const target = dedupeName(baseName(src), taken);
        if (clip.mode === 'cut') await ops.move(src, joinPath(dir, target));
        else await ops.copy(src, joinPath(dir, target));
        taken.add(target);
        pasted += 1;
      }
      // What was cut has moved: there is nothing left to paste.
      if (clip.mode === 'cut') await ops.clearClipboard();
      if (pasted === 0) return;
      await ops.status(`Pasted ${plural(pasted, 'item')}`);
      await after(ops, fns, clip.paths.length === 1 ? baseName(clip.paths[0]) : null);
      return;
    }
  }
  throw new Error(`not a file command: ${name}`);
}
