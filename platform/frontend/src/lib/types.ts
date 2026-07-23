export interface ObjectRef {
  kind: string;
  path: string;
}

export interface Link {
  path: string;
  source: ObjectRef;
  relation_path: string;
  target: ObjectRef;
  metadata: Record<string, unknown>;
  created_at: string;
}

export interface Resource {
  path: string;
  name: string;
  spec: Record<string, unknown>;
  status: Record<string, unknown>;
  revision: number;
  created_at: string;
  updated_at: string;
  links?: Link[];
}

export interface Run {
  path: string;
  request_id: string;
  driver_generation: number | null;
  input: Record<string, unknown>;
  status: 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled';
  output: Record<string, unknown> | null;
  error: string | null;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
}

export interface Driver {
  path: string;
  desired_state: 'running' | 'stopped';
  state: 'stopped' | 'starting' | 'ready' | 'stopping' | 'failed';
  generation: number;
  process_id: number | null;
  metadata: Record<string, unknown>;
  error: string | null;
}

export interface PlannedLink {
  path: string;
  source: ObjectRef;
  relation_path: string;
  target: ObjectRef;
  metadata: Record<string, unknown>;
}

export interface CreateResource {
  path: string;
  manifest: string;
  name: string;
  spec: Record<string, unknown>;
  links?: PlannedLink[];
}

