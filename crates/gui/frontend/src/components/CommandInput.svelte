<script lang="ts">
  import { untrack } from 'svelte';
  import { invoke } from '../lib/ipc';
  import {
    dispatch,
    filterCommands,
    filterCompletions,
    resolveSubmission,
    setEditingTarget,
    shortcutsFor,
  } from '../lib/commands';
  import { focusedWs, store } from '../lib/store.svelte';

  let element = $state<HTMLInputElement | null>(null);
  let draft = $state('');
  let currentWs = $state<string | null>(null);
  let focused = $state(false);
  /** Highlighted suggestion; the first match is selected by default, so
   *  Tab always has a target. Up/Down move it. */
  let selectedIndex = $state(0);

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

  // While a script prompt is active, the list offers the prompt's
  // completions (values, not commands) instead of the command registry.
  // The filter is ordered-substring, so spaces in the draft are term
  // separators ("con def" matches like .*con.*def.*), not an argument boundary.
  const matches = $derived(
    !focused
      ? []
      : store.ui.promptText !== null
        ? filterCompletions(store.ui.promptCompletions, draft).map((name) => ({
            name,
            label: '',
          }))
        : draft.startsWith('!')
          ? []
          : filterCommands(store.commands, draft),
  );
  // Every match is listed; the CSS max-height makes the list scroll, so
  // arrow navigation travels through all possible completions.
  const suggestions = $derived(matches);

  // Typing returns the selection to the best (first) match.
  $effect(() => {
    void draft;
    selectedIndex = 0;
  });

  function scrollSelectionIntoView() {
    requestAnimationFrame(() =>
      document.querySelector('.suggestions .selected')?.scrollIntoView({ block: 'nearest' }),
    );
  }

  function moveSelection(delta: number) {
    if (suggestions.length === 0) return;
    selectedIndex = (selectedIndex + delta + suggestions.length) % suggestions.length;
    scrollSelectionIntoView();
  }

  /** Home/End: jump to the first/last suggestion. */
  function selectEdge(index: number) {
    selectedIndex = index;
    scrollSelectionIntoView();
  }

  /** Writes the suggestion into the input (does not execute it). A prompt
   *  completion is a final value: no trailing space. */
  function acceptSuggestion(name: string) {
    draft = store.ui.promptText !== null ? name : name + ' ';
    selectedIndex = 0;
    element?.focus();
  }

  /** Tab writes the selected suggestion into the input. */
  function completeTab() {
    if (suggestions.length === 0) return;
    acceptSuggestion(suggestions[Math.min(selectedIndex, suggestions.length - 1)].name);
  }

  function cancelPrompt() {
    if (store.ui.promptText === null) return;
    // A dismissed script prompt resolves as "cancel".
    void invoke('prompt_resolve', { confirm: false, text: null });
    store.ui.promptText = null;
    store.ui.promptCompletions = [];
  }

  function unfocus() {
    cancelPrompt();
    element?.blur();
  }

  /** editing:discard — clear the draft entirely, then leave the input. */
  function discard() {
    selectedIndex = 0;
    draft = '';
    if (currentWs !== null) store.inputDrafts[currentWs] = '';
    cancelPrompt();
    element?.blur();
  }

  // Enter runs the highlighted suggestion when the list is non-empty,
  // otherwise the typed text (resolveSubmission). Snapshot before clearing
  // the draft, since `suggestions` recomputes from it. The script-prompt
  // path always confirms with the raw text.
  async function submit() {
    const input = draft;
    const picked =
      store.ui.promptText === null ? resolveSubmission(input, suggestions, selectedIndex) : input;
    draft = '';
    const ws = currentWs;
    if (ws !== null) store.inputDrafts[ws] = '';
    if (store.ui.promptText !== null) {
      // Script prompt (POST /gui/prompt): confirm with the typed text.
      store.ui.promptText = null;
      store.ui.promptCompletions = [];
      element?.blur();
      await invoke('prompt_resolve', { confirm: true, text: input });
      return;
    }
    element?.blur();
    await dispatch(picked);
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
    } else if (event.key === 'Home' && suggestions.length > 0) {
      event.preventDefault();
      selectEdge(0);
    } else if (event.key === 'End' && suggestions.length > 0) {
      event.preventDefault();
      selectEdge(suggestions.length - 1);
    }
  }

  function onFocus() {
    focused = true;
    setEditingTarget({
      confirm: () => void submit(),
      unfocus,
      discard,
      lineStart: () => element?.setSelectionRange(0, 0),
      lineEnd: () => element?.setSelectionRange(draft.length, draft.length),
    });
  }

  function onBlur() {
    focused = false;
    setEditingTarget(null);
  }
</script>

<div class="command-input" class:focused data-help-topic="command-input">
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
  /* Keycap badge so the bound shortcut is easy to spot in the list. */
  .suggestions .shortcut {
    margin-left: auto;
    font-family: var(--mf-font-mono, monospace);
    font-size: 0.85em;
    padding: 0 6px;
    border: 1px solid var(--mf-fg-dim, #8a8a96);
    border-radius: 4px;
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-accent, #4c56c4);
    white-space: nowrap;
  }
  .suggestions li.selected .shortcut {
    color: #fff;
    border-color: rgba(255, 255, 255, 0.6);
    background: transparent;
  }
</style>
