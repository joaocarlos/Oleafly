import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { CopyableBlock } from "@/components/ui/copyable-block";
import { Progress } from "@/components/ui/progress";
import { Spinner } from "@/components/ui/spinner";
import { describeError } from "@/lib/app-error";
import {
  cancelSetup,
  formatBytes,
  hostLabel,
  installHelper,
  onSetupProgress,
  type HostProbe,
  type HostRef,
  type SetupProgress,
} from "@/lib/remote-setup";

/** What the helper needs on a Debian or Ubuntu host. The same on every screen, so it is not translated. */
const LIBS_COMMAND = "sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0";

/**
 * Oleafly's helper on the host. The user sees what will be copied, where and how big, and
 * nothing happens until they press Install.
 */
export function RemoteHelperStep({
  host,
  probe,
  onReady,
  onRecheck,
  onBack,
}: Readonly<{
  host: HostRef;
  probe: HostProbe;
  onReady: () => void;
  onRecheck: () => void;
  onBack: () => void;
}>) {
  const { t } = useTranslation(["library", "common"]);
  const label = hostLabel(host);
  const [installing, setInstalling] = useState(false);
  const [progress, setProgress] = useState<SetupProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const running = useRef(false);

  // Leaving the screen mid-copy stops it, so nothing is left running on the host.
  useEffect(
    () => () => {
      if (running.current) void cancelSetup().catch(() => {});
    },
    [],
  );

  const install = async () => {
    setInstalling(true);
    setError(null);
    setProgress(null);
    running.current = true;
    const stop = await onSetupProgress(setProgress).catch(() => null);
    try {
      await installHelper(host);
      running.current = false;
      onReady();
    } catch (failure) {
      running.current = false;
      setError(describeError(failure));
      setInstalling(false);
    } finally {
      stop?.();
    }
  };

  const system = `${probe.os} ${probe.arch}`;
  const status = (found: boolean) =>
    found ? t(($) => $.library.remote.helper.found) : t(($) => $.library.remote.helper.notFound);
  const percent = progress && progress.total > 0 ? (progress.sent / progress.total) * 100 : 0;
  const phase = progress?.phase ?? "check";
  const missing = probe.missingLibs.length > 0;
  const size = probe.helper.size === null ? null : formatBytes(probe.helper.size);

  return (
    <div className="flex flex-col gap-4" data-testid="remote-helper-step">
      <h3 className="text-sm font-semibold">{t(($) => $.library.remote.helper.title, { host: label })}</h3>
      <ul className="flex flex-col gap-0.5 text-xs text-muted-foreground">
        <li>{t(($) => $.library.remote.helper.system, { system })}</li>
        <li>{t(($) => $.library.remote.helper.latexmk, { status: status(probe.latexmk) })}</li>
        <li>{t(($) => $.library.remote.helper.git, { status: status(probe.git) })}</li>
      </ul>

      {probe.installed?.current ? (
        <p className="text-sm" data-testid="remote-helper-installed">
          {t(($) => $.library.remote.helper.installed, { version: probe.version, host: label })}
        </p>
      ) : probe.helper.available ? (
        <div className="flex flex-col gap-2 rounded-lg border bg-muted/30 p-3">
          {probe.helper.source === "release" ? (
            <p className="text-sm leading-relaxed" data-testid="remote-helper-download-note">
              {t(($) => $.library.remote.helper.downloadBody)}
            </p>
          ) : null}
          <p className="text-sm leading-relaxed">
            {t(($) => $.library.remote.helper.installBody, { dir: probe.helperDir, host: label })}
          </p>
          {size ? <p className="text-xs text-muted-foreground">{t(($) => $.library.remote.helper.size, { size })}</p> : null}
        </div>
      ) : (
        <p role="alert" className="select-text rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive">
          {probe.helper.reason}
        </p>
      )}

      {missing && !probe.installed?.current ? (
        <div role="alert" className="flex flex-col gap-2 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3">
          <p className="flex items-center gap-2 text-sm font-medium">
            <TriangleAlert aria-hidden className="size-4 shrink-0" />
            {t(($) => $.library.remote.helper.libsTitle)}
          </p>
          <p className="text-xs leading-relaxed">
            {t(($) => $.library.remote.helper.libsBody, { host: label, libs: probe.missingLibs.join(", ") })}
          </p>
          <CopyableBlock source={LIBS_COMMAND}>
            <pre className="select-text overflow-x-auto rounded-md border bg-background px-3 py-2 font-mono text-xs">
              {LIBS_COMMAND}
            </pre>
          </CopyableBlock>
          <div>
            <Button type="button" size="sm" variant="outline" disabled={installing} onClick={onRecheck}>
              {t(($) => $.library.remote.helper.libsCheck)}
            </Button>
          </div>
        </div>
      ) : null}

      {installing ? (
        <div className="flex flex-col gap-2" aria-live="polite" data-testid="remote-helper-progress">
          <Progress
            value={percent}
            indeterminate={(phase !== "upload" && phase !== "download") || !progress || progress.total === 0}
            label={t(($) => (phase === "download" ? $.library.remote.helper.downloading : $.library.remote.helper.copying), {
              sent: formatBytes(progress?.sent ?? 0),
              total: formatBytes(progress?.total ?? 0),
            })}
          />
          <p className="text-xs text-muted-foreground">
            {phase === "download" && progress
              ? t(($) => $.library.remote.helper.downloading, {
                  sent: formatBytes(progress.sent),
                  total: formatBytes(progress.total),
                })
              : phase === "upload" && progress
              ? t(($) => $.library.remote.helper.copying, {
                  sent: formatBytes(progress.sent),
                  total: formatBytes(progress.total),
                })
              : phase === "verify" || phase === "done"
                ? t(($) => $.library.remote.helper.verifying)
                : t(($) => $.library.remote.helper.checking, { host: label })}
          </p>
        </div>
      ) : null}
      {error ? (
        <p role="alert" className="select-text rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive">
          {error}
        </p>
      ) : null}

      <div className="flex justify-between gap-2">
        <Button type="button" variant="outline" onClick={installing ? () => void cancelSetup() : onBack}>
          {installing ? t(($) => $.library.remote.helper.stop) : t(($) => $.common.actions.back)}
        </Button>
        {probe.installed?.current ? (
          <Button type="button" onClick={onReady} data-testid="remote-helper-continue">
            {t(($) => $.library.remote.helper.continue)}
          </Button>
        ) : probe.helper.available ? (
          <Button type="button" disabled={installing} onClick={() => void install()} data-testid="remote-helper-install">
            {installing ? <Spinner size="sm" aria-hidden /> : null}
            {t(($) => $.library.remote.helper.install, { host: label })}
          </Button>
        ) : null}
      </div>
    </div>
  );
}
