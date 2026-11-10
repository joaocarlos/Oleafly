import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/**
 * Setting a remote workspace up from the app: look at the host, install Oleafly's helper on it,
 * choose a folder there, and bind a project to it. The backend commands exist only in a build
 * with the remote bridge; `remoteWorkspacesAvailable()` says whether this is one.
 */

export interface HostRef {
  host: string;
  user?: string | null;
  port?: number | null;
}

export interface HostKeyFingerprint {
  keyType: string;
  fingerprint: string;
}

export interface HostProbe {
  os: string;
  arch: string;
  home: string;
  triple: string | null;
  libc: string | null;
  /** Runtime libraries the helper needs and the host lacks (Linux). */
  missingLibs: string[];
  latexmk: boolean;
  git: boolean;
  installed: { path: string; detail: string | null; current: boolean; sha256: string | null } | null;
  helper: {
    available: boolean;
    size: number | null;
    source: "provided" | "nextToApp" | null;
    reason: string | null;
  };
  version: string;
  helperDir: string;
  dataDir: string;
}

export type FailReason = "ssh" | "auth" | "network" | "cancelled" | "other";

export type ProbeOutcome =
  | { status: "ready"; probe: HostProbe }
  | { status: "hostKey"; host: string; keys: HostKeyFingerprint[]; changed: boolean; message: string | null }
  | { status: "failed"; reason: FailReason; message: string };

export interface InstallReport {
  binary: string;
  dataDir: string;
  version: string;
  alreadyInstalled: boolean;
  bytes: number;
  missingLibs: string[];
}

export interface DirListing {
  state: "ok" | "missing" | "denied";
  path: string;
  parent: string | null;
  dirs: string[];
  truncated: boolean;
  entries: number;
  writable: boolean;
  hasGit: boolean;
  hasTex: boolean;
}

export interface SetupProgress {
  host: string;
  phase: "check" | "upload" | "verify" | "done";
  sent: number;
  total: number;
}

export const probeHost = (host: HostRef) => invoke<ProbeOutcome>("remote_probe_host", { host });
export const trustHost = (host: HostRef, fingerprints: string[]) =>
  invoke<void>("remote_trust_host", { host, fingerprints });
export const installHelper = (host: HostRef) => invoke<InstallReport>("remote_install_helper", { host });
export const cancelSetup = () => invoke<void>("remote_cancel_setup");
export const listRemoteDir = (host: HostRef, path: string) =>
  invoke<DirListing>("remote_list_dir", { host, path });
export const makeRemoteDir = (host: HostRef, path: string) =>
  invoke<string>("remote_make_dir", { host, path });
export const createRemoteProject = (request: {
  host: HostRef;
  path: string;
  name: string;
  createFolder?: boolean;
}) => invoke<{ projectId: string }>("remote_create_project", { request });

/** The host name the user sees for a reference: `user@host:port`. */
export function hostLabel(host: HostRef): string {
  const user = host.user ? `${host.user}@` : "";
  const port = host.port ? `:${host.port}` : "";
  return `${user}${host.host}${port}`;
}

/** Upload and check progress from the backend while the helper is installed. */
export function onSetupProgress(handler: (progress: SetupProgress) => void): Promise<UnlistenFn> {
  return listen<SetupProgress>("remote:setup", ({ payload }) => handler(payload));
}

/** The final folder name of a path on the host, for a project's default name. */
export function folderName(path: string): string {
  return path.split("/").filter(Boolean).at(-1) ?? "";
}

/**
 * A folder the backend would refuse as a project: the host's whole disk, the home folder itself,
 * or a top-level system folder. The backend checks again; this only turns the button off early.
 */
export function isTooBroad(path: string, home: string): boolean {
  const normal = path.replace(/\/+$/, "");
  return normal === "" || normal === home.replace(/\/+$/, "") || (normal.match(/\//g) ?? []).length < 2;
}

/** Bytes as a short size: `58 MB`. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${Math.round(bytes / (1024 * 1024))} MB`;
}
