// Pure help logic (panel-shim/help.js): name -> page resolution, grep filter,
// and click-target -> topic resolution. No DOM, no fetch.

import { describe, expect, test } from 'vitest';
import { resolvePage, filterPages, resolveClickTopic } from '../../panel-shim/help.js';

// A miniature manifest mirroring /docs/index.json.
const MANIFEST = [
  {
    id: 'getting-started',
    title: 'Getting started',
    file: 'getting-started.html',
    aliases: ['help', 'start', 'command-input', 'keybindings'],
  },
  {
    id: 'queries',
    title: 'Queries',
    file: 'queries.html',
    aliases: ['query', 'focus-query', 'simplified-query', 'grammar', 'dsl'],
  },
  {
    id: 'metarecord-list',
    title: 'Metarecord list',
    file: 'metarecord-list.html',
    aliases: ['columns'],
  },
  { id: 'repos', title: 'Repositories', file: 'repos.html', aliases: ['repositories'] },
];

describe('resolvePage', () => {
  test('resolves by page id', () => {
    expect(resolvePage(MANIFEST, 'queries')?.id).toBe('queries');
  });

  test('resolves by alias (case-insensitive)', () => {
    expect(resolvePage(MANIFEST, 'focus-query')?.id).toBe('queries');
    expect(resolvePage(MANIFEST, 'FOCUS-QUERY')?.id).toBe('queries');
  });

  test('a namespaced command resolves via a direct alias when one exists', () => {
    // `focus-query` is an alias, so `metarecord-list:focus-query` hits it directly.
    expect(resolvePage(MANIFEST, 'metarecord-list:focus-query')?.id).toBe('queries');
  });

  test('a namespaced command falls back to its panel-type prefix', () => {
    // No `set-page-size` alias: the `metarecord-list` prefix wins.
    expect(resolvePage(MANIFEST, 'metarecord-list:set page-size')?.id).toBe('metarecord-list');
    expect(resolvePage(MANIFEST, 'repos:open')?.id).toBe('repos');
  });

  // The wiki has one note per command, titled by its name (its id cannot hold
  // the colon), and one per concept, titled in words.
  const WIKI_NOTES = [
    { id: 'find-in-a-panel', title: 'Find in a panel', file: 'a.html', aliases: ['find'] },
    { id: 'trash_find', title: 'trash:find', file: 'b.html' },
    { id: 'panel_set', title: 'panel:set', file: 'c.html' },
    { id: 'trash', title: 'Trash', file: 'd.html', aliases: ['bin'] },
  ];

  test('resolves by title, so a command name opens the command note', () => {
    expect(resolvePage(WIKI_NOTES, 'trash:find')?.id).toBe('trash_find');
    expect(resolvePage(WIKI_NOTES, 'Find in a panel')?.id).toBe('find-in-a-panel');
    expect(resolvePage(WIKI_NOTES, 'find in a PANEL')?.id).toBe('find-in-a-panel');
  });

  test('an invocation with arguments opens the note of its command', () => {
    expect(resolvePage(WIKI_NOTES, 'panel:set type treeref')?.id).toBe('panel_set');
  });

  test('a command without a note still falls back to its verb, then its panel', () => {
    expect(resolvePage(WIKI_NOTES, 'recent:find')?.id).toBe('find-in-a-panel');
    expect(resolvePage(WIKI_NOTES, 'trash:restore')?.id).toBe('trash');
  });

  // A panel's own note is titled "<type> panel"; the topic of the same name is
  // another page.
  const PANELS = [
    { id: 'trash', title: 'Trash', file: 'a.html' },
    { id: 'trash-panel', title: 'trash panel', file: 'b.html' },
    { id: 'log-panel', title: 'log panel', file: 'c.html' },
    { id: 'file', title: 'File viewer', file: 'd.html' },
  ];

  test('"<type> panel" opens the panel note, or the page named by the type', () => {
    expect(resolvePage(PANELS, 'trash panel')?.id).toBe('trash-panel');
    // Not migrated yet: the older page, whose id is the panel type.
    expect(resolvePage(PANELS, 'file panel')?.id).toBe('file');
    expect(resolvePage(PANELS, 'nothing panel')).toBeNull();
  });

  test('a bare panel type with no page of its own opens the panel note', () => {
    expect(resolvePage(PANELS, 'log')?.id).toBe('log-panel');
    // A page of that exact name comes first.
    expect(resolvePage(PANELS, 'trash')?.id).toBe('trash');
  });

  test('a #-prefixed term forces grep (null) even on an exact name', () => {
    expect(resolvePage(MANIFEST, '#queries')).toBeNull();
  });

  test('empty / whitespace / unknown resolves to null (grep)', () => {
    expect(resolvePage(MANIFEST, '')).toBeNull();
    expect(resolvePage(MANIFEST, '   ')).toBeNull();
    expect(resolvePage(MANIFEST, 'nonsense')).toBeNull();
    expect(resolvePage(MANIFEST, null)).toBeNull();
  });
});

describe('filterPages', () => {
  const INDEX = [
    { id: 'queries', title: 'Queries', text: 'how to write a simplified query with the grammar' },
    { id: 'repos', title: 'Repositories', text: 'load and unload repositories here' },
    { id: 'files', title: 'Files', text: 'the query word also appears in this body text' },
  ];

  test('empty term returns all pages, ordered by title', () => {
    const out = filterPages(INDEX, '');
    expect(out.map((p) => p.id)).toEqual(['files', 'queries', 'repos']);
  });

  test('title matches rank above body-only matches', () => {
    const out = filterPages(INDEX, 'quer');
    // "Queries" matches the title; "Files" only matches in the body ("query").
    expect(out.map((p) => p.id)).toEqual(['queries', 'files']);
    expect(out.find((p) => p.id === 'repos')).toBeUndefined();
  });

  test('matching is case-insensitive and returns a snippet for body hits', () => {
    const out = filterPages(INDEX, 'GRAMMAR');
    expect(out.map((p) => p.id)).toEqual(['queries']);
    expect(out[0].snippet.toLowerCase()).toContain('grammar');
  });
});

describe('filterPages and the audience', () => {
  const INDEX = [
    { id: 'trash', title: 'Trash', text: 'the bin', audience: 'user' },
    { id: 'trash-layout', title: 'Trash on-disk layout', text: 'blobs of the bin', audience: 'dev' },
    { id: 'legacy', title: 'Old page', text: 'the bin too' },
  ];

  test('developer pages are left out by default', () => {
    expect(filterPages(INDEX, 'bin').map((p) => p.id)).toEqual(['legacy', 'trash']);
    expect(filterPages(INDEX, '').map((p) => p.id)).toEqual(['legacy', 'trash']);
  });

  test('and included on request', () => {
    const out = filterPages(INDEX, 'bin', { includeDev: true });
    expect(out.map((p) => p.id)).toEqual(['legacy', 'trash', 'trash-layout']);
  });

  test('an exact name still opens a developer page', () => {
    const manifest = [{ id: 'trash-layout', title: 'Trash on-disk layout', file: 'x', audience: 'dev' }];
    expect(resolvePage(manifest, 'trash-layout')?.id).toBe('trash-layout');
  });
});

describe('resolveClickTopic', () => {
  // descriptors are in composedPath order (innermost first).
  const slotPanelType = (slot: string) => (slot === 'left' ? 'file' : 'repos');

  test('the nearest data-help-topic wins over an outer one', () => {
    const descriptors = [
      { helpTopic: 'edit-query' },
      { helpTopic: 'metarecord-list' },
      { slotBody: 'left' },
    ];
    expect(resolveClickTopic(descriptors, slotPanelType)).toBe('edit-query');
  });

  test('falls back to the panel of the clicked slot when no topic is tagged', () => {
    const descriptors = [{}, { slotBody: 'right' }, {}];
    expect(resolveClickTopic(descriptors, slotPanelType)).toBe('repos panel');
  });

  test('returns null when neither a topic nor a slot is present', () => {
    expect(resolveClickTopic([{}, {}], slotPanelType)).toBeNull();
  });
});
