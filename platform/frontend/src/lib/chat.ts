import type { CreateResource, ObjectKind, PlannedLink, Resource } from './types';

export const AGENT_MANIFEST = '/manifests/agent';
export const THREAD_MANIFEST = '/manifests/thread';
export const MESSAGE_MANIFEST = '/manifests/message';
export const PARTICIPANTS = '/manifests/thread/relations/participants';
export const AUTHORED_BY = '/manifests/message/relations/authored-by';
export const MESSAGE_THREAD = '/manifests/message/relations/message-thread';
export const MENTIONED = '/manifests/message/relations/mentioned';
export const REPLIES_TO = '/manifests/message/relations/replies-to';

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
  return relationTargets(thread, PARTICIPANTS).filter((path) => path.startsWith('/agents/'));
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
  const path = `/threads/${id}`;
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
    participantPath.startsWith('/users/') ? 'user' : 'resource',
    participantPath
  );
}

export function buildUserMessage(
  id: string,
  body: string,
  userPath: string,
  threadPath: string,
  mentionedAgents: string[],
  parentPath: string | null
): CreateResource {
  const path = `/messages/${id}`;
  const links: PlannedLink[] = [
    link(`${path}/links/authored-by`, path, AUTHORED_BY, 'user', userPath),
    link(`${path}/links/message-thread`, path, MESSAGE_THREAD, 'resource', threadPath)
  ];
  if (parentPath) {
    links.push(link(`${path}/links/replies-to`, path, REPLIES_TO, 'resource', parentPath));
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
