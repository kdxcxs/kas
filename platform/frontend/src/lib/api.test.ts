import { afterEach, describe, expect, it, vi } from 'vitest';
import { FileApi, KasApi, KasApiError } from './api';

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('FileApi', () => {
  it('uploads multipart content without overriding its boundary', async () => {
    const request = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify(resourceDocument('/files/one', '/manifests/file', 'one.txt')),
        { status: 201, headers: { 'Content-Type': 'application/json' } }
      )
    );
    vi.stubGlobal('fetch', request);
    const file = new File(['hello'], 'one.txt', { type: 'text/plain' });

    const uploaded = await new FileApi('/files-api/', 'secret').upload(file);

    expect(uploaded.path).toBe('/files/one');
    expect(request).toHaveBeenCalledWith('/files-api/files', {
      method: 'POST',
      headers: { Authorization: 'Bearer secret' },
      body: expect.any(FormData)
    });
    const body = request.mock.calls[0][1].body as FormData;
    expect(body.get('content')).toMatchObject({
      name: 'one.txt',
      size: 5,
      type: 'text/plain'
    });
  });

  it('downloads content using the same Bearer token', async () => {
    const request = vi.fn().mockResolvedValue(new Response('hello', { status: 200 }));
    vi.stubGlobal('fetch', request);

    const blob = await new FileApi('/files-api', 'secret').download('/files/one');

    expect(await blob.text()).toBe('hello');
    expect(request).toHaveBeenCalledWith('/files-api/files/content?path=%2Ffiles%2Fone', {
      headers: { Authorization: 'Bearer secret' }
    });
  });
});

describe('KasApi', () => {
  it('uses the configured API base and Bearer token', async () => {
    const request = vi.fn().mockResolvedValue(
      new Response('[]', {
        status: 200,
        headers: { 'Content-Type': 'application/json' }
      })
    );
    vi.stubGlobal('fetch', request);

    const resources = await new KasApi('/api/', 'secret').listResources();

    expect(resources).toEqual([]);
    expect(request).toHaveBeenCalledWith('/api/resources', {
      headers: {
        Authorization: 'Bearer secret'
      }
    });
  });

  it('asks KAS to filter Resources by exact Manifest', async () => {
    const request = vi.fn().mockResolvedValue(
      new Response('[]', {
        status: 200,
        headers: { 'Content-Type': 'application/json' }
      })
    );
    vi.stubGlobal('fetch', request);

    await new KasApi('/api', 'secret').listResources('/manifests/agent');

    expect(request).toHaveBeenCalledWith(
      '/api/resources?manifest=%2Fmanifests%2Fagent',
      expect.any(Object)
    );
  });

  it('surfaces the KAS JSON error message', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        new Response('{"error":"permission denied"}', {
          status: 403,
          headers: { 'Content-Type': 'application/json' }
        })
      )
    );

    await expect(new KasApi('/api', 'bad-token').listResources()).rejects.toEqual(
      new KasApiError('permission denied', 403)
    );
  });

  it('updates and deletes a Resource with optimistic revision checks', async () => {
    const request = vi.fn().mockImplementation(async () =>
      new Response(
        JSON.stringify({
          metadata: {
            path: '/agents/demo',
            manifest: '/manifests/agent',
            name: 'demo',
            state: 'available',
            '[kas]': {
              revision: 3,
              observed: {},
              created_at: '2026-01-01T00:00:00Z',
              updated_at: '2026-01-01T00:00:00Z'
            }
          },
          spec: { working_directory: '/tmp/demo' },
          status: {
            metadata: {
              path: '/agents/demo',
              manifest: '/manifests/agent',
              name: 'demo',
              state: 'available',
              '[kas]': {
                revision: 3,
                observed: {},
                created_at: '2026-01-01T00:00:00Z',
                updated_at: '2026-01-01T00:00:00Z'
              }
            },
            spec: { working_directory: '/tmp/demo' }
          }
        }),
        {
          status: 200,
          headers: { 'Content-Type': 'application/json' }
        }
      )
    );
    vi.stubGlobal('fetch', request);
    const api = new KasApi('/api', 'secret');

    await api.updateResource('/agents/demo', {
      expected_revision: 2,
      spec: { state: 'available', working_directory: '/tmp/demo' }
    });
    await api.deleteResource('/agents/demo', 3);

    expect(request).toHaveBeenNthCalledWith(
      1,
      '/api/resources/by-path?path=%2Fagents%2Fdemo',
      expect.objectContaining({
        method: 'PATCH',
        body: JSON.stringify({
          expected_revision: 2,
          spec: { state: 'available', working_directory: '/tmp/demo' }
        })
      })
    );
    expect(request).toHaveBeenNthCalledWith(
      2,
      '/api/resources/by-path?path=%2Fagents%2Fdemo&expected_revision=3',
      expect.objectContaining({ method: 'DELETE' })
    );
  });

  it('lists and reads any KAS object with related links', async () => {
    const request = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify([
            resourceDocument(
              '/agents/demo/service-account',
              '/builtin/service-account',
              'service-account'
            )
          ]),
          {
            status: 200,
            headers: { 'Content-Type': 'application/json' }
          }
        )
      )
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify([
            resourceDocument(
              '/agents/demo/service-account',
              '/builtin/service-account',
              'service-account'
            )
          ]),
          {
            status: 200,
            headers: { 'Content-Type': 'application/json' }
          }
        )
      );
    vi.stubGlobal('fetch', request);
    const api = new KasApi('/api', 'secret');

    await expect(api.listObjects('service_account')).resolves.toHaveLength(1);
    await expect(
      api.getObject('service_account', '/agents/demo/service-account')
    ).resolves.toMatchObject({
      kind: 'service_account',
      links: []
    });

    expect(request).toHaveBeenNthCalledWith(1, '/api/resources', expect.any(Object));
    expect(request).toHaveBeenNthCalledWith(2, '/api/resources', expect.any(Object));
  });
});

function resourceDocument(path: string, manifest: string, name: string) {
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
    metadata,
    spec: {},
    status: { metadata, spec: {} }
  };
}
