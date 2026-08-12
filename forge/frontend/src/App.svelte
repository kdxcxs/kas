<script lang="ts">
  import { onMount } from 'svelte';
  import {
    AGENT_ACTION,
    AGENT_MANIFEST,
    ApiError,
    DECIDED_BY,
    ForgeApi,
    LINK_MANIFEST,
    PACKAGE_MANIFEST,
    REQUEST_MANIFEST,
    REQUESTED_BY,
    RUN_MANIFEST,
    linkTarget,
    state,
    type Resource
  } from './lib/api';

  type View = 'overview' | 'agents' | 'approvals' | 'packages';

  let token = '';
  let tokenInput = '';
  let view: View = 'overview';
  let agents: Resource[] = [];
  let requests: Resource[] = [];
  let packages: Resource[] = [];
  let runs: Resource[] = [];
  let links: Resource[] = [];
  let loading = false;
  let actionBusy = false;
  let notice = '';
  let error = '';
  let selectedAgentPath = '';
  let selectedRequestPath = '';
  let agentName = '';
  let agentDirectory = '';
  let agentDescription = '';
  let createAgentOpen = false;
  let agentListInitialized = false;
  let taskPrompt = '';
  let packageFile: File | null = null;
  let packageReason = '';
  let poll: ReturnType<typeof setInterval> | undefined;
  let api = new ForgeApi('');

  $: selectedAgent = agents.find((agent) => agent.path === selectedAgentPath) ?? agents[0] ?? null;
  $: selectedRequest =
    requests.find((request) => request.path === selectedRequestPath) ?? requests[0] ?? null;
  $: pendingRequests = requests.filter((request) => request.metadata.state === 'pending');
  $: agentRuns = selectedAgent
    ? runs
        .filter((run) => run.spec.resource === selectedAgent.path)
        .sort((left, right) => right.path.localeCompare(left.path))
    : [];

  onMount(() => {
    const queryToken = new URLSearchParams(location.search).get('token');
    token = queryToken || localStorage.getItem('kas-forge-token') || '';
    api = new ForgeApi(token);
    tokenInput = token;
    agentDirectory = '/tmp';
    if (queryToken) {
      localStorage.setItem('kas-forge-token', queryToken);
      history.replaceState({}, '', location.pathname);
    }
    if (token) void refresh();
    poll = setInterval(() => token && void refresh(true), 2000);
    return () => poll && clearInterval(poll);
  });

  async function refresh(silent = false): Promise<void> {
    if (!token || (loading && silent)) return;
    if (!silent) loading = true;
    try {
      [agents, requests, packages, runs, links] = await Promise.all([
        api.list(AGENT_MANIFEST),
        api.list(REQUEST_MANIFEST),
        api.list(PACKAGE_MANIFEST),
        api.list(RUN_MANIFEST),
        api.list(LINK_MANIFEST)
      ]);
      requests = requests.sort((left, right) =>
        String(right.spec.submitted_at).localeCompare(String(left.spec.submitted_at))
      );
      if (!agentListInitialized) {
        createAgentOpen = agents.length === 0;
        agentListInitialized = true;
      }
      if (!selectedAgentPath) selectedAgentPath = agents[0]?.path ?? '';
      if (!selectedRequestPath) selectedRequestPath = requests[0]?.path ?? '';
      error = '';
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) {
        localStorage.removeItem('kas-forge-token');
        token = '';
        tokenInput = '';
        api = new ForgeApi('');
        error = 'This KAS credential is no longer valid. Connect again with a current credential.';
      } else if (!silent) {
        error = message(cause);
      }
    } finally {
      if (!silent) loading = false;
    }
  }

  function connect(): void {
    token = tokenInput.trim();
    if (!token) return;
    api = new ForgeApi(token);
    localStorage.setItem('kas-forge-token', token);
    void refresh();
  }

  async function createAgent(): Promise<void> {
    const slug = agentName
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-|-$/g, '');
    if (!slug || !agentDirectory.trim()) return;
    await perform(async () => {
      const created = await api.create({
        path: `/packages/forge/agent/agents/${slug}`,
        metadata: { manifest: AGENT_MANIFEST, name: agentName.trim() },
        spec: {
          working_directory: agentDirectory.trim(),
          description: agentDescription.trim()
        }
      });
      selectedAgentPath = created.path;
      agentName = '';
      agentDescription = '';
      createAgentOpen = false;
      notice = 'Agent created. Forge is provisioning its scoped ServiceAccount.';
    });
  }

  async function runAgent(): Promise<void> {
    if (!selectedAgent || !taskPrompt.trim()) return;
    await perform(async () => {
      const id = crypto.randomUUID();
      await api.createRun({
        request_id: id,
        resource: selectedAgent.path,
        action: AGENT_ACTION,
        input: { prompt: taskPrompt.trim() }
      });
      taskPrompt = '';
      notice = 'Task queued. The Codex Agent is running with its scoped KAS identity.';
    });
  }

  async function decide(decision: 'approve' | 'reject'): Promise<void> {
    if (!selectedRequest) return;
    await perform(async () => {
      await api.decide(selectedRequest!, decision);
      notice =
        decision === 'approve'
          ? 'Package approved and installation completed.'
          : 'Package Request rejected; no capability was installed.';
    });
  }

  async function submitPackage(): Promise<void> {
    if (!packageFile || !packageReason.trim()) return;
    await perform(async () => {
      const request = await api.submitPackage(packageFile!, packageReason.trim());
      selectedRequestPath = request.path;
      packageFile = null;
      packageReason = '';
      notice = 'Package validated and submitted for approval.';
    });
  }

  async function perform(operation: () => Promise<void>): Promise<void> {
    actionBusy = true;
    notice = '';
    error = '';
    try {
      await operation();
      await refresh(true);
    } catch (cause) {
      error = message(cause);
    } finally {
      actionBusy = false;
    }
  }

  function requester(request: Resource): string {
    return linkTarget(links, request.path, REQUESTED_BY) || 'Resolving identity…';
  }

  function approver(request: Resource): string {
    return linkTarget(links, request.path, DECIDED_BY) || '';
  }

  function response(run: Resource): string {
    const output = run.spec.output as Record<string, unknown> | undefined;
    return typeof output?.response === 'string' ? output.response : '';
  }

  function message(cause: unknown): string {
    return cause instanceof Error ? cause.message : String(cause);
  }

  function short(path: string): string {
    return path.split('/').filter(Boolean).at(-1) || path;
  }

  function formatBytes(value: unknown): string {
    const bytes = Number(value || 0);
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  }
</script>

{#if !token}
  <main class="login-shell">
    <section class="login-card">
      <div class="brand-mark">K</div>
      <p class="eyebrow">KAS FORGE</p>
      <h1>Give Agents room to build.<br />Keep humans in control.</h1>
      <p class="muted">
        Connect with a KAS credential to manage engineering Agents and approve new capabilities.
      </p>
      {#if error}<div class="banner error">{error}</div>{/if}
      <label>
        KAS credential
        <input bind:value={tokenInput} onkeydown={(event) => event.key === 'Enter' && connect()} placeholder="kas_…" />
      </label>
      <button class="primary" onclick={connect}>Open Forge</button>
    </section>
  </main>
{:else}
  <div class="app-shell">
    <aside class="sidebar">
      <header class="brand">
        <div class="brand-mark">K</div>
        <div><strong>KAS FORGE</strong><small>Agent-native engineering</small></div>
      </header>

      <nav>
        <p class="nav-label">WORKSPACE</p>
        <button class:active={view === 'overview'} onclick={() => (view = 'overview')}><span>⌂</span>Overview</button>
        <button class:active={view === 'agents'} onclick={() => (view = 'agents')}><span>△</span>Agents<em>{agents.length}</em></button>
        <button class:active={view === 'approvals'} onclick={() => (view = 'approvals')}><span>✓</span>Approvals<em class:alert={pendingRequests.length > 0}>{pendingRequests.length}</em></button>
        <button class:active={view === 'packages'} onclick={() => (view = 'packages')}><span>◇</span>Packages<em>{packages.length}</em></button>
      </nav>

      <div class="sidebar-foot">
        <div class="status-dot"></div>
        <div><strong>Control plane connected</strong><small>Refreshes every 2 seconds</small></div>
      </div>
    </aside>

    <main class="workspace">
      <header class="topbar">
        <div>
          <p class="eyebrow">{view === 'overview' ? 'ENGINEERING CONTROL PLANE' : view.toUpperCase()}</p>
          <h1>{view === 'overview' ? 'Forge' : view === 'agents' ? 'Engineering Agents' : view === 'approvals' ? 'Capability approvals' : 'Installed Packages'}</h1>
        </div>
        <button class="icon-button" onclick={() => refresh()} aria-label="Refresh">↻</button>
      </header>

      {#if error}<div class="banner error">{error}<button onclick={() => (error = '')}>×</button></div>{/if}
      {#if notice}<div class="banner success">{notice}<button onclick={() => (notice = '')}>×</button></div>{/if}

      {#if view === 'overview'}
        <section class="hero">
          <div>
            <p class="eyebrow">CONTROLLED SELF-EXTENSION</p>
            <h2>Agents can propose the next capability.<br /><span>You decide what becomes real.</span></h2>
            <p>Forge gives every Agent a scoped identity. It can inspect the Resource graph, work in a repository, and submit a validated Package—but installation stays behind a human approval boundary.</p>
            <div class="hero-actions"><button class="primary" onclick={() => (view = 'agents')}>Run an Agent</button><button onclick={() => (view = 'approvals')}>Review requests</button></div>
          </div>
          <div class="loop-card" aria-label="Capability loop">
            <div><b>1</b><span>Agent detects a missing capability</span></div>
            <i></i>
            <div><b>2</b><span>Builds and validates a Package</span></div>
            <i></i>
            <div class="highlight"><b>3</b><span>User reviews and approves</span></div>
            <i></i>
            <div><b>4</b><span>KAS installs it as new Resources</span></div>
          </div>
        </section>

        <section class="metrics">
          <article><small>AVAILABLE AGENTS</small><strong>{agents.filter((agent) => state(agent) === 'available').length}</strong><span>Scoped Codex workers</span></article>
          <article><small>PENDING APPROVALS</small><strong class:accent={pendingRequests.length > 0}>{pendingRequests.length}</strong><span>Waiting for a human</span></article>
          <article><small>INSTALLED PACKAGES</small><strong>{packages.length}</strong><span>Accumulated capabilities</span></article>
        </section>

        <section class="panel recent">
          <div class="section-heading"><div><p class="eyebrow">INBOX</p><h3>Latest capability requests</h3></div><button onclick={() => (view = 'approvals')}>View all →</button></div>
          {#if requests.length === 0}<p class="empty">No Package Requests yet.</p>{:else}
            {#each requests.slice(0, 4) as request}
              <button class="request-row" onclick={() => { selectedRequestPath = request.path; view = 'approvals'; }}>
                <span class="package-glyph">◇</span><span><strong>{String(request.spec.name)}</strong><small>{requester(request)}</small></span><code>{String(request.spec.package_path)}</code><span class="pill {request.metadata.state}">{request.metadata.state}</span>
              </button>
            {/each}
          {/if}
        </section>
      {:else if view === 'agents'}
        <section class="split-layout agents-layout">
          <aside class="index-panel">
            <div class="section-heading"><div><p class="eyebrow">WORKERS</p><h3>{agents.length} Agents</h3></div></div>
            {#each agents as agent}
              <button class:active={selectedAgent?.path === agent.path} onclick={() => (selectedAgentPath = agent.path)}>
                <span class="avatar">{agent.metadata.name.slice(0, 1).toUpperCase()}</span>
                <span><strong>{agent.metadata.name}</strong><small>{short(agent.path)}</small></span>
                <span class="status-dot" class:pending={state(agent) !== 'available'}></span>
              </button>
            {/each}
            <details class="create-box" bind:open={createAgentOpen}>
              <summary>＋ New Agent</summary>
              <label>Name<input bind:value={agentName} placeholder="Release Engineer" /></label>
              <label>Working directory<input bind:value={agentDirectory} placeholder="/path/to/repository" /></label>
              <label>Description<textarea bind:value={agentDescription} placeholder="What this Agent owns"></textarea></label>
              <button class="primary" disabled={actionBusy} onclick={createAgent}>Create Agent</button>
            </details>
          </aside>

          <div class="detail-panel">
            {#if selectedAgent}
              <header class="agent-header"><span class="avatar large">{selectedAgent.metadata.name.slice(0, 1).toUpperCase()}</span><div><p class="eyebrow">{state(selectedAgent)}</p><h2>{selectedAgent.metadata.name}</h2><code>{selectedAgent.path}</code></div></header>
              <div class="agent-meta"><div><small>WORKING DIRECTORY</small><strong>{String(selectedAgent.spec.working_directory)}</strong></div><div><small>IDENTITY</small><strong>Scoped ServiceAccount</strong></div><div><small>PACKAGE INSTALL</small><strong>Approval required</strong></div></div>
              <section class="task-composer">
                <p class="eyebrow">NEW ENGINEERING TASK</p>
                <textarea bind:value={taskPrompt} placeholder="Inspect this repository and propose the KAS Package needed to…"></textarea>
                <div><small>The Agent can edit this workspace and submit Packages, but cannot install them.</small><button class="primary" disabled={actionBusy || state(selectedAgent) !== 'available'} onclick={runAgent}>Run task <span>↗</span></button></div>
              </section>
              <section class="run-list"><p class="eyebrow">RECENT RUNS</p>
                {#if agentRuns.length === 0}<p class="empty">No tasks have been run with this Agent.</p>{/if}
                {#each agentRuns.slice(0, 8) as run}
                  <article><header><span class="pill {run.metadata.state}">{run.metadata.state}</span><code>{short(run.path)}</code></header><p>{String((run.spec.input as Record<string, unknown>)?.prompt || '')}</p>{#if response(run)}<pre>{response(run)}</pre>{/if}{#if run.spec.error}<div class="run-error">{String(run.spec.error)}</div>{/if}</article>
                {/each}
              </section>
            {:else}<div class="empty-state"><span>△</span><h2>Create the first Agent</h2><p>Each Agent receives its own KAS ServiceAccount and least-privilege Role.</p></div>{/if}
          </div>
        </section>
      {:else if view === 'approvals'}
        <section class="approval-summary"><article><small>PENDING</small><strong>{pendingRequests.length}</strong></article><article><small>INSTALLED</small><strong>{requests.filter((request) => request.metadata.state === 'installed').length}</strong></article><article><small>REJECTED / FAILED</small><strong>{requests.filter((request) => ['rejected','failed'].includes(request.metadata.state)).length}</strong></article></section>
        <section class="split-layout approval-layout">
          <aside class="index-panel">
            <div class="section-heading"><div><p class="eyebrow">PACKAGE REQUESTS</p><h3>Review queue</h3></div></div>
            {#each requests as request}
              <button class:active={selectedRequest?.path === request.path} onclick={() => (selectedRequestPath = request.path)}>
                <span class="package-glyph">◇</span><span><strong>{String(request.spec.name)}</strong><small>{requester(request)}</small></span><span class="pill {request.metadata.state}">{request.metadata.state}</span>
              </button>
            {/each}
            {#if requests.length === 0}<p class="empty">The approval inbox is empty.</p>{/if}
          </aside>

          <div class="detail-panel">
            {#if selectedRequest}
              <header class="request-title"><div><p class="eyebrow">CAPABILITY PROPOSAL</p><h2>{String(selectedRequest.spec.name)}</h2><code>{String(selectedRequest.spec.package_path)}</code></div><span class="pill large {selectedRequest.metadata.state}">{selectedRequest.metadata.state}</span></header>
              <div class="reason"><small>WHY THIS IS NEEDED</small><p>{String(selectedRequest.spec.reason)}</p><span>Requested by <code>{requester(selectedRequest)}</code></span></div>
              <div class="package-grid"><div><small>MANIFEST</small><code>{String(selectedRequest.spec.manifest_path)}</code></div><div><small>VERSION</small><strong>v{String(selectedRequest.spec.version)}</strong></div><div><small>RESOURCES</small><strong>{String(selectedRequest.spec.resource_count)}</strong></div><div><small>DRIVER</small><strong>{selectedRequest.spec.has_driver ? 'Included' : 'No process'}</strong></div><div><small>ARTIFACT</small><strong>{formatBytes(selectedRequest.spec.size_bytes)}</strong></div><div><small>DIGEST</small><code>{String(selectedRequest.spec.digest).slice(0, 20)}…</code></div></div>
              <section class="boundary"><div class="shield">✓</div><div><strong>Validated before review</strong><p>The archive contains normalized files, a sandboxed Manifest, valid Resource documents, and every declared Driver entrypoint.</p></div></section>
              {#if selectedRequest.metadata.state === 'pending'}
                <div class="decision-bar"><div><strong>This action changes the control plane</strong><small>Your own KAS permission is checked again during installation.</small></div><button disabled={actionBusy} onclick={() => decide('reject')}>Reject</button><button class="primary" disabled={actionBusy} onclick={() => decide('approve')}>Approve & install</button></div>
              {:else}
                <div class="decision-record"><small>DECISION RECORD</small><strong>{String((selectedRequest.spec.decision as Record<string, unknown>)?.outcome || selectedRequest.metadata.state)}</strong><span>{approver(selectedRequest)}</span>{#if (selectedRequest.spec.decision as Record<string, unknown>)?.error}<p>{String((selectedRequest.spec.decision as Record<string, unknown>).error)}</p>{/if}</div>
              {/if}
            {:else}<div class="empty-state"><span>✓</span><h2>No requests to review</h2><p>Agent-submitted Packages will appear here after validation.</p></div>{/if}
          </div>
        </section>
        <details class="upload-panel"><summary>Submit a Package manually</summary><div><label>.kas archive<input type="file" accept=".kas,application/x-tar" onchange={(event) => (packageFile = event.currentTarget.files?.[0] || null)} /></label><label>Reason<input bind:value={packageReason} placeholder="Why Forge needs this capability" /></label><button class="primary" disabled={actionBusy || !packageFile} onclick={submitPackage}>Validate & submit</button></div></details>
      {:else}
        <section class="packages-grid">
          {#each packages as pkg}
            <article><span class="package-glyph large">◇</span><div><p class="eyebrow">PACKAGE</p><h3>{pkg.metadata.name}</h3><code>{pkg.path}</code></div><dl><dt>Manifest</dt><dd>{String(pkg.spec.manifest)}</dd><dt>Version</dt><dd>{String(pkg.spec.manifest_version)}</dd><dt>Size</dt><dd>{formatBytes(pkg.spec.size_bytes)}</dd><dt>Digest</dt><dd><code>{String(pkg.spec.digest).slice(0, 24)}…</code></dd></dl></article>
          {/each}
        </section>
      {/if}
    </main>
  </div>
{/if}
