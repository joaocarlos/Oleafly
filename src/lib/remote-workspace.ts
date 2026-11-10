import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { toast } from "@/lib/toast";

/**
 * A project bound to a folder on another machine. Its terminal and agents run
 * there: calls for it go through the app's ssh bridge (`remote_call`) instead
 * of the local backend. Bindings come from `remote-workspaces.json`; the
 * backend only has the commands when it is built with `remote-bridge`.
 */
export interface RemoteBinding {
  projectId: string;
  host: string;
  path: string;
}

let bindings = new Map<string, RemoteBinding>();
/** Project file commands the backend runs on the host; agent and terminal commands go by prefix. */
let fileCommands = new Set<string>();
/** The project the last project-scoped call was for. */
let lastProjectId: string | null = null;

interface RemoteInfo {
  bindings: RemoteBinding[];
  files: string[];
}

/** What the app's ssh bridge says about its link to a host (`remote:state`). */
export interface RemoteState {
  host: string;
  state: "connected" | "disconnected" | "reconnecting" | "lost" | "gap" | "stale";
  detail?: string;
}

export interface RemoteNotice {
  kind: "info" | "success" | "error";
  key: string;
  text: string;
}

const hostsDown = new Set<string>();

/**
 * The message for a link change, or null when there is nothing to say. The first connection is
 * silent; coming back after a drop is announced, so the user knows the terminal they left is live.
 */
export function describeRemoteState(change: RemoteState): RemoteNotice | null {
  const key = `remote-${change.host}`;
  switch (change.state) {
    case "reconnecting":
      hostsDown.add(change.host);
      return { kind: "error", key, text: `Lost the connection to ${change.host}. Reconnecting…` };
    case "connected":
      if (!hostsDown.delete(change.host)) return null;
      return { kind: "success", key, text: `Reconnected to ${change.host}. Its terminals and agents kept running.` };
    case "lost":
      hostsDown.delete(change.host);
      return {
        kind: "error",
        key,
        text: `Could not reach ${change.host} again. Its terminals and agents may still be running there.`,
      };
    case "gap":
      return { kind: "info", key: `${key}-gap`, text: "Some terminal output was lost while the connection was down." };
    case "stale":
      return {
        kind: "info",
        key: `${key}-stale`,
        text: `${change.host} still runs an older Oleafly program because sessions are open there. It updates when they end.`,
      };
    default:
      return null;
  }
}

let stopWatching: (() => void) | null = null;

async function watchRemoteState(): Promise<void> {
  if (stopWatching) return;
  try {
    stopWatching = await listen<RemoteState>("remote:state", ({ payload }) => {
      const notice = describeRemoteState(payload);
      if (!notice) return;
      if (notice.kind === "error") toast.errorUnique(notice.key, notice.text, undefined, true);
      else if (notice.kind === "success") toast.successUnique(notice.key, notice.text);
      else toast.infoUnique(notice.key, notice.text);
    });
  } catch {
    stopWatching = null;
  }
}

/** Whether this build of the app can run a project on another machine (its backend has the remote commands). */
let available = false;

async function loadRemoteInfo(): Promise<boolean> {
  try {
    const info = await invoke<RemoteInfo>("remote_bindings");
    const list = Array.isArray(info?.bindings) ? info.bindings : [];
    bindings = new Map(list.map((binding) => [binding.projectId, binding]));
    fileCommands = new Set(Array.isArray(info?.files) ? info.files : []);
    if (bindings.size > 0) void watchRemoteState();
    return true;
  } catch {
    return false;
  }
}

export async function initRemoteWorkspaces(): Promise<void> {
  available = await loadRemoteInfo();
  if (!available) {
    bindings = new Map();
    fileCommands = new Set();
  }
}

/**
 * Reads the bindings again, after one was added at run time. Until then a new project's first
 * calls would still go to this computer. A failed read keeps what is already known.
 */
export async function refreshRemoteWorkspaces(): Promise<void> {
  if (!available) return;
  await loadRemoteInfo();
}

export function remoteWorkspacesAvailable(): boolean {
  return available;
}

export function remoteBindingFor(projectId: string): RemoteBinding | null {
  return bindings.get(projectId) ?? null;
}

/** Commands with no project that act on whatever the user is working on. */
const FOLLOWS_LAST_PROJECT = new Set(["cancel_compile"]);

const isAgentOrTerminal = (command: string): boolean =>
  command.startsWith("acp_") || command.startsWith("term_") || FOLLOWS_LAST_PROJECT.has(command);

/**
 * The remote workspace a call belongs to, or null for a local one. Only agent,
 * terminal and project file commands can run on a host. A call without a
 * project (agent catalog, preferences, shells) follows the project the user
 * last worked in, so the picker lists the machine that will run it; every
 * other command without a project stays local.
 */
export function remoteTargetFor(command: string, args?: Record<string, unknown>): RemoteBinding | null {
  const projectId = typeof args?.projectId === "string" ? args.projectId : null;
  if (projectId) lastProjectId = projectId;
  if (bindings.size === 0) return null;
  const agentOrTerminal = isAgentOrTerminal(command);
  if (!agentOrTerminal && !fileCommands.has(command)) return null;
  const id = projectId ?? (agentOrTerminal ? lastProjectId : null);
  return id ? (bindings.get(id) ?? null) : null;
}

/**
 * Calls that must stay on this computer: a native dialog, the editor buffers
 * only this window has, and an export that writes to a local path.
 */
const LOCAL_ONLY = new Set(["acp_pick_agent_program", "acp_buffer_reply", "acp_session_export"]);

/** `invoke` that sends the call to the project's machine when it has one. */
export function routedInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const target = LOCAL_ONLY.has(command) ? null : remoteTargetFor(command, args);
  if (!target) return args === undefined ? invoke<T>(command) : invoke<T>(command, args);
  if (command === "trust_folder") {
    return invoke<T>("remote_trust_folder", { projectId: target.projectId, scope: args?.scope });
  }
  const { channel, ...rest } = args ?? {};
  const call = { projectId: target.projectId, cmd: command, args: rest };
  return channel === undefined
    ? invoke<T>("remote_call", call)
    : invoke<T>("remote_call_channel", { ...call, channel });
}
