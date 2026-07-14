// Example panel type. Copy this directory under
// ~/.config/metafolder/gui/panel-types/<your-name>/ to start a custom panel.
// The shell calls `mount(root, metafolder)` after the panel's markup
// (index.html) is in `root` (a Shadow DOM root). Return an optional cleanup fn.

import { byId } from '/__ui.js';

/** @param {ShadowRoot} root @param {MetafolderApi} metafolder */
export function mount(root, metafolder) {
  const ws = byId(root, 'workspace');
  const repo = byId(root, 'repo');

  ws.textContent = `workspace: ${metafolder.workspaceId}`;
  // A workspace variable is `unknown`: say how it becomes text.
  /** @param {unknown} value */
  const show = (value) => {
    repo.textContent = `active_repo: ${typeof value === 'string' ? value : 'none'}`;
  };
  void metafolder.workspace.get('active_repo').then(show);
  metafolder.workspace.onChange('active_repo', show);

  void metafolder.commands.register('hello:greet', {
    label: 'Hello: greet in the status bar',
    handler: () => metafolder.statusBar.message('Hello!', 3000),
  });
}
