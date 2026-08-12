import { describe, expect, it } from 'vitest';
import { FRONTEND_PLUGIN_BUNDLE, frontendPluginEntries, isPluginRequest } from './plugins';
import type { Resource } from './types';

describe('frontend plugins', () => {
  it('expands available sidebar contributions backed by a bundle Link', () => {
    const plugin = {
      path: '/packages/studio/frontend/plugins/registry',
      manifest: '/packages/studio/frontend/manifest',
      name: 'Registry',
      state: 'available',
      status_state: 'available',
      spec: {
        api_version: 1,
        slug: 'registry',
        entrypoint: 'index.html',
        contributes: {
          sidebar: [
            {
              id: 'registry',
              label: 'Registry',
              description: 'All Resources',
              icon: '◇',
              section: 'workspace',
              order: 50,
              route: '/registry'
            }
          ]
        }
      },
      links: [
        {
          relation_path: FRONTEND_PLUGIN_BUNDLE,
          source: { path: '/packages/studio/frontend/plugins/registry' },
          target: { path: '/packages/studio/file/files/packages/studio/frontend/plugins/registry/bundle' }
        }
      ]
    } as unknown as Resource;

    expect(frontendPluginEntries([plugin])).toEqual([
      expect.objectContaining({
        pluginPath: plugin.path,
        bundlePath: '/packages/studio/file/files/packages/studio/frontend/plugins/registry/bundle',
        slug: 'registry',
        entrypoint: 'index.html',
        id: 'registry'
      })
    ]);
  });

  it('accepts only bridge request messages', () => {
    expect(
      isPluginRequest({
        source: 'kas-frontend-plugin',
        type: 'request',
        id: '1',
        method: 'resources.list',
        params: {}
      })
    ).toBe(true);
    expect(isPluginRequest({ source: 'other', type: 'request' })).toBe(false);
  });
});
