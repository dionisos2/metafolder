// Completion candidates for the command input (spec-gui "Completion views").
//
// A candidate is a couple: the *label* the list shows (and a typed answer
// matches on, whole) and the *value* the command receives. A plain string is
// both — every completion that has nothing to gain from the split stays a
// plain list. What the split buys: the list can name things the way humans
// read them while the command gets what it must act on (a metarecord's uuid,
// say), and it buys the generic duplicate-label rule below.
//
// An argument's completion may also be several *views* of its candidates —
// usually one, sometimes a few the user cycles through (`completion:cycle`).
// Each view is a builder called with the typed text, so a source that talks to
// the daemon can narrow as the user types instead of shipping every row.
//
// The candidate *rules* (normalization, the duplicate-label rule) live in the
// shared shim (/__completions.js): the panels that name and resolve values
// need exactly the forms the shell displays.

import { completionLabels, completionPage } from '../../../panel-shim/completions.js';

export { completionLabels, completionPage };

/** One candidate. A plain string is its own label and value. */
export type CompletionItem = string | { label: string; value: string };

/** One page of candidates. `more` says the source holds more than it handed
 *  over — so narrowing on the typed text must ask it again rather than filter
 *  this page alone (a daemon page is bounded; see the `limit` callers use). */
export interface CompletionPage {
  items: CompletionItem[];
  more?: boolean;
}

export type CompletionResult = CompletionItem[] | CompletionPage;

/** Builds one view's candidates for the typed text (`partial`), given the
 *  arguments collected so far (`prior`). */
export type CompletionFn = (
  partial: string,
  prior: string[],
) => CompletionResult | Promise<CompletionResult>;

/** One cycled view: its builder, plus the title `completion:cycle` shows for
 *  it (e.g. the columns the candidates are named by). */
export interface CompletionView {
  title?: string;
  items: CompletionFn;
}

/** A view bound to one prompt: `prior` is already applied, `partial` is all
 *  that is left to ask with. */
export interface LoadedView {
  title?: string;
  items: (partial: string) => Promise<CompletionPage>;
}

/**
 * Binds a view to one prompt's `prior` and memoizes its last page: the eager
 * first page handed to the driver and the driver's own first load are one and
 * the same call, and a re-render must not re-query the daemon.
 */
export function bindView(view: CompletionView, prior: string[]): LoadedView {
  let last: { partial: string; page: Promise<CompletionPage> } | null = null;
  return {
    title: view.title,
    items: (partial) => {
      if (last && last.partial === partial) return last.page;
      const page = Promise.resolve(view.items(partial, prior)).then(completionPage);
      last = { partial, page };
      return page;
    },
  };
}
