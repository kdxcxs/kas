<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { KasApi, KasApiError } from './lib/api';
  import {
    AGENT_MANIFEST,
    AUTHORED_BY,
    MESSAGE_MANIFEST,
    PARTICIPANTS,
    THREAD_MANIFEST,
    buildThread,
    buildUserMessage,
    mentionHandle,
    mentionedAgentPaths,
    mentionRunPath,
    messagesForThread,
    participantAgentPaths,
    participantsForThread,
    relationTarget,
    slugify,
    threadParticipantLink,
    threadsForAgent
  } from './lib/chat';
  import type {
    Driver,
    ObjectDetail,
    ObjectKind,
    ObjectRef,
    Resource,
    Run
  } from './lib/types';

  const SETTINGS_KEY = 'kas-platform-settings';
  const DEFAULT_API_BASE = import.meta.env.VITE_KAS_API_URL || '/api';

  interface Settings {
    apiBase: string;
    token: string;
    userPath: string;
  }

  type View = 'chat' | 'agents' | 'threads' | 'objects';

  const OBJECT_KINDS: ObjectKind[] = [
    'resource',
    'link',
    'manifest',
    'relation',
    'action',
    'driver',
    'run',
    'user',
    'service_account',
    'role',
    'role_binding',
    'credential',
    'package'
  ];

  let settings: Settings = {
    apiBase: DEFAULT_API_BASE,
    token: '',
    userPath: '/users/admin'
  };
  let draftSettings: Settings = { ...settings };
  let agents: Resource[] = [];
  let threads: Resource[] = [];
  let messages: Resource[] = [];
  let selectedAgentPath = '';
  let activeThreadPath: string | null = null;
  let driver: Driver | null = null;
  let loading = false;
  let sending = false;
  let connecting = false;
  let error = '';
  let notice = '';
  let composer = '';
  let showSettings = false;
  let showCreateAgent = false;
  let showCreateThread = false;
  let createThreadTitle = '';
  let createThreadAgents: string[] = [];
  let selectedManagedThreadPath = '';
  let editThreadTitle = '';
  let editThreadAgents: string[] = [];
  let savingThread = false;
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
  let objects: ObjectRef[] = [];
  let objectKind: ObjectKind = 'resource';
  let objectSearch = '';
  let selectedObjectPath = '';
  let objectDetail: ObjectDetail | null = null;
  let loadingObjects = false;
  let objectDetailElement: HTMLElement;

  $: activeThread =
    activeThreadPath === null
      ? null
      : threads.find((thread) => thread.path === activeThreadPath) ?? null;
  $: activeMessages = activeThread ? messagesForThread(messages, activeThread.path) : [];
  $: activeParticipants = activeThread ? participantsForThread(activeThread, agents) : [];
  $: managedThread =
    threads.find((thread) => thread.path === selectedManagedThreadPath) ?? null;
  $: filteredObjects = objects.filter(
    (object) =>
      object.kind === objectKind &&
      object.path.toLowerCase().includes(objectSearch.trim().toLowerCase())
  );
  $: objectCounts = Object.fromEntries(
    OBJECT_KINDS.map((kind) => [
      kind,
      objects.filter((object) => object.kind === kind).length
    ])
  ) as Record<ObjectKind, number>;
  $: pageTitle =
    view === 'agents'
      ? 'Agent management'
      : view === 'threads'
        ? 'Thread management'
        : view === 'objects'
          ? 'Object explorer'
          : activeThread
            ? titleOf(activeThread)
            : 'Choose a Thread';

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
      const [agentResources, threadResources, messageResources] = await Promise.all([
        api.listResources(AGENT_MANIFEST),
        api.listResources(THREAD_MANIFEST),
        api.listResources(MESSAGE_MANIFEST)
      ]);
      agents = agentResources
        .filter((resource) => resource.manifest === AGENT_MANIFEST)
        .sort((left, right) => left.name.localeCompare(right.name));
      threads = (
        await Promise.all(
          threadResources
            .filter((resource) => resource.manifest === THREAD_MANIFEST)
            .map((resource) => api.getResource(resource.path, true))
        )
      ).sort((left, right) => right.updated_at.localeCompare(left.updated_at));
      messages = await Promise.all(
        messageResources
          .filter((resource) => resource.manifest === MESSAGE_MANIFEST)
          .map((resource) => api.getResource(resource.path, true))
      );
      driver = await api.getAgentDriver();
      if (!agents.some((agent) => agent.path === selectedAgentPath)) {
        selectedAgentPath = agents[0]?.path ?? '';
      }
      syncThreadSelection();
    } finally {
      loading = false;
    }
  }

  function syncThreadSelection(): void {
    if (activeThreadPath && threads.some((thread) => thread.path === activeThreadPath)) return;
    activeThreadPath = threads[0]?.path ?? null;
  }

  function chooseThread(path: string): void {
    activeThreadPath = path;
    view = 'chat';
    error = '';
    notice = '';
  }

  function chooseAgent(path: string): void {
    selectedAgentPath = path;
    view = 'chat';
    activeThreadPath = threadsForAgent(threads, path)[0]?.path ?? null;
    error = '';
  }

  function openAgentManagement(): void {
    view = 'agents';
    error = '';
    notice = '';
  }

  function openThreadManagement(path = activeThreadPath): void {
    view = 'threads';
    const requested = path ? threads.find((thread) => thread.path === path) : null;
    selectManagedThread(requested?.path ?? threads[0]?.path ?? '');
    error = '';
    notice = '';
  }

  function selectManagedThread(path: string): void {
    selectedManagedThreadPath = path;
    const thread = threads.find((candidate) => candidate.path === path);
    editThreadTitle = thread ? titleOf(thread) : '';
    editThreadAgents = thread ? participantAgentPaths(thread) : [];
  }

  function toggleManagedThreadAgent(path: string): void {
    editThreadAgents = editThreadAgents.includes(path)
      ? editThreadAgents.filter((candidate) => candidate !== path)
      : [...editThreadAgents, path];
  }

  async function saveManagedThread(): Promise<void> {
    if (!managedThread) return;
    const title = editThreadTitle.trim();
    if (!title || editThreadAgents.length === 0) {
      error = 'A Thread title and at least one Agent are required.';
      return;
    }

    savingThread = true;
    error = '';
    try {
      const api = client();
      const currentLinks =
        managedThread.links?.filter(
          (entry) =>
            entry.relation_path === PARTICIPANTS &&
            entry.source.path === managedThread?.path &&
            entry.target.path.startsWith('/agents/')
        ) ?? [];
      const currentPaths = new Set(currentLinks.map((entry) => entry.target.path));
      const nextPaths = new Set(editThreadAgents);

      await api.updateResource(managedThread.path, {
        expected_revision: managedThread.revision,
        spec: { ...managedThread.spec, title }
      });
      await Promise.all(
        editThreadAgents
          .filter((path) => !currentPaths.has(path))
          .map((path) => api.createLink(threadParticipantLink(managedThread!.path, path)))
      );
      await Promise.all(
        currentLinks
          .filter((entry) => !nextPaths.has(entry.target.path))
          .map((entry) => api.deleteResource(entry.path, entry.revision))
      );

      const path = managedThread.path;
      await loadData(api);
      selectManagedThread(path);
      notice = `${title} was updated`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingThread = false;
    }
  }

  function openManagedThreadChat(): void {
    if (!managedThread) return;
    activeThreadPath = managedThread.path;
    view = 'chat';
    error = '';
    notice = '';
  }

  async function openObjectExplorer(): Promise<void> {
    view = 'objects';
    error = '';
    notice = '';
    await loadObjects();
  }

  async function loadObjects(api = client()): Promise<void> {
    loadingObjects = true;
    try {
      objects = (await api.listObjects()).sort(
        (left, right) =>
          OBJECT_KINDS.indexOf(left.kind) - OBJECT_KINDS.indexOf(right.kind) ||
          left.path.localeCompare(right.path)
      );
      const selected = objects.find(
        (object) => object.kind === objectKind && object.path === selectedObjectPath
      );
      const next = selected ?? objects.find((object) => object.kind === objectKind) ?? objects[0];
      if (!next) {
        selectedObjectPath = '';
        objectDetail = null;
        return;
      }
      objectKind = next.kind;
      await selectObject(next, api);
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      loadingObjects = false;
    }
  }

  async function chooseObjectKind(kind: ObjectKind): Promise<void> {
    objectKind = kind;
    objectSearch = '';
    const next = objects.find((object) => object.kind === kind);
    if (next) {
      await selectObject(next);
    } else {
      selectedObjectPath = '';
      objectDetail = null;
    }
  }

  async function selectObject(object: ObjectRef, api = client()): Promise<void> {
    objectKind = object.kind;
    selectedObjectPath = object.path;
    loadingObjects = true;
    error = '';
    try {
      objectDetail = await api.getObject(object.kind, object.path);
      await tick();
      objectDetailElement?.scrollTo({ top: 0 });
    } catch (cause) {
      error = messageOf(cause);
      objectDetail = null;
    } finally {
      loadingObjects = false;
    }
  }

  async function reloadObjectDetail(): Promise<void> {
    if (!objectDetail) return;
    await selectObject({ kind: objectDetail.kind, path: objectDetail.path });
  }

  async function selectEndpoint(object: ObjectRef | null): Promise<void> {
    if (object) await selectObject(object);
  }

  async function refreshCurrentView(): Promise<void> {
    if (view === 'objects') {
      await loadObjects();
    } else {
      await loadData();
      if (view === 'threads') {
        selectManagedThread(selectedManagedThreadPath || threads[0]?.path || '');
      }
    }
  }

  function startThread(): void {
    if (agents.length === 0) {
      openAgentDialog();
      return;
    }
    createThreadTitle = 'New conversation';
    createThreadAgents = [];
    showCreateThread = true;
    error = '';
  }

  function toggleThreadAgent(path: string): void {
    createThreadAgents = createThreadAgents.includes(path)
      ? createThreadAgents.filter((candidate) => candidate !== path)
      : [...createThreadAgents, path];
  }

  async function createThread(): Promise<void> {
    const title = createThreadTitle.trim();
    if (!title || createThreadAgents.length === 0) {
      error = 'A Thread title and at least one Agent are required.';
      return;
    }
    loading = true;
    error = '';
    try {
      const resource = buildThread(
        crypto.randomUUID(),
        title,
        settings.userPath,
        createThreadAgents
      );
      await client().createResource(resource);
      await loadData();
      activeThreadPath = resource.path;
      selectedAgentPath = createThreadAgents[0];
      showCreateThread = false;
      composer = '';
      if (view === 'threads') selectManagedThread(resource.path);
      notice = `${title} was created`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      loading = false;
    }
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
    if (!body || !activeThread || sending) return;
    sending = true;
    error = '';
    const mentioned = mentionedAgentPaths(body, activeParticipants);
    notice = mentioned.length > 0 ? 'Mentioned Agents are working…' : 'Sending Message…';
    const parent = activeMessages.at(-1)?.path ?? null;
    const messageId = crypto.randomUUID();
    const userMessage = buildUserMessage(
      messageId,
      body,
      settings.userPath,
      activeThread.path,
      mentioned,
      parent
    );
    try {
      const api = client();
      await api.createResource(userMessage);
      composer = '';
      await loadData(api);
      activeThreadPath = activeThread.path;
      if (mentioned.length === 0) {
        notice = 'Message sent; no Agent was mentioned';
        return;
      }
      const runs = await Promise.all(
        mentioned.map((agentPath) =>
          waitForRun(api, mentionRunPath(userMessage.path, agentPath))
        )
      );
      const failed = runs.find((run) => run.status !== 'succeeded');
      if (failed) throw new Error(failed.error || `Agent Run ended as ${failed.status}.`);
      await loadData(api);
      activeThreadPath = activeThread.path;
      notice = runs.length === 1 ? 'Reply received' : `${runs.length} replies received`;
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
      try {
        const run = await api.getRun(path);
        if (['succeeded', 'failed', 'cancelled'].includes(run.status)) return run;
      } catch (cause) {
        if (!(cause instanceof KasApiError && cause.status === 404)) throw cause;
      }
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

  function titleOf(thread: Resource): string {
    return typeof thread.spec.title === 'string' ? thread.spec.title : thread.name;
  }

  function authorOf(message: Resource): string {
    const authorPath = relationTarget(message, AUTHORED_BY);
    if (!authorPath || authorPath === settings.userPath) return 'You';
    return agents.find((agent) => agent.path === authorPath)?.name ?? authorPath;
  }

  function insertMention(agent: Resource): void {
    const mention = `@${mentionHandle(agent)}`;
    const separator = composer.length > 0 && !composer.endsWith(' ') ? ' ' : '';
    composer = `${composer}${separator}${mention} `;
  }

  function stringSpec(resource: Resource, key: string): string {
    const value = resource.spec[key];
    return typeof value === 'string' ? value : '';
  }

  function resourceState(resource: Resource): string {
    return resource.status_state;
  }

  function resourceConverged(resource: Resource): boolean {
    return JSON.stringify(resource.spec) === JSON.stringify(resource.status);
  }

  function kindLabel(kind: ObjectKind): string {
    return kind
      .split('_')
      .map((part) => part.slice(0, 1).toUpperCase() + part.slice(1))
      .join(' ');
  }

  function detailName(detail: ObjectDetail): string {
    if (
      typeof detail.value === 'object' &&
      detail.value !== null &&
      typeof detail.value.metadata?.name === 'string'
    ) {
      return detail.value.metadata.name;
    }
    return detail.path.split('/').at(-1) || detail.path;
  }

  function formattedValue(value: unknown): string {
    return JSON.stringify(value, null, 2);
  }

  function timeOf(timestamp: string): string {
    return new Intl.DateTimeFormat(undefined, {
      hour: '2-digit',
      minute: '2-digit'
    }).format(new Date(timestamp));
  }
</script>

<svelte:head>
  <title>{pageTitle} · KAS</title>
</svelte:head>

<div class="shell">
  <aside class="sidebar">
    <header class="brand">
      <div class="brand-mark">K</div>
      <div>
        <strong>KAS</strong>
        <span>Collaboration console</span>
      </div>
    </header>

    <div class="sidebar-section-title">
      <span>Threads</span>
      <span class="sidebar-title-actions">
        <button class="icon-button" aria-label="Object Explorer" onclick={() => void openObjectExplorer()}>⌘</button>
        <button class="icon-button" aria-label="Manage Threads" onclick={() => openThreadManagement()}>≡</button>
        <button class="icon-button" aria-label="Manage Agents" onclick={openAgentManagement}>A</button>
        <button class="icon-button" aria-label="Create Thread" onclick={startThread}>+</button>
      </span>
    </div>

    <nav class="thread-list" aria-label="Threads">
      {#if threads.length === 0 && !loading}
        <button class="empty-thread" onclick={startThread}>
          <span>+</span>
          Create your first Thread
        </button>
      {/if}
      {#each threads as thread}
        <button
          class:active={thread.path === activeThreadPath}
          class="thread-item"
          onclick={() => chooseThread(thread.path)}
        >
          <span class="thread-avatar">#</span>
          <span class="thread-copy">
            <strong>{titleOf(thread)}</strong>
            <small>
              {participantAgentPaths(thread).length} Agents ·
              {messagesForThread(messages, thread.path).length} Messages
            </small>
          </span>
        </button>
      {/each}
    </nav>

    <footer class="sidebar-footer">
      <div class="connection">
        <span class:online={driver?.state === 'running'} class="status-dot"></span>
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
        <p class="eyebrow">
          {view === 'agents'
            ? 'Workspace'
            : view === 'threads'
              ? 'Conversations'
              : view === 'objects'
                ? 'KAS Registry'
                : 'Current Thread'}
        </p>
        <h1>{pageTitle}</h1>
      </div>
      <div class="header-actions">
        {#if view === 'agents'}
          <button class="quiet-button" disabled={!activeThread} onclick={() => (view = 'chat')}>
            Open chat
          </button>
          <button class="quiet-button" onclick={() => openThreadManagement()}>Manage Threads</button>
          <button class="primary-button" onclick={openAgentDialog}>Create Agent</button>
        {:else if view === 'threads'}
          <button class="quiet-button" disabled={!managedThread} onclick={openManagedThreadChat}>
            Open chat
          </button>
          <button class="quiet-button" onclick={openAgentManagement}>Manage Agents</button>
          <button class="primary-button" disabled={agents.length === 0} onclick={startThread}>
            New Thread
          </button>
        {:else if view === 'objects'}
          <button class="quiet-button" disabled={!activeThread} onclick={() => (view = 'chat')}>
            Open chat
          </button>
          <button class="quiet-button" onclick={() => openThreadManagement()}>Manage Threads</button>
          <button class="quiet-button" onclick={openAgentManagement}>Manage Agents</button>
        {:else}
          <button
            class="quiet-button"
            disabled={!activeThread}
            onclick={() => openThreadManagement(activeThreadPath)}
          >
            Thread settings
          </button>
          <button class="quiet-button" onclick={openAgentManagement}>Manage Agents</button>
          <button class="quiet-button" onclick={() => void openObjectExplorer()}>Objects</button>
          <button class="quiet-button" disabled={agents.length === 0} onclick={startThread}>
            New Thread
          </button>
        {/if}
        <button
          class="refresh-button"
          aria-label="Refresh"
          disabled={loading || loadingObjects}
          onclick={() => refreshCurrentView().catch((cause) => (error = messageOf(cause)))}
        >
          ↻
        </button>
      </div>
    </header>

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
    {:else if view === 'threads'}
      <section class="thread-management" aria-label="Thread management">
        <div class="management-summary">
          <div>
            <strong>{threads.length}</strong>
            <span>Threads</span>
          </div>
          <div>
            <strong>{messages.length}</strong>
            <span>Messages</span>
          </div>
          <div>
            <strong>{agents.length}</strong>
            <span>Available Agents</span>
          </div>
        </div>

        {#if threads.length === 0 && !loading}
          <div class="management-empty">
            <p class="eyebrow">No Threads</p>
            <h2>Create a shared conversation.</h2>
            <button class="primary-button" disabled={agents.length === 0} onclick={startThread}>
              Create Thread
            </button>
          </div>
        {:else}
          <div class="thread-manager-layout">
            <nav class="thread-manager-list" aria-label="Threads">
              {#each threads as thread}
                <button
                  class:active={thread.path === selectedManagedThreadPath}
                  onclick={() => selectManagedThread(thread.path)}
                >
                  <span>
                    <strong>{titleOf(thread)}</strong>
                    <code>{thread.path}</code>
                  </span>
                  <small>
                    {participantAgentPaths(thread).length} Agents ·
                    {messagesForThread(messages, thread.path).length} Messages
                  </small>
                </button>
              {/each}
            </nav>

            {#if managedThread}
              <form
                class="thread-editor"
                onsubmit={(event) => { event.preventDefault(); void saveManagedThread(); }}
              >
                <header>
                  <div>
                    <p class="eyebrow">Thread Resource</p>
                    <h2>{titleOf(managedThread)}</h2>
                    <code>{managedThread.path}</code>
                  </div>
                  <span class="state-pill">{resourceState(managedThread)}</span>
                </header>

                <label class="thread-title-field">
                  Thread name
                  <input bind:value={editThreadTitle} required />
                </label>

                <fieldset class="participant-picker">
                  <legend>Agent participants</legend>
                  {#each agents as agent}
                    <label>
                      <input
                        type="checkbox"
                        checked={editThreadAgents.includes(agent.path)}
                        onchange={() => toggleManagedThreadAgent(agent.path)}
                      />
                      <span>
                        <strong>{agent.name}</strong>
                        <code>{agent.path}</code>
                      </span>
                    </label>
                  {/each}
                </fieldset>

                <p class="thread-editor-note">
                  User participants are retained. Agent membership is represented by
                  <code>participants</code> Links.
                </p>

                <div class="modal-actions">
                  <button type="button" class="quiet-button" onclick={openManagedThreadChat}>
                    Open chat
                  </button>
                  <button type="submit" class="primary-button" disabled={savingThread}>
                    {savingThread ? 'Saving…' : 'Save changes'}
                  </button>
                </div>
              </form>
            {/if}
          </div>
        {/if}
      </section>
    {:else if view === 'objects'}
      <section class="object-explorer" aria-label="KAS object explorer">
        <nav class="object-kind-strip" aria-label="Object kinds">
          {#each OBJECT_KINDS as kind}
            <button
              class:active={kind === objectKind}
              onclick={() => void chooseObjectKind(kind)}
            >
              <span>{kindLabel(kind)}</span>
              <small>{objectCounts[kind]}</small>
            </button>
          {/each}
        </nav>

        <div class="object-browser">
          <aside class="object-index">
            <label class="object-search">
              <span class="visually-hidden">Filter {kindLabel(objectKind)} objects</span>
              <input bind:value={objectSearch} placeholder={`Filter ${kindLabel(objectKind)} paths…`} />
              <small>{filteredObjects.length}</small>
            </label>
            <div class="object-list">
              {#if filteredObjects.length === 0}
                <div class="object-list-empty">
                  No {kindLabel(objectKind)} objects are visible.
                </div>
              {:else}
                {#each filteredObjects as object}
                  <button
                    class:active={object.path === selectedObjectPath}
                    onclick={() => void selectObject(object)}
                  >
                    <strong>{object.path.split('/').at(-1) || object.path}</strong>
                    <code>{object.path}</code>
                  </button>
                {/each}
              {/if}
            </div>
          </aside>

          <article class="object-detail" bind:this={objectDetailElement}>
            {#if loadingObjects && !objectDetail}
              <div class="object-detail-empty">Loading object…</div>
            {:else if objectDetail}
              <header class="object-detail-header">
                <div>
                  <span class="object-kind-pill">{kindLabel(objectDetail.kind)}</span>
                  <h2>{detailName(objectDetail)}</h2>
                  <code>{objectDetail.path}</code>
                </div>
                <button
                  class="quiet-button"
                  onclick={() => void reloadObjectDetail()}
                  disabled={loadingObjects}
                >
                  Reload
                </button>
              </header>

              <section class="object-relations" aria-label="Related objects">
                <div class="object-section-title">
                  <span>Links &amp; related objects</span>
                  <small>{objectDetail.links.length}</small>
                </div>
                {#if objectDetail.links.length === 0}
                  <p>No visible links for this object.</p>
                {:else}
                  <div class="relation-list">
                    {#each objectDetail.links as link}
                      <article class="relation-card">
                        <button
                          class="relation-name"
                          onclick={() => void selectObject({ kind: 'link', path: link.path })}
                        >
                          <strong>{link.path.split('/').at(-1) || link.path}</strong>
                          <code>{link.path}</code>
                        </button>
                        <div class="relation-route">
                          {#if link.source}
                            <button onclick={() => void selectEndpoint(link.source)}>
                              <small>{kindLabel(link.source.kind)}</small>
                              <code>{link.source.path}</code>
                            </button>
                          {:else}
                            <span class="empty-endpoint">Any source</span>
                          {/if}
                          <button
                            class="relation-edge"
                            onclick={() =>
                              void selectObject({ kind: 'relation', path: link.relation_path })}
                          >
                            <span>→</span>
                            <code>{link.relation_path}</code>
                          </button>
                          {#if link.target}
                            <button onclick={() => void selectEndpoint(link.target)}>
                              <small>{kindLabel(link.target.kind)}</small>
                              <code>{link.target.path}</code>
                            </button>
                          {:else}
                            <span class="empty-endpoint">Any target</span>
                          {/if}
                        </div>
                      </article>
                    {/each}
                  </div>
                {/if}
              </section>

              <section class="object-payload">
                <div class="object-section-title"><span>Object data</span></div>
                <pre><code>{formattedValue(objectDetail.value)}</code></pre>
              </section>
            {:else}
              <div class="object-detail-empty">
                Select an object to inspect its data and relationships.
              </div>
            {/if}
          </article>
        </div>
      </section>
    {:else}
      <section class="conversation" aria-live="polite">
        {#if !activeThread}
          <div class="empty-state compact">
            <p class="eyebrow">No Thread selected</p>
            <h2>Create a place for Agents to collaborate.</h2>
            <p>A Thread is an independent Resource and may contain one or many Agents.</p>
            {#if agents.length === 0}
              <button class="primary-button" onclick={openAgentDialog}>Create an Agent first</button>
            {:else}
              <button class="primary-button" onclick={startThread}>Create Thread</button>
            {/if}
          </div>
        {:else if activeMessages.length === 0}
          <div class="empty-state compact">
            <p class="eyebrow">New Thread</p>
            <h2>{titleOf(activeThread)}</h2>
            <p>Mention an Agent with @handle to ask it to work.</p>
          </div>
        {:else}
          <div class="message-list">
            {#each activeMessages as message}
              <article class:assistant={roleOf(message) === 'assistant'} class="message">
                <div class="message-meta">
                  <span>{authorOf(message)}</span>
                  <time datetime={message.created_at}>{timeOf(message.created_at)}</time>
                </div>
                <p>{bodyOf(message)}</p>
              </article>
            {/each}
            {#if sending}
              <article class="message assistant pending">
                <div class="message-meta"><span>Mentioned Agents</span></div>
                <div class="thinking"><i></i><i></i><i></i></div>
              </article>
            {/if}
          </div>
        {/if}
      </section>
    {/if}

    {#if view === 'chat' && activeThread}
      <form class="composer" onsubmit={(event) => { event.preventDefault(); void sendMessage(); }}>
        <div class="mention-picker" aria-label="Thread Agents">
          <span>Mention:</span>
          {#each activeParticipants as agent}
            <button type="button" onclick={() => insertMention(agent)}>
              @{mentionHandle(agent)}
            </button>
          {/each}
        </div>
        <textarea
          bind:value={composer}
          aria-label="Message"
          placeholder="Message this Thread… use @handle to trigger an Agent"
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
        <div class="composer-note">Only mentioned Agents run · Enter to send</div>
      </form>
    {/if}
  </main>
</div>

{#if showCreateThread}
  <div class="modal-backdrop" role="presentation">
    <div class="modal wide" role="dialog" aria-modal="true" aria-labelledby="thread-title">
      <div class="modal-kicker">New Resource</div>
      <h2 id="thread-title">Create a Thread</h2>
      <p>Select every Agent that may participate. Only Agents mentioned in a Message will run.</p>
      <form onsubmit={(event) => { event.preventDefault(); void createThread(); }}>
        <label>
          Title
          <input bind:value={createThreadTitle} placeholder="New conversation" required />
        </label>
        <fieldset class="participant-picker">
          <legend>Agent participants</legend>
          {#each agents as agent}
            <label>
              <input
                type="checkbox"
                checked={createThreadAgents.includes(agent.path)}
                onchange={() => toggleThreadAgent(agent.path)}
              />
              <span>
                <strong>{agent.name}</strong>
                <code>{agent.path}</code>
              </span>
            </label>
          {/each}
        </fieldset>
        <div class="modal-actions">
          <button type="button" class="quiet-button" onclick={() => (showCreateThread = false)}>
            Cancel
          </button>
          <button type="submit" class="primary-button" disabled={loading}>
            {loading ? 'Creating…' : 'Create Thread'}
          </button>
        </div>
      </form>
    </div>
  </div>
{/if}

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
