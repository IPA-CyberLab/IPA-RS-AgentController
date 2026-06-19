export type RuntimeOptions = {
  agentfs?: string;
  config?: string;
};

export type StudioConfig = {
  agentfs: string;
  default_source?: string;
  platform: "macos" | "windows" | "linux";
};

export type PathResponse = {
  path?: string;
};

export type EnvState =
  | "created"
  | "running"
  | "stopped"
  | "failed"
  | "quota_exceeded";

export type Env = {
  id: string;
  base_id: string;
  backend: string;
  rootfs_path: string;
  machine_name: string;
  state: EnvState;
  profile: string;
  created_at: string;
  last_active_at: string;
  sessions: string[];
  limits: {
    cpu_max: string;
    memory_max: string;
    pids_max: number;
    disk_max: string;
    network: string;
    idle_timeout: string;
    max_runtime: string;
  };
};

export type EnvStatus = {
  env: Env;
  disk_used?: string;
  source_root?: string;
  env_path?: string;
};

export type EnvsResponse = {
  type: "envs";
  envs: EnvStatus[];
};
