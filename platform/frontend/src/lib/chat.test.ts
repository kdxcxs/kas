import { describe, expect, it } from 'vitest';
import {
  AUTHORED_BY,
  MESSAGE_THREAD,
  MENTIONED,
  PARTICIPANTS,
  REPLIES_TO,
  buildThread,
  buildUserMessage,
  mentionRunPath,
  mentionedAgentPaths,
  messagesForThread,
  participantAgentPaths,
  slugify,
  threadParticipantLink,
  threadsForAgent
} from './chat';
import type { Link, Resource } from './types';

describe('slugify', () => {
  it('creates a path-safe Agent segment', () => {
    expect(slugify(' Release Planner / EU ')).toBe('release-planner-eu');
  });
});

describe('Thread resources', () => {
  it('creates an independent Thread with User and Agent participants', () => {
    const thread = buildThread(
      'thread-1',
      'Release planning',
      '/users/admin',
      ['/agents/planner', '/agents/reviewer']
    );

    expect(thread.path).toBe('/threads/thread-1');
    expect(thread.manifest).toBe('/manifests/thread');
    expect(thread.links?.map((link) => link.relation_path)).toEqual([
      PARTICIPANTS,
      PARTICIPANTS,
      PARTICIPANTS
    ]);
    expect(thread.links?.map((link) => link.target.path)).toEqual([
      '/users/admin',
      '/agents/planner',
      '/agents/reviewer'
    ]);
  });

  it('filters Threads by Agent participation', () => {
    const planner = resource('/threads/planning', '/manifests/thread', 'Planning', {
      title: 'Planning'
    });
    planner.links = [
      link(
        '/threads/planning/links/participants/planner',
        PARTICIPANTS,
        planner.path,
        '/agents/planner'
      )
    ];

    expect(threadsForAgent([planner], '/agents/planner')).toEqual([planner]);
    expect(threadsForAgent([planner], '/agents/reviewer')).toEqual([]);
  });

  it('returns Agent participants and builds their stable Link path', () => {
    const thread = resource('/threads/planning', '/manifests/thread', 'Planning', {
      title: 'Planning'
    });
    thread.links = [
      link(
        '/threads/planning/links/participants/admin',
        PARTICIPANTS,
        thread.path,
        '/users/admin'
      ),
      link(
        '/threads/planning/links/participants/agents-planner',
        PARTICIPANTS,
        thread.path,
        '/agents/planner'
      )
    ];

    expect(participantAgentPaths(thread)).toEqual(['/agents/planner']);
    expect(threadParticipantLink(thread.path, '/agents/reviewer').path).toBe(
      '/threads/planning/links/participants/agents-reviewer'
    );
  });
});

describe('Message resources', () => {
  it('links a Message to its Thread and every mentioned Agent', () => {
    const message = buildUserMessage(
      'message-1',
      '@planner hello',
      '/users/admin',
      '/threads/planning',
      ['/agents/planner'],
      '/messages/previous'
    );

    expect(message.links?.map((entry) => entry.relation_path)).toEqual([
      AUTHORED_BY,
      MESSAGE_THREAD,
      REPLIES_TO,
      MENTIONED
    ]);
    expect(message.links?.find((entry) => entry.relation_path === MESSAGE_THREAD)?.target.path).toBe(
      '/threads/planning'
    );
    expect(mentionRunPath(message.path, '/agents/planner')).toBe(
      '/messages/message-1/links/mentioned/agents-planner/run'
    );
  });

  it('selects Messages using message-thread instead of a root Message', () => {
    const message = resource('/messages/one', '/manifests/message', 'one', {
      role: 'user',
      body: 'hello'
    });
    message.links = [
      link(
        '/messages/one/links/message-thread',
        MESSAGE_THREAD,
        message.path,
        '/threads/planning'
      )
    ];

    expect(messagesForThread([message], '/threads/planning')).toEqual([message]);
    expect(messagesForThread([message], '/threads/other')).toEqual([]);
  });
});

describe('@Agent mentions', () => {
  const planner = resource('/agents/planner', '/manifests/agent', 'Planner', {});
  const reviewer = resource('/agents/reviewer', '/manifests/agent', 'Reviewer', {});

  it('returns only explicitly mentioned Thread participants', () => {
    expect(
      mentionedAgentPaths('@planner please plan; reviewer can wait', [planner, reviewer])
    ).toEqual(['/agents/planner']);
    expect(mentionedAgentPaths('@outsider hello', [planner, reviewer])).toEqual([]);
  });

  it('supports multiple mentions', () => {
    expect(mentionedAgentPaths('@planner plan, @reviewer review', [planner, reviewer])).toEqual([
      '/agents/planner',
      '/agents/reviewer'
    ]);
  });
});

function resource(
  path: string,
  manifest: string,
  name: string,
  spec: Record<string, unknown>
): Resource {
  const metadata = {
    path,
    manifest,
    name,
    state: 'available',
    '[kas]': {
      revision: 0,
      observed: {},
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z'
    }
  };
  return {
    path,
    manifest,
    name,
    state: 'available',
    status_state: 'available',
    spec,
    status: spec,
    revision: 0,
    created_at: metadata['[kas]'].created_at,
    updated_at: metadata['[kas]'].updated_at,
    document: {
      metadata,
      spec,
      status: { metadata, spec }
    },
    links: []
  };
}

function link(path: string, relation: string, source: string, target: string): Link {
  return {
    path,
    source: { kind: 'resource', path: source },
    relation_path: relation,
    target: { kind: 'resource', path: target },
    spec: { relation, source, target, metadata: {} },
    status: { relation, source, target, metadata: {} },
    metadata: {},
    revision: 0,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z'
  };
}
