<script lang="ts">
  import { onMount } from 'svelte';
  import { KasApi, KasApiError } from './lib/api';
  import {
    MESSAGE_ACTION,
    buildUserMessage,
    firstMessageBody,
    groupThreads,
    messagesForAgent,
    slugify,
    threadRootOf
  } from './lib/chat';
  import type { Driver, Resource, Run } from './lib/types';

  const SETTINGS_KEY = 'kas-platform-settings';
  const DEFAULT_API_BASE = import.meta.env.VITE_KAS_API_URL || '/api';

  interface Settings {
    apiBase: string;
    token: string;
    userPath: string;
  }

  type View = 'chat' | 'agents';

  let settings: Settings = {
    apiBase: DEFAULT_API_BASE,
    token: '',
    userPath: '/users/admin'
  };
  let draftSettings: Settings = { ...settings };
  let agents: Resource[] = [];
  let messages: Resource[] = [];
  let selectedAgentPath = '';
  let activeThreadRoot: string | null = null;
  let driver: Driver | null = null;
  let loading = false;
  let sending = false;
  let connecting = false;
  let error = '';
  let notice = '';
  let composer = '';
  let showSettings = false;
  let showCreateAgent = false;
  let createName = '';
  let createPath = '';
  let createWorkingDirectory = '';
  let createInstructions = '';
  let view: View = 'chat';
  let showEditAgent = false;
  let editAgentPath = '';
  let editWorkingDirectory = '';
  let editInstructions = '';
  let deleteTarget: Resource | null = null;
  let savingAgent = false;
  let deletingAgentPath = '';

  $: selectedAgent = agents.find((agent) => agent.path === selectedAgentPath) ?? null;
  $: currentAgentMessages = selectedAgentPath
    ? messagesForAgent(messages, selectedAgentPath)
    : [];
  $: threadEntries = Array.from(groupThreads(currentAgentMessages).entries()).sort(
    ([, left], [, right]) =>
      (right.at(-1)?.created_at ?? '').localeCompare(left.at(-1)?.created_at ?? '')
  );
  $: activeMessages =
    activeThreadRoot === null
      ? []
      : currentAgentMessages.filter((message) => threadRootOf(message) === activeThreadRoot);

  onMount(() => {
    const saved = localStorage.getItem(SETTINGS_KEY);
    if (saved) {
      try {
        settings = { ...settings, ...(JSON.parse(saved) as Partial<Settings>) };
        draftSettings = { ...settings };
      } catch {
        localStorage.removeItem(SETTINGS_KEY);
      }
    }
    showSettings = !settings.token;
    if (settings.token) void connect();
  });

  function client(): KasApi {
    return new KasApi(settings.apiBase, settings.token);
  }

  async function connect(): Promise<void> {
    connecting = true;
    error = '';
    try {
      const api = client();
      if (!(await api.health())) throw new Error('KAS health check failed.');
      await loadData(api);
      showSettings = false;
      notice = 'Connected to KAS';
    } catch (cause) {
      error = messageOf(cause);
      showSettings = true;
    } finally {
      connecting = false;
    }
  }

  async function loadData(api = client()): Promise<void> {
    loading = true;
    try {
      const resources = await api.listResources();
      agents = resources
        .filter((resource) => resource.path.startsWith('/agents/'))
        .sort((left, right) => left.name.localeCompare(right.name));
      const messageResources = resources.filter((resource) =>
        resource.path.startsWith('/messages/')
      );
      messages = await Promise.all(
        messageResources.map((resource) => api.getResource(resource.path, true))
      );
      driver = await api.getAgentDriver();
      if (!agents.some((agent) => agent.path === selectedAgentPath)) {
        selectedAgentPath = agents[0]?.path ?? '';
        activeThreadRoot = null;
      }
      syncThreadSelection();
    } finally {
      loading = false;
    }
  }

  function syncThreadSelection(): void {
    if (!selectedAgentPath) {
      activeThreadRoot = null;
      return;
    }
    const grouped = groupThreads(messagesForAgent(messages, selectedAgentPath));
    if (activeThreadRoot && grouped.has(activeThreadRoot)) return;
    const latest = Array.from(grouped.entries()).sort(([, left], [, right]) =>
      (right.at(-1)?.created_at ?? '').localeCompare(left.at(-1)?.created_at ?? '')
    )[0];
    activeThreadRoot = latest?.[0] ?? null;
  }

  function chooseAgent(path: string): void {
    selectedAgentPath = path;
    view = 'chat';
    activeThreadRoot = null;
    syncThreadSelection();
    error = '';
  }

  function openAgentManagement(): void {
    view = 'agents';
    error = '';
    notice = '';
  }

  function startThread(): void {
    activeThreadRoot = null;
    composer = '';
  }

  async function saveSettings(): Promise<void> {
    settings = {
      apiBase: draftSettings.apiBase.trim() || '/api',
      token: draftSettings.token.trim(),
      userPath: draftSettings.userPath.trim()
    };
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
    await connect();
  }

  function openAgentDialog(): void {
    createName = '';
    createPath = '';
    createWorkingDirectory = '';
    createInstructions = '';
    showCreateAgent = true;
    error = '';
  }

  function updateAgentName(): void {
    createPath = `/agents/${slugify(createName)}`;
  }

  async function createAgent(): Promise<void> {
    error = '';
    const name = createName.trim();
    const path = createPath.trim();
    if (!name || !path || !createWorkingDirectory.trim()) {
      error = 'Agent name, path, and working directory are required.';
      return;
    }
    loading = true;
    try {
      await client().createResource({
        path,
        manifest: '/manifests/agent',
        name,
        spec: {
          instructions: createInstructions.trim(),
          working_directory: createWorkingDirectory.trim()
        }
      });
      await loadData();
      selectedAgentPath = path;
      activeThreadRoot = null;
      showCreateAgent = false;
      notice = `${name} was created`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      loading = false;
    }
  }

  function openEditAgent(agent: Resource): void {
    editAgentPath = agent.path;
    editWorkingDirectory = stringSpec(agent, 'working_directory');
    editInstructions = stringSpec(agent, 'instructions');
    showEditAgent = true;
    error = '';
  }

  async function saveAgent(): Promise<void> {
    const agent = agents.find((candidate) => candidate.path === editAgentPath);
    if (!agent || !editWorkingDirectory.trim()) {
      error = 'Working directory is required.';
      return;
    }
    savingAgent = true;
    error = '';
    try {
      await client().updateResource(agent.path, {
        expected_revision: agent.revision,
        spec: {
          ...agent.spec,
          instructions: editInstructions.trim(),
          working_directory: editWorkingDirectory.trim()
        }
      });
      showEditAgent = false;
      await loadData();
      notice = `${agent.name} update requested`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingAgent = false;
    }
  }

  async function deleteAgent(): Promise<void> {
    const agent = deleteTarget;
    if (!agent) return;
    deletingAgentPath = agent.path;
    error = '';
    try {
      const api = client();
      await api.deleteResource(agent.path, agent.revision);
      deleteTarget = null;
      notice = `Deleting ${agent.name}…`;
      await waitForResourceDeletion(api, agent.path);
      await loadData(api);
      notice = `${agent.name} was deleted`;
    } catch (cause) {
      error = messageOf(cause);
      await loadData().catch(() => undefined);
    } finally {
      deletingAgentPath = '';
    }
  }

  async function waitForResourceDeletion(api: KasApi, path: string): Promise<void> {
    const deadline = Date.now() + 20_000;
    while (Date.now() < deadline) {
      try {
        await api.getResource(path);
      } catch (cause) {
        if (cause instanceof KasApiError && cause.status === 404) return;
        throw cause;
      }
      await new Promise((resolve) => setTimeout(resolve, 400));
    }
    throw new Error('Agent deletion is still waiting for Driver reconciliation.');
  }

  async function sendMessage(): Promise<void> {
    const body = composer.trim();
    if (!body || !selectedAgent || sending) return;
    sending = true;
    error = '';
    notice = 'Codex is working…';
    const parent = activeMessages.at(-1)?.path ?? null;
    const messageId = crypto.randomUUID();
    const userMessage = buildUserMessage(
      messageId,
      body,
      settings.userPath,
      selectedAgent.path,
      activeThreadRoot,
      parent
    );
    const root = activeThreadRoot ?? userMessage.path;
    try {
      const api = client();
      await api.createResource(userMessage);
      composer = '';
      activeThreadRoot = root;
      await loadData(api);
      const requestId = crypto.randomUUID();
      const runPath = `${selectedAgent.path}/runs/${requestId}`;
      await api.createRun({
        path: runPath,
        request_id: requestId,
        resource: selectedAgent.path,
        action: MESSAGE_ACTION,
        input: { message_path: userMessage.path }
      });
      const run = await waitForRun(api, runPath);
      if (run.status !== 'succeeded') {
        throw new Error(run.error || `Agent Run ended as ${run.status}.`);
      }
      await loadData(api);
      activeThreadRoot = root;
      notice = 'Reply received';
    } catch (cause) {
      error = messageOf(cause);
      await loadData().catch(() => undefined);
    } finally {
      sending = false;
    }
  }

  async function waitForRun(api: KasApi, path: string): Promise<Run> {
    const deadline = Date.now() + 180_000;
    while (Date.now() < deadline) {
      const run = await api.getRun(path);
      if (['succeeded', 'failed', 'cancelled'].includes(run.status)) return run;
      await new Promise((resolve) => setTimeout(resolve, 600));
    }
    throw new Error('Codex did not reply within three minutes.');
  }

  function messageOf(cause: unknown): string {
    return cause instanceof Error ? cause.message : String(cause);
  }

  function roleOf(message: Resource): string {
    return typeof message.spec.role === 'string' ? message.spec.role : 'unknown';
  }

  function bodyOf(message: Resource): string {
    return typeof message.spec.body === 'string' ? message.spec.body : '';
  }

  function stringSpec(resource: Resource, key: string): string {
    const value = resource.spec[key];
    return typeof value === 'string' ? value : '';
  }

  function resourceState(resource: Resource): string {
    const value = resource.status.state;
    return typeof value === 'string' ? value : 'unknown';
  }

  function resourceConverged(resource: Resource): boolean {
    return JSON.stringify(resource.spec) === JSON.stringify(resource.status);
  }

  function timeOf(timestamp: string): string {
    return new Intl.DateTimeFormat(undefined, {
      hour: '2-digit',
      minute: '2-digit'
    }).format(new Date(timestamp));
  }
</script>

<svelte:head>
  <title>{selectedAgent ? `${selectedAgent.name} · KAS` : 'KAS Agent Console'}</title>
</svelte:head>

<div class="shell">
  <aside class="sidebar">
    <header class="brand">
      <div class="brand-mark">K</div>
      <div>
        <strong>KAS</strong>
        <span>Agent console</span>
      </div>
    </header>

    <div class="sidebar-section-title">
      <span>Agents</span>
      <span class="sidebar-title-actions">
        <button class="icon-button" aria-label="Manage Agents" onclick={openAgentManagement}>≡</button>
        <button class="icon-button" aria-label="Create Agent" onclick={openAgentDialog}>+</button>
      </span>
    </div>

    <nav class="agent-list" aria-label="Agents">
      {#if agents.length === 0 && !loading}
        <button class="empty-agent" onclick={openAgentDialog}>
          <span>+</span>
          Create your first Agent
        </button>
      {/if}
      {#each agents as agent}
        <button
          class:active={agent.path === selectedAgentPath}
          class="agent-item"
          onclick={() => chooseAgent(agent.path)}
        >
          <span class="agent-avatar">{agent.name.slice(0, 1).toUpperCase()}</span>
          <span class="agent-copy">
            <strong>{agent.name}</strong>
            <small class:syncing={!resourceConverged(agent)}>{resourceState(agent)}</small>
          </span>
        </button>
      {/each}
    </nav>

    <footer class="sidebar-footer">
      <div class="connection">
        <span class:online={driver?.state === 'ready'} class="status-dot"></span>
        <span>
          <strong>{driver?.state ?? 'disconnected'}</strong>
          <small>Agent Driver</small>
        </span>
      </div>
      <button class="settings-button" aria-label="Connection settings" onclick={() => (showSettings = true)}>
        ⚙
      </button>
    </footer>
  </aside>

  <main class="workspace">
    <header class="workspace-header">
      <div>
        <p class="eyebrow">{view === 'agents' ? 'Workspace' : 'Current Agent'}</p>
        <h1>{view === 'agents' ? 'Agent management' : (selectedAgent?.name ?? 'Choose an Agent')}</h1>
      </div>
      <div class="header-actions">
        {#if view === 'agents'}
          <button class="quiet-button" disabled={!selectedAgent} onclick={() => (view = 'chat')}>
            Open chat
          </button>
          <button class="primary-button" onclick={openAgentDialog}>Create Agent</button>
        {:else}
          <button class="quiet-button" onclick={openAgentManagement}>Manage Agents</button>
          <button class="quiet-button" disabled={!selectedAgent} onclick={startThread}>
            New thread
          </button>
        {/if}
        <button
          class="refresh-button"
          aria-label="Refresh"
          disabled={loading}
          onclick={() => loadData().catch((cause) => (error = messageOf(cause)))}
        >
          ↻
        </button>
      </div>
    </header>

    {#if view === 'chat' && selectedAgent}
      <div class="thread-strip" aria-label="Conversations">
        {#each threadEntries as [root, thread]}
          <button
            class:active={root === activeThreadRoot}
            onclick={() => (activeThreadRoot = root)}
          >
            <span>{firstMessageBody(thread)}</span>
            <small>{thread.length}</small>
          </button>
        {/each}
        {#if threadEntries.length === 0}
          <span class="thread-hint">No conversations yet</span>
        {/if}
      </div>
    {/if}

    {#if error}
      <div class="banner error-banner" role="alert">
        <span>{error}</span>
        <button aria-label="Dismiss error" onclick={() => (error = '')}>×</button>
      </div>
    {:else if notice}
      <div class="banner notice-banner">
        <span>{notice}</span>
        <button aria-label="Dismiss notice" onclick={() => (notice = '')}>×</button>
      </div>
    {/if}

    {#if view === 'agents'}
      <section class="agent-management" aria-label="Agent management">
        <div class="management-summary">
          <div>
            <strong>{agents.length}</strong>
            <span>Agents</span>
          </div>
          <div>
            <strong>{agents.filter(resourceConverged).length}</strong>
            <span>Converged</span>
          </div>
          <div>
            <strong>{driver?.state ?? 'offline'}</strong>
            <span>Driver</span>
          </div>
        </div>

        {#if agents.length === 0 && !loading}
          <div class="management-empty">
            <p class="eyebrow">No Agents</p>
            <h2>Create the first working context.</h2>
            <button class="primary-button" onclick={openAgentDialog}>Create Agent</button>
          </div>
        {:else}
          <div class="agent-grid">
            {#each agents as agent}
              <article class="agent-card">
                <div class="agent-card-head">
                  <span class="agent-avatar large">{agent.name.slice(0, 1).toUpperCase()}</span>
                  <div>
                    <h2>{agent.name}</h2>
                    <code>{agent.path}</code>
                  </div>
                  <span
                    class:pending={!resourceConverged(agent)}
                    class="state-pill"
                  >
                    {resourceConverged(agent) ? resourceState(agent) : 'reconciling'}
                  </span>
                </div>
                <dl class="agent-details">
                  <div>
                    <dt>Working directory</dt>
                    <dd>{stringSpec(agent, 'working_directory')}</dd>
                  </div>
                  <div>
                    <dt>Instructions</dt>
                    <dd>{stringSpec(agent, 'instructions') || 'No custom instructions'}</dd>
                  </div>
                  <div>
                    <dt>Revision</dt>
                    <dd>{agent.revision}</dd>
                  </div>
                </dl>
                <div class="agent-card-actions">
                  <button class="quiet-button" onclick={() => chooseAgent(agent.path)}>
                    Open chat
                  </button>
                  <button class="quiet-button" onclick={() => openEditAgent(agent)}>Edit</button>
                  <button
                    class="danger-button"
                    disabled={deletingAgentPath === agent.path}
                    onclick={() => (deleteTarget = agent)}
                  >
                    {deletingAgentPath === agent.path ? 'Deleting…' : 'Delete'}
                  </button>
                </div>
              </article>
            {/each}
          </div>
        {/if}
      </section>
    {:else}
      <section class="conversation" aria-live="polite">
        {#if !selectedAgent}
          <div class="empty-state">
            <div class="empty-orbit"><span>K</span></div>
            <p class="eyebrow">No Agent selected</p>
            <h2>Give Codex a place to work.</h2>
            <p>Create an Agent with a working directory, then start a conversation.</p>
            <button class="primary-button" onclick={openAgentDialog}>Create Agent</button>
          </div>
        {:else if activeMessages.length === 0}
          <div class="empty-state compact">
            <p class="eyebrow">New conversation</p>
            <h2>What should {selectedAgent.name} work on?</h2>
            <p>Your message becomes a Resource. The reply does too.</p>
          </div>
        {:else}
          <div class="message-list">
            {#each activeMessages as message}
              <article class:assistant={roleOf(message) === 'assistant'} class="message">
                <div class="message-meta">
                  <span>{roleOf(message) === 'assistant' ? selectedAgent.name : 'You'}</span>
                  <time datetime={message.created_at}>{timeOf(message.created_at)}</time>
                </div>
                <p>{bodyOf(message)}</p>
              </article>
            {/each}
            {#if sending}
              <article class="message assistant pending">
                <div class="message-meta"><span>{selectedAgent.name}</span></div>
                <div class="thinking"><i></i><i></i><i></i></div>
              </article>
            {/if}
          </div>
        {/if}
      </section>
    {/if}

    {#if view === 'chat' && selectedAgent}
      <form class="composer" onsubmit={(event) => { event.preventDefault(); void sendMessage(); }}>
        <textarea
          bind:value={composer}
          aria-label="Message"
          placeholder={`Message ${selectedAgent.name}…`}
          rows="1"
          disabled={sending}
          onkeydown={(event) => {
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault();
              void sendMessage();
            }
          }}
        ></textarea>
        <button class="send-button" type="submit" disabled={sending || !composer.trim()}>
          {sending ? '···' : '↑'}
        </button>
        <div class="composer-note">Enter to send · Shift + Enter for a new line</div>
      </form>
    {/if}
  </main>
</div>

{#if showSettings}
  <div class="modal-backdrop" role="presentation">
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby="settings-title">
      <div class="modal-kicker">Connection</div>
      <h2 id="settings-title">Connect to KAS</h2>
      <p>Credentials stay in this browser.</p>
      <form onsubmit={(event) => { event.preventDefault(); void saveSettings(); }}>
        <label>
          API base
          <input bind:value={draftSettings.apiBase} placeholder="/api" />
        </label>
        <label>
          Bearer token
          <input bind:value={draftSettings.token} type="password" autocomplete="off" required />
        </label>
        <label>
          Your User path
          <input bind:value={draftSettings.userPath} placeholder="/users/admin" required />
        </label>
        <div class="modal-actions">
          {#if settings.token}
            <button type="button" class="quiet-button" onclick={() => (showSettings = false)}>
              Cancel
            </button>
          {/if}
          <button type="submit" class="primary-button" disabled={connecting}>
            {connecting ? 'Connecting…' : 'Connect'}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}

{#if showCreateAgent}
  <div class="modal-backdrop" role="presentation">
    <div class="modal wide" role="dialog" aria-modal="true" aria-labelledby="agent-title">
      <div class="modal-kicker">New Resource</div>
      <h2 id="agent-title">Create an Agent</h2>
      <p>Each Agent uses the shared Codex Driver but keeps its own working context.</p>
      <form onsubmit={(event) => { event.preventDefault(); void createAgent(); }}>
        <div class="field-grid">
          <label>
            Name
            <input
              bind:value={createName}
              oninput={updateAgentName}
              placeholder="Release planner"
              required
            />
          </label>
          <label>
            Resource path
            <input bind:value={createPath} placeholder="/agents/release-planner" required />
          </label>
        </div>
        <label>
          Working directory
          <input bind:value={createWorkingDirectory} placeholder="/absolute/path/to/project" required />
        </label>
        <label>
          Instructions
          <textarea
            bind:value={createInstructions}
            rows="4"
            placeholder="How should this Agent approach its work?"
          ></textarea>
        </label>
        <div class="modal-actions">
          <button type="button" class="quiet-button" onclick={() => (showCreateAgent = false)}>
            Cancel
          </button>
          <button type="submit" class="primary-button" disabled={loading}>
            {loading ? 'Creating…' : 'Create Agent'}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}

{#if showEditAgent}
  <div class="modal-backdrop" role="presentation">
    <div class="modal wide" role="dialog" aria-modal="true" aria-labelledby="edit-agent-title">
      <div class="modal-kicker">Agent Resource</div>
      <h2 id="edit-agent-title">Edit Agent</h2>
      <p>
        Updating spec schedules reconciliation through the shared Agent Driver.
      </p>
      <form onsubmit={(event) => { event.preventDefault(); void saveAgent(); }}>
        <label>
          Resource path
          <input value={editAgentPath} disabled />
        </label>
        <label>
          Working directory
          <input bind:value={editWorkingDirectory} required />
        </label>
        <label>
          Instructions
          <textarea bind:value={editInstructions} rows="5"></textarea>
        </label>
        <div class="modal-actions">
          <button type="button" class="quiet-button" onclick={() => (showEditAgent = false)}>
            Cancel
          </button>
          <button type="submit" class="primary-button" disabled={savingAgent}>
            {savingAgent ? 'Saving…' : 'Save changes'}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}

{#if deleteTarget}
  <div class="modal-backdrop" role="presentation">
    <div class="modal destructive-modal" role="alertdialog" aria-modal="true" aria-labelledby="delete-agent-title">
      <div class="modal-kicker danger-text">Delete Resource</div>
      <h2 id="delete-agent-title">Delete {deleteTarget.name}?</h2>
      <p>
        KAS will set its desired state to deleted. The Agent Driver will reconcile it before
        the Resource and its Runs are removed.
      </p>
      <code class="delete-path">{deleteTarget.path}</code>
      <div class="modal-actions">
        <button type="button" class="quiet-button" onclick={() => (deleteTarget = null)}>
          Cancel
        </button>
        <button type="button" class="danger-button solid" onclick={() => void deleteAgent()}>
          Delete Agent
        </button>
      </div>
    </div>
  </div>
{/if}
