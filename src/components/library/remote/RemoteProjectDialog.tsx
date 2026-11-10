import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { ComputeHostKeyDialog } from "@/components/settings/research/ComputeHostKeyDialog";
import type { HostKeyPrompt } from "@/components/settings/research/compute-api";
import { describeError } from "@/lib/app-error";
import {
  createRemoteProject,
  hostLabel,
  probeHost,
  trustHost,
  type FailReason,
  type HostProbe,
  type HostRef,
} from "@/lib/remote-setup";
import { refreshRemoteWorkspaces } from "@/lib/remote-workspace";
import { RemoteFolderStep } from "./RemoteFolderStep";
import { RemoteHelperStep } from "./RemoteHelperStep";
import { RemoteHostStep } from "./RemoteHostStep";

type Phase =
  | { name: "host" }
  | { name: "checking" }
  | { name: "failed"; reason: FailReason; message: string }
  | { name: "helper"; probe: HostProbe }
  | { name: "folder"; probe: HostProbe };

/**
 * Starts a project whose folder is on another computer: host, then the Oleafly helper on it,
 * then the folder. The local project is created only by the last button.
 */
export function RemoteProjectDialog({
  open,
  onClose,
  onCreated,
}: Readonly<{
  open: boolean;
  onClose: () => void;
  onCreated: (projectId: string) => Promise<void> | void;
}>) {
  return (
    <Dialog open={open} onOpenChange={(next) => !next && onClose()}>
      {open ? <RemoteFlow onCreated={onCreated} /> : null}
    </Dialog>
  );
}

function RemoteFlow({ onCreated }: Readonly<{ onCreated: (projectId: string) => Promise<void> | void }>) {
  const { t } = useTranslation(["library", "common"]);
  const [phase, setPhase] = useState<Phase>({ name: "host" });
  const [host, setHost] = useState<HostRef | null>(null);
  const [prompt, setPrompt] = useState<HostKeyPrompt | null>(null);
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);

  const connect = async (target: HostRef) => {
    setHost(target);
    setPhase({ name: "checking" });
    try {
      const outcome = await probeHost(target);
      if (outcome.status === "ready") {
        // The right helper is already there: nothing to ask, go on to the folder.
        const { probe } = outcome;
        setPhase(probe.installed?.current ? { name: "folder", probe } : { name: "helper", probe });
      } else if (outcome.status === "hostKey" && !outcome.changed) {
        setPrompt({ host: outcome.host, keys: outcome.keys, changed: false });
      } else if (outcome.status === "hostKey") {
        setPhase({ name: "failed", reason: "other", message: outcome.message ?? outcome.host });
      } else {
        setPhase({ name: "failed", reason: outcome.reason, message: outcome.message });
      }
    } catch (error) {
      setPhase({ name: "failed", reason: "other", message: describeError(error) });
    }
  };

  const create = async (request: { path: string; name: string }) => {
    if (!host) return;
    setCreating(true);
    setCreateError(null);
    try {
      const { projectId } = await createRemoteProject({ host, ...request });
      // The new binding must be known before the project opens, or its first calls stay local.
      await refreshRemoteWorkspaces();
      await onCreated(projectId);
    } catch (error) {
      setCreateError(describeError(error) || t(($) => $.library.remote.createFailed));
      setCreating(false);
    }
  };

  const label = host ? hostLabel(host) : "";
  const failedText = (reason: FailReason, message: string): string => {
    if (reason === "auth") return t(($) => $.library.remote.failed.auth, { host: label });
    if (reason === "network") return t(($) => $.library.remote.failed.network, { host: label });
    if (reason === "ssh") return t(($) => $.library.remote.failed.ssh);
    return message;
  };

  return (
    <DialogContent
      className="max-h-[calc(100vh-2rem)] gap-5 overflow-y-auto sm:max-w-xl"
      data-testid="remote-project-dialog"
      closeDisabled={creating}
      onEscapeKeyDown={(event) => creating && event.preventDefault()}
      onInteractOutside={(event) => event.preventDefault()}
    >
      <DialogHeader>
        <DialogTitle>{t(($) => $.library.kinds.remote.title)}</DialogTitle>
        <DialogDescription>{t(($) => $.library.remote.description)}</DialogDescription>
      </DialogHeader>

      {phase.name === "host" ? <RemoteHostStep initial={host} onConnect={(target) => void connect(target)} /> : null}

      {phase.name === "checking" ? (
        <p className="flex items-center gap-2 py-6 text-sm text-muted-foreground" aria-live="polite" data-testid="remote-checking">
          <Spinner aria-hidden />
          {t(($) => $.library.remote.checking, { host: label })}
        </p>
      ) : null}

      {phase.name === "failed" ? (
        <div className="flex flex-col gap-3" data-testid="remote-failed">
          <h3 className="text-sm font-semibold">{t(($) => $.library.remote.failed.title, { host: label })}</h3>
          <p role="alert" className="select-text rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {failedText(phase.reason, phase.message)}
          </p>
          <div className="flex justify-between gap-2">
            <Button type="button" variant="outline" onClick={() => setPhase({ name: "host" })}>
              {t(($) => $.common.actions.back)}
            </Button>
            <Button type="button" onClick={() => host && void connect(host)}>
              {t(($) => $.common.actions.retry)}
            </Button>
          </div>
        </div>
      ) : null}

      {phase.name === "helper" && host ? (
        <RemoteHelperStep
          host={host}
          probe={phase.probe}
          onReady={() => setPhase({ name: "folder", probe: phase.probe })}
          onRecheck={() => void connect(host)}
          onBack={() => setPhase({ name: "host" })}
        />
      ) : null}

      {phase.name === "folder" && host ? (
        <RemoteFolderStep
          host={host}
          probe={phase.probe}
          busy={creating}
          error={createError}
          onCreate={(request) => void create(request)}
          onBack={() => setPhase({ name: "host" })}
        />
      ) : null}

      {prompt && host ? (
        <ComputeHostKeyDialog
          prompt={prompt}
          onClose={() => {
            setPrompt(null);
            setPhase({ name: "host" });
          }}
          onTrust={async (fingerprints) => {
            await trustHost(host, fingerprints);
            setPrompt(null);
            await connect(host);
          }}
        />
      ) : null}
    </DialogContent>
  );
}
