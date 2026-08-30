<script lang="ts">
  import { tick, untrack } from 'svelte';
  import { commonPrefix, insertCandidate } from '../lib/bash';
  import { invoke } from '../lib/ipc';
  import {
    dispatch,
    filterCommands,
    filterCompletions,
    promptsForInput,
    resolvePromptValue,
    resolveSubmission,
    setEditingTarget,
    shortcutsFor,
  } from '../lib/commands';
  import { focusedWs, store } from '../lib/store.svelte';
  import { attachHistory } from '../../../panel-shim/history.js';

  let element = $state<HTMLInputElement | null>(null);
  let containerEl = $state<HTMLDivElement | null>(null);
  let draft = $state('');
  let currentWs = $state<string | null>(null);
  let focused = $state(false);
  /** Highlighted suggestion; the first match is selected by default, so
   *  Tab always has a target. Up/Down move it. */
  let selectedIndex = $state(0);
  /** `command` (`:` prompt, registry autocomplete) or `bash` (`!` prompt,
   *  the line runs as a shell command, Tab asks bash for completions). */
  let mode = $state<'command' | 'bash'>('command');
  /** Bash completion state, valid until the next edit: the candidates
   *  offered by the last Tab, the word they replace and its end position. */
  let bashCandidates = $state<string[]>([]);
  let bashWord = $state('');
  let bashPoint = $state(0);

  function draftsOf(which: 'command' | 'bash') {
    return which === 'bash' ? store.bashDrafts : store.inputDrafts;
  }

  // Per-repo input history (spec-gui "Input history"): ctrl-p/ctrl-n walk,
  // ctrl-r OSM search. The zone follows the mode (`:` vs `!` are separate
  // histories) and turns off while a script prompt is active. The store is
  // GUI-side (the history_read/history_append Tauri commands — the panel
  // equivalent is `metafolder.history.*`).
  let history: { push: (text: string) => void; detach: () => void } | null = null;
  $effect(() => {
    if (!element || !containerEl) return;
    const attached = attachHistory(element, {
      zone: () =>
        store.ui.promptText !== null ? null : mode === 'bash' ? 'shell:bash' : 'shell:command',
      read: (repo: string, zone: string) => invoke('history_read', { repo, zone }),
      append: (repo: string, zone: string, entry: string) =>
        invoke('history_append', { repo, zone, entry }),
      getRepo: async () => {
        if (currentWs === null) return null;
        const value = await invoke('ws_get_var', { wsId: currentWs, key: 'active_repo' });
        return typeof value === 'string' ? value : null;
      },
      container: containerEl,
    });
    history = attached;
    return () => {
      attached.detach();
      history = null;
    };
  });

  /** Moves the cursor once the DOM input has caught up with the draft. */
  function setCursorSoon(position: number) {
    void tick().then(() => element?.setSelectionRange(position, position));
  }

  // The draft is per-workspace and per-mode (spec-gui "Command input"):
  // switching the focused slot to another workspace restores that
  // workspace's draft for the current mode.
  $effect(() => {
    const ws = focusedWs();
    if (ws !== currentWs) {
      if (currentWs !== null) draftsOf(mode)[currentWs] = draft;
      draft = (ws !== null && draftsOf(mode)[ws]) || '';
      currentWs = ws;
      bashCandidates = [];
    }
  });

  // Pick up drafts injected by commands (e.g. bare `workspace:rename`). Suspended
  // while a prompt owns the input, so an interactive argument's pre-filled
  // `initial` value is not clobbered by a stale workspace draft.
  $effect(() => {
    const ws = currentWs;
    if (store.ui.promptText !== null) return;
    if (ws !== null && store.inputDrafts[ws] !== undefined && !focused && mode === 'command') {
      draft = store.inputDrafts[ws];
    }
  });

  /** command-input:activate / bash-input:activate: focus the always-visible
   *  input in the given mode, swapping the per-mode drafts. */
  function activate(target: 'command' | 'bash') {
    if (mode !== target) {
      if (currentWs !== null) draftsOf(mode)[currentWs] = draft;
      mode = target;
      draft = (currentWs !== null && draftsOf(target)[currentWs]) || '';
      bashCandidates = [];
      selectedIndex = 0;
    }
    // A frontend prompt (interactive argument collection) pre-fills its
    // editable `initial` value, overriding the restored draft.
    if (store.ui.promptText !== null && store.ui.promptResolver !== null) {
      draft = store.ui.promptInitial;
      selectedIndex = 0;
    }
    element?.focus();
    setCursorSoon(draft.length);
  }

  // Only the ticks are tracked: reading element/draft without untrack would
  // re-run these (and steal the focus) on every draft swap, e.g. when
  // panel:focus-next changes the focused workspace.
  $effect(() => {
    if (store.ui.commandInputFocusTick === 0) return;
    untrack(() => activate('command'));
  });
  $effect(() => {
    if (store.ui.bashInputFocusTick === 0) return;
    untrack(() => activate('bash'));
  });

  // While a script prompt is active, the list offers the prompt's
  // completions (values, not commands) instead of the command registry.
  // The filter is ordered-substring, so spaces in the draft are term
  // separators ("con def" matches like .*con.*def.*), not an argument boundary.
  // In bash mode the list holds the candidates of the last Tab completion.
  const matches = $derived(
    !focused
      ? []
      : store.ui.promptText !== null
        ? filterCompletions(store.ui.promptCompletions, draft).map((name) => ({
            name,
            label: '',
          }))
        : mode === 'bash'
          ? bashCandidates.map((name) => ({ name, label: '' }))
          : filterCommands(store.commands, draft),
  );
  // The best-ranked matches are listed (filterCompletions caps prompt
  // candidates so a huge set stays cheap to render); the CSS max-height makes
  // the list scroll, and typing narrows it toward the wanted value.
  const suggestions = $derived(matches);

  // The suggestion list holds command names only when the command input is in
  // its plain command mode (not a script/argument prompt, not bash completion).
  // The "…" prompt marker is meaningful only there.
  const listingCommands = $derived(store.ui.promptText === null && mode === 'command');

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
    // In a prompt, index -1 is the minibuffer itself (no completion
    // highlighted): Up past the top deselects so Enter submits the typed
    // free value. Command/bash mode keep a highlight always (spec-gui).
    const min = store.ui.promptText !== null ? -1 : 0;
    const span = suggestions.length - min;
    selectedIndex = ((((selectedIndex - min + delta) % span) + span) % span) + min;
    scrollSelectionIntoView();
  }

  /** Home/End: jump to the first/last suggestion. */
  function selectEdge(index: number) {
    selectedIndex = index;
    scrollSelectionIntoView();
  }

  /** Writes the suggestion into the input (does not execute it). A prompt
   *  completion is a final value: no trailing space. A bash candidate
   *  replaces only the completed word. */
  function acceptSuggestion(name: string) {
    if (store.ui.promptText === null && mode === 'bash') {
      const insertion = insertCandidate(draft, bashPoint, bashWord, name, true);
      draft = insertion.text;
      bashCandidates = [];
      selectedIndex = 0;
      element?.focus();
      setCursorSoon(insertion.cursor);
      return;
    }
    draft = store.ui.promptText !== null ? name : name + ' ';
    selectedIndex = 0;
    element?.focus();
  }

  /** Tab writes the selected suggestion into the input. */
  function completeTab() {
    if (suggestions.length === 0) return;
    acceptSuggestion(suggestions[Math.min(selectedIndex, suggestions.length - 1)].name);
  }

  /** Tab in bash mode: ask bash to complete the word before the cursor.
   *  One candidate is inserted directly; several first extend the word to
   *  their common prefix (like bash) and are listed for Up/Down + Tab. */
  async function completeBash() {
    if (bashCandidates.length > 0) {
      completeTab();
      return;
    }
    const cursor = element?.selectionStart ?? draft.length;
    let completion: { word: string; candidates: string[] };
    try {
      completion = await invoke<{ word: string; candidates: string[] }>('bash_complete', {
        line: draft.slice(0, cursor),
      });
    } catch (error) {
      const ws = focusedWs();
      if (ws) await invoke('post_status', { wsId: ws, text: String(error), kind: 'error', timeoutMs: 5000 });
      return;
    }
    if (completion.candidates.length === 0) return;
    if (completion.candidates.length === 1) {
      const insertion = insertCandidate(draft, cursor, completion.word, completion.candidates[0], true);
      draft = insertion.text;
      setCursorSoon(insertion.cursor);
      return;
    }
    let word = completion.word;
    let point = cursor;
    const prefix = commonPrefix(completion.candidates);
    if (prefix.length > word.length) {
      const insertion = insertCandidate(draft, cursor, word, prefix, false);
      draft = insertion.text;
      word = prefix;
      point = insertion.cursor;
      setCursorSoon(insertion.cursor);
    }
    bashWord = word;
    bashPoint = point;
    bashCandidates = completion.candidates;
    selectedIndex = 0;
  }

  /** Resolves the active prompt and clears its state. A frontend argument
   *  collection (promptResolver) is resolved locally; a script prompt goes to
   *  Rust (prompt_resolve). Cancel → the collector aborts / the script sees
   *  "cancel". */
  function resolvePrompt(confirm: boolean, text: string | null) {
    const resolver = store.ui.promptResolver;
    store.ui.promptText = null;
    store.ui.promptCompletions = [];
    store.ui.promptInitial = '';
    store.ui.promptResolver = null;
    if (resolver) resolver(confirm ? (text ?? '') : null);
    else void invoke('prompt_resolve', { confirm, text: confirm ? text : null });
  }

  function cancelPrompt() {
    if (store.ui.promptText === null) return;
    resolvePrompt(false, null);
  }

  function unfocus() {
    cancelPrompt();
    element?.blur();
  }

  /** editing:discard — clear the draft entirely, then leave the input. */
  function discard() {
    selectedIndex = 0;
    draft = '';
    bashCandidates = [];
    if (currentWs !== null) draftsOf(mode)[currentWs] = '';
    cancelPrompt();
    element?.blur();
  }

  // Enter runs the highlighted suggestion when the list is non-empty,
  // otherwise the typed text (resolveSubmission). Snapshot before clearing
  // the draft, since `suggestions` recomputes from it. In a prompt, plain
  // Enter accepts the highlighted completion while `raw` (Ctrl-Enter, or a
  // deselected list) confirms the typed free value.
  async function submit(raw = false) {
    const input = draft;
    const picked =
      store.ui.promptText === null ? resolveSubmission(input, suggestions, selectedIndex) : input;
    const promptValue = resolvePromptValue(input, suggestions, selectedIndex, raw);
    draft = '';
    bashCandidates = [];
    const ws = currentWs;
    if (ws !== null) draftsOf(mode)[ws] = '';
    if (store.ui.promptText !== null) {
      // Prompt (script POST /gui/prompt or interactive argument collection):
      // confirm with the resolved value (highlighted completion or, on
      // Ctrl-Enter / a deselected list, the typed text). The resolver routes
      // it to Rust or to the frontend collector, which then prompts for the
      // next argument (if any).
      element?.blur();
      resolvePrompt(true, promptValue);
      return;
    }
    element?.blur();
    if (mode === 'bash') {
      // The bash line runs through the same dispatcher as a `!` invocation
      // (%-placeholder expansion, message-panel switch, run_shell). Enter
      // always runs the typed line, as in bash — candidates insert with Tab.
      history?.push(input);
      await dispatch('!' + input);
      return;
    }
    // The history records what actually ran (the resolved suggestion), not
    // the abbreviation that was typed.
    history?.push(picked);
    await dispatch(picked);
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      // Ctrl/Cmd-Enter confirms the typed free value even when a completion is
      // highlighted (spec-gui "Interactive command arguments").
      void submit(event.ctrlKey || event.metaKey);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      unfocus();
    } else if (event.key === 'Tab') {
      event.preventDefault();
      if (mode === 'bash' && store.ui.promptText === null) void completeBash();
      else completeTab();
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
    bashCandidates = [];
    setEditingTarget(null);
  }

  /** User edits invalidate the last Tab's candidates; a leading `!` in
   *  command mode switches to bash mode (continuity with the old
   *  `!command` syntax of the single input). A bare `!` restores the
   *  workspace's bash draft — like bash-input:activate — while a pasted
   *  `!command` keeps the pasted line. */
  function onInput() {
    bashCandidates = [];
    if (mode === 'command' && store.ui.promptText === null && draft.startsWith('!')) {
      const rest = draft.slice(1);
      mode = 'bash';
      draft = rest !== '' ? rest : (currentWs !== null && store.bashDrafts[currentWs]) || '';
      selectedIndex = 0;
      setCursorSoon(draft.length);
    }
  }
</script>

<div class="command-input" class:focused data-help-topic="command-input" bind:this={containerEl}>
  {#if suggestions.length > 0}
    <ul class="suggestions">
      {#each suggestions as suggestion, index (suggestion.name)}
        <li class:selected={index === selectedIndex}>
          <button onmousedown={(e) => e.preventDefault()} onclick={() => acceptSuggestion(suggestion.name)}>
            <span class="name"
              >{suggestion.name}{#if listingCommands && promptsForInput(suggestion.name)}<span
                  class="prompts"
                  title="Reopens the input to collect a value">…</span
                >{/if}</span
            >
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
    <span class="prompt">{store.ui.promptText ?? (mode === 'bash' ? '!' : ':')}</span>
    <input
      bind:this={element}
      bind:value={draft}
      onkeydown={onKeydown}
      oninput={onInput}
      onfocus={onFocus}
      onblur={onBlur}
      placeholder={focused ? '' : mode === 'bash' ? 'shell — ! to focus, : for commands' : 'command — : to focus, ! for shell'}
      spellcheck="false"
      autocomplete="off"
    />
    {#if store.ui.promptText !== null && suggestions.length > 0}
      <span class="hint" title="Enter picks the highlighted value; Ctrl-Enter keeps what you typed">⏎ pick · ⌃⏎ new</span>
    {/if}
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
  .hint {
    flex: none;
    color: var(--mf-fg-dim, #8a8a96);
    font-family: var(--mf-font-mono, monospace);
    font-size: 0.8em;
    opacity: 0.7;
    white-space: nowrap;
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
  /* The "…" marks a command that reopens the input to collect a value
     (menu-item ellipsis convention); dimmed so it reads as a hint, not a
     character of the command name. */
  .suggestions .name .prompts {
    color: var(--mf-fg-dim, #8a8a96);
    font-weight: bold;
  }
  .suggestions li.selected .name .prompts {
    color: rgba(255, 255, 255, 0.7);
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
