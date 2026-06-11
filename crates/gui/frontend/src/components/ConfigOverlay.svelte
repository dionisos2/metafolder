<script lang="ts">
  import { invoke } from '../lib/ipc';
  import { applyStyle, store } from '../lib/store.svelte';
  import type { ConfigInfo } from '../lib/types';

  $effect(() => {
    if (store.ui.configOpen && store.ui.configInfo === null) {
      void invoke<ConfigInfo>('config_info').then((info) => {
        store.ui.configInfo = info;
      });
    }
  });

  async function reloadStyle() {
    applyStyle(await invoke<string>('load_style'));
  }

  function close() {
    store.ui.configOpen = false;
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
<div class="backdrop" onclick={close}>
  <div class="dialog" onclick={(e) => e.stopPropagation()}>
    <header>
      <h2>Settings</h2>
      <button onclick={close}>✕</button>
    </header>
    {#if store.ui.configInfo}
      <dl>
        <dt>Daemon URL</dt>
        <dd><code>{store.daemonUrl}</code></dd>
        <dt>Config directory</dt>
        <dd><code>{store.ui.configInfo.root}</code></dd>
        <dt>Keybindings</dt>
        <dd><code>{store.ui.configInfo.keybindings}</code></dd>
        <dt>Stylesheet</dt>
        <dd>
          <code>{store.ui.configInfo.style_css}</code>
          <button onclick={reloadStyle}>Reload</button>
        </dd>
        <dt>Panel types</dt>
        <dd><code>{store.ui.configInfo.panel_types}</code></dd>
      </dl>
      <p class="hint">
        The stylesheet also reloads automatically when the file changes.
      </p>
    {/if}
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 50;
  }
  .dialog {
    background: var(--mf-bg-raised, #26262e);
    border: 1px solid var(--mf-fg-dim, #8a8a96);
    border-radius: 6px;
    padding: 12px 18px;
    min-width: 32em;
    max-width: 80vw;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  h2 {
    margin: 0;
    font-size: 1.1em;
  }
  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 6px 16px;
  }
  dt {
    color: var(--mf-fg-dim, #8a8a96);
  }
  dd {
    margin: 0;
    display: flex;
    gap: 8px;
    align-items: center;
  }
  code {
    font-family: var(--mf-font-mono, monospace);
    font-size: 0.9em;
  }
  .hint {
    color: var(--mf-fg-dim, #8a8a96);
    font-size: 0.85em;
  }
  button {
    font: inherit;
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-fg, #d8d8e0);
    border: 1px solid var(--mf-fg-dim, #8a8a96);
    border-radius: 3px;
    cursor: pointer;
  }
</style>
