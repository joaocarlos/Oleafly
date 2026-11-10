// @vitest-environment jsdom

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { DirListing, HostProbe, ProbeOutcome } from "@/lib/remote-setup";

const setup = vi.hoisted(() => ({
  probeHost: vi.fn(),
  trustHost: vi.fn(),
  installHelper: vi.fn(),
  cancelSetup: vi.fn(),
  listRemoteDir: vi.fn(),
  makeRemoteDir: vi.fn(),
  createRemoteProject: vi.fn(),
  onSetupProgress: vi.fn(),
}));
const workspace = vi.hoisted(() => ({ refreshRemoteWorkspaces: vi.fn() }));

vi.mock("@/lib/remote-setup", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/remote-setup")>()),
  ...setup,
}));
vi.mock("@/lib/remote-workspace", () => workspace);
vi.mock("@/components/settings/research/compute-api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/components/settings/research/compute-api")>()),
  sshConfigHosts: vi.fn(async () => [{ alias: "lab-gpu", hostName: "10.0.0.5" }]),
}));

import { RemoteProjectDialog } from "./RemoteProjectDialog";

const VERSION = "oleafly-remoted 0.4.11 protocol 7";

function probe(over: Partial<HostProbe> = {}): HostProbe {
  return {
    os: "Linux",
    arch: "x86_64",
    home: "/home/ana",
    triple: "x86_64-unknown-linux-gnu",
    libc: "glibc 2.35",
    missingLibs: [],
    latexmk: true,
    git: false,
    installed: { path: "/home/ana/.oleafly/remoted/0.4.11/oleafly-remoted", detail: VERSION, current: true, sha256: null },
    helper: { available: true, size: 58 * 1024 * 1024, source: "provided", reason: null },
    version: VERSION,
    helperDir: "/home/ana/.oleafly/remoted/0.4.11",
    dataDir: "/home/ana/.oleafly/remote-data",
    ...over,
  };
}

function listing(over: Partial<DirListing> = {}): DirListing {
  return {
    state: "ok",
    path: "/home/ana",
    parent: "/home",
    dirs: ["papers", "data"],
    truncated: false,
    entries: 5,
    writable: true,
    hasGit: false,
    hasTex: false,
    ...over,
  };
}

const ready = (over: Partial<HostProbe> = {}): ProbeOutcome => ({ status: "ready", probe: probe(over) });

function renderDialog() {
  const onCreated = vi.fn(async () => {});
  const onClose = vi.fn();
  render(<RemoteProjectDialog open onClose={onClose} onCreated={onCreated} />);
  return { onCreated, onClose };
}

async function connectTo(host: string) {
  fireEvent.change(await screen.findByTestId("remote-host"), { target: { value: host } });
  fireEvent.click(screen.getByTestId("remote-connect"));
}

describe("RemoteProjectDialog", () => {
  beforeEach(() => {
    for (const mock of Object.values(setup)) mock.mockReset();
    workspace.refreshRemoteWorkspaces.mockReset();
    workspace.refreshRemoteWorkspaces.mockResolvedValue(undefined);
    setup.onSetupProgress.mockResolvedValue(() => {});
    setup.cancelSetup.mockResolvedValue(undefined);
    setup.listRemoteDir.mockResolvedValue(listing());
  });

  it("offers the hosts of the user's ssh configuration and keeps Connect off until there is a host", async () => {
    renderDialog();
    expect(await screen.findByRole("button", { name: "lab-gpu" })).toBeInTheDocument();
    expect(screen.getByTestId("remote-connect")).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "lab-gpu" }));
    expect(screen.getByTestId("remote-host")).toHaveValue("lab-gpu");
    expect(screen.getByTestId("remote-connect")).toBeEnabled();
    fireEvent.change(screen.getByTestId("remote-port"), { target: { value: "70000" } });
    expect(screen.getByTestId("remote-connect")).toBeDisabled();
  });

  it("goes straight to the folder when the right helper is already on the host, and creates the project there", async () => {
    setup.probeHost.mockResolvedValue(ready());
    setup.createRemoteProject.mockResolvedValue({ projectId: "p-1" });
    const { onCreated } = renderDialog();
    await connectTo("lab");
    expect(setup.probeHost).toHaveBeenCalledWith({ host: "lab", user: null, port: null });

    await screen.findByTestId("remote-folder-step");
    expect(setup.listRemoteDir).toHaveBeenCalledWith({ host: "lab", user: null, port: null }, "~");
    // The home folder itself is too broad to be a project.
    await screen.findByText(/Choose a folder inside \/home\/ana/);
    expect(screen.getByTestId("remote-create")).toBeDisabled();

    setup.listRemoteDir.mockResolvedValue(listing({ path: "/home/ana/papers", parent: "/home/ana", dirs: [], hasTex: true, entries: 3 }));
    fireEvent.click(await screen.findByRole("button", { name: "papers" }));
    await waitFor(() => expect(screen.getByTestId("remote-project-name")).toHaveValue("papers"));
    expect(screen.getByText("Has LaTeX files")).toBeInTheDocument();
    expect(screen.getByTestId("remote-create")).toBeEnabled();

    fireEvent.change(screen.getByTestId("remote-project-name"), { target: { value: "  Thesis " } });
    fireEvent.click(screen.getByTestId("remote-create"));
    await waitFor(() => expect(onCreated).toHaveBeenCalledExactlyOnceWith("p-1"));
    expect(setup.createRemoteProject).toHaveBeenCalledWith({
      host: { host: "lab", user: null, port: null },
      path: "/home/ana/papers",
      name: "Thesis",
    });
    // The binding is read again before the project opens, or its first calls would stay local.
    expect(workspace.refreshRemoteWorkspaces.mock.invocationCallOrder[0]).toBeLessThan(onCreated.mock.invocationCallOrder[0]);
  });

  it("shows what will be installed, installs only when asked, then moves on to the folder", async () => {
    setup.probeHost.mockResolvedValue(ready({ installed: null }));
    setup.installHelper.mockResolvedValue({ binary: "/b", dataDir: "/d", version: VERSION, alreadyInstalled: false, bytes: 1, missingLibs: [] });
    renderDialog();
    await connectTo("lab");

    await screen.findByTestId("remote-helper-step");
    expect(screen.getByText(/to \/home\/ana\/\.oleafly\/remoted\/0\.4\.11 on lab/)).toBeInTheDocument();
    expect(screen.getByText("Size: 58 MB")).toBeInTheDocument();
    expect(screen.getByText("System: Linux x86_64")).toBeInTheDocument();
    expect(screen.getByText("LaTeX build (latexmk): found")).toBeInTheDocument();
    expect(screen.getByText("Git: not found")).toBeInTheDocument();
    expect(setup.installHelper).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("remote-helper-install"));
    await screen.findByTestId("remote-folder-step");
    expect(setup.installHelper).toHaveBeenCalledExactlyOnceWith({ host: "lab", user: null, port: null });
  });

  it("warns about missing libraries with the command to run, and checks again on request", async () => {
    setup.probeHost.mockResolvedValue(ready({ installed: null, missingLibs: ["libwebkit2gtk-4.1.so.0"] }));
    renderDialog();
    await connectTo("lab");
    await screen.findByText("Libraries the helper needs are missing");
    expect(screen.getByText("sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    await waitFor(() => expect(setup.probeHost).toHaveBeenCalledTimes(2));
  });

  it("says why a host cannot get the helper and offers no install", async () => {
    setup.probeHost.mockResolvedValue(
      ready({ installed: null, helper: { available: false, size: null, source: null, reason: "Oleafly has no helper for FreeBSD amd64 yet." } }),
    );
    renderDialog();
    await connectTo("lab");
    expect(await screen.findByText("Oleafly has no helper for FreeBSD amd64 yet.")).toBeInTheDocument();
    expect(screen.queryByTestId("remote-helper-install")).not.toBeInTheDocument();
  });

  it("shows a failed install and lets the user try again", async () => {
    setup.probeHost.mockResolvedValue(ready({ installed: null }));
    setup.installHelper.mockRejectedValueOnce("The copy on the host did not match the original, so it was discarded.");
    renderDialog();
    await connectTo("lab");
    fireEvent.click(await screen.findByTestId("remote-helper-install"));
    expect(await screen.findByText(/did not match the original/)).toBeInTheDocument();
    expect(screen.getByTestId("remote-helper-install")).toBeEnabled();
  });

  it("asks before trusting a new host key, then looks at the host again", async () => {
    setup.probeHost.mockResolvedValueOnce({
      status: "hostKey",
      host: "lab",
      keys: [{ keyType: "ssh-ed25519", fingerprint: "SHA256:abc" }],
      changed: false,
      message: null,
    } satisfies ProbeOutcome);
    setup.probeHost.mockResolvedValueOnce(ready());
    setup.trustHost.mockResolvedValue(undefined);
    renderDialog();
    await connectTo("lab");
    await screen.findByText(/SHA256:abc/);
    expect(setup.trustHost).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("compute-host-key-trust"));
    await screen.findByTestId("remote-folder-step");
    expect(setup.trustHost).toHaveBeenCalledWith({ host: "lab", user: null, port: null }, ["SHA256:abc"]);
    expect(setup.probeHost).toHaveBeenCalledTimes(2);
  });

  it("does not offer to trust a host key that has changed", async () => {
    setup.probeHost.mockResolvedValue({
      status: "hostKey",
      host: "lab",
      keys: [],
      changed: true,
      message: "The host key of lab has changed. Remove it with ssh-keygen -R lab.",
    } satisfies ProbeOutcome);
    renderDialog();
    await connectTo("lab");
    expect(await screen.findByText(/has changed\. Remove it with ssh-keygen -R lab/)).toBeInTheDocument();
    expect(screen.queryByTestId("compute-host-key-trust")).not.toBeInTheDocument();
  });

  it.each([
    ["auth", /did not accept your SSH keys/],
    ["network", /could not reach lab/],
    ["ssh", /OpenSSH is not installed/],
  ] as const)("explains a %s failure in words, and retries", async (reason, message) => {
    setup.probeHost.mockResolvedValueOnce({ status: "failed", reason, message: "raw ssh text" } satisfies ProbeOutcome);
    setup.probeHost.mockResolvedValueOnce(ready());
    renderDialog();
    await connectTo("lab");
    expect(await screen.findByText(message)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await screen.findByTestId("remote-folder-step");
  });

  it("shows the host's own words for a failure it has no wording for", async () => {
    setup.probeHost.mockResolvedValue({ status: "failed", reason: "other", message: "The host did not say what system it runs." });
    renderDialog();
    await connectTo("lab");
    expect(await screen.findByText("The host did not say what system it runs.")).toBeInTheDocument();
  });

  it("keeps the folder and the name when the project cannot be created, and says why", async () => {
    setup.probeHost.mockResolvedValue(ready());
    setup.listRemoteDir.mockResolvedValue(listing({ path: "/home/ana/papers", parent: "/home/ana", dirs: [] }));
    setup.createRemoteProject.mockRejectedValue("The host did not accept the project: no daemon");
    const { onCreated } = renderDialog();
    await connectTo("lab");
    await screen.findByTestId("remote-folder-step");
    await waitFor(() => expect(screen.getByTestId("remote-create")).toBeEnabled());
    fireEvent.click(screen.getByTestId("remote-create"));
    expect(await screen.findByText(/did not accept the project: no daemon/)).toBeInTheDocument();
    expect(onCreated).not.toHaveBeenCalled();
    expect(workspace.refreshRemoteWorkspaces).not.toHaveBeenCalled();
    expect(screen.getByTestId("remote-project-name")).toHaveValue("papers");
    expect(screen.getByTestId("remote-create")).toBeEnabled();
  });

  it("offers to create a folder that does not exist, and refuses one the user cannot write to", async () => {
    setup.probeHost.mockResolvedValue(ready());
    setup.listRemoteDir.mockResolvedValueOnce(listing({ state: "missing", path: "/home/ana/new", dirs: [], parent: "/home/ana" }));
    setup.makeRemoteDir.mockResolvedValue("/home/ana/new");
    renderDialog();
    await connectTo("lab");
    expect(await screen.findByText("This folder does not exist on the host.")).toBeInTheDocument();
    setup.listRemoteDir.mockResolvedValue(listing({ path: "/home/ana/new", parent: "/home/ana", dirs: [], writable: false }));
    fireEvent.click(screen.getByRole("button", { name: "Create it" }));
    await screen.findByText("You cannot write to this folder on the host.");
    expect(setup.makeRemoteDir).toHaveBeenCalledWith({ host: "lab", user: null, port: null }, "/home/ana/new");
    expect(screen.getByTestId("remote-create")).toBeDisabled();
  });

  it("stops a copy in progress when the dialog goes away", async () => {
    setup.probeHost.mockResolvedValue(ready({ installed: null }));
    setup.installHelper.mockReturnValue(new Promise(() => {}));
    const { unmount } = (() => {
      const onCreated = vi.fn();
      return render(<RemoteProjectDialog open onClose={vi.fn()} onCreated={onCreated} />);
    })();
    await connectTo("lab");
    fireEvent.click(await screen.findByTestId("remote-helper-install"));
    await waitFor(() => expect(setup.installHelper).toHaveBeenCalled());
    unmount();
    expect(setup.cancelSetup).toHaveBeenCalled();
  });
});
