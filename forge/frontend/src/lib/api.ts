export const AGENT_MANIFEST = '/packages/forge/agent/manifest';
export const AGENT_ACTION = '/packages/forge/agent/actions/run';
export const REQUEST_MANIFEST = '/packages/forge/package-request/manifest';
export const PACKAGE_MANIFEST = '/packages/kas/package/manifest';
export const RUN_MANIFEST = '/packages/kas/run/manifest';
export const LINK_MANIFEST = '/packages/kas/link/manifest';
export const REQUESTED_BY = '/packages/forge/package-request/relations/requested-by';
export const DECIDED_BY = '/packages/forge/package-request/relations/decided-by';

export interface ResourceMetadata {
  manifest: string;
  name: string;
  state: string;
  '[kas]': {
    revision: number;
    created_at: string;
    updated_at: string;
  };
}

export interface Resource {
  path: string;
  metadata: ResourceMetadata;
  spec: Record<string, unknown>;
  status: {
    metadata: ResourceMetadata;
    spec: Record<string, unknown>;
  };
}

export class ApiError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
  }
}

export class ForgeApi {
  constructor(readonly token: string) {}

  async list(manifest: string): Promise<Resource[]> {
    return this.request<Resource[]>(`/api/resources?${new URLSearchParams({ manifest })}`);
  }

  async create(resource: Record<string, unknown>): Promise<Resource> {
    return this.request<Resource>('/api/resources', {
      method: 'POST',
      body: JSON.stringify(resource)
    });
  }

  async createRun(input: {
    request_id: string;
    resource: string;
    action: string;
    input: Record<string, unknown>;
  }): Promise<Resource> {
    return this.request<Resource>('/api/runs', {
      method: 'POST',
      body: JSON.stringify(input)
    });
  }

  async decide(request: Resource, decision: 'approve' | 'reject'): Promise<Resource> {
    const query = new URLSearchParams({
      path: request.path,
      expected_revision: String(revision(request))
    });
    return this.request<Resource>(`/package-api/package-requests/decide?${query}`, {
      method: 'POST',
      body: JSON.stringify({ decision })
    });
  }

  async submitPackage(file: File, reason: string): Promise<Resource> {
    const response = await fetch('/package-api/package-requests', {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${this.token}`,
        'Content-Type': 'application/vnd.kas.manifest+tar',
        'X-KAS-Reason': reason
      },
      body: file
    });
    return decode<Resource>(response);
  }

  private async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const response = await fetch(path, {
      ...init,
      headers: {
        Authorization: `Bearer ${this.token}`,
        ...(init.body ? { 'Content-Type': 'application/json' } : {}),
        ...init.headers
      }
    });
    return decode<T>(response);
  }
}

export function revision(resource: Resource): number {
  return Number(resource.metadata['[kas]']?.revision || 0);
}

export function state(resource: Resource): string {
  return resource.status?.metadata?.state || resource.metadata.state;
}

export function linkTarget(
  links: Resource[],
  source: string,
  relation: string
): string | null {
  const link = links.find(
    (candidate) => candidate.spec.source === source && candidate.spec.relation === relation
  );
  return typeof link?.spec.target === 'string' ? link.spec.target : null;
}

async function decode<T>(response: Response): Promise<T> {
  const text = await response.text();
  if (!response.ok) {
    let message = text || response.statusText;
    try {
      const value = JSON.parse(text) as { error?: string };
      message = value.error || message;
    } catch {
      // Preserve the response body.
    }
    throw new ApiError(message, response.status);
  }
  return JSON.parse(text) as T;
}
