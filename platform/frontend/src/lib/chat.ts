import type { CreateResource, Link, PlannedLink, Resource } from './types';

export const AGENT_MANIFEST = '/manifests/agent';
export const MESSAGE_MANIFEST = '/manifests/message';
export const MESSAGE_ACTION = '/manifests/agent/actions/message';
export const AUTHORED_BY = '/manifests/message/relations/authored-by';
export const ADDRESSED_TO = '/manifests/message/relations/addressed-to';
export const REPLIES_TO = '/manifests/message/relations/replies-to';
export const THREAD_ROOT = '/manifests/message/relations/thread-root';

export function slugify(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-|-$/g, '');
}

export function relationTarget(resource: Resource, relation: string): string | null {
  return (
    resource.links?.find(
      (link) => link.relation_path === relation && link.source.path === resource.path
    )?.target.path ?? null
  );
}

export function threadRootOf(message: Resource): string {
  return relationTarget(message, THREAD_ROOT) ?? message.path;
}

export function messagesForAgent(messages: Resource[], agentPath: string): Resource[] {
  return messages
    .filter(
      (message) =>
        relationTarget(message, ADDRESSED_TO) === agentPath ||
        relationTarget(message, AUTHORED_BY) === agentPath
    )
    .sort((left, right) => left.created_at.localeCompare(right.created_at));
}

export function groupThreads(messages: Resource[]): Map<string, Resource[]> {
  const threads = new Map<string, Resource[]>();
  for (const message of messages) {
    const root = threadRootOf(message);
    const thread = threads.get(root) ?? [];
    thread.push(message);
    threads.set(root, thread);
  }
  return threads;
}

export function buildUserMessage(
  id: string,
  body: string,
  userPath: string,
  agentPath: string,
  threadRoot: string | null,
  parentPath: string | null
): CreateResource {
  const path = `/messages/${id}`;
  const root = threadRoot ?? path;
  const links: PlannedLink[] = [
    link(`${path}/links/authored-by`, path, AUTHORED_BY, 'user', userPath),
    link(`${path}/links/addressed-to`, path, ADDRESSED_TO, 'resource', agentPath),
    link(`${path}/links/thread-root`, path, THREAD_ROOT, 'resource', root)
  ];
  if (parentPath) {
    links.push(link(`${path}/links/replies-to`, path, REPLIES_TO, 'resource', parentPath));
  }
  return {
    path,
    manifest: MESSAGE_MANIFEST,
    name: 'user-message',
    spec: { role: 'user', body },
    links
  };
}

export function firstMessageBody(messages: Resource[]): string {
  const body = messages.find((message) => message.spec.role === 'user')?.spec.body;
  return typeof body === 'string' ? body : 'New conversation';
}

export function link(
  path: string,
  sourcePath: string,
  relationPath: string,
  targetKind: string,
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

export function hasRelation(link: Link, relation: string): boolean {
  return link.relation_path === relation;
}

