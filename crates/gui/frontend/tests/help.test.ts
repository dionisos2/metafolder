// Pure help logic (panel-shim/help.js): name -> page resolution, grep filter,
// and click-target -> topic resolution. No DOM, no fetch.

import { describe, expect, test } from 'vitest';
import { resolvePage, filterPages, resolveClickTopic, mergeManifests } from '../../panel-shim/help.js';

// A miniature manifest mirroring pages/index.json.
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

describe('mergeManifests', () => {
  const LEGACY = [
    { id: 'trash', title: 'Trash panel', file: 'trash.html', aliases: ['bin'] },
    { id: 'repos', title: 'Repositories', file: 'repos.html' },
  ];
  const WIKI = [{ id: 'trash', title: 'Trash', file: 'trash.html', aliases: ['bin'] }];

  test('every page remembers where it is fetched from', () => {
    const out = mergeManifests(LEGACY, '/panel/help/pages', WIKI, '/docs');
    expect(out.map((p) => [p.id, p.base])).toEqual([
      ['trash', '/docs'],
      ['repos', '/panel/help/pages'],
    ]);
  });

  test('a migrated page replaces the old one, and wins name resolution', () => {
    const out = mergeManifests(LEGACY, '/old', WIKI, '/docs');
    expect(out.filter((p) => p.id === 'trash')).toHaveLength(1);
    expect(resolvePage(out, 'bin')?.base).toBe('/docs');
  });

  test('no wiki (not installed) leaves the old pages', () => {
    expect(mergeManifests(LEGACY, '/old', [], '/docs').map((p) => p.id)).toEqual(['trash', 'repos']);
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

  test('falls back to the slot panel type when no topic is tagged', () => {
    const descriptors = [{}, { slotBody: 'right' }, {}];
    expect(resolveClickTopic(descriptors, slotPanelType)).toBe('repos');
  });

  test('returns null when neither a topic nor a slot is present', () => {
    expect(resolveClickTopic([{}, {}], slotPanelType)).toBeNull();
  });
});
