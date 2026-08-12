import type { CreateResource, ObjectKind, PlannedLink, Resource } from './types';

export const AGENT_MANIFEST = '/packages/studio/agent/manifest';
export const THREAD_MANIFEST = '/packages/studio/thread/manifest';
export const MESSAGE_MANIFEST = '/packages/studio/message/manifest';
export const FILE_MANIFEST = '/packages/studio/file/manifest';
export const SESSION_MANIFEST = '/packages/studio/session/manifest';
export const SKILL_MANIFEST = '/packages/studio/skill/manifest';
export const APPROVAL_MANIFEST = '/packages/studio/approval/manifest';
export const APPROVAL_RESULT_MANIFEST = '/packages/studio/approval-result/manifest';
export const APPROVAL_REQUESTED_BY = '/packages/studio/approval/relations/requested-by';
export const APPROVAL_DECIDES = '/packages/studio/approval/relations/decides';
export const APPROVAL_DECIDED_BY = '/packages/studio/approval/relations/decided-by';
export const APPROVAL_RESULT_OF = '/packages/studio/approval/relations/result-of';
export const APPROVAL_PRODUCED_BY = '/packages/studio/approval/relations/produced-by';
export const PARTICIPANTS = '/packages/studio/thread/relations/participants';
export const AUTHORED_BY = '/packages/studio/message/relations/authored-by';
export const MESSAGE_THREAD = '/packages/studio/message/relations/message-thread';
export const MENTIONED = '/packages/studio/message/relations/mentioned';
export const REPLIES_TO = '/packages/studio/message/relations/replies-to';
export const ATTACHED_TO = '/packages/studio/file/relations/attached-to';
export const USES_SKILL = '/packages/studio/skill/relations/uses';

export interface ComposerKeyEvent {
  key: string;
  shiftKey: boolean;
  isComposing: boolean;
  keyCode: number;
}

export function shouldSubmitComposer(
  event: ComposerKeyEvent,
  compositionActive = false
): boolean {
  return (
    event.key === 'Enter' &&
    !event.shiftKey &&
    !event.isComposing &&
    !compositionActive &&
    event.keyCode !== 229
  );
}

export function slugify(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '');
}

export function relationTargets(resource: Resource, relation: string): string[] {
  return (
    resource.links
      ?.filter((link) => link.relation_path === relation && link.source.path === resource.path)
      .map((link) => link.target.path) ?? []
  );
}

export function relationTarget(resource: Resource, relation: string): string | null {
  return relationTargets(resource, relation)[0] ?? null;
}

export function threadsForAgent(threads: Resource[], agentPath: string): Resource[] {
  return threads
    .filter((thread) => relationTargets(thread, PARTICIPANTS).includes(agentPath))
    .sort((left, right) => right.updated_at.localeCompare(left.updated_at));
}

export function messagesForThread(messages: Resource[], threadPath: string): Resource[] {
  return messages
    .filter((message) => relationTarget(message, MESSAGE_THREAD) === threadPath)
    .sort((left, right) => left.created_at.localeCompare(right.created_at));
}

export function participantsForThread(thread: Resource, agents: Resource[]): Resource[] {
  const participantPaths = new Set(relationTargets(thread, PARTICIPANTS));
  return agents.filter((agent) => participantPaths.has(agent.path));
}

export function participantAgentPaths(thread: Resource): string[] {
  return relationTargets(thread, PARTICIPANTS).filter((path) => path.startsWith('/packages/studio/agent/agents/'));
}

export function mentionHandle(agent: Resource): string {
  return agent.path.split('/').filter(Boolean).at(-1) ?? slugify(agent.name);
}

export function mentionedAgentPaths(body: string, participants: Resource[]): string[] {
  const normalized = body.toLowerCase();
  return participants
    .filter((agent) => {
      const handle = mentionHandle(agent).toLowerCase().replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      return new RegExp(`(^|\\s)@${handle}(?=\\s|[,.!?;:]|$)`, 'i').test(normalized);
    })
    .map((agent) => agent.path);
}

export function buildThread(
  id: string,
  title: string,
  userPath: string,
  agentPaths: string[]
): CreateResource {
  const path = `/packages/studio/thread/threads/${id}`;
  const participants = [userPath, ...agentPaths];
  return {
    path,
    manifest: THREAD_MANIFEST,
    name: title,
    spec: { title },
    links: participants.map((participant) => threadParticipantLink(path, participant))
  };
}

export function threadParticipantLink(
  threadPath: string,
  participantPath: string
): PlannedLink {
  return link(
    `${threadPath}/links/participants/${slugify(participantPath)}`,
    threadPath,
    PARTICIPANTS,
    participantPath.startsWith('/packages/kas/user/users/') ? 'user' : 'resource',
    participantPath
  );
}

export function sessionPath(threadPath: string, agentPath: string): string {
  return `/packages/studio/session/sessions/${slugify(threadPath)}-${slugify(agentPath)}`;
}

export function sessionForThreadAgent(
  sessions: Resource[],
  threadPath: string,
  agentPath: string
): Resource | null {
  const path = sessionPath(threadPath, agentPath);
  return sessions.find((session) => session.path === path) ?? null;
}

export function buildUserMessage(
  id: string,
  body: string,
  userPath: string,
  threadPath: string,
  mentionedAgents: string[],
  parentPath: string | null,
  attachmentPaths: string[] = []
): CreateResource {
  const path = `/packages/studio/message/messages/${id}`;
  const links: PlannedLink[] = [
    link(`${path}/links/authored-by`, path, AUTHORED_BY, 'user', userPath),
    link(`${path}/links/message-thread`, path, MESSAGE_THREAD, 'resource', threadPath)
  ];
  if (parentPath) {
    links.push(link(`${path}/links/replies-to`, path, REPLIES_TO, 'resource', parentPath));
  }
  for (const filePath of attachmentPaths) {
    links.push(
      link(
        `${path}/links/attachments/${slugify(filePath)}`,
        filePath,
        ATTACHED_TO,
        'resource',
        path
      )
    );
  }
  for (const agentPath of mentionedAgents) {
    links.push(
      link(
        mentionLinkPath(path, agentPath),
        path,
        MENTIONED,
        'resource',
        agentPath
      )
    );
  }
  return {
    path,
    manifest: MESSAGE_MANIFEST,
    name: 'user-message',
    spec: { role: 'user', body },
    links
  };
}

export function mentionLinkPath(messagePath: string, agentPath: string): string {
  return `${messagePath}/links/mentioned/${slugify(agentPath)}`;
}

export function mentionRunPath(messagePath: string, agentPath: string): string {
  return `${mentionLinkPath(messagePath, agentPath)}/run`;
}

export function link(
  path: string,
  sourcePath: string,
  relationPath: string,
  targetKind: ObjectKind,
  targetPath: string
): PlannedLink {
  return {
    path,
    source: { kind: 'resource', path: sourcePath },
    relation_path: relationPath,
    target: { kind: targetKind, path: targetPath },
    metadata: {}
  };
}
