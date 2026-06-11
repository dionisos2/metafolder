<script lang="ts">
  import { invoke } from '../lib/ipc';
  import { applyStyle, store } from '../lib/store.svelte';
  import type { Binding, ConfigInfo } from '../lib/types';

  let combo = $state('');
  let command = $state('');
  let when = $state('');
  let textInput = $state(false);
  let bindingError = $state('');

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

  function prefill(binding: Binding) {
    combo = binding.keys.join(' ');
    command = binding.invocation;
    when = binding.when ?? '';
    textInput = binding.text_input;
  }

  async function saveBinding() {
    bindingError = '';
    try {
      store.keytable = await invoke<Binding[]>('set_user_keybinding', {
        combo,
        command,
        when: when.trim() === '' ? null : when.trim(),
        textInput,
      });
      combo = '';
      command = '';
    } catch (error) {
      bindingError = String(error);
    }
  }

  async function resetBinding(binding: Binding) {
    bindingError = '';
    try {
      store.keytable = await invoke<Binding[]>('remove_user_keybinding', {
        combo: binding.keys.join(' '),
      });
    } catch (error) {
      bindingError = String(error);
    }
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

      <h3>Keybindings</h3>
      <div class="binding-form">
        <input placeholder="combo (e.g. ctrl+k or g g)" bind:value={combo} />
        <input placeholder="command (e.g. tab:new)" bind:value={command} />
        <input placeholder="when (panel type, empty = global)" bind:value={when} />
        <label><input type="checkbox" bind:checked={textInput} /> text-input</label>
        <button onclick={saveBinding} disabled={!combo.trim() || !command.trim()}>Save</button>
      </div>
      {#if bindingError}<p class="error">{bindingError}</p>{/if}
      <div class="binding-table">
        <table>
          <tbody>
            {#each store.keytable as binding (binding.keys.join(' ') + (binding.when ?? ''))}
              <tr>
                <td class="combo">{binding.keys.join(' ')}</td>
                <td>{binding.invocation}</td>
                <td class="scope">{binding.when ?? 'global'}{binding.text_input ? ' ⌨' : ''}</td>
                <td>
                  <button onclick={() => prefill(binding)}>Edit</button>
                  <button onclick={() => resetBinding(binding)}>Reset</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
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
  h3 {
    font-size: 1em;
    margin: 14px 0 6px;
  }
  .binding-form {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    align-items: center;
  }
  .binding-form input[type='text'],
  .binding-form input:not([type]) {
    font-family: var(--mf-font-mono, monospace);
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-fg, #d8d8e0);
    border: 1px solid var(--mf-fg-dim, #8a8a96);
    border-radius: 3px;
    padding: 2px 6px;
  }
  .binding-table {
    max-height: 14em;
    overflow-y: auto;
    margin-top: 8px;
    border: 1px solid var(--mf-bg, #1e1e24);
  }
  .binding-table table {
    width: 100%;
    border-collapse: collapse;
  }
  .binding-table td {
    padding: 2px 8px;
    font-size: 0.9em;
  }
  .combo {
    font-family: var(--mf-font-mono, monospace);
  }
  .scope {
    color: var(--mf-fg-dim, #8a8a96);
  }
  .error {
    color: var(--mf-error, #c44c56);
  }
</style>
