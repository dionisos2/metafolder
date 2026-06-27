// Pure help logic (panel-shim/help.js): name -> page resolution, grep filter,
// and click-target -> topic resolution. No DOM, no fetch.

import { describe, expect, test } from 'vitest';
// @ts-expect-error plain-JS module shared with the panel shim
import { resolvePage, filterPages, resolveClickTopic } from '../../panel-shim/help.js';

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
    aliases: ['query', 'edit-query', 'simplified-query', 'grammar', 'dsl'],
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
    expect(resolvePage(MANIFEST, 'edit-query')?.id).toBe('queries');
    expect(resolvePage(MANIFEST, 'EDIT-QUERY')?.id).toBe('queries');
  });

  test('a namespaced command resolves via a direct alias when one exists', () => {
    // `edit-query` is an alias, so `metarecord-list:edit-query` hits it directly.
    expect(resolvePage(MANIFEST, 'metarecord-list:edit-query')?.id).toBe('queries');
  });

  test('a namespaced command falls back to its panel-type prefix', () => {
    // No `set-page-size` alias: the `metarecord-list` prefix wins.
    expect(resolvePage(MANIFEST, 'metarecord-list:set-page-size')?.id).toBe('metarecord-list');
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
