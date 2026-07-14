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
  void metafolder.workspace.get('active_repo').then((active) => {
    repo.textContent = `active_repo: ${active ?? 'none'}`;
  });
  metafolder.workspace.onChange('active_repo', (value) => {
    repo.textContent = `active_repo: ${value ?? 'none'}`;
  });

  metafolder.commands.register('hello:greet', {
    label: 'Hello: greet in the status bar',
    handler: () => metafolder.statusBar.message('Hello!', 3000),
  });
}
