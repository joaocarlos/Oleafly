// @vitest-environment jsdom

import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const remote = vi.hoisted(() => ({ available: vi.fn(() => false) }));
vi.mock("@/lib/remote-workspace", () => ({ remoteWorkspacesAvailable: remote.available }));

import { ProjectKindChooser } from "./ProjectKindChooser";

beforeEach(() => {
  remote.available.mockReturnValue(false);
});

describe("ProjectKindChooser", () => {
  it("offers the three starting points with a thumbnail each and reports the choice", () => {
    const onChoose = vi.fn();
    render(<ProjectKindChooser open onClose={vi.fn()} onChoose={onChoose} />);

    const chooser = screen.getByTestId("project-kind-chooser");
    expect(chooser).toHaveTextContent("Start a new piece of work");
    for (const id of ["research", "import", "template"]) {
      const card = screen.getByTestId(`project-kind-${id}`);
      expect(card.querySelector("img")).toHaveAttribute(
        "src",
        expect.stringContaining(".webp"),
      );
      expect(card.querySelector("img")).toHaveAttribute("alt", "");
      expect(card).toHaveAccessibleDescription(/.+/);
      expect(card.outerHTML).not.toMatch(/(^|[\s"])(focus-visible:)?(ring|outline)-/);
    }

    fireEvent.click(screen.getByTestId("project-kind-research"));
    expect(onChoose).toHaveBeenCalledExactlyOnceWith("research");
    fireEvent.click(screen.getByTestId("project-kind-import"));
    expect(onChoose).toHaveBeenLastCalledWith("import");
    fireEvent.click(screen.getByTestId("project-kind-template"));
    expect(onChoose).toHaveBeenLastCalledWith("template");
  });

  it("offers no remote workspace in a build that cannot run one", () => {
    render(<ProjectKindChooser open onClose={vi.fn()} onChoose={vi.fn()} />);
    expect(screen.queryByTestId("project-kind-remote")).not.toBeInTheDocument();
    expect(screen.getAllByTestId(/^project-kind-(research|import|template)$/)).toHaveLength(3);
  });

  it("adds a remote workspace as a fourth starting point when the build can run one", () => {
    remote.available.mockReturnValue(true);
    const onChoose = vi.fn();
    render(<ProjectKindChooser open onClose={vi.fn()} onChoose={onChoose} />);

    const card = screen.getByTestId("project-kind-remote");
    expect(card).toHaveTextContent("Remote workspace");
    expect(card.querySelector("img")).toHaveAttribute("src", expect.stringContaining(".webp"));
    expect(card).toHaveAccessibleDescription(/another computer/);
    for (const id of ["research", "import", "template"]) {
      expect(screen.getByTestId(`project-kind-${id}`)).toBeInTheDocument();
    }
    fireEvent.click(card);
    expect(onChoose).toHaveBeenCalledExactlyOnceWith("remote");
  });

  it("closes on escape, and refuses to while closing is held back", () => {
    const onClose = vi.fn();
    const { rerender } = render(
      <ProjectKindChooser open onClose={onClose} onChoose={vi.fn()} />,
    );
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Escape" });
    expect(onClose).toHaveBeenCalledOnce();

    onClose.mockClear();
    rerender(
      <ProjectKindChooser open allowClose={false} onClose={onClose} onChoose={vi.fn()} />,
    );
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("renders nothing while closed", () => {
    render(<ProjectKindChooser open={false} onClose={vi.fn()} onChoose={vi.fn()} />);
    expect(screen.queryByTestId("project-kind-chooser")).not.toBeInTheDocument();
  });
});
