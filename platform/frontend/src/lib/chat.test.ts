import { describe, expect, it } from 'vitest';
import {
  ADDRESSED_TO,
  AUTHORED_BY,
  REPLIES_TO,
  THREAD_ROOT,
  buildUserMessage,
  messagesForAgent,
  slugify,
  threadRootOf
} from './chat';
import type { Resource } from './types';

describe('slugify', () => {
  it('creates a path-safe Agent segment', () => {
    expect(slugify(' Release Planner / EU ')).toBe('release-planner-eu');
  });
});

describe('buildUserMessage', () => {
  it('creates a root Message with platform relations', () => {
    const message = buildUserMessage(
      'root',
      'hello',
      '/users/admin',
      '/agents/demo',
      null,
      null
    );
    expect(message.path).toBe('/messages/root');
    expect(message.links?.map((link) => link.relation_path)).toEqual([
      AUTHORED_BY,
      ADDRESSED_TO,
      THREAD_ROOT
    ]);
    expect(message.links?.at(-1)?.target?.path).toBe('/messages/root');
  });

  it('adds replies_to and preserves the thread root for follow-ups', () => {
    const message = buildUserMessage(
      'next',
      'continue',
      '/users/admin',
      '/agents/demo',
      '/messages/root',
      '/messages/reply'
    );
    expect(message.links?.find((link) => link.relation_path === THREAD_ROOT)?.target?.path).toBe(
      '/messages/root'
    );
    expect(message.links?.find((link) => link.relation_path === REPLIES_TO)?.target?.path).toBe(
      '/messages/reply'
    );
  });
});

describe('conversation selection', () => {
  const base: Resource = {
    path: '/messages/root',
    name: 'message',
    spec: { role: 'user', body: 'hello' },
    status: {},
    revision: 0,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
    links: [
      {
        path: '/messages/root/links/addressed-to',
        source: { kind: 'resource', path: '/messages/root' },
        relation_path: ADDRESSED_TO,
        target: { kind: 'resource', path: '/agents/demo' },
        spec: { state: 'available' },
        status: { state: 'available' },
        metadata: {},
        revision: 0,
        created_at: '2026-01-01T00:00:00Z',
        updated_at: '2026-01-01T00:00:00Z'
      },
      {
        path: '/messages/root/links/thread-root',
        source: { kind: 'resource', path: '/messages/root' },
        relation_path: THREAD_ROOT,
        target: { kind: 'resource', path: '/messages/root' },
        spec: { state: 'available' },
        status: { state: 'available' },
        metadata: {},
        revision: 0,
        created_at: '2026-01-01T00:00:00Z',
        updated_at: '2026-01-01T00:00:00Z'
      }
    ]
  };

  it('filters messages related to the selected Agent', () => {
    expect(messagesForAgent([base], '/agents/demo')).toEqual([base]);
    expect(messagesForAgent([base], '/agents/other')).toEqual([]);
  });

  it('reads the canonical thread root Link', () => {
    expect(threadRootOf(base)).toBe('/messages/root');
  });
});
