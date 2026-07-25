import type {
  CreateResource,
  Driver,
  ObjectDetail,
  ObjectKind,
  ObjectRef,
  Resource,
  Run,
  UpdateResource
} from './types';

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

  async listResources(): Promise<Resource[]> {
    return this.request('/resources');
  }

  async getResource(path: string, includeRelations = false): Promise<Resource> {
    const query = new URLSearchParams({ path });
    if (includeRelations) query.set('include', 'relations');
    return this.request(`/resources/by-path?${query}`);
  }

  async createResource(resource: CreateResource): Promise<Resource> {
    return this.request('/resources', {
      method: 'POST',
      body: JSON.stringify(resource)
    });
  }

  async updateResource(path: string, update: UpdateResource): Promise<Resource> {
    return this.request(`/resources/by-path?${new URLSearchParams({ path })}`, {
      method: 'PATCH',
      body: JSON.stringify(update)
    });
  }

  async deleteResource(path: string, expectedRevision: number): Promise<Resource> {
    return this.request(
      `/resources/by-path?${new URLSearchParams({
        path,
        expected_revision: String(expectedRevision)
      })}`,
      { method: 'DELETE' }
    );
  }

  async createRun(run: {
    path: string;
    request_id: string;
    resource: string;
    action: string;
    input: Record<string, unknown>;
  }): Promise<Run> {
    return this.request('/runs', {
      method: 'POST',
      body: JSON.stringify(run)
    });
  }

  async getRun(path: string): Promise<Run> {
    return this.request(`/runs/by-path?${new URLSearchParams({ path })}`);
  }

  async getAgentDriver(): Promise<Driver | null> {
    return this.request(
      `/manifests/driver?${new URLSearchParams({ path: '/manifests/agent' })}`
    );
  }

  async listObjects(kind?: ObjectKind): Promise<ObjectRef[]> {
    const query = kind ? `?${new URLSearchParams({ kind })}` : '';
    return this.request(`/objects${query}`);
  }

  async getObject(kind: ObjectKind, path: string): Promise<ObjectDetail> {
    return this.request(
      `/objects/by-path?${new URLSearchParams({
        kind,
        path,
        include: 'links'
      })}`
    );
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
