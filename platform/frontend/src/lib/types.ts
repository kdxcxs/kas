export type ObjectKind =
  | 'manifest'
  | 'action'
  | 'relation'
  | 'resource'
  | 'driver'
  | 'run'
  | 'link'
  | 'user'
  | 'service_account'
  | 'role'
  | 'role_binding'
  | 'credential'
  | 'package';

export interface ObjectRef {
  kind: ObjectKind;
  path: string;
  name?: string;
  state?: string;
  manifest?: string;
}

export interface DriverObservation {
  driver_revision: number;
  resource_revision: number;
}

export interface ResourceMetadata {
  path: string;
  manifest: string;
  name: string;
  state: string;
  '[kas]': {
    revision: number;
    observed: Record<string, DriverObservation>;
    created_at: string;
    updated_at: string;
  };
}

export interface ResourceDocument {
  metadata: ResourceMetadata;
  spec: Record<string, unknown>;
  status: {
    metadata: ResourceMetadata;
    spec: Record<string, unknown>;
  };
}

export interface Link {
  path: string;
  source: ObjectRef;
  relation_path: string;
  target: ObjectRef;
  spec: Record<string, unknown>;
  status: Record<string, unknown>;
  metadata: Record<string, unknown>;
  revision: number;
  created_at: string;
  updated_at: string;
}

export interface Resource {
  path: string;
  manifest: string;
  name: string;
  state: string;
  status_state: string;
  spec: Record<string, unknown>;
  status: Record<string, unknown>;
  revision: number;
  created_at: string;
  updated_at: string;
  document: ResourceDocument;
  links?: Link[];
}

export interface Run {
  resource: Resource;
  status: 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled';
  output: Record<string, unknown> | null;
  error: string | null;
}

export interface Driver {
  path: string;
  state: 'stopped' | 'starting' | 'running' | 'stopping' | 'failed';
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

export interface UpdateResource {
  expected_revision: number;
  spec: Record<string, unknown>;
}

export interface ObjectDetail {
  kind: ObjectKind;
  path: string;
  value: ResourceDocument;
  links: Link[];
}
