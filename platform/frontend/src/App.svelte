<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import { FileApi, KasApi, KasApiError, SkillApi } from './lib/api';
  import {
    AGENT_MANIFEST,
    ATTACHED_TO,
    AUTHORED_BY,
    FILE_MANIFEST,
    MESSAGE_MANIFEST,
    PARTICIPANTS,
    SESSION_MANIFEST,
    SKILL_MANIFEST,
    THREAD_MANIFEST,
    USES_SKILL,
    buildThread,
    buildUserMessage,
    mentionHandle,
    mentionedAgentPaths,
    mentionRunPath,
    messagesForThread,
    participantAgentPaths,
    participantsForThread,
    relationTarget,
    sessionForThreadAgent,
    shouldSubmitComposer,
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
  const DEFAULT_FILE_API_BASE = import.meta.env.VITE_KAS_FILE_API_URL || '/files-api';
  const DEFAULT_SKILL_API_BASE = import.meta.env.VITE_KAS_SKILL_API_URL || '/skills-api';

  interface Settings {
    apiBase: string;
    token: string;
    userPath: string;
  }

  type View = 'chat' | 'agents' | 'skills' | 'threads' | 'objects';

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
  let files: Resource[] = [];
  let sessions: Resource[] = [];
  let skills: Resource[] = [];
  let selectedAgentPath = '';
  let activeThreadPath: string | null = null;
  let driver: Driver | null = null;
  let loading = false;
  let sending = false;
  let connecting = false;
  let error = '';
  let notice = '';
  let composer = '';
  let composerCompositionActive = false;
  let pendingFiles: File[] = [];
  let fileInput: HTMLInputElement;
  let previewUrls: Record<string, string> = {};
  let previewingFilePath = '';
  let downloadingFilePath = '';
  let showSettings = false;
  let showCreateAgent = false;
  let showCreateThread = false;
  let createThreadTitle = '';
  let createThreadAgents: string[] = [];
  let selectedManagedThreadPath = '';
  let editThreadTitle = '';
  let editThreadAgents: string[] = [];
  let savingThread = false;
  let resettingSessionPath = '';
  let createName = '';
  let createPath = '';
  let createWorkingDirectory = '';
  let view: View = 'chat';
  let showEditAgent = false;
  let editAgentPath = '';
  let editWorkingDirectory = '';
  let deleteTarget: Resource | null = null;
  let savingAgent = false;
  let selectedSkillPath = '';
  let createSkillPath = '';
  let createSkillBundle: File | null = null;
  let replacementSkillBundle: File | null = null;
  let skillBundleInput: HTMLInputElement;
  let replacementSkillBundleInput: HTMLInputElement;
  let savingSkill = false;
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
  $: selectedSkill = skills.find((skill) => skill.path === selectedSkillPath) ?? null;
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
      : view === 'skills'
        ? 'Skill management'
      : view === 'threads'
        ? 'Thread management'
        : view === 'objects'
          ? 'Resource management'
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

  onDestroy(() => {
    Object.values(previewUrls).forEach((url) => URL.revokeObjectURL(url));
  });

  function client(): KasApi {
    return new KasApi(settings.apiBase, settings.token);
  }

  function fileClient(): FileApi {
    return new FileApi(DEFAULT_FILE_API_BASE, settings.token);
  }

  function skillClient(): SkillApi {
    return new SkillApi(DEFAULT_SKILL_API_BASE, settings.token);
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
      const [
        agentResources,
        threadResources,
        messageResources,
        sessionResources,
        fileResources,
        skillResources
      ] =
        await Promise.all([
          api.listResources(AGENT_MANIFEST),
          api.listResources(THREAD_MANIFEST),
          api.listResources(MESSAGE_MANIFEST),
          api.listResources(SESSION_MANIFEST),
          api.listResources(FILE_MANIFEST),
          api.listResources(SKILL_MANIFEST)
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
      sessions = await Promise.all(
        sessionResources
          .filter((resource) => resource.manifest === SESSION_MANIFEST)
          .map((resource) => api.getResource(resource.path, true))
      );
      files = await Promise.all(
        fileResources
          .filter((resource) => resource.manifest === FILE_MANIFEST)
          .map((resource) => api.getResource(resource.path, true))
      );
      skills = (
        await Promise.all(
          skillResources
            .filter((resource) => resource.manifest === SKILL_MANIFEST)
            .map((resource) => api.getResource(resource.path, true))
        )
      ).sort((left, right) => left.name.localeCompare(right.name));
      if (!skills.some((skill) => skill.path === selectedSkillPath)) {
        selectedSkillPath = skills[0]?.path ?? '';
      }
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

  function openSkillManagement(path = selectedSkillPath): void {
    view = 'skills';
    selectedSkillPath =
      (path && skills.some((skill) => skill.path === path) ? path : skills[0]?.path) ?? '';
    error = '';
    notice = '';
  }

  function selectSkill(path: string): void {
    selectedSkillPath = path;
    replacementSkillBundle = null;
    if (replacementSkillBundleInput) replacementSkillBundleInput.value = '';
  }

  function selectSkillBundle(event: Event, replacement: boolean): void {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0] ?? null;
    if (replacement) replacementSkillBundle = file;
    else createSkillBundle = file;
  }

  async function createSkill(): Promise<void> {
    const path = createSkillPath.trim();
    if (!path || !createSkillBundle || savingSkill) return;
    savingSkill = true;
    error = '';
    try {
      const created = await skillClient().create(path, createSkillBundle);
      await loadData();
      selectedSkillPath = created.path;
      createSkillPath = '';
      createSkillBundle = null;
      if (skillBundleInput) skillBundleInput.value = '';
      notice = `Skill ${created.path} was created`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingSkill = false;
    }
  }

  async function replaceSkillBundle(): Promise<void> {
    if (!selectedSkill || !replacementSkillBundle || savingSkill) return;
    savingSkill = true;
    error = '';
    try {
      await skillClient().update(
        selectedSkill.path,
        selectedSkill.revision,
        replacementSkillBundle
      );
      await loadData();
      selectedSkillPath = selectedSkill.path;
      replacementSkillBundle = null;
      if (replacementSkillBundleInput) replacementSkillBundleInput.value = '';
      notice = `Skill ${selectedSkill.path} now uses the new immutable bundle`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingSkill = false;
    }
  }

  function skillAssignment(skill: Resource, agent: Resource) {
    return skill.links?.find(
      (link) =>
        link.relation_path === USES_SKILL &&
        link.source.path === agent.path &&
        link.target.path === skill.path
    );
  }

  async function toggleAgentSkill(skill: Resource, agent: Resource): Promise<void> {
    if (skill.path === '/skills/kas') return;
    savingSkill = true;
    error = '';
    try {
      const assignment = skillAssignment(skill, agent);
      if (assignment) {
        await client().deleteResource(assignment.path, assignment.revision);
      } else {
        const skillName = stringSpec(skill, 'name') || slugify(skill.name);
        await client().createLink({
          path: `${agent.path}/links/skills/${skillName}`,
          source: { kind: 'resource', path: agent.path },
          relation_path: USES_SKILL,
          target: { kind: 'resource', path: skill.path },
          metadata: { mode: 'available' }
        });
      }
      await loadData();
      selectedSkillPath = skill.path;
      notice = `${skill.name} assignment updated`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingSkill = false;
    }
  }

  async function deleteSkill(skill: Resource): Promise<void> {
    if (skill.path === '/skills/kas' || savingSkill) return;
    savingSkill = true;
    error = '';
    try {
      await client().deleteResource(skill.path, skill.revision);
      await loadData();
      notice = `Skill ${skill.path} is being deleted`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingSkill = false;
    }
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

  async function resetAgentSession(thread: Resource, agent: Resource): Promise<void> {
    const session = sessionForThreadAgent(sessions, thread.path, agent.path);
    if (!session || resettingSessionPath) return;
    resettingSessionPath = session.path;
    error = '';
    try {
      const api = client();
      for (const link of session.links ?? []) {
        await api.deleteResource(link.path, link.revision);
        await waitForResourceDeletion(api, link.path, 'Session Link');
      }
      await api.deleteResource(session.path, session.revision);
      await waitForResourceDeletion(api, session.path, 'Session');
      await loadData(api);
      selectManagedThread(thread.path);
      notice = `${agent.name}'s Session was reset`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      resettingSessionPath = '';
    }
  }

  async function openObjectExplorer(): Promise<void> {
    view = 'objects';
    error = '';
    notice = '';
    await loadObjects();
  }

  async function openObjectKind(kind: ObjectKind): Promise<void> {
    objectKind = kind;
    objectSearch = '';
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
      await waitForResourceDeletion(api, agent.path, 'Agent');
      await loadData(api);
      notice = `${agent.name} was deleted`;
    } catch (cause) {
      error = messageOf(cause);
      await loadData().catch(() => undefined);
    } finally {
      deletingAgentPath = '';
    }
  }

  async function waitForResourceDeletion(
    api: KasApi,
    path: string,
    resourceKind: string
  ): Promise<void> {
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
    throw new Error(`${resourceKind} deletion is still waiting for Driver reconciliation.`);
  }

  async function sendMessage(): Promise<void> {
    const body = composer.trim();
    if ((!body && pendingFiles.length === 0) || !activeThread || sending) return;
    sending = true;
    error = '';
    const mentioned = mentionedAgentPaths(body, activeParticipants);
    notice = mentioned.length > 0 ? 'Mentioned Agents are working…' : 'Sending Message…';
    const parent = activeMessages.at(-1)?.path ?? null;
    const messageId = crypto.randomUUID();
    const uploaded: Resource[] = [];
    let messageCreated = false;
    try {
      const api = client();
      for (const file of pendingFiles) {
        uploaded.push(await fileClient().upload(file));
      }
      const userMessage = buildUserMessage(
        messageId,
        body,
        settings.userPath,
        activeThread.path,
        mentioned,
        parent,
        uploaded.map((file) => file.path)
      );
      await api.createResource(userMessage);
      messageCreated = true;
      composer = '';
      pendingFiles = [];
      if (fileInput) fileInput.value = '';
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
      if (!messageCreated && uploaded.length > 0) {
        const api = client();
        await Promise.allSettled(
          uploaded.map((file) => api.deleteResource(file.path, file.revision))
        );
      }
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

  function attachmentsFor(message: Resource): Resource[] {
    return files.filter((file) =>
      file.links?.some(
        (link) =>
          link.relation_path === ATTACHED_TO &&
          link.source.path === file.path &&
          link.target.path === message.path
      )
    );
  }

  function fileName(file: Resource): string {
    return typeof file.spec.filename === 'string' ? file.spec.filename : file.name;
  }

  function fileMediaType(file: Resource): string {
    return typeof file.spec.media_type === 'string'
      ? file.spec.media_type
      : 'application/octet-stream';
  }

  function fileSize(file: Resource): string {
    const size = typeof file.spec.size === 'number' ? file.spec.size : 0;
    if (size < 1024) return `${size} B`;
    if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
    return `${(size / 1024 / 1024).toFixed(1)} MB`;
  }

  function isPreviewable(file: Resource): boolean {
    return /^(image|video|audio)\//.test(fileMediaType(file));
  }

  async function previewFile(file: Resource): Promise<void> {
    if (previewUrls[file.path]) return;
    previewingFilePath = file.path;
    error = '';
    try {
      const blob = await fileClient().download(file.path);
      previewUrls = { ...previewUrls, [file.path]: URL.createObjectURL(blob) };
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      previewingFilePath = '';
    }
  }

  async function downloadFile(file: Resource): Promise<void> {
    downloadingFilePath = file.path;
    error = '';
    try {
      const blob = await fileClient().download(file.path);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement('a');
      anchor.href = url;
      anchor.download = fileName(file);
      anchor.click();
      URL.revokeObjectURL(url);
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      downloadingFilePath = '';
    }
  }

  function selectFiles(event: Event): void {
    const input = event.currentTarget as HTMLInputElement;
    pendingFiles = [...pendingFiles, ...Array.from(input.files ?? [])];
    input.value = '';
  }

  function removePendingFile(index: number): void {
    pendingFiles = pendingFiles.filter((_, candidate) => candidate !== index);
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

  function detailSpec(detail: ObjectDetail | null): Record<string, unknown> {
    return detail?.value.spec ?? {};
  }

  function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
  }

  function records(value: unknown): Record<string, unknown>[] {
    return Array.isArray(value) ? value.filter(isRecord) : [];
  }

  function record(value: unknown): Record<string, unknown> {
    return isRecord(value) ? value : {};
  }

  function strings(value: unknown): string[] {
    return Array.isArray(value)
      ? value.filter((entry): entry is string => typeof entry === 'string')
      : [];
  }

  function fieldLabel(field: string): string {
    return field
      .replaceAll('_', ' ')
      .replace(/\b\w/g, (character) => character.toUpperCase());
  }

  function displayValue(value: unknown): string {
    if (value === null || value === undefined || value === '') return 'Not set';
    if (typeof value === 'boolean') return value ? 'Enabled' : 'Disabled';
    if (typeof value === 'number') return String(value);
    if (typeof value === 'string') return value;
    if (Array.isArray(value)) return value.length === 0 ? 'None' : `${value.length} items`;
    if (isRecord(value)) return Object.keys(value).length === 0 ? 'Empty' : `${Object.keys(value).length} fields`;
    return String(value);
  }

  function objectRefForPath(path: string): ObjectRef {
    return objects.find((object) => object.path === path) ?? {
      kind: 'resource',
      path
    };
  }

  function detailConverged(detail: ObjectDetail): boolean {
    return (
      detail.value.metadata.state === detail.value.status.metadata.state &&
      JSON.stringify(detail.value.spec) === JSON.stringify(detail.value.status.spec)
    );
  }

  function formatTimestamp(value: string): string {
    return new Intl.DateTimeFormat(undefined, {
      dateStyle: 'medium',
      timeStyle: 'short'
    }).format(new Date(value));
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

    <div class="sidebar-scroll">
      <div class="sidebar-section-title">Workspace</div>
      <nav class="platform-nav" aria-label="Workspace">
        <button
          class:active={view === 'chat'}
          disabled={!activeThread}
          onclick={() => (view = 'chat')}
        >
          <span class="nav-icon">◉</span>
          <span><strong>Chat</strong><small>Current conversation</small></span>
        </button>
        <button class:active={view === 'threads'} onclick={() => openThreadManagement()}>
          <span class="nav-icon">#</span>
          <span><strong>Threads</strong><small>{threads.length} conversations</small></span>
        </button>
        <button class:active={view === 'agents'} onclick={openAgentManagement}>
          <span class="nav-icon">A</span>
          <span><strong>Agents</strong><small>{agents.length} workers</small></span>
        </button>
        <button class:active={view === 'skills'} onclick={() => openSkillManagement()}>
          <span class="nav-icon">⌁</span>
          <span><strong>Skills</strong><small>{skills.length} capabilities</small></span>
        </button>
        <button class:active={view === 'objects'} onclick={() => void openObjectExplorer()}>
          <span class="nav-icon">◇</span>
          <span><strong>Objects</strong><small>Complete registry</small></span>
        </button>
      </nav>

      <div class="sidebar-section-title resource-title">Resources</div>
      <nav class="resource-nav" aria-label="Resource shortcuts">
        <button class:active={view === 'objects' && objectKind === 'resource'} onclick={() => void openObjectKind('resource')}>
          <span>Resources</span><small>All</small>
        </button>
        <button class:active={view === 'objects' && objectKind === 'link'} onclick={() => void openObjectKind('link')}>
          <span>Links</span><small>Relations</small>
        </button>
        <button class:active={view === 'objects' && objectKind === 'manifest'} onclick={() => void openObjectKind('manifest')}>
          <span>Manifests</span><small>Types</small>
        </button>
        <button class:active={view === 'objects' && objectKind === 'service_account'} onclick={() => void openObjectKind('service_account')}>
          <span>Service Accounts</span><small>Identity</small>
        </button>
        <button class:active={view === 'objects' && objectKind === 'role'} onclick={() => void openObjectKind('role')}>
          <span>Roles</span><small>RBAC</small>
        </button>
        <button class:active={view === 'objects' && objectKind === 'package'} onclick={() => void openObjectKind('package')}>
          <span>Packages</span><small>Installed</small>
        </button>
      </nav>

      <div class="sidebar-section-title context-title">Current Thread</div>
      {#if activeThread}
        <button class="current-thread-card" onclick={() => chooseThread(activeThread.path)}>
          <span class="thread-avatar">#</span>
          <span class="thread-copy">
            <strong>{titleOf(activeThread)}</strong>
            <small>
              {participantAgentPaths(activeThread).length} Agents ·
              {messagesForThread(messages, activeThread.path).length} Messages
            </small>
          </span>
        </button>
      {:else}
        <button class="empty-thread" onclick={startThread}>
          <span>+</span>
          Create a Thread
        </button>
      {/if}
    </div>

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
            : view === 'skills'
              ? 'Capabilities'
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
          <button class="primary-button" onclick={openAgentDialog}>Create Agent</button>
        {:else if view === 'threads'}
          <button class="quiet-button" disabled={!managedThread} onclick={openManagedThreadChat}>
            Open chat
          </button>
          <button class="primary-button" disabled={agents.length === 0} onclick={startThread}>
            New Thread
          </button>
        {:else if view === 'chat'}
          <button
            class="quiet-button"
            disabled={!activeThread}
            onclick={() => openThreadManagement(activeThreadPath)}
          >
            Thread settings
          </button>
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
                    <dt>Skills</dt>
                    <dd>
                      {skills
                        .filter((skill) => skillAssignment(skill, agent))
                        .map((skill) => `$${stringSpec(skill, 'name')}`)
                        .join(', ') || 'No assigned Skills'}
                    </dd>
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
    {:else if view === 'skills'}
      <section class="skill-management" aria-label="Skill management">
        <div class="management-summary">
          <div>
            <strong>{skills.length}</strong>
            <span>Skills</span>
          </div>
          <div>
            <strong>{skills.filter(resourceConverged).length}</strong>
            <span>Validated</span>
          </div>
          <div>
            <strong>{agents.length}</strong>
            <span>Assignable Agents</span>
          </div>
        </div>

        <div class="skill-manager-layout">
          <aside class="skill-index">
            <form
              class="skill-create"
              onsubmit={(event) => { event.preventDefault(); void createSkill(); }}
            >
              <strong>Import Skill Bundle</strong>
              <input
                bind:value={createSkillPath}
                aria-label="New Skill Resource path"
                placeholder="/skills/example"
                required
              />
              <input
                bind:this={skillBundleInput}
                aria-label="New Skill bundle"
                type="file"
                accept=".skill,.zip,application/zip,application/vnd.kas.skill+zip"
                required
                onchange={(event) => selectSkillBundle(event, false)}
              />
              <button
                class="primary-button"
                type="submit"
                disabled={savingSkill || !createSkillPath.trim() || !createSkillBundle}
              >
                {savingSkill ? 'Importing…' : 'Create Skill'}
              </button>
            </form>

            <nav aria-label="Skills">
              {#each skills as skill}
                <button
                  class:active={skill.path === selectedSkillPath}
                  onclick={() => selectSkill(skill.path)}
                >
                  <span>
                    <strong>${stringSpec(skill, 'name')}</strong>
                    <code>{skill.path}</code>
                  </span>
                  <small>{resourceConverged(skill) ? resourceState(skill) : 'validating'}</small>
                </button>
              {/each}
            </nav>
          </aside>

          {#if selectedSkill}
            {@const bundleLink = selectedSkill.links?.find(
              (link) =>
                link.relation_path === '/manifests/skill/relations/bundle' &&
                link.source.path === selectedSkill.path
            )}
            <article class="skill-editor">
              <header>
                <div>
                  <p class="eyebrow">Skill Resource</p>
                  <h2>${stringSpec(selectedSkill, 'name')}</h2>
                  <code>{selectedSkill.path}</code>
                </div>
                <span class:pending={!resourceConverged(selectedSkill)} class="state-pill">
                  {resourceConverged(selectedSkill)
                    ? resourceState(selectedSkill)
                    : 'validating'}
                </span>
              </header>

              <p class="skill-description">{stringSpec(selectedSkill, 'description')}</p>
              <dl class="agent-details">
                <div>
                  <dt>Bundle File</dt>
                  <dd>{bundleLink?.target.path ?? 'Waiting for bundle Link'}</dd>
                </div>
                <div>
                  <dt>Implicit invocation</dt>
                  <dd>
                    {selectedSkill.spec.allow_implicit_invocation ? 'Allowed' : 'Explicit only'}
                  </dd>
                </div>
                <div>
                  <dt>Revision</dt>
                  <dd>{selectedSkill.revision}</dd>
                </div>
              </dl>

              <form
                class="skill-replacement"
                onsubmit={(event) => { event.preventDefault(); void replaceSkillBundle(); }}
              >
                <label>
                  Replace immutable bundle
                  <input
                    bind:this={replacementSkillBundleInput}
                    type="file"
                    accept=".skill,.zip,application/zip,application/vnd.kas.skill+zip"
                    required
                    onchange={(event) => selectSkillBundle(event, true)}
                  />
                </label>
                <button
                  class="quiet-button"
                  type="submit"
                  disabled={savingSkill || !replacementSkillBundle}
                >
                  Replace Bundle
                </button>
              </form>

              <fieldset class="participant-picker">
                <legend>Assigned Agents</legend>
                {#each agents as agent}
                  <label>
                    <input
                      type="checkbox"
                      checked={Boolean(skillAssignment(selectedSkill, agent))}
                      disabled={savingSkill || selectedSkill.path === '/skills/kas'}
                      onchange={() => void toggleAgentSkill(selectedSkill, agent)}
                    />
                    <span>
                      <strong>{agent.name}</strong>
                      <code>{agent.path}</code>
                    </span>
                  </label>
                {/each}
              </fieldset>

              <div class="skill-actions">
                {#if selectedSkill.path === '/skills/kas'}
                  <small>The platform Skill is always assigned and cannot be deleted.</small>
                {:else}
                  <button
                    class="danger-button"
                    disabled={savingSkill}
                    onclick={() => void deleteSkill(selectedSkill)}
                  >
                    Delete Skill
                  </button>
                {/if}
              </div>
            </article>
          {:else}
            <div class="management-empty">
              <p class="eyebrow">No Skills</p>
              <h2>Import a standard Skill Bundle.</h2>
            </div>
          {/if}
        </div>
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

                <section class="session-list" aria-label="Agent Sessions">
                  <div class="session-list-title">
                    <span>Codex Sessions</span>
                    <small>One per Thread-Agent pair</small>
                  </div>
                  {#each agents.filter((agent) => editThreadAgents.includes(agent.path)) as agent}
                    {@const session = sessionForThreadAgent(
                      sessions,
                      managedThread.path,
                      agent.path
                    )}
                    <article>
                      <div>
                        <strong>{agent.name}</strong>
                        {#if session}
                          <code>{String(session.spec.session_id)}</code>
                          <small>Cursor: {String(session.spec.cursor)}</small>
                        {:else}
                          <small>Starts when this Agent is first mentioned.</small>
                        {/if}
                      </div>
                      {#if session}
                        <button
                          type="button"
                          class="danger-button"
                          disabled={resettingSessionPath === session.path}
                          onclick={() => void resetAgentSession(managedThread, agent)}
                        >
                          {resettingSessionPath === session.path ? 'Resetting…' : 'Reset Session'}
                        </button>
                      {/if}
                    </article>
                  {/each}
                </section>

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
                    <span class="object-list-title">
                      <strong>{object.name || object.path.split('/').at(-1) || object.path}</strong>
                      {#if object.state}<small>{object.state}</small>{/if}
                    </span>
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

              <section class="resource-overview" aria-label="Resource overview">
                <article>
                  <small>Current state</small>
                  <strong>{objectDetail.value.status.metadata.state}</strong>
                  <span>{detailConverged(objectDetail) ? 'Reconciled' : 'Reconciliation pending'}</span>
                </article>
                <article>
                  <small>Desired state</small>
                  <strong>{objectDetail.value.metadata.state}</strong>
                  <span>Revision {objectDetail.value.metadata['[kas]'].revision}</span>
                </article>
                <article>
                  <small>Manifest</small>
                  <strong>{objectDetail.value.metadata.manifest.split('/').at(-1)}</strong>
                  <code>{objectDetail.value.metadata.manifest}</code>
                </article>
                <article>
                  <small>Last updated</small>
                  <strong>{formatTimestamp(objectDetail.value.metadata['[kas]'].updated_at)}</strong>
                  <span>Created {formatTimestamp(objectDetail.value.metadata['[kas]'].created_at)}</span>
                </article>
              </section>

              <section class="resource-configuration" aria-label="Resource configuration">
                <div class="object-section-title">
                  <span>Configuration</span>
                  <small>{Object.keys(detailSpec(objectDetail)).length} fields</small>
                </div>

                {#if objectDetail.kind === 'role'}
                  <p class="resource-description">
                    {displayValue(detailSpec(objectDetail).description)}
                  </p>
                  <div class="rule-list">
                    {#each records(detailSpec(objectDetail).rules) as rule, index}
                      <article class="rule-card">
                        <header>
                          <strong>Rule {index + 1}</strong>
                          <span>{strings(rule.verbs).length} verbs</span>
                        </header>
                        <div>
                          <small>Verbs</small>
                          <div class="value-chips">
                            {#each strings(rule.verbs) as verb}<span>{verb}</span>{/each}
                          </div>
                        </div>
                        <div>
                          <small>Manifests</small>
                          <div class="path-stack">
                            {#each strings(rule.manifests) as manifest}<code>{manifest}</code>{/each}
                          </div>
                        </div>
                        <div>
                          <small>Paths</small>
                          <div class="path-stack">
                            {#each strings(rule.paths) as path}<code>{path}</code>{/each}
                          </div>
                        </div>
                      </article>
                    {/each}
                  </div>
                {:else if objectDetail.kind === 'role_binding'}
                  <div class="binding-layout">
                    <div>
                      <small>Bound role</small>
                      <button
                        onclick={() =>
                          void selectObject(
                            objectRefForPath(String(detailSpec(objectDetail).role ?? ''))
                          )}
                      >
                        <span class="object-kind-pill">Role</span>
                        <code>{displayValue(detailSpec(objectDetail).role)}</code>
                      </button>
                    </div>
                    <div>
                      <small>Subjects</small>
                      {#each strings(detailSpec(objectDetail).subjects) as subject}
                        <button onclick={() => void selectObject(objectRefForPath(subject))}>
                          <span class="object-kind-pill">{kindLabel(objectRefForPath(subject).kind)}</span>
                          <code>{subject}</code>
                        </button>
                      {/each}
                    </div>
                  </div>
                {:else if objectDetail.kind === 'link'}
                  <div class="link-route-large">
                    <button
                      onclick={() =>
                        void selectObject(
                          objectRefForPath(String(detailSpec(objectDetail).source ?? ''))
                        )}
                    >
                      <small>Source</small>
                      <code>{displayValue(detailSpec(objectDetail).source)}</code>
                    </button>
                    <button
                      class="link-relation"
                      onclick={() =>
                        void selectObject(
                          objectRefForPath(String(detailSpec(objectDetail).relation ?? ''))
                        )}
                    >
                      <span>→</span>
                      <small>Relation</small>
                      <code>{displayValue(detailSpec(objectDetail).relation)}</code>
                    </button>
                    <button
                      onclick={() =>
                        void selectObject(
                          objectRefForPath(String(detailSpec(objectDetail).target ?? ''))
                        )}
                    >
                      <small>Target</small>
                      <code>{displayValue(detailSpec(objectDetail).target)}</code>
                    </button>
                  </div>
                {:else if objectDetail.kind === 'manifest'}
                  <div class="property-grid">
                    {#each ['description', 'version', 'initial_state', 'default_state'] as field}
                      <article>
                        <small>{fieldLabel(field)}</small>
                        <strong>{displayValue(detailSpec(objectDetail)[field])}</strong>
                      </article>
                    {/each}
                  </div>
                  {@const schema = record(detailSpec(objectDetail).resource_schema)}
                  {@const properties = record(schema.properties)}
                  {@const required = strings(schema.required)}
                  <div class="schema-table">
                    <header>
                      <strong>Resource fields</strong>
                      <span>{Object.keys(properties).length}</span>
                    </header>
                    {#each Object.entries(properties) as [field, definition]}
                      <article>
                        <strong>{field}</strong>
                        <span>{isRecord(definition) ? displayValue(definition.type) : 'Any'}</span>
                        <small>{required.includes(field) ? 'Required' : 'Optional'}</small>
                      </article>
                    {/each}
                  </div>
                {:else if objectDetail.kind === 'driver'}
                  <div class="property-grid">
                    {#each ['runtime', 'entrypoint', 'restart', 'service_account'] as field}
                      <article>
                        <small>{fieldLabel(field)}</small>
                        <strong>{displayValue(detailSpec(objectDetail)[field])}</strong>
                      </article>
                    {/each}
                  </div>
                  <div class="driver-scope">
                    <div>
                      <small>Manages</small>
                      <div class="path-stack">
                        {#each strings(detailSpec(objectDetail).manages) as path}<code>{path}</code>{/each}
                      </div>
                    </div>
                    <div>
                      <small>Watches</small>
                      <div class="path-stack">
                        {#each strings(detailSpec(objectDetail).watches) as path}<code>{path}</code>{/each}
                        {#if strings(detailSpec(objectDetail).watches).length === 0}<span>None</span>{/if}
                      </div>
                    </div>
                  </div>
                {:else if Object.keys(detailSpec(objectDetail)).length === 0}
                  <div class="configuration-empty">
                    This {kindLabel(objectDetail.kind)} has no configurable fields.
                  </div>
                {:else}
                  <div class="property-grid">
                    {#each Object.entries(detailSpec(objectDetail)) as [field, value]}
                      <article>
                        <small>{fieldLabel(field)}</small>
                        {#if Array.isArray(value)}
                          <div class="value-chips">
                            {#each value as entry}<span>{displayValue(entry)}</span>{/each}
                            {#if value.length === 0}<span>None</span>{/if}
                          </div>
                        {:else if isRecord(value)}
                          <dl class="nested-fields">
                            {#each Object.entries(value) as [nestedField, nestedValue]}
                              <div>
                                <dt>{fieldLabel(nestedField)}</dt>
                                <dd>{displayValue(nestedValue)}</dd>
                              </div>
                            {/each}
                          </dl>
                        {:else}
                          <strong>{displayValue(value)}</strong>
                        {/if}
                      </article>
                    {/each}
                  </div>
                {/if}
              </section>

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

            {:else}
              <div class="object-detail-empty">
                Select an object to manage its configuration and relationships.
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
                {#if bodyOf(message)}
                  <p>{bodyOf(message)}</p>
                {/if}
                {#if attachmentsFor(message).length > 0}
                  <div class="message-attachments">
                    {#each attachmentsFor(message) as file}
                      <article class="attachment-card">
                        <div class="attachment-summary">
                          <div class="attachment-icon">{fileMediaType(file).split('/')[0]}</div>
                          <div>
                            <strong>{fileName(file)}</strong>
                            <span>{fileMediaType(file)} · {fileSize(file)}</span>
                          </div>
                        </div>
                        {#if previewUrls[file.path]}
                          {#if fileMediaType(file).startsWith('image/')}
                            <img src={previewUrls[file.path]} alt={fileName(file)} />
                          {:else if fileMediaType(file).startsWith('video/')}
                            <!-- svelte-ignore a11y_media_has_caption -->
                            <video src={previewUrls[file.path]} controls></video>
                          {:else if fileMediaType(file).startsWith('audio/')}
                            <audio src={previewUrls[file.path]} controls></audio>
                          {/if}
                        {/if}
                        <div class="attachment-actions">
                          {#if isPreviewable(file) && !previewUrls[file.path]}
                            <button
                              type="button"
                              disabled={previewingFilePath === file.path}
                              onclick={() => void previewFile(file)}
                            >
                              {previewingFilePath === file.path ? 'Loading…' : 'Preview'}
                            </button>
                          {/if}
                          <button
                            type="button"
                            disabled={downloadingFilePath === file.path}
                            onclick={() => void downloadFile(file)}
                          >
                            {downloadingFilePath === file.path ? 'Downloading…' : 'Download'}
                          </button>
                        </div>
                      </article>
                    {/each}
                  </div>
                {/if}
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
          <button type="button" class="attach-button" onclick={() => fileInput?.click()}>
            + File
          </button>
        </div>
        <input
          class="file-input"
          bind:this={fileInput}
          type="file"
          multiple
          disabled={sending}
          onchange={selectFiles}
        />
        {#if pendingFiles.length > 0}
          <div class="pending-attachments" aria-label="Pending attachments">
            {#each pendingFiles as file, index}
              <span>
                <strong>{file.name}</strong>
                <small>{file.size} B</small>
                <button
                  type="button"
                  aria-label={`Remove ${file.name}`}
                  onclick={() => removePendingFile(index)}
                >×</button>
              </span>
            {/each}
          </div>
        {/if}
        <textarea
          bind:value={composer}
          aria-label="Message"
          placeholder="Message this Thread… use @handle to trigger an Agent"
          rows="1"
          disabled={sending}
          oncompositionstart={() => (composerCompositionActive = true)}
          oncompositionend={() => (composerCompositionActive = false)}
          onkeydown={(event) => {
            if (shouldSubmitComposer(event, composerCompositionActive)) {
              event.preventDefault();
              void sendMessage();
            }
          }}
        ></textarea>
        <button
          class="send-button"
          type="submit"
          disabled={sending || (!composer.trim() && pendingFiles.length === 0)}
        >
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
