import { invoke } from "@tauri-apps/api/core";
import type {
  EnvsResponse,
  PathResponse,
  RuntimeOptions,
  StudioConfig,
  TextResponse
} from "./types";

export async function appConfig(options: RuntimeOptions) {
  return invoke<StudioConfig>("app_config", { options });
}

export async function listEnvs(options: RuntimeOptions) {
  return invoke<EnvsResponse>("list_envs", { options });
}

export async function createLane(
  options: RuntimeOptions,
  input: {
    target: string;
    source: string;
    backend?: string;
    profile?: string;
    network?: string;
  }
) {
  return invoke("create_lane", { options, input });
}

export async function pickSourceRoot(
  options: RuntimeOptions,
  defaultPath?: string
) {
  return invoke<PathResponse>("pick_source_root", {
    options,
    input: { default_path: defaultPath || null }
  }).then((response) => response.path);
}

export async function removeLane(options: RuntimeOptions, envId: string) {
  return invoke("remove_lane", { options, input: { env_id: envId } });
}

export async function changedPaths(options: RuntimeOptions, envId: string) {
  return invoke<TextResponse>("changed_paths", {
    options,
    input: { env_id: envId }
  });
}

export async function openIde(
  options: RuntimeOptions,
  envId: string,
  app: string,
  relativePath: string
) {
  return invoke("open_ide", {
    options,
    input: { env_id: envId, app, relative_path: relativePath }
  });
}

export async function openShell(options: RuntimeOptions, envId: string) {
  return invoke("open_shell", { options, input: { env_id: envId } });
}
