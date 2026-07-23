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
});

