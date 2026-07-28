<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import { ApprovalApi, FileApi, KasApi, KasApiError, SkillApi } from './lib/api';
  import {
    AGENT_MANIFEST,
    APPROVAL_DECIDED_BY,
    APPROVAL_DECIDES,
    APPROVAL_MANIFEST,
    APPROVAL_PRODUCED_BY,
    APPROVAL_REQUESTED_BY,
    APPROVAL_RESULT_MANIFEST,
    APPROVAL_RESULT_OF,
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
  import {
    FRONTEND_PLUGIN_MANIFEST,
    frontendPluginEntries,
    isPluginRequest
  } from './lib/plugins';
  import { embeddedContext, embeddedView } from './lib/embedded';
  import type { FrontendPluginEntry } from './lib/plugins';
  import type {
    CreateResource,
    Driver,
    ObjectDetail,
    ObjectKind,
    ObjectRef,
    Resource,
    Run,
    UpdateResource
  } from './lib/types';

  const SETTINGS_KEY = 'kas-platform-settings';
  const DEFAULT_API_BASE = import.meta.env.VITE_KAS_API_URL || '/api';
  const DEFAULT_FILE_API_BASE = import.meta.env.VITE_KAS_FILE_API_URL || '/files-api';
  const DEFAULT_SKILL_API_BASE = import.meta.env.VITE_KAS_SKILL_API_URL || '/skills-api';
  const DEFAULT_APPROVAL_API_BASE =
    import.meta.env.VITE_KAS_APPROVAL_API_URL || '/approvals-api';
  const TELEGRAM_MANIFEST = '/manifests/telegram';
  const TELEGRAM_THREAD_TOPIC = '/manifests/telegram/relations/thread-topic';
  const TELEGRAM_BINDING_REQUEST = '/manifests/telegram/relations/binding-request';
  const TELEGRAM_USER_BINDING = '/manifests/telegram/relations/user-binding';

  interface Settings {
    apiBase: string;
    token: string;
    userPath: string;
  }

  type View =
    | 'chat'
    | 'agents'
    | 'skills'
    | 'approvals'
    | 'threads'
    | 'telegram'
    | 'objects'
    | 'plugin';

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
  let approvals: Resource[] = [];
  let approvalResults: Resource[] = [];
  let frontendPlugins: Resource[] = [];
  let telegramConfigurations: Resource[] = [];
  let currentUser: Resource | null = null;
  let selectedTelegramPath = '';
  let createTelegramName = '';
  let createTelegramPath = '';
  let createTelegramToken = '';
  let createTelegramChatId = '';
  let createTelegramMode = 'bidirectional';
  let createTelegramApiBase = '';
  let editTelegramToken = '';
  let editTelegramChatId = '';
  let editTelegramMode = 'bidirectional';
  let editTelegramApiBase = '';
  let telegramMappingThreadPath = '';
  let telegramBindingUrl = '';
  let bindingTelegram = false;
  let savingTelegram = false;
  let deleteTelegramTarget: Resource | null = null;
  let selectedPlugin: FrontendPluginEntry | null = null;
  let pluginUrl = '';
  let pluginFrame: HTMLIFrameElement;
  let selectedApprovalPath = '';
  let savingApproval = false;
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
  $: selectedTelegram =
    telegramConfigurations.find((configuration) => configuration.path === selectedTelegramPath) ??
    null;
  $: telegramTopicLinks = selectedTelegram
    ? (selectedTelegram.links ?? [])
        .filter(
          (link) =>
            link.relation_path === TELEGRAM_THREAD_TOPIC &&
            link.target.path === selectedTelegram?.path
        )
        .sort((left, right) => left.source.path.localeCompare(right.source.path))
    : [];
  $: selectedTelegramBinding =
    selectedTelegram && currentUser
      ? (currentUser.links ?? []).find(
          (link) =>
            link.relation_path === TELEGRAM_USER_BINDING &&
            link.source.path === currentUser?.path &&
            link.metadata.configuration === selectedTelegram?.path
        ) ?? null
      : null;
  $: approvalRequests = approvals.filter((approval) => approval.spec.kind === 'request');
  $: selectedApproval =
    approvalRequests.find((approval) => approval.path === selectedApprovalPath) ?? null;
  $: selectedApprovalDecisions = selectedApproval
    ? approvals
        .filter(
          (approval) =>
            approval.spec.kind === 'decision' &&
            relationTarget(approval, APPROVAL_DECIDES) === selectedApproval.path
        )
        .sort((left, right) => right.created_at.localeCompare(left.created_at))
    : [];
  $: selectedApprovalResults = selectedApproval
    ? approvalResults
        .filter(
          (result) => relationTarget(result, APPROVAL_RESULT_OF) === selectedApproval.path
        )
        .sort((left, right) => right.created_at.localeCompare(left.created_at))
    : [];
  $: pendingApprovalCount = approvalRequests.filter(
    (approval) => approval.state === 'pending'
  ).length;
  $: pluginEntries = frontendPluginEntries(frontendPlugins);
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
        : view === 'approvals'
          ? 'Approval management'
          : view === 'threads'
            ? 'Thread management'
            : view === 'telegram'
              ? 'Telegram bridge'
            : view === 'objects'
              ? 'Resource management'
              : view === 'plugin'
                ? selectedPlugin?.label ?? 'Frontend Plugin'
              : activeThread
                ? titleOf(activeThread)
                : 'Choose a Thread';

  onMount(() => {
    window.addEventListener('message', handlePluginMessage);
    if (embeddedView) {
      view = embeddedView;
      showSettings = false;
      void embeddedContext.then((context) => {
        settings = {
          apiBase: DEFAULT_API_BASE,
          token: '',
          userPath: context.subject?.path || '/users/admin'
        };
        draftSettings = { ...settings };
        if (context.workspace?.activeThread) {
          activeThreadPath = context.workspace.activeThread;
        }
        void connect();
      });
      return;
    }
    const saved = localStorage.getItem(SETTINGS_KEY);
    if (saved) {
      try {
        settings = { ...settings, ...(JSON.parse(saved) as Partial<Settings>) };
        draftSettings = { ...settings };
      } catch {
        localStorage.removeItem(SETTINGS_KEY);
      }
    }
    showSettings = true;
    void restoreConnection();
  });

  onDestroy(() => {
    window.removeEventListener('message', handlePluginMessage);
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

  function approvalClient(): ApprovalApi {
    return new ApprovalApi(DEFAULT_APPROVAL_API_BASE, settings.token);
  }

  async function openFrontendPlugin(entry: FrontendPluginEntry): Promise<void> {
    error = '';
    notice = '';
    pluginUrl = `/plugins/${encodeURIComponent(entry.slug)}/${entry.entrypoint
      .split('/')
      .map(encodeURIComponent)
      .join('/')}`;
    selectedPlugin = entry;
    view = 'plugin';
  }

  function handlePluginMessage(event: MessageEvent<unknown>): void {
    if (!pluginFrame?.contentWindow || event.source !== pluginFrame.contentWindow) return;
    if (!isRecord(event.data) || event.data.source !== 'kas-frontend-plugin') return;
    if (event.data.type === 'ready') {
      postPluginContext();
      return;
    }
    if (!isPluginRequest(event.data)) return;
    void dispatchPluginRequest(event.data.id, event.data.method, event.data.params ?? {});
  }

  function postPluginContext(): void {
    pluginFrame?.contentWindow?.postMessage(
      {
        source: 'kas-frontend-host',
        type: 'context',
        context: {
          apiVersion: 1,
          plugin: selectedPlugin,
          subject: { path: settings.userPath, manifest: '/builtin/user' },
          workspace: {
            activeThread: activeThreadPath,
            selectedResource: selectedObjectPath || undefined,
            theme: 'dark',
            locale: navigator.language
          }
        }
      },
      '*'
    );
  }

  async function dispatchPluginRequest(
    id: string,
    method: string,
    params: Record<string, unknown>
  ): Promise<void> {
    try {
      const api = client();
      let result: unknown;
      switch (method) {
        case 'resources.list':
          result = await api.listResources(stringParam(params.manifest) || undefined, true);
          break;
        case 'resources.get':
          result = await api.getResource(requiredStringParam(params, 'path'), true);
          break;
        case 'resources.create':
          result = await api.createResource(requiredRecordParam(params, 'resource') as unknown as CreateResource);
          break;
        case 'resources.update':
          result = await api.updateResource(
            requiredStringParam(params, 'path'),
            requiredRecordParam(params, 'update') as unknown as UpdateResource
          );
          break;
        case 'resources.delete':
          result = await api.deleteResource(
            requiredStringParam(params, 'path'),
            requiredNumberParam(params, 'expectedRevision')
          );
          break;
        case 'links.list':
          result = (await api.getResource(requiredStringParam(params, 'path'), true)).links ?? [];
          break;
        case 'auth.context':
          result = await api.authContext();
          break;
        case 'auth.check':
          result = await api.checkAuthorization({
            manifest: requiredStringParam(params, 'manifest'),
            verb: requiredStringParam(params, 'verb'),
            path: requiredStringParam(params, 'path')
          });
          break;
        case 'api.request': {
          const body = params.body;
          result = await api.rawRequest(requiredStringParam(params, 'path'), {
            method: stringParam(params.method) || 'GET',
            body: body === undefined ? undefined : JSON.stringify(body)
          });
          break;
        }
        case 'gateway.fetch': {
          const path = requiredStringParam(params, 'path');
          if (
            !['/api/', '/files-api/', '/skills-api/', '/approvals-api/'].some((prefix) =>
              path.startsWith(prefix)
            )
          ) {
            throw new Error('Frontend Plugin gateway requests must target an approved API prefix.');
          }
          const headers = new Headers(optionalStringRecordParam(params, 'headers'));
          for (const name of ['authorization', 'cookie', 'host', 'content-length']) {
            headers.delete(name);
          }
          const method = stringParam(params.method) || 'GET';
          const body = params.body instanceof ArrayBuffer ? params.body : undefined;
          const response = await fetch(path, {
            method,
            headers,
            body: method === 'GET' || method === 'HEAD' ? undefined : body,
            credentials: 'same-origin'
          });
          result = {
            status: response.status,
            statusText: response.statusText,
            headers: Object.fromEntries(response.headers.entries()),
            body: await response.arrayBuffer()
          };
          break;
        }
        case 'navigation.openThread':
          chooseThread(requiredStringParam(params, 'path'));
          result = null;
          break;
        case 'navigation.openResource':
          await openObjectExplorer();
          await selectObject(objectRefForPath(requiredStringParam(params, 'path')));
          result = null;
          break;
        default:
          throw new Error(`Unsupported Frontend Plugin method: ${method}`);
      }
      postPluginResponse(id, result);
    } catch (cause) {
      postPluginResponse(id, undefined, messageOf(cause));
    }
  }

  function postPluginResponse(id: string, result?: unknown, responseError?: string): void {
    pluginFrame?.contentWindow?.postMessage(
      {
        source: 'kas-frontend-host',
        type: 'response',
        id,
        result,
        error: responseError
      },
      '*'
    );
  }

  function requiredStringParam(params: Record<string, unknown>, key: string): string {
    const value = params[key];
    if (typeof value !== 'string' || !value) throw new Error(`${key} must be a string`);
    return value;
  }

  function stringParam(value: unknown): string {
    return typeof value === 'string' ? value : '';
  }

  function requiredNumberParam(params: Record<string, unknown>, key: string): number {
    const value = params[key];
    if (typeof value !== 'number' || !Number.isFinite(value)) {
      throw new Error(`${key} must be a number`);
    }
    return value;
  }

  function requiredRecordParam(
    params: Record<string, unknown>,
    key: string
  ): Record<string, unknown> {
    const value = params[key];
    if (!isRecord(value)) throw new Error(`${key} must be an object`);
    return value;
  }

  function optionalStringRecordParam(
    params: Record<string, unknown>,
    key: string
  ): Record<string, string> {
    const value = params[key];
    if (!isRecord(value)) return {};
    return Object.fromEntries(
      Object.entries(value).filter(
        (entry): entry is [string, string] => typeof entry[1] === 'string'
      )
    );
  }

  async function connect(): Promise<void> {
    connecting = true;
    error = '';
    try {
      if (settings.token) {
        const response = await fetch('/gateway/session', {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ token: settings.token })
        });
        if (!response.ok) throw new Error('KAS credential was rejected.');
        settings = { ...settings, token: '' };
        draftSettings = { ...settings };
      }
      const api = client();
      if (!(await api.health())) throw new Error('KAS health check failed.');
      await loadData(api);
      showSettings = false;
      notice = 'Connected to KAS';
    } catch (cause) {
      error = messageOf(cause);
      showSettings = !embeddedView;
    } finally {
      connecting = false;
    }
  }

  async function restoreConnection(): Promise<void> {
    try {
      const response = await fetch('/gateway/session', { credentials: 'same-origin' });
      if (!response.ok) return;
      await connect();
    } finally {
      if (showSettings && !connecting) showSettings = true;
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
        skillResources,
        approvalResources,
        approvalResultResources,
        frontendPluginResources,
        telegramResources,
        currentUserResource
      ] =
        await Promise.all([
          api.listResources(AGENT_MANIFEST),
          api.listResources(THREAD_MANIFEST),
          api.listResources(MESSAGE_MANIFEST),
          api.listResources(SESSION_MANIFEST),
          api.listResources(FILE_MANIFEST),
          api.listResources(SKILL_MANIFEST),
          api.listResources(APPROVAL_MANIFEST),
          api.listResources(APPROVAL_RESULT_MANIFEST),
          api.listResources(FRONTEND_PLUGIN_MANIFEST),
          api.listResources(TELEGRAM_MANIFEST),
          api.getResource(settings.userPath, true)
        ]);
      currentUser = currentUserResource;
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
      approvals = (
        await Promise.all(
          approvalResources
            .filter((resource) => resource.manifest === APPROVAL_MANIFEST)
            .map((resource) => api.getResource(resource.path, true))
        )
      ).sort((left, right) => right.created_at.localeCompare(left.created_at));
      approvalResults = (
        await Promise.all(
          approvalResultResources
            .filter((resource) => resource.manifest === APPROVAL_RESULT_MANIFEST)
            .map((resource) => api.getResource(resource.path, true))
        )
      ).sort((left, right) => right.created_at.localeCompare(left.created_at));
      frontendPlugins = await Promise.all(
        frontendPluginResources
          .filter((resource) => resource.manifest === FRONTEND_PLUGIN_MANIFEST)
          .map((resource) => api.getResource(resource.path, true))
      );
      telegramConfigurations = (
        await Promise.all(
          telegramResources
            .filter((resource) => resource.manifest === TELEGRAM_MANIFEST)
            .map((resource) => api.getResource(resource.path, true))
        )
      ).sort((left, right) => left.name.localeCompare(right.name));
      if (
        !telegramConfigurations.some(
          (configuration) => configuration.path === selectedTelegramPath
        )
      ) {
        selectTelegramConfiguration(telegramConfigurations[0]?.path ?? '');
      } else {
        selectTelegramConfiguration(selectedTelegramPath);
      }
      if (!skills.some((skill) => skill.path === selectedSkillPath)) {
        selectedSkillPath = skills[0]?.path ?? '';
      }
      const currentApprovalRequests = approvals.filter(
        (approval) => approval.spec.kind === 'request'
      );
      if (
        !currentApprovalRequests.some((approval) => approval.path === selectedApprovalPath)
      ) {
        selectedApprovalPath = currentApprovalRequests[0]?.path ?? '';
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
    const threadPath = threadsForAgent(threads, path)[0]?.path ?? null;
    if (embeddedView && threadPath) {
      navigateHostThread(threadPath);
      return;
    }
    view = 'chat';
    activeThreadPath = threadPath;
    error = '';
  }

  function openAgentManagement(): void {
    if (!embeddedView && openBuiltInPlugin('agents')) return;
    view = 'agents';
    error = '';
    notice = '';
  }

  function openSkillManagement(path = selectedSkillPath): void {
    if (!embeddedView && openBuiltInPlugin('skills')) return;
    view = 'skills';
    selectedSkillPath =
      (path && skills.some((skill) => skill.path === path) ? path : skills[0]?.path) ?? '';
    error = '';
    notice = '';
  }

  function openApprovalManagement(path = selectedApprovalPath): void {
    if (!embeddedView && openBuiltInPlugin('approvals')) return;
    view = 'approvals';
    selectedApprovalPath =
      (path && approvalRequests.some((approval) => approval.path === path)
        ? path
        : approvalRequests[0]?.path) ?? '';
    error = '';
    notice = '';
  }

  function selectApproval(path: string): void {
    selectedApprovalPath = path;
    error = '';
    notice = '';
  }

  async function decideApproval(
    approval: Resource,
    decision: 'approve' | 'reject'
  ): Promise<void> {
    if (savingApproval || approval.state !== 'pending') return;
    savingApproval = true;
    error = '';
    try {
      await approvalClient().decide(approval.path, approval.revision, decision);
      await loadData();
      selectedApprovalPath = approval.path;
      notice =
        decision === 'approve'
          ? 'Your decision was recorded. The Driver verified your permission before execution.'
          : 'The Approval request was rejected.';
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingApproval = false;
    }
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

  function openTelegramManagement(path = selectedTelegramPath): void {
    if (!embeddedView && openBuiltInPlugin('telegram')) return;
    view = 'telegram';
    selectTelegramConfiguration(
      telegramConfigurations.some((configuration) => configuration.path === path)
        ? path
        : telegramConfigurations[0]?.path ?? ''
    );
    error = '';
    notice = '';
  }

  function updateTelegramCreatePath(): void {
    createTelegramPath = `/telegram/${slugify(createTelegramName)}`;
  }

  function selectTelegramConfiguration(path: string): void {
    selectedTelegramPath = path;
    telegramBindingUrl = '';
    const configuration = telegramConfigurations.find((candidate) => candidate.path === path);
    editTelegramToken = '';
    editTelegramChatId = configuration ? stringSpec(configuration, 'chat_id') : '';
    editTelegramMode = configuration
      ? stringSpec(configuration, 'mode') || 'bidirectional'
      : 'bidirectional';
    editTelegramApiBase = configuration ? stringSpec(configuration, 'api_base') : '';
    telegramMappingThreadPath = threads[0]?.path ?? '';
  }

  function selectTelegramMappingThread(path: string): void {
    telegramMappingThreadPath = path;
  }

  function telegramSpec(
    botToken: string,
    chatId: string,
    mode: string,
    apiBase: string,
    botUsername = ''
  ): Record<string, unknown> {
    return {
      bot_token: botToken,
      chat_id: chatId,
      mode,
      ...(apiBase ? { api_base: apiBase } : {}),
      ...(botUsername ? { bot_username: botUsername } : {})
    };
  }

  function validTelegramChatId(value: string): boolean {
    return /^-?[0-9]+$/.test(value);
  }

  async function createTelegramConfiguration(): Promise<void> {
    const name = createTelegramName.trim();
    const path = createTelegramPath.trim();
    const botToken = createTelegramToken.trim();
    const chatId = createTelegramChatId.trim();
    const apiBase = createTelegramApiBase.trim();
    if (!name || !path || savingTelegram) return;
    if (botToken.length < 20) {
      error = 'The Telegram bot token must contain at least 20 characters.';
      return;
    }
    if (!validTelegramChatId(chatId)) {
      error = 'Telegram chat ID must be an integer, optionally beginning with a minus sign.';
      return;
    }
    savingTelegram = true;
    error = '';
    try {
      const created = await client().createResource({
        path,
        manifest: TELEGRAM_MANIFEST,
        name,
        spec: telegramSpec(botToken, chatId, createTelegramMode, apiBase)
      });
      await loadData();
      selectTelegramConfiguration(created.path);
      createTelegramName = '';
      createTelegramPath = '';
      createTelegramToken = '';
      createTelegramChatId = '';
      createTelegramMode = 'bidirectional';
      createTelegramApiBase = '';
      notice = `Telegram bridge ${created.name} was created`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingTelegram = false;
    }
  }

  async function saveTelegramConfiguration(): Promise<void> {
    if (!selectedTelegram || savingTelegram) return;
    const replacementToken = editTelegramToken.trim();
    const chatId = editTelegramChatId.trim();
    const apiBase = editTelegramApiBase.trim();
    if (replacementToken && replacementToken.length < 20) {
      error = 'A replacement Telegram bot token must contain at least 20 characters.';
      return;
    }
    if (!validTelegramChatId(chatId)) {
      error = 'Telegram chat ID must be an integer, optionally beginning with a minus sign.';
      return;
    }
    savingTelegram = true;
    error = '';
    const path = selectedTelegram.path;
    try {
      await client().updateResource(path, {
        expected_revision: selectedTelegram.revision,
        spec: telegramSpec(
          replacementToken || stringSpec(selectedTelegram, 'bot_token'),
          chatId,
          editTelegramMode,
          apiBase,
          stringSpec(selectedTelegram, 'bot_username')
        )
      });
      await loadData();
      selectTelegramConfiguration(path);
      notice = `Telegram bridge ${selectedTelegram.name} was updated`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingTelegram = false;
    }
  }

  async function deleteTelegramConfiguration(): Promise<void> {
    const configuration = deleteTelegramTarget;
    if (!configuration || savingTelegram) return;
    savingTelegram = true;
    error = '';
    try {
      const api = client();
      const links =
        configuration.links?.filter(
          (link) =>
            link.relation_path === TELEGRAM_THREAD_TOPIC &&
            link.target.path === configuration.path
        ) ?? [];
      await Promise.all(
        links.map((link) => api.deleteResource(link.path, link.revision))
      );
      await api.deleteResource(configuration.path, configuration.revision);
      deleteTelegramTarget = null;
      await loadData(api);
      notice = `Telegram bridge ${configuration.name} is being deleted`;
    } catch (cause) {
      error = messageOf(cause);
      await loadData().catch(() => undefined);
    } finally {
      savingTelegram = false;
    }
  }

  async function createTelegramTopicLink(): Promise<void> {
    if (!selectedTelegram || savingTelegram) return;
    const threadPath = telegramMappingThreadPath;
    const thread = threads.find((candidate) => candidate.path === threadPath);
    if (!thread) {
      error = 'Choose a Thread to create its Telegram Topic.';
      return;
    }
    const topicName = titleOf(thread);
    if (telegramTopicLinks.some((link) => link.source.path === threadPath)) {
      error = 'This Thread already has a managed Topic for the selected bridge.';
      return;
    }
    savingTelegram = true;
    error = '';
    const configurationPath = selectedTelegram.path;
    try {
      await client().createLink({
        path: `${threadPath}/links/telegram/${slugify(configurationPath)}`,
        source: { kind: 'resource', path: threadPath },
        relation_path: TELEGRAM_THREAD_TOPIC,
        target: { kind: 'resource', path: configurationPath },
        metadata: { managed: true, topic_name: topicName }
      });
      await loadData();
      selectTelegramConfiguration(configurationPath);
      notice = `Telegram Topic “${topicName}” is being created`;
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingTelegram = false;
    }
  }

  async function deleteTelegramTopicLink(linkPath: string, revision: number): Promise<void> {
    if (!selectedTelegram || savingTelegram) return;
    savingTelegram = true;
    error = '';
    const configurationPath = selectedTelegram.path;
    try {
      await client().deleteResource(linkPath, revision);
      await loadData();
      selectTelegramConfiguration(configurationPath);
      notice = 'Telegram Topic mapping removed';
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      savingTelegram = false;
    }
  }

  function randomTelegramBindingToken(): string {
    const bytes = crypto.getRandomValues(new Uint8Array(24));
    let binary = '';
    for (const byte of bytes) binary += String.fromCharCode(byte);
    return btoa(binary).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
  }

  async function sha256Hex(value: string): Promise<string> {
    const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(value));
    return Array.from(new Uint8Array(digest))
      .map((byte) => byte.toString(16).padStart(2, '0'))
      .join('');
  }

  async function createTelegramBindingRequest(): Promise<void> {
    if (!selectedTelegram || !currentUser || bindingTelegram) return;
    const botUsername = stringSpec(selectedTelegram, 'bot_username');
    const configurationPath = selectedTelegram.path;
    if (!botUsername) {
      error = 'The Telegram Driver is still discovering the Bot username. Refresh and try again.';
      return;
    }
    bindingTelegram = true;
    error = '';
    try {
      const api = client();
      const existingChallenges = (currentUser.links ?? []).filter(
        (link) =>
          link.relation_path === TELEGRAM_BINDING_REQUEST &&
          link.target.path === configurationPath
      );
      await Promise.all(
        existingChallenges.map((link) => api.deleteResource(link.path, link.revision))
      );
      const token = randomTelegramBindingToken();
      await api.createLink({
        path: `${currentUser.path}/links/telegram-bindings/${crypto.randomUUID()}`,
        source: { kind: 'user', path: currentUser.path },
        relation_path: TELEGRAM_BINDING_REQUEST,
        target: { kind: 'resource', path: configurationPath },
        metadata: {
          token_hash: await sha256Hex(token),
          expires_at: new Date(Date.now() + 10 * 60 * 1000).toISOString()
        }
      });
      telegramBindingUrl = `https://t.me/${botUsername}?start=${token}`;
      await loadData(api);
      selectTelegramConfiguration(configurationPath);
      telegramBindingUrl = `https://t.me/${botUsername}?start=${token}`;
      notice = 'Binding link created. Open it in Telegram within 10 minutes.';
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      bindingTelegram = false;
    }
  }

  async function deleteTelegramBinding(): Promise<void> {
    if (!selectedTelegramBinding || bindingTelegram) return;
    bindingTelegram = true;
    error = '';
    const configurationPath = selectedTelegramPath;
    try {
      await client().deleteResource(
        selectedTelegramBinding.path,
        selectedTelegramBinding.revision
      );
      await loadData();
      selectTelegramConfiguration(configurationPath);
      notice = 'Telegram account was unbound';
    } catch (cause) {
      error = messageOf(cause);
    } finally {
      bindingTelegram = false;
    }
  }

  function openThreadManagement(path = activeThreadPath): void {
    if (!embeddedView && openBuiltInPlugin('threads')) return;
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
    if (embeddedView) {
      navigateHostThread(managedThread.path);
      return;
    }
    activeThreadPath = managedThread.path;
    view = 'chat';
    error = '';
    notice = '';
  }

  function openBuiltInPlugin(id: string): boolean {
    const entry = pluginEntries.find((candidate) => candidate.id === id);
    if (!entry) return false;
    void openFrontendPlugin(entry);
    return true;
  }

  function navigateHostThread(path: string): void {
    window.parent.postMessage(
      {
        source: 'kas-frontend-plugin',
        type: 'request',
        id: `navigation-${Date.now()}`,
        method: 'navigation.openThread',
        params: { path }
      },
      '*'
    );
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
      if (view === 'plugin' && selectedPlugin) {
        selectedPlugin =
          pluginEntries.find(
            (entry) =>
              entry.pluginPath === selectedPlugin?.pluginPath &&
              entry.id === selectedPlugin?.id
          ) ?? null;
        postPluginContext();
      }
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
    localStorage.setItem(
      SETTINGS_KEY,
      JSON.stringify({ apiBase: settings.apiBase, userPath: settings.userPath })
    );
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

  function approvalOperation(approval: Resource | null): Record<string, unknown> {
    return approval ? record(approval.spec.operation) : {};
  }

  function approvalRequester(approval: Resource | null): string {
    return approval ? relationTarget(approval, APPROVAL_REQUESTED_BY) ?? 'Unknown requester' : '';
  }

  function decisionApprover(decision: Resource): string {
    return relationTarget(decision, APPROVAL_DECIDED_BY) ?? 'Unknown approver';
  }

  function resultDecision(result: Resource): string {
    return relationTarget(result, APPROVAL_PRODUCED_BY) ?? 'Unknown decision';
  }

  function resultRequest(result: Resource): string {
    return relationTarget(result, APPROVAL_RESULT_OF) ?? 'Unknown request';
  }

  function operationVerb(approval: Resource | null): string {
    return String(approvalOperation(approval).verb ?? 'unknown');
  }

  function operationPath(approval: Resource | null): string {
    const operation = approvalOperation(approval);
    if (typeof operation.path === 'string') return operation.path;
    if (typeof operation.path_prefix === 'string') return operation.path_prefix;
    if (operation.verb === 'list' && typeof operation.manifest === 'string') {
      return operation.manifest;
    }
    return String(record(operation.resource).path ?? 'Unknown target');
  }

  function operationManifest(approval: Resource | null): string {
    const operation = approvalOperation(approval);
    if (typeof operation.manifest === 'string') return operation.manifest;
    const resource = record(operation.resource);
    return String(record(resource.metadata).manifest ?? 'Existing Resource');
  }

  function operationFields(approval: Resource | null): [string, string][] {
    const operation = approvalOperation(approval);
    const verb = operationVerb(approval);
    const payload =
      verb === 'create'
        ? record(record(operation.resource).spec)
        : verb === 'update'
          ? record(record(operation.update).spec)
          : {};
    const fields = Object.entries(payload).map(
      ([key, value]) => [fieldLabel(key), displayValue(value)] as [string, string]
    );
    if (verb === 'list') {
      if (typeof operation.path_prefix === 'string') {
        fields.push(['Path prefix', operation.path_prefix]);
      }
      if (typeof operation.limit === 'number') {
        fields.push(['Result limit', String(operation.limit)]);
      }
    }
    return fields;
  }

  function resultResponse(result: Resource | null): Record<string, unknown> {
    return result ? record(result.spec.response) : {};
  }

  function resultBodyRows(result: Resource | null): [string, string][] {
    const body = resultResponse(result).body;
    if (Array.isArray(body)) {
      if (body.length === 0) return [['Items', 'No results']];
      return body.slice(0, 12).map((value, index) => [
        `Item ${index + 1}`,
        resultValueSummary(value)
      ]);
    }
    if (isRecord(body)) {
      const entries = Object.entries(body);
      if (entries.length === 0) return [['Body', 'Empty object']];
      return entries.slice(0, 24).map(([key, value]) => [
        fieldLabel(key),
        resultValueSummary(value)
      ]);
    }
    return [['Body', displayValue(body)]];
  }

  function resultBodyOverflow(result: Resource | null): number {
    const body = resultResponse(result).body;
    if (Array.isArray(body)) return Math.max(0, body.length - 12);
    if (isRecord(body)) return Math.max(0, Object.keys(body).length - 24);
    return 0;
  }

  function resultValueSummary(value: unknown): string {
    if (Array.isArray(value)) return `${value.length} item${value.length === 1 ? '' : 's'}`;
    if (isRecord(value)) {
      const identity = value.path ?? value.name;
      if (typeof identity === 'string') return identity;
      const count = Object.keys(value).length;
      return `${count} field${count === 1 ? '' : 's'}`;
    }
    return displayValue(value);
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

<div class:embedded={Boolean(embeddedView)} class="shell">
  {#if !embeddedView}
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
        {#each pluginEntries.filter((entry) => entry.section === 'workspace') as entry}
          <button
            class:active={view === 'plugin' &&
              selectedPlugin?.pluginPath === entry.pluginPath &&
              selectedPlugin?.id === entry.id}
            onclick={() => void openFrontendPlugin(entry)}
          >
            <span class="nav-icon">{entry.icon}</span>
            <span><strong>{entry.label}</strong><small>{entry.description}</small></span>
          </button>
        {/each}
      </nav>

      {#if pluginEntries.some((entry) => entry.section === 'resources')}
        <div class="sidebar-section-title resource-title">Resources</div>
        <nav class="resource-nav" aria-label="Resource plugins">
          {#each pluginEntries.filter((entry) => entry.section === 'resources') as entry}
            <button
              class:active={view === 'plugin' &&
                selectedPlugin?.pluginPath === entry.pluginPath &&
                selectedPlugin?.id === entry.id}
              onclick={() => void openFrontendPlugin(entry)}
            >
              <span>{entry.label}</span><small>{entry.description}</small>
            </button>
          {/each}
        </nav>
      {/if}

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
  {/if}

  <main class="workspace">
    <header class="workspace-header">
      <div>
        <p class="eyebrow">
          {view === 'agents'
            ? 'Workspace'
            : view === 'skills'
              ? 'Capabilities'
              : view === 'approvals'
                ? 'Delegated authority'
                : view === 'threads'
                  ? 'Conversations'
                  : view === 'telegram'
                    ? 'Integrations'
                : view === 'objects'
                    ? 'KAS Registry'
                    : view === 'plugin'
                      ? 'Frontend Plugin'
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
        {:else if view === 'telegram'}
          <button
            class="danger-button"
            disabled={!selectedTelegram || savingTelegram}
            onclick={() => (deleteTelegramTarget = selectedTelegram)}
          >
            Delete bridge
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
    {:else if view === 'approvals'}
      <section class="approval-management" aria-label="Approval management">
        <div class="management-summary">
          <div>
            <strong>{pendingApprovalCount}</strong>
            <span>Pending</span>
          </div>
          <div>
            <strong>{approvalRequests.length}</strong>
            <span>Total requests</span>
          </div>
          <div>
            <strong>{approvalRequests.filter((approval) => approval.state === 'succeeded').length}</strong>
            <span>Executed</span>
          </div>
        </div>

        {#if approvalRequests.length === 0}
          <div class="management-empty">
            <p class="eyebrow">No Approval requests</p>
            <h2>Agent escalation requests will appear here.</h2>
            <p>Agents remain constrained by RBAC until a User approves one exact operation.</p>
          </div>
        {:else}
          <div class="approval-layout">
            <aside class="approval-index">
              <nav aria-label="Approval requests">
                {#each approvalRequests as approval}
                  <button
                    class:active={approval.path === selectedApprovalPath}
                    onclick={() => selectApproval(approval.path)}
                  >
                    <span>
                      <strong>{operationVerb(approval).toUpperCase()} {operationPath(approval)}</strong>
                      <small>{approvalRequester(approval)}</small>
                    </span>
                    <span class:pending={approval.state === 'pending'} class="state-pill">
                      {approval.state}
                    </span>
                  </button>
                {/each}
              </nav>
            </aside>

            {#if selectedApproval}
              <article class="approval-detail">
                <header>
                  <div>
                    <p class="eyebrow">Exact delegated operation</p>
                    <h2>{operationVerb(selectedApproval).toUpperCase()}</h2>
                    <code>{operationPath(selectedApproval)}</code>
                  </div>
                  <span class:pending={selectedApproval.state === 'pending'} class="state-pill">
                    {selectedApproval.state}
                  </span>
                </header>

                <section class="approval-reason">
                  <span>Reason supplied by Agent</span>
                  <p>{String(selectedApproval.spec.reason ?? 'No reason supplied')}</p>
                </section>

                <dl class="approval-facts">
                  <div>
                    <dt>Requested by</dt>
                    <dd>{approvalRequester(selectedApproval)}</dd>
                  </div>
                  <div>
                    <dt>Target Manifest</dt>
                    <dd>{operationManifest(selectedApproval)}</dd>
                  </div>
                  <div>
                    <dt>Expires</dt>
                    <dd>{formatTimestamp(String(selectedApproval.spec.expires_at))}</dd>
                  </div>
                  <div>
                    <dt>Request revision</dt>
                    <dd>{selectedApproval.revision}</dd>
                  </div>
                </dl>

                {#if operationFields(selectedApproval).length > 0}
                  <section class="approval-payload">
                    <h3>Requested fields</h3>
                    <dl>
                      {#each operationFields(selectedApproval) as [label, value]}
                        <div><dt>{label}</dt><dd>{value}</dd></div>
                      {/each}
                    </dl>
                  </section>
                {/if}

                {#if selectedApproval.state === 'pending'}
                  <div class="approval-actions">
                    <p>
                      Your decision is recorded under your own Approval path. The Driver verifies
                      your permission for this exact operation before it executes anything.
                    </p>
                    <div>
                      <button
                        class="danger-button"
                        disabled={savingApproval}
                        onclick={() => void decideApproval(selectedApproval!, 'reject')}
                      >
                        Reject
                      </button>
                      <button
                        class="primary-button"
                        disabled={savingApproval}
                        onclick={() => void decideApproval(selectedApproval!, 'approve')}
                      >
                        {savingApproval ? 'Executing…' : 'Approve & Execute'}
                      </button>
                    </div>
                  </div>
                {/if}

                {#if selectedApprovalDecisions.length > 0}
                  <section class="approval-records">
                    <div class="approval-section-heading">
                      <p class="eyebrow">Decision history</p>
                      <span>{selectedApprovalDecisions.length}</span>
                    </div>
                    {#each selectedApprovalDecisions as decision}
                      <article class:invalid={decision.spec.outcome === 'invalid'} class="decision-card">
                        <header>
                          <div>
                            <p class="eyebrow">Immutable decision record</p>
                            <h3>{String(decision.spec.outcome ?? decision.state)}</h3>
                          </div>
                          <span
                            class:invalid={decision.spec.outcome === 'invalid'}
                            class:superseded={decision.spec.outcome === 'superseded'}
                            class="state-pill"
                          >
                            {decision.state}
                          </span>
                        </header>
                        <dl class="approval-facts">
                          <div>
                            <dt>Decided by</dt>
                            <dd>{decisionApprover(decision)}</dd>
                          </div>
                          <div>
                            <dt>Decision path</dt>
                            <dd>{decision.path}</dd>
                          </div>
                          <div>
                            <dt>Decided at</dt>
                            <dd>{formatTimestamp(String(decision.spec.decided_at))}</dd>
                          </div>
                          <div>
                            <dt>Completed at</dt>
                            <dd>
                              {decision.spec.completed_at
                                ? formatTimestamp(String(decision.spec.completed_at))
                                : 'In progress'}
                            </dd>
                          </div>
                        </dl>
                        {#if decision.spec.error}
                          <p class="decision-error">{String(decision.spec.error)}</p>
                        {:else if decision.spec.outcome === 'invalid'}
                          <p class="decision-error">
                            This approver did not have permission to perform the requested
                            operation. The request remains available to another approver.
                          </p>
                        {:else if decision.spec.outcome === 'superseded'}
                          <p class="decision-result">
                            Another valid decision claimed this request first, so this decision did
                            not execute the operation.
                          </p>
                        {/if}
                      </article>
                    {/each}
                  </section>
                {/if}

                {#if selectedApprovalResults.length > 0}
                  <section class="approval-records">
                    <div class="approval-section-heading">
                      <p class="eyebrow">Protected results</p>
                      <span>{selectedApprovalResults.length}</span>
                    </div>
                    {#each selectedApprovalResults as result}
                      {@const response = resultResponse(result)}
                      <article class="approval-result-card">
                        <header>
                          <div>
                            <p class="eyebrow">Protected approval result</p>
                            <h3>API response</h3>
                          </div>
                          <span class="state-pill">{result.state}</span>
                        </header>
                        <dl class="approval-facts">
                          <div>
                            <dt>HTTP status</dt>
                            <dd>{String(response.status ?? 'Unknown')}</dd>
                          </div>
                          <div>
                            <dt>Content type</dt>
                            <dd>{String(response.content_type ?? 'Not supplied')}</dd>
                          </div>
                          <div>
                            <dt>Result path</dt>
                            <dd>{result.path}</dd>
                          </div>
                          <div>
                            <dt>Produced by</dt>
                            <dd>{resultDecision(result)}</dd>
                          </div>
                          <div>
                            <dt>Request</dt>
                            <dd>{resultRequest(result)}</dd>
                          </div>
                        </dl>
                        <div class="approval-result-body">
                          <h4>Response body</h4>
                          <dl>
                            {#each resultBodyRows(result) as [label, value]}
                              <div>
                                <dt>{label}</dt>
                                <dd>{value}</dd>
                              </div>
                            {/each}
                          </dl>
                          {#if resultBodyOverflow(result) > 0}
                            <p>
                              {resultBodyOverflow(result)} additional
                              {resultBodyOverflow(result) === 1 ? ' entry' : ' entries'}
                              omitted from this summary.
                            </p>
                          {/if}
                        </div>
                      </article>
                    {/each}
                  </section>
                {/if}
              </article>
            {/if}
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
    {:else if view === 'telegram'}
      <section class="telegram-management" aria-label="Telegram bridge management">
        <div class="management-summary">
          <div>
            <strong>{telegramConfigurations.length}</strong>
            <span>Bridges</span>
          </div>
          <div>
            <strong>{telegramConfigurations.filter(resourceConverged).length}</strong>
            <span>Converged</span>
          </div>
          <div>
            <strong>{telegramTopicLinks.length}</strong>
            <span>Topic mappings</span>
          </div>
        </div>

        <div class="telegram-manager-layout">
          <aside class="telegram-index">
            <form
              class="telegram-create"
              onsubmit={(event) => {
                event.preventDefault();
                void createTelegramConfiguration();
              }}
            >
              <div>
                <strong>New Telegram bridge</strong>
                <small>Connect one forum group to KAS Threads.</small>
              </div>
              <label>
                Name
                <input
                  bind:value={createTelegramName}
                  oninput={updateTelegramCreatePath}
                  placeholder="Team Telegram"
                  required
                />
              </label>
              <label>
                Resource path
                <input bind:value={createTelegramPath} placeholder="/telegram/team" required />
              </label>
              <label>
                Bot token
                <input
                  bind:value={createTelegramToken}
                  type="password"
                  minlength="20"
                  autocomplete="new-password"
                  placeholder="Token from BotFather"
                  required
                />
              </label>
              <label>
                Forum group chat ID
                <input
                  bind:value={createTelegramChatId}
                  inputmode="numeric"
                  pattern="-?[0-9]+"
                  placeholder="-1001234567890"
                  required
                />
              </label>
              <label>
                Sync mode
                <select bind:value={createTelegramMode}>
                  <option value="bidirectional">Bidirectional</option>
                  <option value="telegram-to-kas">Telegram → KAS</option>
                  <option value="kas-to-telegram">KAS → Telegram</option>
                </select>
              </label>
              <label>
                API base <small>Optional</small>
                <input
                  bind:value={createTelegramApiBase}
                  type="url"
                  placeholder="https://api.telegram.org"
                />
              </label>
              <button class="primary-button" type="submit" disabled={savingTelegram}>
                {savingTelegram ? 'Creating…' : 'Create bridge'}
              </button>
            </form>

            <nav aria-label="Telegram bridges">
              {#each telegramConfigurations as configuration}
                <button
                  class:active={configuration.path === selectedTelegramPath}
                  onclick={() => selectTelegramConfiguration(configuration.path)}
                >
                  <span>
                    <strong>{configuration.name}</strong>
                    <code>{configuration.path}</code>
                  </span>
                  <small>{resourceConverged(configuration) ? resourceState(configuration) : 'reconciling'}</small>
                </button>
              {/each}
            </nav>
          </aside>

          {#if selectedTelegram}
            <article class="telegram-editor">
              <header>
                <div>
                  <p class="eyebrow">Telegram Resource</p>
                  <h2>{selectedTelegram.name}</h2>
                  <code>{selectedTelegram.path}</code>
                </div>
                <span class:pending={!resourceConverged(selectedTelegram)} class="state-pill">
                  {resourceConverged(selectedTelegram)
                    ? resourceState(selectedTelegram)
                    : 'reconciling'}
                </span>
              </header>

              <form
                class="telegram-settings"
                onsubmit={(event) => {
                  event.preventDefault();
                  void saveTelegramConfiguration();
                }}
              >
                <div class="telegram-field-grid">
                  <label>
                    Forum group chat ID
                    <input
                      bind:value={editTelegramChatId}
                      inputmode="numeric"
                      pattern="-?[0-9]+"
                      required
                    />
                  </label>
                  <label>
                    Sync mode
                    <select bind:value={editTelegramMode}>
                      <option value="bidirectional">Bidirectional</option>
                      <option value="telegram-to-kas">Telegram → KAS</option>
                      <option value="kas-to-telegram">KAS → Telegram</option>
                    </select>
                  </label>
                </div>
                <label>
                  Replacement bot token
                  <input
                    bind:value={editTelegramToken}
                    type="password"
                    minlength="20"
                    autocomplete="new-password"
                    placeholder="Leave blank to keep the current token"
                  />
                  <small>The existing token is never shown. Leave this field empty to retain it.</small>
                </label>
                <label>
                  API base <small>Optional</small>
                  <input
                    bind:value={editTelegramApiBase}
                    type="url"
                    placeholder="https://api.telegram.org"
                  />
                </label>
                <div class="telegram-form-actions">
                  <span>Revision {selectedTelegram.revision}</span>
                  <button class="primary-button" type="submit" disabled={savingTelegram}>
                    {savingTelegram ? 'Saving…' : 'Save changes'}
                  </button>
                </div>
              </form>

              <section class="telegram-account-binding" aria-label="Telegram account binding">
                <header>
                  <div>
                    <strong>Your Telegram account</strong>
                    <small>
                      Bind Telegram to <code>{settings.userPath}</code> for attributed messages and
                      private Approval buttons.
                    </small>
                  </div>
                  {#if selectedTelegramBinding}
                    <span class="state-pill">bound</span>
                  {:else}
                    <span class="state-pill pending">not bound</span>
                  {/if}
                </header>

                {#if selectedTelegramBinding}
                  <div class="telegram-binding-card">
                    <div>
                      <strong>
                        {typeof selectedTelegramBinding.metadata.username === 'string' &&
                        selectedTelegramBinding.metadata.username
                          ? `@${selectedTelegramBinding.metadata.username}`
                          : `Telegram ${selectedTelegramBinding.metadata.user_id}`}
                      </strong>
                      <code>{selectedTelegramBinding.target.path}</code>
                      <small>
                        Telegram user ID {String(selectedTelegramBinding.metadata.user_id)}
                      </small>
                    </div>
                    <button
                      class="danger-button"
                      type="button"
                      disabled={bindingTelegram}
                      onclick={() => void deleteTelegramBinding()}
                    >
                      {bindingTelegram ? 'Unbinding…' : 'Unbind'}
                    </button>
                  </div>
                {:else}
                  <p class="telegram-managed-note">
                    The link must be opened by you in a private chat with the Bot. Telegram usernames
                    are display-only; the numeric Telegram user ID is bound to your KAS User.
                  </p>
                  <div class="telegram-binding-actions">
                    <button
                      class="quiet-button"
                      type="button"
                      disabled={bindingTelegram}
                      onclick={() => void createTelegramBindingRequest()}
                    >
                      {bindingTelegram ? 'Generating…' : 'Generate binding link'}
                    </button>
                    {#if telegramBindingUrl}
                      <a
                        class="primary-button telegram-open-link"
                        href={telegramBindingUrl}
                        target="_blank"
                        rel="noreferrer"
                      >
                        Open Telegram
                      </a>
                      <small>Expires in 10 minutes. Refresh after the Bot confirms the binding.</small>
                    {/if}
                  </div>
                {/if}
              </section>

              <section class="telegram-mappings" aria-label="Thread to Telegram Topic mappings">
                <header>
                  <div>
                    <strong>Managed Telegram Topics</strong>
                    <small>KAS asks the Bot to create one forum Topic for each mapped Thread.</small>
                  </div>
                  <span>{telegramTopicLinks.length}</span>
                </header>

                <p class="telegram-managed-note">
                  Only Topics created here are synchronized. Existing Telegram Topics and the
                  General Topic are ignored. The Thread title is the Topic name and renaming the
                  Thread updates it in Telegram.
                </p>

                <form
                  class="telegram-mapping-create"
                  onsubmit={(event) => {
                    event.preventDefault();
                    void createTelegramTopicLink();
                  }}
                >
                  <label>
                    KAS Thread
                    <select
                      value={telegramMappingThreadPath}
                      onchange={(event) =>
                        selectTelegramMappingThread(
                          (event.currentTarget as HTMLSelectElement).value
                        )}
                      required
                    >
                      <option value="" disabled>Choose a Thread</option>
                      {#each threads as thread}
                        <option value={thread.path}>{titleOf(thread)} · {thread.path}</option>
                      {/each}
                    </select>
                  </label>
                  <button
                    class="quiet-button"
                    type="submit"
                    disabled={savingTelegram || threads.length === 0}
                  >
                    {savingTelegram ? 'Creating…' : 'Create Telegram Topic'}
                  </button>
                </form>

                {#if telegramTopicLinks.length === 0}
                  <p class="telegram-mapping-empty">
                    No managed Topics yet. Choose a Thread above and let the Bot create one.
                  </p>
                {:else}
                  <div class="telegram-mapping-list">
                    {#each telegramTopicLinks as link}
                      {@const thread = threads.find((candidate) => candidate.path === link.source.path)}
                      <article>
                        <div>
                          <strong>{thread ? titleOf(thread) : link.source.path}</strong>
                          <code>{link.source.path}</code>
                        </div>
                        <span>↔</span>
                        <div>
                          <small>Telegram Topic</small>
                          <strong>
                            {typeof link.metadata.topic_name === 'string'
                              ? link.metadata.topic_name
                              : thread
                                ? titleOf(thread)
                                : 'Managed Topic'}
                          </strong>
                          {#if typeof link.metadata.topic_id === 'number' ||
                          typeof link.metadata.topic_id === 'string'}
                            <code>Topic ID {link.metadata.topic_id}</code>
                          {:else}
                            <span class="telegram-provisioning">Provisioning…</span>
                          {/if}
                        </div>
                        <button
                          class="danger-button"
                          type="button"
                          disabled={savingTelegram}
                          onclick={() => void deleteTelegramTopicLink(link.path, link.revision)}
                        >
                          Remove
                        </button>
                      </article>
                    {/each}
                  </div>
                {/if}
              </section>
            </article>
          {:else}
            <div class="management-empty">
              <p class="eyebrow">No Telegram bridges</p>
              <h2>Connect a Telegram forum group.</h2>
              <p>Create a bridge with the form to begin syncing Topics and KAS Threads.</p>
            </div>
          {/if}
        </div>
      </section>
    {:else if view === 'plugin'}
      <section class="frontend-plugin-host" aria-label={selectedPlugin?.label ?? 'Frontend Plugin'}>
        {#if loading || !pluginUrl}
          <div class="plugin-loading">Loading Frontend Plugin…</div>
        {:else}
          <iframe
            bind:this={pluginFrame}
            src={pluginUrl}
            title={selectedPlugin?.label ?? 'Frontend Plugin'}
            sandbox="allow-scripts allow-forms"
          ></iframe>
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

{#if showSettings && !embeddedView}
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

{#if deleteTelegramTarget}
  <div class="modal-backdrop" role="presentation">
    <div
      class="modal destructive-modal"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="delete-telegram-title"
    >
      <div class="modal-kicker danger-text">Delete Resource</div>
      <h2 id="delete-telegram-title">Delete {deleteTelegramTarget.name}?</h2>
      <p>
        This removes its Thread ↔ Topic Links before deleting the Telegram bridge. Messages and
        Threads already created through the bridge are retained.
      </p>
      <code class="delete-path">{deleteTelegramTarget.path}</code>
      <div class="modal-actions">
        <button
          type="button"
          class="quiet-button"
          onclick={() => (deleteTelegramTarget = null)}
        >
          Cancel
        </button>
        <button
          type="button"
          class="danger-button solid"
          disabled={savingTelegram}
          onclick={() => void deleteTelegramConfiguration()}
        >
          {savingTelegram ? 'Deleting…' : 'Delete bridge'}
        </button>
      </div>
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
