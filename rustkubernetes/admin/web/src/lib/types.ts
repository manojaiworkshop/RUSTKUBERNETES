export interface Grant { cluster: string; namespace: string; role: "admin" | "edit" | "view" }

export interface Me { username: string; superadmin: boolean; permissions: string[]; grants: Grant[] }

export interface Meta {
  version: string;
  superUser: string;
  permissions: { id: string; description: string }[];
  bundle: { version?: string; size_human?: string; created?: string; built: boolean; binaries_dir: string; images?: string[]; binaries?: string[]; arch?: string };
  missingBinaries: string[];
  sshPublicKey: string;
  localAddress: string;
  localUser: string;
  hasLocalMachine: boolean;
  kubeIdentity: string | null;
}

export interface Machine {
  id: string; name: string; address: string; transport: "local" | "ssh";
  sshUser: string; sshPort: number; sshAuth: string; prefix: string;
  status: "new" | "installing" | "ready" | "failed"; message: string;
  facts: Record<string, string>; version: string; created: string; createdBy: string;
  cluster: string | null; role: "master" | "worker" | null;
}

export interface Ports { api: number; kubelet: number; controller_manager: number; scheduler: number; proxy_healthz: number; proxy_metrics: number }

export interface ClusterSummary {
  name: string; kind: "managed" | "imported"; status: "provisioning" | "running" | "stopped" | "failed" | "deleting";
  message: string; endpoint: string; master: string; workers: string[]; dns: boolean; created: string; createdBy: string;
  reachable: boolean; nodes: number; nodesReady: number; pods: number; podsRunning: number; podsPending: number; podsFailed: number;
  deployments: number; services: number; namespaces: number; ports: Ports;
}

export interface PodRow { name: string; namespace: string; status: string; phase: string; ready: string; restarts: number; node: string; ip: string; created: string; owner: string; containers: { name: string; image: string }[] }
export interface NodeRow { name: string; ready: boolean; ip: string; unschedulable: boolean; version: string; os: string; kernel: string; runtime: string; cpu: string; memory: string; maxPods: string; created: string; taints: { key: string; effect: string; value?: string }[] }
export interface DeploymentRow { name: string; namespace: string; replicas: number; ready: number; updated: number; available: number; images: string[]; created: string }
export interface ServiceRow { name: string; namespace: string; type: string; clusterIP: string; ports: string; selector: Record<string, string>; created: string }
export interface NamespaceRow { name: string; phase: string; created: string }
export interface EventRow { namespace: string; type: string; reason: string; object: string; message: string; count: number; time: string }

export interface ClusterSnapshot {
  reachable: boolean; restricted?: boolean;
  nodes: NodeRow[]; pods: PodRow[]; deployments: DeploymentRow[]; services: ServiceRow[]; namespaces: NamespaceRow[]; events: EventRow[];
}

export interface Job { id: string; kind: string; title: string; target: string; state: "running" | "succeeded" | "failed"; user: string; started: string; finished: string | null; log: string[] }

export interface User { username: string; permissions: string[]; grants: Grant[]; disabled: boolean; created: string; createdBy: string }

export interface AuditEntry { time: string; user: string; action: string; detail: string }
