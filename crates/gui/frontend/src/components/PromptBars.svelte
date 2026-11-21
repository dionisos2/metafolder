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
  <!-- Two lines of a fixed height, asking or not: the panels above keep their
       size from one question to the next, whatever its length. A question
       longer than its two lines is cut, and given whole on hover; the hint
       gives way before it (doc "Status bar and task bar"). -->
  <div class="input-wait" data-help-topic="scripting">
    {#if !question}
      <span class="question dim">no question asked</span>
      <div class="side">
        <div class="line">{@render hide()}</div>
      </div>
    {:else}
      <span class="question" title={question.prompt}>{question.prompt}</span>
      <div class="side">
        <div class="line">
          {#each question.keys as key (key)}
            <span class="keycap" class:off={!store.ui.scriptKeys}>{key}</span>
          {/each}
          {@render hide()}
        </div>
        <div class="line">
          <span class="hint dim"
            >escape gives the keys back, shift+escape stops the script, ctrl+escape kills it</span
          >
          <!-- The script's keys are borrowed from the panels: this is where
               they are given back, without abandoning the question (doc
               "Script sessions"). -->
          <label
            class="script-keys"
            title="Let the script's keys take priority over the GUI shortcuts"
          >
            <input
              type="checkbox"
              checked={store.ui.scriptKeys}
              onchange={(event) => {
                // A focused checkbox IS a text input as far as key handling
                // goes, and would swallow every shortcut — including the
                // script's own keys — until the user clicked elsewhere. Hand
                // the focus straight back.
                (event.currentTarget as HTMLElement).blur();
                void dispatch('script-keys:toggle');
              }}
            />
            script keys
          </label>
        </div>
      </div>
    {/if}
  </div>
{/if}

{#snippet hide()}
  <button
    class="hide"
    title="Hide the question bar (bar:toggle question)"
    onclick={(event) => {
      // Hand the focus straight back, or the button would keep the keys.
      (event.currentTarget as HTMLElement).blur();
      void dispatch('bar:toggle question');
    }}>×</button
  >
{/snippet}

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
     from the status/error line so the two never compete for the same slot. Its
     height is two lines, never more nor less: the question on the left, cut
     after two lines; on the right the keys, then the hint and the checkbox —
     the hint is cut first when room runs out. */
  .input-wait {
    --line: 1.5em;
    box-sizing: border-box;
    height: calc(2 * var(--line) + 14px);
    display: grid;
    grid-template-columns: fit-content(60%) 1fr;
    align-items: center;
    gap: 0 16px;
    padding: 6px 10px;
    overflow: hidden;
    background: var(--mf-bg-raised, #26262e);
    border-top: 2px solid var(--mf-accent, #4c56c4);
    font-size: 1em;
    line-height: var(--line);
  }
  .input-wait .question {
    font-weight: 600;
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    overflow: hidden;
    overflow-wrap: anywhere;
  }
  .input-wait .question.dim {
    font-weight: normal;
    color: var(--mf-fg-dim, #8a8a96);
    font-size: 0.9em;
  }
  /* As wide as its keys at least: they are never cut. */
  .input-wait .side {
    display: flex;
    flex-direction: column;
  }
  .input-wait .line {
    height: var(--line);
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 10px;
    white-space: nowrap;
    overflow: hidden;
  }
  .input-wait .keycap {
    flex: none;
    font-family: var(--mf-font-mono, monospace);
    font-weight: 600;
    line-height: 1.2;
    padding: 0 7px;
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
    flex: none;
    display: inline-flex;
    align-items: center;
    gap: 5px;
    color: var(--mf-fg-dim, #8a8a96);
    font-size: 0.9em;
    cursor: pointer;
  }
  .input-wait .hide {
    flex: none;
    padding: 0 4px;
    border: none;
    background: none;
    color: var(--mf-fg-dim, #8a8a96);
    font: inherit;
    cursor: pointer;
  }
  .input-wait .hide:hover {
    color: var(--mf-fg, #d8d8e0);
  }
  /* Takes what the line leaves, and claims no width of its own (contain):
     the question and the keys are served first. */
  .input-wait .hint.dim {
    flex: 1 1 0;
    min-width: 0;
    contain: inline-size;
    text-align: right;
    overflow: hidden;
    text-overflow: ellipsis;
    color: var(--mf-fg-dim, #8a8a96);
    font-size: 0.9em;
  }
</style>
