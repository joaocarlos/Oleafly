import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import {
  describeRemoteState,
  initRemoteWorkspaces,
  refreshRemoteWorkspaces,
  remoteTargetFor,
  remoteWorkspacesAvailable,
  routedInvoke,
} from "./remote-workspace";

const remote = { projectId: "p-remote", host: "example-host", path: "~/work/analysis" };

const files = ["read_file", "write_file", "list_file_tree", "get_project", "git_status", "git_commit", "compile_project", "read_compiled_pdf", "cancel_compile"];

async function bind(list: unknown[]): Promise<void> {
  mocks.invoke.mockReset();
  mocks.invoke.mockResolvedValueOnce({ bindings: list, files });
  await initRemoteWorkspaces();
  mocks.invoke.mockReset();
  mocks.invoke.mockResolvedValue("ok");
}

describe("remote workspace routing", () => {
  beforeEach(async () => {
    await bind([]);
  });

  it("sends every call to the local backend when no project is bound", async () => {
    await routedInvoke("acp_start", { projectId: "p-local", agentId: "claude" });
    await routedInvoke("term_shells");
    expect(mocks.invoke.mock.calls).toEqual([
      ["acp_start", { projectId: "p-local", agentId: "claude" }],
      ["term_shells"],
    ]);
  });

  it("keeps working when the backend has no remote commands", async () => {
    mocks.invoke.mockReset();
    mocks.invoke.mockRejectedValueOnce(new Error("Command remote_bindings not found"));
    await initRemoteWorkspaces();
    mocks.invoke.mockResolvedValue("ok");
    await routedInvoke("acp_start", { projectId: remote.projectId });
    expect(mocks.invoke).toHaveBeenLastCalledWith("acp_start", { projectId: remote.projectId });
  });

  it("relays a bound project's calls through the ssh bridge", async () => {
    await bind([remote]);
    await routedInvoke("acp_prompt", { projectId: remote.projectId, sessionId: "s1", input: { text: "hi" } });
    expect(mocks.invoke).toHaveBeenCalledWith("remote_call", {
      projectId: remote.projectId,
      cmd: "acp_prompt",
      args: { projectId: remote.projectId, sessionId: "s1", input: { text: "hi" } },
    });
  });

  it("passes the terminal channel through the channel variant", async () => {
    await bind([remote]);
    const channel = { id: 7 };
    await routedInvoke("term_open", { projectId: remote.projectId, cols: 80, rows: 24, channel });
    expect(mocks.invoke).toHaveBeenCalledWith("remote_call_channel", {
      projectId: remote.projectId,
      cmd: "term_open",
      args: { projectId: remote.projectId, cols: 80, rows: 24 },
      channel,
    });
  });

  it("sends calls without a project to the machine of the project last used", async () => {
    await bind([remote]);
    await routedInvoke("acp_start", { projectId: remote.projectId, agentId: "claude" });
    mocks.invoke.mockClear();
    await routedInvoke("acp_catalog", { probe: false });
    expect(mocks.invoke).toHaveBeenCalledWith("remote_call", {
      projectId: remote.projectId,
      cmd: "acp_catalog",
      args: { probe: false },
    });
    await routedInvoke("acp_start", { projectId: "p-local", agentId: "claude" });
    mocks.invoke.mockClear();
    await routedInvoke("acp_catalog", { probe: false });
    expect(mocks.invoke).toHaveBeenCalledWith("acp_catalog", { probe: false });
  });

  it("keeps dialogs, editor buffers and local exports on this computer", async () => {
    await bind([remote]);
    await routedInvoke("acp_start", { projectId: remote.projectId, agentId: "claude" });
    mocks.invoke.mockClear();
    await routedInvoke("acp_pick_agent_program", { agentId: "claude" });
    await routedInvoke("acp_buffer_reply", { requestId: "r", content: null });
    await routedInvoke("acp_session_export", { projectId: remote.projectId, sessionId: "s", path: "/tmp/x.md" });
    expect(mocks.invoke.mock.calls.map(([command]) => command)).toEqual([
      "acp_pick_agent_program",
      "acp_buffer_reply",
      "acp_session_export",
    ]);
  });

  it("relays the project file commands the host runs", async () => {
    await bind([remote]);
    await routedInvoke("read_file", { projectId: remote.projectId, path: "paper.tex" });
    await routedInvoke("write_file", { projectId: remote.projectId, path: "paper.tex", content: "x" });
    expect(mocks.invoke.mock.calls.map(([command, args]) => [command, args.cmd])).toEqual([
      ["remote_call", "read_file"],
      ["remote_call", "write_file"],
    ]);
  });

  it("runs the project's source control on the host, and a local project's on this computer", async () => {
    await bind([remote]);
    await routedInvoke("git_status", { projectId: remote.projectId });
    await routedInvoke("git_commit", { projectId: remote.projectId, message: "m" });
    await routedInvoke("git_status", { projectId: "p-local" });
    expect(mocks.invoke.mock.calls.map(([command, args]) => [command, args.cmd ?? null])).toEqual([
      ["remote_call", "git_status"],
      ["remote_call", "git_commit"],
      ["git_status", null],
    ]);
  });

  it("builds on the host and reads the PDF from there", async () => {
    await bind([remote]);
    await routedInvoke("compile_project", { projectId: remote.projectId, mainDoc: "paper.tex" });
    await routedInvoke("read_compiled_pdf", { projectId: remote.projectId });
    expect(mocks.invoke.mock.calls.map(([command, args]) => [command, args.cmd])).toEqual([
      ["remote_call", "compile_project"],
      ["remote_call", "read_compiled_pdf"],
    ]);
  });

  it("stops the build of the project the user is working in, though the call names no project", async () => {
    await bind([remote]);
    await routedInvoke("compile_project", { projectId: remote.projectId, mainDoc: "paper.tex" });
    mocks.invoke.mockClear();
    await routedInvoke("cancel_compile", {});
    expect(mocks.invoke).toHaveBeenCalledWith("remote_call", {
      projectId: remote.projectId,
      cmd: "cancel_compile",
      args: {},
    });
    await routedInvoke("compile_project", { projectId: "p-local", mainDoc: "main.tex" });
    mocks.invoke.mockClear();
    await routedInvoke("cancel_compile", {});
    expect(mocks.invoke).toHaveBeenCalledWith("cancel_compile", {});
  });

  it("keeps every command the host does not run on this computer, even for a remote project", async () => {
    await bind([remote]);
    await routedInvoke("git_clone_project", { projectId: remote.projectId, url: "https://example.org/x.git" });
    await routedInvoke("compile_isolated", { projectId: remote.projectId, source: "x", offline: false });
    expect(mocks.invoke.mock.calls.map(([command]) => command)).toEqual(["git_clone_project", "compile_isolated"]);
  });

  it("does not send projectless non-agent commands to the host after a remote project", async () => {
    await bind([remote]);
    await routedInvoke("read_file", { projectId: remote.projectId, path: "a.tex" });
    mocks.invoke.mockClear();
    await routedInvoke("list_projects");
    await routedInvoke("read_file", { path: "a.tex" });
    expect(mocks.invoke.mock.calls.map(([command]) => command)).toEqual(["list_projects", "read_file"]);
  });

  it("finds no target for an unknown project", () => {
    expect(remoteTargetFor("read_file", { projectId: "nobody" })).toBeNull();
  });
});

describe("remote workspaces added while the app runs", () => {
  const created = { projectId: "p-new", host: "lab", path: "/home/ana/papers" };

  it("knows whether the backend can run a project on another computer", async () => {
    await bind([]);
    expect(remoteWorkspacesAvailable()).toBe(true);
    mocks.invoke.mockReset();
    mocks.invoke.mockRejectedValueOnce(new Error("Command remote_bindings not found"));
    await initRemoteWorkspaces();
    expect(remoteWorkspacesAvailable()).toBe(false);
  });

  it("sends a new project's calls to its host as soon as the bindings are read again", async () => {
    await bind([]);
    await routedInvoke("list_file_tree", { projectId: created.projectId });
    expect(mocks.invoke).toHaveBeenLastCalledWith("list_file_tree", { projectId: created.projectId });

    mocks.invoke.mockReset();
    mocks.invoke.mockResolvedValueOnce({ bindings: [created], files });
    await refreshRemoteWorkspaces();
    mocks.invoke.mockReset();
    mocks.invoke.mockResolvedValue("ok");
    await routedInvoke("list_file_tree", { projectId: created.projectId });
    expect(mocks.invoke).toHaveBeenCalledWith("remote_call", {
      projectId: created.projectId,
      cmd: "list_file_tree",
      args: { projectId: created.projectId },
    });
  });

  it("keeps the bindings it has when reading them again fails", async () => {
    await bind([remote]);
    mocks.invoke.mockReset();
    mocks.invoke.mockRejectedValueOnce(new Error("backend busy"));
    await refreshRemoteWorkspaces();
    expect(remoteWorkspacesAvailable()).toBe(true);
    expect(remoteTargetFor("read_file", { projectId: remote.projectId })).not.toBeNull();
  });

  it("does nothing when the backend has no remote commands", async () => {
    mocks.invoke.mockReset();
    mocks.invoke.mockRejectedValueOnce(new Error("Command remote_bindings not found"));
    await initRemoteWorkspaces();
    mocks.invoke.mockReset();
    await refreshRemoteWorkspaces();
    expect(mocks.invoke).not.toHaveBeenCalled();
  });
});

describe("remote link notices", () => {
  it("says nothing for the first connection, and welcomes the user back after a drop", () => {
    expect(describeRemoteState({ host: "h1", state: "connected" })).toBeNull();
    expect(describeRemoteState({ host: "h1", state: "disconnected" })).toBeNull();
    expect(describeRemoteState({ host: "h1", state: "reconnecting" })).toMatchObject({ kind: "error", key: "remote-h1" });
    expect(describeRemoteState({ host: "h1", state: "connected" })).toMatchObject({
      kind: "success",
      text: expect.stringContaining("kept running"),
    });
    expect(describeRemoteState({ host: "h1", state: "connected" })).toBeNull();
  });

  it("keeps hosts apart and reports giving up and lost output", () => {
    describeRemoteState({ host: "a", state: "reconnecting" });
    expect(describeRemoteState({ host: "b", state: "connected" })).toBeNull();
    expect(describeRemoteState({ host: "a", state: "lost" })).toMatchObject({ kind: "error" });
    expect(describeRemoteState({ host: "a", state: "gap" })).toMatchObject({ kind: "info" });
    expect(describeRemoteState({ host: "a", state: "stale" })).toMatchObject({ kind: "info" });
  });
});
