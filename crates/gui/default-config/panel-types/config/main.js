// config panel: the configuration files — every one of the configuration
// repository and of the workspace's repository, filtered as a path is typed
// (doc "config panel"). The keys are edited in the keybindings panel.

import { byId, el } from '/__ui.js';

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export async function mount(root, metafolder) {
  const files = byId(root, 'files');
  const fileFilter = byId(root, 'file-filter', HTMLInputElement);
  const filesCount = byId(root, 'files-count');
  const roots = byId(root, 'roots');

  /** @type {Metafolder.ConfigFile[]} */
  let configFiles = [];
  // Only the latest list is shown: the repository may change while one is read.
  let filesSeq = 0;

  /** @param {string} what */
  const reloadButton = (what) =>
    el(
      'button',
      {
        class: 'reload',
        title: `config:reload ${what}`,
        onclick: () => metafolder.invoke(['config:reload', what]),
      },
      'Reload',
    );

  /** The viewer panel follows the selection: the file becomes it — a path with
   *  no metarecord, the metarecord written first as every panel publishes the
   *  pair (doc "Cross-panel selection") — and the panel is opened beside.
   *  @param {string} path */
  async function showInFilePanel(path) {
    await metafolder.workspace.set('selected_metarecord', null);
    await metafolder.workspace.set('selected_paths', [path]);
    await metafolder.invoke(['panel:open', 'other', 'viewer']);
  }

  function renderFiles() {
    const typed = fileFilter.value.trim().toLowerCase();
    const shown = configFiles.filter((file) => file.path.toLowerCase().includes(typed));
    filesCount.textContent =
      shown.length === configFiles.length
        ? `${configFiles.length} files`
        : `${shown.length} of ${configFiles.length} files`;
    /** @type {Map<string, Metafolder.ConfigFile[]>} */
    const byCategory = new Map();
    for (const file of shown) {
      byCategory.set(file.category, [...(byCategory.get(file.category) ?? []), file]);
    }
    files.replaceChildren(
      ...[...byCategory].flatMap(([category, inCategory]) => [
        el('tr', { class: 'category' }, el('th', { colspan: 2 }, category)),
        ...inCategory.map((file) =>
          el(
            'tr',
            {},
            el('td', {}, el('code', {}, file.path)),
            el(
              'td',
              { class: 'actions' },
              el(
                'button',
                { title: 'Show it in the viewer panel', onclick: () => void showInFilePanel(file.path) },
                'Show',
              ),
              file.reload ? reloadButton(file.reload) : null,
            ),
          ),
        ),
      ]),
    );
  }

  /** Reads the list again: a file may have been added, and the workspace's
   *  repository changed. */
  async function showFiles() {
    const seq = ++filesSeq;
    const repo = /** @type {string|null} */ ((await metafolder.workspace.get('active_repo')) ?? null);
    const info = await metafolder.config.info(repo);
    if (seq !== filesSeq) return;
    configFiles = info.files;
    roots.replaceChildren(
      el('span', {}, 'Configuration ', el('code', {}, info.root)),
      ...(info.repo_config ? [el('span', {}, 'Repository ', el('code', {}, info.repo_config))] : []),
    );
    renderFiles();
  }

  fileFilter.addEventListener('input', renderFiles);
  byId(root, 'reload-all', HTMLButtonElement).addEventListener('click', () => {
    void metafolder.invoke(['config:reload', 'all']);
  });

  metafolder.workspace.onChange('active_repo', () => void showFiles());
  metafolder.provide({ refresh: showFiles });
  void metafolder.commands.register('config:focus', {
    label: 'Config: focus the filter of the files',
    args: [{ name: 'zone', prompt: () => 'Which zone? (filter)', complete: () => ['filter'] }],
    handler: (_mf, /** @type {string} */ zone) => {
      if (zone !== 'filter') throw new Error(`unknown zone: "${zone ?? ''}" (expected filter)`);
      fileFilter.focus();
    },
  });
  // Shown again: a file added meanwhile is listed.
  metafolder.onVisibility((visible) => {
    if (visible) void showFiles();
  });

  await showFiles();
}
