import { afterEach, describe, expect, it, vi } from 'vitest';
import { KasApi, KasApiError } from './api';

afterEach(() => {
  vi.unstubAllGlobals();
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
          path: '/agents/demo',
          name: 'demo',
          spec: { state: 'available' },
          status: { state: 'available' },
          revision: 3,
          created_at: '2026-01-01T00:00:00Z',
          updated_at: '2026-01-01T00:00:00Z'
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
            { kind: 'service_account', path: '/agents/demo/service-account' }
          ]),
          {
            status: 200,
            headers: { 'Content-Type': 'application/json' }
          }
        )
      )
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            kind: 'service_account',
            path: '/agents/demo/service-account',
            value: { path: '/agents/demo/service-account' },
            links: []
          }),
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

    expect(request).toHaveBeenNthCalledWith(
      1,
      '/api/objects?kind=service_account',
      expect.any(Object)
    );
    expect(request).toHaveBeenNthCalledWith(
      2,
      '/api/objects/by-path?kind=service_account&path=%2Fagents%2Fdemo%2Fservice-account&include=links',
      expect.any(Object)
    );
  });
});
