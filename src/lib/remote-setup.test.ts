import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }));

import {
  cancelSetup,
  createRemoteProject,
  folderName,
  formatBytes,
  hostLabel,
  installHelper,
  isTooBroad,
  listRemoteDir,
  makeRemoteDir,
  onSetupProgress,
  probeHost,
  trustHost,
} from "./remote-setup";

describe("remote setup commands", () => {
  beforeEach(() => {
    mocks.invoke.mockReset();
    mocks.invoke.mockResolvedValue(null);
  });

  it("calls the backend commands with the argument names the backend reads", async () => {
    const host = { host: "lab", user: "ana", port: 2222 };
    await probeHost(host);
    await trustHost(host, ["SHA256:x"]);
    await installHelper(host);
    await cancelSetup();
    await listRemoteDir(host, "~");
    await makeRemoteDir(host, "~/papers");
    await createRemoteProject({ host, path: "/home/ana/papers", name: "Paper", createFolder: true });
    expect(mocks.invoke.mock.calls).toEqual([
      ["remote_probe_host", { host }],
      ["remote_trust_host", { host, fingerprints: ["SHA256:x"] }],
      ["remote_install_helper", { host }],
      ["remote_cancel_setup"],
      ["remote_list_dir", { host, path: "~" }],
      ["remote_make_dir", { host, path: "~/papers" }],
      ["remote_create_project", { request: { host, path: "/home/ana/papers", name: "Paper", createFolder: true } }],
    ]);
  });

  it("hands upload progress to the listener and lets it stop", async () => {
    const stop = vi.fn();
    let deliver: (event: { payload: unknown }) => void = () => {};
    mocks.listen.mockImplementation(async (_name: string, handler: typeof deliver) => {
      deliver = handler;
      return stop;
    });
    const seen: unknown[] = [];
    const unlisten = await onSetupProgress((progress) => seen.push(progress));
    deliver({ payload: { host: "lab", phase: "upload", sent: 5, total: 10 } });
    expect(mocks.listen).toHaveBeenCalledWith("remote:setup", expect.any(Function));
    expect(seen).toEqual([{ host: "lab", phase: "upload", sent: 5, total: 10 }]);
    unlisten();
    expect(stop).toHaveBeenCalled();
  });
});

describe("remote setup helpers", () => {
  it("names a host the way ssh does", () => {
    expect(hostLabel({ host: "lab" })).toBe("lab");
    expect(hostLabel({ host: "lab", user: "ana", port: 2222 })).toBe("ana@lab:2222");
    expect(hostLabel({ host: "lab", user: null, port: null })).toBe("lab");
  });

  it("takes a project's default name from the last folder", () => {
    expect(folderName("/home/ana/papers/thesis")).toBe("thesis");
    expect(folderName("/home/ana/papers/thesis/")).toBe("thesis");
    expect(folderName("/")).toBe("");
  });

  it("turns away the disk, the home folder and the top-level system folders", () => {
    expect(isTooBroad("/", "/home/ana")).toBe(true);
    expect(isTooBroad("/home/ana", "/home/ana")).toBe(true);
    expect(isTooBroad("/home/ana/", "/home/ana")).toBe(true);
    expect(isTooBroad("/srv", "/home/ana")).toBe(true);
    expect(isTooBroad("/home/ana/papers", "/home/ana")).toBe(false);
    expect(isTooBroad("/srv/data/paper", "/home/ana")).toBe(false);
  });

  it("writes sizes the way a person reads them", () => {
    expect(formatBytes(500)).toBe("1 KB");
    expect(formatBytes(900 * 1024)).toBe("900 KB");
    expect(formatBytes(58 * 1024 * 1024)).toBe("58 MB");
  });
});
