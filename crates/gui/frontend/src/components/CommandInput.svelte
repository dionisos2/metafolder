<script lang="ts">
  import { untrack } from 'svelte';
  import { invoke } from '../lib/ipc';
  import {
    commonPrefix,
    dispatch,
    filterCommands,
    setEditingTarget,
    shortcutsFor,
  } from '../lib/commands';
  import { focusedWs, store } from '../lib/store.svelte';

  let element = $state<HTMLInputElement | null>(null);
  let draft = $state('');
  let currentWs = $state<string | null>(null);
  let focused = $state(false);
  /** Highlighted suggestion while navigating with Up/Down; -1 = none. */
  let selectedIndex = $state(-1);

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
    if (ws !== null && store.inputDrafts[ws] !== undefined && !focused) {
      draft = store.inputDrafts[ws];
    }
  });

  // command-input:activate focuses the always-visible input. Only the
  // tick is tracked: reading element/draft without untrack would re-run
  // this (and steal the focus) on every draft swap, e.g. when
  // panel:focus-next changes the focused workspace.
  $effect(() => {
    if (store.ui.commandInputFocusTick === 0) return;
    untrack(() => {
      element?.focus();
      element?.setSelectionRange(draft.length, draft.length);
    });
  });

  const matches = $derived(
    !focused || store.ui.promptText !== null || draft.startsWith('!') || draft.includes(' ')
      ? []
      : filterCommands(store.commands, draft),
  );
  // Every match is listed; the CSS max-height makes the list scroll, so
  // arrow navigation travels through all possible completions.
  const suggestions = $derived(matches);

  // Typing leaves list navigation (the list itself just refilters).
  $effect(() => {
    void draft;
    selectedIndex = -1;
  });

  function moveSelection(delta: number) {
    if (suggestions.length === 0) return;
    selectedIndex =
      selectedIndex < 0 && delta < 0
        ? suggestions.length - 1
        : (selectedIndex + delta + suggestions.length) % suggestions.length;
    requestAnimationFrame(() =>
      document.querySelector('.suggestions .selected')?.scrollIntoView({ block: 'nearest' }),
    );
  }

  /** Writes the suggestion into the input (does not execute it). */
  function acceptSuggestion(name: string) {
    draft = name + ' ';
    selectedIndex = -1;
    element?.focus();
  }

  /** Shell-style Tab: one match completes it, several complete the
   *  longest common prefix. */
  function completeTab() {
    if (selectedIndex >= 0) {
      acceptSuggestion(suggestions[selectedIndex].name);
      return;
    }
    if (matches.length === 1) {
      acceptSuggestion(matches[0].name);
      return;
    }
    const prefix = commonPrefix(matches.map((m) => m.name));
    if (prefix.startsWith(draft) && prefix.length > draft.length) draft = prefix;
  }

  function unfocus() {
    // First Escape leaves list navigation, the next one the input.
    if (selectedIndex >= 0) {
      selectedIndex = -1;
      return;
    }
    if (store.ui.promptText !== null) {
      // A script prompt dismissed with Escape resolves as "cancel".
      void invoke('prompt_resolve', { confirm: false, text: null });
      store.ui.promptText = null;
    }
    element?.blur();
  }

  async function submit() {
    // Enter while navigating writes the suggestion, it does not execute.
    if (selectedIndex >= 0) {
      acceptSuggestion(suggestions[selectedIndex].name);
      return;
    }
    const input = draft;
    draft = '';
    const ws = currentWs;
    if (ws !== null) store.inputDrafts[ws] = '';
    if (store.ui.promptText !== null) {
      // Script prompt (POST /gui/prompt): confirm with the typed text.
      store.ui.promptText = null;
      element?.blur();
      await invoke('prompt_resolve', { confirm: true, text: input });
      return;
    }
    element?.blur();
    await dispatch(input);
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      void submit();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      unfocus();
    } else if (event.key === 'Tab') {
      event.preventDefault();
      completeTab();
    } else if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveSelection(1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveSelection(-1);
    }
  }

  function onFocus() {
    focused = true;
    setEditingTarget({
      confirm: () => void submit(),
      unfocus,
      lineStart: () => element?.setSelectionRange(0, 0),
      lineEnd: () => element?.setSelectionRange(draft.length, draft.length),
    });
  }

  function onBlur() {
    focused = false;
    setEditingTarget(null);
  }
</script>

<div class="command-input" class:focused>
  {#if suggestions.length > 0}
    <ul class="suggestions">
      {#each suggestions as suggestion, index (suggestion.name)}
        <li class:selected={index === selectedIndex}>
          <button onmousedown={(e) => e.preventDefault()} onclick={() => acceptSuggestion(suggestion.name)}>
            <span class="name">{suggestion.name}</span>
            <span class="label">{suggestion.label}</span>
            {#if shortcutsFor(store.keytable, suggestion.name).length > 0}
              <span class="shortcut">{shortcutsFor(store.keytable, suggestion.name).join(', ')}</span>
            {/if}
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
      placeholder={focused ? '' : 'command — press : to focus'}
      spellcheck="false"
      autocomplete="off"
    />
  </div>
</div>

<style>
  .command-input {
    flex: none;
    background: var(--mf-bg-raised, #26262e);
    border-top: 1px solid var(--mf-bg, #1e1e24);
  }
  .command-input.focused {
    border-top-color: var(--mf-accent, #4c56c4);
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
  input::placeholder {
    color: var(--mf-fg-dim, #8a8a96);
    opacity: 0.6;
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
  .suggestions li.selected button {
    background: var(--mf-accent, #4c56c4);
    color: #fff;
  }
  .suggestions li.selected .label {
    color: rgba(255, 255, 255, 0.7);
  }
  .suggestions .name {
    font-family: var(--mf-font-mono, monospace);
  }
  .suggestions .label {
    color: var(--mf-fg-dim, #8a8a96);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .suggestions .shortcut {
    margin-left: auto;
    font-family: var(--mf-font-mono, monospace);
    color: var(--mf-accent, #4c56c4);
    white-space: nowrap;
  }
  .suggestions li.selected .shortcut {
    color: rgba(255, 255, 255, 0.85);
  }
</style>
