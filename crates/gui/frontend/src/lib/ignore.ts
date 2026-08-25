// Ignore patterns (spec-gui "Ignore patterns"): the pieces shared by the
// `ignore:*` commands and the file manager's Ignore menu — preset completion,
// ad-hoc pattern construction, and the target resolution that makes the
// nearest-ancestor-wins rule survivable.

// The pattern builders and the target resolution live in the panel shim: the
// file manager's Ignore menu needs them too, and a panel can only import a
// served module (`/__ignore.js`), never the shell's bundle.
export {
  escapeRegex,
  ignoreTarget,
  patternForExtension,
  patternForPath,
} from '../../../panel-shim/ignore.js';

/** One installed preset, already expanded by the backend. */
export interface PresetInfo {
  name: string;
  description: string;
  patterns: string[];
}

/** The Tauri `invoke` shape these helpers need (injected, so tests can stub). */
type Invoke = <T>(cmd: string, args?: any) => Promise<T>;

/** The daemon-call shape these helpers need: `metafolder.daemon.call`. */
type DaemonCall = (method: string, path: string, body?: unknown) => Promise<any>;

/** The installed presets, fetched once per session (a config file changes only
 *  through `metafolder-sync-config`, i.e. between runs). */
let presetsCache: PresetInfo[] | null = null;

/** Test hook: forget the memoised presets. */
export function __resetIgnoreCaches(): void {
  presetsCache = null;
}

async function presets(invoke: Invoke): Promise<PresetInfo[]> {
  if (!presetsCache) presetsCache = await invoke<PresetInfo[]>('ignore_presets');
  return presetsCache;
}

/** The completion line of a preset — the `script:run` shape, description
 *  omitted when it has none. */
function presetLine(preset: PresetInfo): string {
  return preset.description ? `${preset.name} — ${preset.description}` : preset.name;
}

/** Completion candidates for the `preset` argument of the `ignore:*` commands. */
export async function ignorePresetCandidates(invoke: Invoke): Promise<string[]> {
  return (await presets(invoke)).map(presetLine);
}

/** Resolves a picked argument back to a preset name: the full completion line,
 *  or a bare name typed by hand / coming from a keybinding. */
export async function resolvePresetName(choice: string, invoke: Invoke): Promise<string | null> {
  const want = choice.trim();
  const hit = (await presets(invoke)).find((p) => presetLine(p) === want || p.name === want);
  return hit?.name ?? null;
}

/** Where an ignore write should land, and what had to be materialised first. */
export interface IgnoreTarget {
  /** The target metarecord (the directory itself). */
  uuid: string;
  relPath: string;
  /** Inherited patterns the caller must write before applying its change, so
   *  the write extends the effective set instead of shadowing it. Empty when
   *  the target already has its own set, inherits nothing, or the user chose to
   *  start from an empty set. */
  copied: string[];
}

export interface TargetDirOptions {
  call: DaemonCall;
  repo: string;
  /** Absolute path of the repository root. */
  repoRoot: string;
  /** Absolute path of the file manager's current directory, when one is open. */
  fmDir: string | null;
  /** The workspace's selected metarecord, when any. */
  selected: { uuid: string } | null;
}

/** Strips `repoRoot` off an absolute path, yielding the repo-root-relative form
 *  (`""` for the root itself). Null when the path is outside the repository. */
function relativeToRoot(repoRoot: string, abs: string): string | null {
  const root = repoRoot.replace(/\/+$/, '');
  if (abs === root) return '';
  return abs.startsWith(`${root}/`) ? abs.slice(root.length) : null;
}

/** The directory an `ignore:*` command targets (spec-gui "Ignore patterns"):
 *  the file manager's current directory, else the selected metarecord's
 *  directory (itself when it is one, its parent otherwise), else the repository
 *  root. Always a repo-root-relative path. */
export async function targetDir(opts: TargetDirOptions): Promise<string> {
  const { call, repo, repoRoot, fmDir, selected } = opts;
  if (fmDir) {
    const rel = relativeToRoot(repoRoot, fmDir);
    if (rel !== null) return rel;
  }
  if (selected?.uuid) {
    const record = await call('GET', `/repos/${repo}/metarecords/${selected.uuid}`);
    const type = record?.fields?.find((f: any) => f.name === 'mfr_type')?.value?.value;
    const resolved = await call(
      'GET',
      `/repos/${repo}/metarecords/${selected.uuid}/fields/mfr_path/resolve-tree`,
    );
    const path: string | undefined = resolved?.paths?.[0];
    if (typeof path === 'string') {
      if (type === 'dir') return path;
      const cut = path.lastIndexOf('/');
      return cut <= 0 ? '' : path.slice(0, cut);
    }
  }
  return '';
}
