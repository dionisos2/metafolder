<script lang="ts">
  // The two bars above the command line: the continuations of a key sequence
  // being typed, which floats over the bottom of the panels for the instant it
  // lasts, and a running script's question, which takes its own place and stays
  // once shown, until the user hides it (doc "Status bar and task bar"). App
  // mounts this twice, one bar each (`keysOnly`, `questionOnly`).
  import { onMount } from 'svelte';
  import { activeQuestion, store } from '../lib/store.svelte';
  import { dispatch } from '../lib/commands';
  import { bars } from '../lib/bars';

  const { keysOnly = false, questionOnly = false }: { keysOnly?: boolean; questionOnly?: boolean } =
    $props();
  // A script's question belongs to the workspace it is asked in: switching to
  // another tab shows that tab's question, or none, instead of leaving it on
  // screen over unrelated panels (doc "Script sessions"). A wait owned by
  // nobody is always shown.
  const question = $derived(activeQuestion());

  // Shown from the first question on, kept once answered, hidden by the user,
  // shown again by every new question (lib/bars).
  let shownBar = $state(bars.shown('question'));
  onMount(() => bars.subscribe((state) => (shownBar = state.question)));
  $effect(() => {
    if (!keysOnly) bars.note('question', question ? [String(question.id)] : []);
  });

  // Pending key sequence: one entry per continuation, sorted by keys. A
  // continuation bound to the same command in two scopes (a global binding and
  // a panel's) is one entry: they say the same thing.
  const keyHints = $derived.by(() => {
    const pending = store.ui.pendingKeys;
    if (!pending) return null;
    const hints = pending.candidates
      .map((c) => ({
        keys: c.keys.slice(pending.prefix.length).join(' '),
        invocation: c.invocation,
      }))
      .filter(
        (hint, index, all) =>
          all.findIndex((o) => o.keys === hint.keys && o.invocation === hint.invocation) === index,
      );
    hints.sort((a, b) => a.keys.localeCompare(b.keys));
    return { prefix: pending.prefix.join(' '), hints };
  });
</script>

{#if keyHints && !questionOnly}
  <div class="key-hints" data-help-topic="keybindings">
    <span class="prefix">{keyHints.prefix}</span>
    {#each keyHints.hints as hint (`${hint.keys}\n${hint.invocation}`)}
      <span class="hint"><span class="keys">{hint.keys}</span> {hint.invocation}</span>
    {/each}
    <span class="hint dim">escape cancels</span>
  </div>
{/if}

<!-- A running script's question (POST /gui/input) lives here, not on the
     status line, so an error message can never hide it. -->
{#if shownBar && !keysOnly}
  <div class="input-wait" data-help-topic="scripting">
    {#if !question}
      <span class="hint dim">no question asked</span>
    {:else}
    <span class="question">{question.prompt}</span>
    {#each question.keys as key (key)}
      <span class="keycap" class:off={!store.ui.scriptKeys}>{key}</span>
    {/each}
    <!-- The script's keys are borrowed from the panels: this is where they are
         given back, without abandoning the question (doc "Script sessions"). -->
    <label class="script-keys" title="Let the script's keys take priority over the GUI shortcuts">
      <input
        type="checkbox"
        checked={store.ui.scriptKeys}
        onchange={(event) => {
          // A focused checkbox IS a text input as far as key handling goes, and
          // would swallow every shortcut — including the script's own keys —
          // until the user clicked elsewhere. Hand the focus straight back.
          (event.currentTarget as HTMLElement).blur();
          void dispatch('script-keys:toggle');
        }}
      />
      script keys
    </label>
    <span class="hint dim">escape gives the keys back, shift+escape stops the script, ctrl+escape kills it</span>
    {/if}
    <button
      class="hide"
      class:alone={!question}
      title="Hide the question bar (bar:toggle question)"
      onclick={(event) => {
        // Hand the focus straight back, or the button would keep the keys.
        (event.currentTarget as HTMLElement).blur();
        void dispatch('bar:toggle question');
      }}>×</button
    >
  </div>
{/if}

<style>
  .key-hints {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 16px;
    padding: 6px 10px;
    background: var(--mf-bg-raised, #26262e);
    border-top: 2px solid var(--mf-accent, #4c56c4);
    font-size: 1em;
  }
  .key-hints .hint {
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }
  /* Keycap badges make the pending prefix and its continuations stand out. */
  .key-hints .prefix,
  .key-hints .keys {
    font-family: var(--mf-font-mono, monospace);
    font-weight: 600;
    padding: 1px 7px;
    border-radius: 4px;
    border: 1px solid var(--mf-accent, #4c56c4);
  }
  .key-hints .prefix {
    background: var(--mf-accent, #4c56c4);
    color: #fff;
  }
  .key-hints .keys {
    color: var(--mf-accent, #4c56c4);
    background: var(--mf-bg, #1e1e24);
  }
  .key-hints .hint.dim {
    margin-left: auto;
    color: var(--mf-fg-dim, #8a8a96);
  }
  /* A script's input question: a dedicated, persistent bar, visually distinct
     from the status/error line so the two never compete for the same slot. */
  .input-wait {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
    padding: 6px 10px;
    background: var(--mf-bg-raised, #26262e);
    border-top: 2px solid var(--mf-accent, #4c56c4);
    font-size: 1em;
  }
  .input-wait .question {
    font-weight: 600;
  }
  .input-wait .keycap {
    font-family: var(--mf-font-mono, monospace);
    font-weight: 600;
    padding: 1px 7px;
    border-radius: 4px;
    border: 1px solid var(--mf-accent, #4c56c4);
    color: var(--mf-accent, #4c56c4);
    background: var(--mf-bg, #1e1e24);
  }
  /* Keys handed back to the panels: shown, but plainly not live. */
  .input-wait .keycap.off {
    border-color: var(--mf-fg-dim, #8a8a96);
    color: var(--mf-fg-dim, #8a8a96);
    text-decoration: line-through;
  }
  .input-wait .script-keys {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    margin-left: auto;
    color: var(--mf-fg-dim, #8a8a96);
    font-size: 0.9em;
    cursor: pointer;
  }
  .input-wait .hide {
    padding: 0 4px;
    border: none;
    background: none;
    color: var(--mf-fg-dim, #8a8a96);
    font: inherit;
    cursor: pointer;
  }
  .input-wait .hide.alone {
    margin-left: auto;
  }
  .input-wait .hide:hover {
    color: var(--mf-fg, #d8d8e0);
  }
  .input-wait .hint.dim {
    color: var(--mf-fg-dim, #8a8a96);
    font-size: 0.9em;
  }
</style>
