<script lang="ts">
  import { invoke } from '../lib/ipc';
  import { dispatch, filterCommands, setEditingTarget } from '../lib/commands';
  import { focusedWs, store } from '../lib/store.svelte';

  let element = $state<HTMLInputElement | null>(null);
  let draft = $state('');
  let currentWs = $state<string | null>(null);

  // The draft is per-workspace (spec-gui "Command input"): switching the
  // focused slot to another workspace restores that workspace's draft.
  $effect(() => {
    const ws = focusedWs();
    if (ws !== currentWs) {
      if (currentWs !== null) store.inputDrafts[currentWs] = draft;
      draft = (ws !== null && store.inputDrafts[ws]) || '';
      currentWs = ws;
    }
  });

  // Pick up drafts injected by commands (e.g. bare `tab:rename`).
  $effect(() => {
    const ws = currentWs;
    if (ws !== null && store.inputDrafts[ws] !== undefined && !store.ui.commandInputActive) {
      draft = store.inputDrafts[ws];
    }
  });

  $effect(() => {
    // The tick re-triggers focusing when ':' is pressed while the input
    // is already open but unfocused.
    void store.ui.commandInputFocusTick;
    if (store.ui.commandInputActive && element) {
      element.focus();
      element.setSelectionRange(draft.length, draft.length);
    }
  });

  const suggestions = $derived(
    store.ui.promptText !== null || draft.startsWith('!') || draft.includes(' ')
      ? []
      : filterCommands(store.commands, draft).slice(0, 8),
  );

  function close() {
    if (store.ui.promptText !== null) {
      // A script prompt dismissed with Escape resolves as "cancel".
      void invoke('prompt_resolve', { confirm: false, text: null });
      store.ui.promptText = null;
    }
    store.ui.commandInputActive = false;
    element?.blur();
  }

  async function submit() {
    const input = draft;
    draft = '';
    const ws = currentWs;
    if (ws !== null) store.inputDrafts[ws] = '';
    if (store.ui.promptText !== null) {
      // Script prompt (POST /gui/prompt): confirm with the typed text.
      store.ui.promptText = null;
      store.ui.commandInputActive = false;
      element?.blur();
      await invoke('prompt_resolve', { confirm: true, text: input });
      return;
    }
    close();
    await dispatch(input);
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      void submit();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      close();
    } else if (event.key === 'Tab') {
      event.preventDefault();
      if (suggestions.length > 0) draft = suggestions[0].name + ' ';
    }
  }

  function onFocus() {
    setEditingTarget({
      confirm: () => void submit(),
      unfocus: close,
      lineStart: () => element?.setSelectionRange(0, 0),
      lineEnd: () => element?.setSelectionRange(draft.length, draft.length),
    });
  }

  function onBlur() {
    setEditingTarget(null);
  }
</script>

{#if store.ui.commandInputActive}
  <div class="command-input">
    {#if suggestions.length > 0}
      <ul class="suggestions">
        {#each suggestions as suggestion (suggestion.name)}
          <li>
            <button
              onclick={() => {
                draft = suggestion.name + ' ';
                element?.focus();
              }}
            >
              <span class="name">{suggestion.name}</span>
              <span class="label">{suggestion.label}</span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}
    <div class="line">
      <span class="prompt">{store.ui.promptText ?? ':'}</span>
      <input
        bind:this={element}
        bind:value={draft}
        onkeydown={onKeydown}
        onfocus={onFocus}
        onblur={onBlur}
        spellcheck="false"
        autocomplete="off"
      />
    </div>
  </div>
{/if}

<style>
  .command-input {
    flex: none;
    background: var(--mf-bg-raised, #26262e);
    border-top: 1px solid var(--mf-fg-dim, #8a8a96);
  }
  .line {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 3px 8px;
  }
  .prompt {
    color: var(--mf-accent, #4c56c4);
    font-family: var(--mf-font-mono, monospace);
  }
  input {
    flex: 1;
    background: transparent;
    border: none;
    outline: none;
    color: var(--mf-fg, #d8d8e0);
    font-family: var(--mf-font-mono, monospace);
  }
  .suggestions {
    list-style: none;
    margin: 0;
    padding: 2px 0;
    max-height: 14em;
    overflow-y: auto;
    border-bottom: 1px solid var(--mf-bg, #1e1e24);
  }
  .suggestions button {
    display: flex;
    gap: 12px;
    width: 100%;
    border: none;
    background: transparent;
    color: var(--mf-fg, #d8d8e0);
    font: inherit;
    padding: 2px 10px;
    cursor: pointer;
    text-align: left;
  }
  .suggestions button:hover {
    background: var(--mf-bg, #1e1e24);
  }
  .suggestions .name {
    font-family: var(--mf-font-mono, monospace);
  }
  .suggestions .label {
    color: var(--mf-fg-dim, #8a8a96);
  }
</style>
