import type { Resource } from './types';

export const FRONTEND_PLUGIN_MANIFEST = '/manifests/frontend-plugin';
export const FRONTEND_PLUGIN_BUNDLE = '/manifests/frontend-plugin/relations/bundle';

export interface FrontendPluginEntry {
  pluginPath: string;
  bundlePath: string;
  slug: string;
  entrypoint: string;
  id: string;
  label: string;
  description: string;
  icon: string;
  section: 'workspace' | 'resources';
  order: number;
  route: string;
}

export interface PluginRequest {
  source: 'kas-frontend-plugin';
  type: 'request';
  id: string;
  method: string;
  params?: Record<string, unknown>;
}

interface SidebarContribution {
  id: string;
  label: string;
  description?: string;
  icon: string;
  section: 'workspace' | 'resources';
  order: number;
  route: string;
}

export function frontendPluginEntries(plugins: Resource[]): FrontendPluginEntry[] {
  return plugins
    .filter(
      (plugin) =>
        plugin.manifest === FRONTEND_PLUGIN_MANIFEST &&
        plugin.state === 'available' &&
        plugin.status_state === 'available' &&
        plugin.spec.api_version === 1
    )
    .flatMap((plugin) => {
      const bundle = (plugin.links ?? []).find(
        (link) =>
          link.relation_path === FRONTEND_PLUGIN_BUNDLE &&
          link.source.path === plugin.path
      );
      if (!bundle) return [];
      return sidebarContributions(plugin.spec.contributes).map((entry) => ({
        pluginPath: plugin.path,
        bundlePath: bundle.target.path,
        slug: String(plugin.spec.slug),
        entrypoint: String(plugin.spec.entrypoint),
        id: entry.id,
        label: entry.label,
        description: entry.description ?? 'Frontend plugin',
        icon: entry.icon,
        section: entry.section,
        order: entry.order,
        route: entry.route
      }));
    })
    .sort((left, right) => left.order - right.order || left.label.localeCompare(right.label));
}

export function isPluginRequest(value: unknown): value is PluginRequest {
  if (!isRecord(value)) return false;
  return (
    value.source === 'kas-frontend-plugin' &&
    value.type === 'request' &&
    typeof value.id === 'string' &&
    typeof value.method === 'string' &&
    (value.params === undefined || isRecord(value.params))
  );
}

function sidebarContributions(value: unknown): SidebarContribution[] {
  if (!isRecord(value) || !Array.isArray(value.sidebar)) return [];
  return value.sidebar.filter((entry): entry is SidebarContribution => {
    if (!isRecord(entry)) return false;
    return (
      typeof entry.id === 'string' &&
      typeof entry.label === 'string' &&
      (entry.description === undefined || typeof entry.description === 'string') &&
      typeof entry.icon === 'string' &&
      (entry.section === 'workspace' || entry.section === 'resources') &&
      typeof entry.order === 'number' &&
      Number.isInteger(entry.order) &&
      typeof entry.route === 'string'
    );
  });
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
