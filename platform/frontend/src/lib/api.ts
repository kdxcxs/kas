import type {
  CreateResource,
  Driver,
  Link,
  ObjectDetail,
  ObjectKind,
  ObjectRef,
  PlannedLink,
  Resource,
  ResourceDocument,
  Run,
  UpdateResource
} from './types';

const LINK_MANIFEST = '/builtin/link';

const KIND_BY_MANIFEST: Record<string, ObjectKind> = {
  '/builtin/manifest': 'manifest',
  '/builtin/action': 'action',
  '/builtin/relation': 'relation',
  '/builtin/driver': 'driver',
  '/builtin/run': 'run',
  '/builtin/link': 'link',
  '/builtin/user': 'user',
  '/builtin/service-account': 'service_account',
  '/builtin/role': 'role',
  '/builtin/role-binding': 'role_binding',
  '/builtin/credential': 'credential',
  '/builtin/package': 'package'
};

export class KasApiError extends Error {
  constructor(
    message: string,
    readonly status: number
  ) {
    super(message);
  }
}

export class KasApi {
  constructor(
    readonly baseUrl: string,
    readonly token: string
  ) {}

  async health(): Promise<boolean> {
    const response = await fetch(this.url('/health'));
    return response.ok;
  }

  async listResources(manifest?: string): Promise<Resource[]> {
    const query = manifest ? `?${new URLSearchParams({ manifest })}` : '';
    const documents = await this.request<ResourceDocument[]>(`/resources${query}`);
    return documents.map(resourceFromDocument);
  }

  async getResource(path: string, includeRelations = false): Promise<Resource> {
    const document = await this.request<ResourceDocument>(
      `/resources/by-path?${new URLSearchParams({ path })}`
    );
    const resource = resourceFromDocument(document);
    if (includeRelations) {
      const all = await this.listResources();
      resource.links = linksFor(path, all);
    }
    return resource;
  }

  async createResource(resource: CreateResource): Promise<Resource> {
    const created = resourceFromDocument(
      await this.request<ResourceDocument>('/resources', {
        method: 'POST',
        body: JSON.stringify(resourcePayload(resource))
      })
    );
    for (const link of resource.links ?? []) {
      await this.createLink(link);
    }
    return created;
  }

  async updateResource(path: string, update: UpdateResource): Promise<Resource> {
    const document = await this.request<ResourceDocument>(
      `/resources/by-path?${new URLSearchParams({ path })}`,
      {
        method: 'PATCH',
        body: JSON.stringify(update)
      }
    );
    return resourceFromDocument(document);
  }

  async deleteResource(path: string, expectedRevision: number): Promise<Resource> {
    const document = await this.request<ResourceDocument>(
      `/resources/by-path?${new URLSearchParams({
        path,
        expected_revision: String(expectedRevision)
      })}`,
      { method: 'DELETE' }
    );
    return resourceFromDocument(document);
  }

  async getRun(path: string): Promise<Run> {
    return runFromResource(await this.getResource(path));
  }

  async getAgentDriver(): Promise<Driver | null> {
    try {
      const driver = await this.getResource('/manifests/agent/driver');
      return {
        path: driver.path,
        state: driver.status_state as Driver['state']
      };
    } catch (cause) {
      if (cause instanceof KasApiError && cause.status === 404) return null;
      throw cause;
    }
  }

  async listObjects(kind?: ObjectKind): Promise<ObjectRef[]> {
    return (await this.listResources())
      .map((resource) => ({
        kind: kindForManifest(resource.manifest),
        path: resource.path,
        name: resource.name,
        state: resource.status_state || resource.state,
        manifest: resource.manifest
      }))
      .filter((object) => !kind || object.kind === kind);
  }

  async getObject(kind: ObjectKind, path: string): Promise<ObjectDetail> {
    const all = await this.listResources();
    const resource = all.find((candidate) => candidate.path === path);
    if (!resource) throw new KasApiError(`Resource ${path} was not found`, 404);
    return {
      kind,
      path,
      value: resource.document,
      links: linksFor(path, all)
    };
  }

  async createLink(link: PlannedLink): Promise<Resource> {
    const source = link.source.path;
    const target = link.target.path;
    return this.createResource({
      path: link.path,
      manifest: LINK_MANIFEST,
      name: link.path.split('/').at(-1) || 'link',
      spec: {
        relation: link.relation_path,
        source,
        target,
        metadata: link.metadata
      }
    });
  }

  private async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const response = await fetch(this.url(path), {
      ...init,
      headers: {
        Authorization: `Bearer ${this.token}`,
        ...(init.body ? { 'Content-Type': 'application/json' } : {}),
        ...init.headers
      }
    });
    if (!response.ok) {
      let message = `${response.status} ${response.statusText}`;
      try {
        const body = (await response.json()) as { error?: string };
        if (body.error) message = body.error;
      } catch {
        // Keep the HTTP status when the response is not JSON.
      }
      throw new KasApiError(message, response.status);
    }
    return (await response.json()) as T;
  }

  private url(path: string): string {
    return `${this.baseUrl.replace(/\/$/, '')}${path}`;
  }
}

export class FileApi {
  constructor(
    readonly baseUrl: string,
    readonly token: string
  ) {}

  async upload(file: File, path?: string): Promise<Resource> {
    const form = new FormData();
    form.append('content', file, file.name);
    const query = path ? `?${new URLSearchParams({ path })}` : '';
    const response = await fetch(`${this.baseUrl.replace(/\/$/, '')}/files${query}`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${this.token}` },
      body: form
    });
    await requireResponse(response);
    return resourceFromDocument((await response.json()) as ResourceDocument);
  }

  async download(path: string): Promise<Blob> {
    const response = await fetch(
      `${this.baseUrl.replace(/\/$/, '')}/files/content?${new URLSearchParams({ path })}`,
      { headers: { Authorization: `Bearer ${this.token}` } }
    );
    await requireResponse(response);
    return response.blob();
  }
}

export class SkillApi {
  constructor(
    readonly baseUrl: string,
    readonly token: string
  ) {}

  async create(path: string, bundle: File): Promise<Resource> {
    return this.upload('POST', path, bundle);
  }

  async update(path: string, expectedRevision: number, bundle: File): Promise<Resource> {
    return this.upload('PATCH', path, bundle, expectedRevision);
  }

  private async upload(
    method: 'POST' | 'PATCH',
    path: string,
    bundle: File,
    expectedRevision?: number
  ): Promise<Resource> {
    const form = new FormData();
    form.append('bundle', bundle, bundle.name);
    const query = new URLSearchParams({ path });
    if (expectedRevision !== undefined) {
      query.set('expected_revision', String(expectedRevision));
    }
    const response = await fetch(
      `${this.baseUrl.replace(/\/$/, '')}/skills?${query}`,
      {
        method,
        headers: { Authorization: `Bearer ${this.token}` },
        body: form
      }
    );
    await requireResponse(response);
    return resourceFromDocument((await response.json()) as ResourceDocument);
  }
}

async function requireResponse(response: Response): Promise<void> {
  if (response.ok) return;
  let message = `${response.status} ${response.statusText}`;
  try {
    const body = (await response.json()) as { error?: string };
    if (body.error) message = body.error;
  } catch {
    // Keep the HTTP status when the response is not JSON.
  }
  throw new KasApiError(message, response.status);
}

function resourcePayload(resource: CreateResource): unknown {
  return {
    metadata: {
      path: resource.path,
      manifest: resource.manifest,
      name: resource.name
    },
    spec: resource.spec
  };
}

export function resourceFromDocument(document: ResourceDocument): Resource {
  return {
    path: document.metadata.path,
    manifest: document.metadata.manifest,
    name: document.metadata.name,
    state: document.metadata.state,
    status_state: document.status.metadata.state,
    spec: document.spec,
    status: document.status.spec,
    revision: document.metadata['[kas]'].revision,
    created_at: document.metadata['[kas]'].created_at,
    updated_at: document.metadata['[kas]'].updated_at,
    document
  };
}

function kindForManifest(manifest: string): ObjectKind {
  return KIND_BY_MANIFEST[manifest] ?? 'resource';
}

function linksFor(path: string, resources: Resource[]): Link[] {
  const byPath = new Map(resources.map((resource) => [resource.path, resource]));
  return resources
    .filter((resource) => resource.manifest === LINK_MANIFEST && resource.state !== 'deleted')
    .flatMap((resource) => {
      const relation = stringValue(resource.spec.relation);
      const sourcePath = stringValue(resource.spec.source);
      const targetPath = stringValue(resource.spec.target);
      if (!relation || !sourcePath || !targetPath || (sourcePath !== path && targetPath !== path)) {
        return [];
      }
      const source = byPath.get(sourcePath);
      const target = byPath.get(targetPath);
      return [
        {
          path: resource.path,
          source: {
            kind: kindForManifest(source?.manifest ?? ''),
            path: sourcePath
          },
          relation_path: relation,
          target: {
            kind: kindForManifest(target?.manifest ?? ''),
            path: targetPath
          },
          spec: resource.spec,
          status: resource.status,
          metadata: objectValue(resource.spec.metadata) ?? {},
          revision: resource.revision,
          created_at: resource.created_at,
          updated_at: resource.updated_at
        }
      ];
    });
}

function runFromResource(resource: Resource): Run {
  return {
    resource,
    status: resource.status_state as Run['status'],
    output: objectValue(resource.spec.output, null),
    error: typeof resource.spec.error === 'string' ? resource.spec.error : null
  };
}

function stringValue(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function objectValue(value: unknown, fallback: Record<string, unknown> | null = {}): Record<
  string,
  unknown
> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : fallback;
}
