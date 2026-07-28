export type EmbeddedView = 'agents' | 'skills' | 'approvals' | 'threads' | 'telegram';

interface HostContext {
  subject?: {
    path?: string;
  };
  workspace?: {
    activeThread?: string | null;
    selectedResource?: string;
  };
}

interface PendingFetch {
  resolve: (response: Response) => void;
  reject: (error: Error) => void;
}

const HOST_SOURCE = 'kas-frontend-host';
const PLUGIN_SOURCE = 'kas-frontend-plugin';
const embeddedNames = new Set<EmbeddedView>([
  'agents',
  'skills',
  'approvals',
  'threads',
  'telegram'
]);
const pending = new Map<string, PendingFetch>();
let sequence = 0;
let resolveContext: ((context: HostContext) => void) | undefined;

export const embeddedView = detectEmbeddedView();
export const embeddedContext = new Promise<HostContext>((resolve) => {
  resolveContext = resolve;
});

if (embeddedView) {
  window.addEventListener('message', receiveHostMessage);
  installFetchBridge();
  window.parent.postMessage({ source: PLUGIN_SOURCE, type: 'ready' }, '*');
}

function detectEmbeddedView(): EmbeddedView | null {
  const name = window.location.pathname.split('/').at(-1)?.replace(/\.html$/, '') ?? '';
  return embeddedNames.has(name as EmbeddedView) ? (name as EmbeddedView) : null;
}

function receiveHostMessage(event: MessageEvent<unknown>): void {
  if (!isRecord(event.data) || event.data.source !== HOST_SOURCE) return;
  if (event.data.type === 'context') {
    resolveContext?.((event.data.context as HostContext | undefined) ?? {});
    resolveContext = undefined;
    return;
  }
  if (event.data.type !== 'response' || typeof event.data.id !== 'string') return;
  const operation = pending.get(event.data.id);
  if (!operation) return;
  pending.delete(event.data.id);
  if (typeof event.data.error === 'string' && event.data.error) {
    operation.reject(new Error(event.data.error));
    return;
  }
  if (!isRecord(event.data.result)) {
    operation.reject(new Error('Frontend host returned an invalid response.'));
    return;
  }
  const body = event.data.result.body;
  operation.resolve(
    new Response(body instanceof ArrayBuffer ? body : undefined, {
      status: numberValue(event.data.result.status, 500),
      statusText: stringValue(event.data.result.statusText),
      headers: stringRecord(event.data.result.headers)
    })
  );
}

function installFetchBridge(): void {
  window.fetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const request = new Request(input, init);
    const url = new URL(request.url, window.location.href);
    if (url.origin !== window.location.origin) {
      throw new Error('Embedded Frontend Plugins may only request the KAS gateway origin.');
    }
    const body =
      request.method === 'GET' || request.method === 'HEAD'
        ? undefined
        : await request.arrayBuffer();
    const id = `embedded-fetch-${++sequence}`;
    const response = new Promise<Response>((resolve, reject) => {
      pending.set(id, { resolve, reject });
    });
    window.parent.postMessage(
      {
        source: PLUGIN_SOURCE,
        type: 'request',
        id,
        method: 'gateway.fetch',
        params: {
          path: `${url.pathname}${url.search}`,
          method: request.method,
          headers: Object.fromEntries(request.headers.entries()),
          body
        }
      },
      '*'
    );
    return response;
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringValue(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function numberValue(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function stringRecord(value: unknown): Record<string, string> {
  if (!isRecord(value)) return {};
  return Object.fromEntries(
    Object.entries(value).filter(
      (entry): entry is [string, string] => typeof entry[1] === 'string'
    )
  );
}
