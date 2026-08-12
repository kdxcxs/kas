import { describe, expect, it } from 'vitest';
import {
  ATTACHED_TO,
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
  sessionForThreadAgent,
  sessionPath,
  shouldSubmitComposer,
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

describe('composer keyboard handling', () => {
  const enter = {
    key: 'Enter',
    shiftKey: false,
    isComposing: false,
    keyCode: 13
  };

  it('submits a normal Enter key', () => {
    expect(shouldSubmitComposer(enter)).toBe(true);
  });

  it('does not submit while an input method is composing', () => {
    expect(shouldSubmitComposer({ ...enter, isComposing: true })).toBe(false);
    expect(shouldSubmitComposer(enter, true)).toBe(false);
    expect(shouldSubmitComposer({ ...enter, keyCode: 229 })).toBe(false);
  });

  it('keeps Shift+Enter as a newline', () => {
    expect(shouldSubmitComposer({ ...enter, shiftKey: true })).toBe(false);
  });
});

describe('Thread resources', () => {
  it('creates an independent Thread with User and Agent participants', () => {
    const thread = buildThread(
      'thread-1',
      'Release planning',
      '/packages/kas/user/users/admin',
      ['/packages/studio/agent/agents/planner', '/packages/studio/agent/agents/reviewer']
    );

    expect(thread.path).toBe('/packages/studio/thread/threads/thread-1');
    expect(thread.manifest).toBe('/packages/studio/thread/manifest');
    expect(thread.links?.map((link) => link.relation_path)).toEqual([
      PARTICIPANTS,
      PARTICIPANTS,
      PARTICIPANTS
    ]);
    expect(thread.links?.map((link) => link.target.path)).toEqual([
      '/packages/kas/user/users/admin',
      '/packages/studio/agent/agents/planner',
      '/packages/studio/agent/agents/reviewer'
    ]);
  });

  it('filters Threads by Agent participation', () => {
    const planner = resource('/packages/studio/thread/threads/planning', '/packages/studio/thread/manifest', 'Planning', {
      title: 'Planning'
    });
    planner.links = [
      link(
        '/packages/studio/thread/threads/planning/links/participants/planner',
        PARTICIPANTS,
        planner.path,
        '/packages/studio/agent/agents/planner'
      )
    ];

    expect(threadsForAgent([planner], '/packages/studio/agent/agents/planner')).toEqual([planner]);
    expect(threadsForAgent([planner], '/packages/studio/agent/agents/reviewer')).toEqual([]);
  });

  it('returns Agent participants and builds their stable Link path', () => {
    const thread = resource('/packages/studio/thread/threads/planning', '/packages/studio/thread/manifest', 'Planning', {
      title: 'Planning'
    });
    thread.links = [
      link(
        '/packages/studio/thread/threads/planning/links/participants/admin',
        PARTICIPANTS,
        thread.path,
        '/packages/kas/user/users/admin'
      ),
      link(
        '/packages/studio/thread/threads/planning/links/participants/agents-planner',
        PARTICIPANTS,
        thread.path,
        '/packages/studio/agent/agents/planner'
      )
    ];

    expect(participantAgentPaths(thread)).toEqual(['/packages/studio/agent/agents/planner']);
    expect(threadParticipantLink(thread.path, '/packages/studio/agent/agents/reviewer').path).toBe(
      '/packages/studio/thread/threads/planning/links/participants/packages-studio-agent-agents-reviewer'
    );
  });

  it('addresses one Session per Thread-Agent pair', () => {
    const session = resource(
      '/packages/studio/session/sessions/packages-studio-thread-threads-planning-packages-studio-agent-agents-planner',
      '/packages/studio/session/manifest',
      'planning-planner',
      {
        provider: 'codex',
        session_id: 'session-1',
        cursor: '/packages/studio/message/messages/one'
      }
    );

    expect(sessionPath('/packages/studio/thread/threads/planning', '/packages/studio/agent/agents/planner')).toBe(session.path);
    expect(
      sessionForThreadAgent([session], '/packages/studio/thread/threads/planning', '/packages/studio/agent/agents/planner')
    ).toBe(session);
    expect(
      sessionForThreadAgent([session], '/packages/studio/thread/threads/planning', '/packages/studio/agent/agents/reviewer')
    ).toBeNull();
  });
});

describe('Message resources', () => {
  it('links a Message to its Thread and every mentioned Agent', () => {
    const message = buildUserMessage(
      'message-1',
      '@planner hello',
      '/packages/kas/user/users/admin',
      '/packages/studio/thread/threads/planning',
      ['/packages/studio/agent/agents/planner'],
      '/packages/studio/message/messages/previous',
      ['/packages/studio/file/files/one']
    );

    expect(message.links?.map((entry) => entry.relation_path)).toEqual([
      AUTHORED_BY,
      MESSAGE_THREAD,
      REPLIES_TO,
      ATTACHED_TO,
      MENTIONED
    ]);
    expect(message.links?.find((entry) => entry.relation_path === ATTACHED_TO)).toMatchObject({
      source: { path: '/packages/studio/file/files/one' },
      target: { path: '/packages/studio/message/messages/message-1' }
    });
    expect(message.links?.find((entry) => entry.relation_path === MESSAGE_THREAD)?.target.path).toBe(
      '/packages/studio/thread/threads/planning'
    );
    expect(mentionRunPath(message.path, '/packages/studio/agent/agents/planner')).toBe(
      '/packages/studio/message/messages/message-1/links/mentioned/packages-studio-agent-agents-planner/run'
    );
  });

  it('selects Messages using message-thread instead of a root Message', () => {
    const message = resource('/packages/studio/message/messages/one', '/packages/studio/message/manifest', 'one', {
      role: 'user',
      body: 'hello'
    });
    message.links = [
      link(
        '/packages/studio/message/messages/one/links/message-thread',
        MESSAGE_THREAD,
        message.path,
        '/packages/studio/thread/threads/planning'
      )
    ];

    expect(messagesForThread([message], '/packages/studio/thread/threads/planning')).toEqual([message]);
    expect(messagesForThread([message], '/packages/studio/thread/threads/other')).toEqual([]);
  });
});

describe('@Agent mentions', () => {
  const planner = resource('/packages/studio/agent/agents/planner', '/packages/studio/agent/manifest', 'Planner', {});
  const reviewer = resource('/packages/studio/agent/agents/reviewer', '/packages/studio/agent/manifest', 'Reviewer', {});

  it('returns only explicitly mentioned Thread participants', () => {
    expect(
      mentionedAgentPaths('@planner please plan; reviewer can wait', [planner, reviewer])
    ).toEqual(['/packages/studio/agent/agents/planner']);
    expect(mentionedAgentPaths('@outsider hello', [planner, reviewer])).toEqual([]);
  });

  it('supports multiple mentions', () => {
    expect(mentionedAgentPaths('@planner plan, @reviewer review', [planner, reviewer])).toEqual([
      '/packages/studio/agent/agents/planner',
      '/packages/studio/agent/agents/reviewer'
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
      path,
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
