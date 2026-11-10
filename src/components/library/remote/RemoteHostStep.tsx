import { useEffect, useId, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { sshConfigHosts, type SshConfigHost } from "@/components/settings/research/compute-api";
import type { HostRef } from "@/lib/remote-setup";

/** Examples of what to type, not words: the same in every language. */
const HOST_EXAMPLE = "lab-gpu.example.edu";
const PORT_EXAMPLE = "22";

function details(host: SshConfigHost): string {
  const user = host.user ? `${host.user}@` : "";
  const port = host.port ? `:${host.port}` : "";
  return `${user}${host.hostName ?? host.alias}${port}`;
}

/** The computer: a host from the user's SSH configuration, or one typed in. */
export function RemoteHostStep({
  initial,
  onConnect,
}: Readonly<{ initial: HostRef | null; onConnect: (host: HostRef) => void }>) {
  const { t } = useTranslation(["library"]);
  const [host, setHost] = useState(initial?.host ?? "");
  const [user, setUser] = useState(initial?.user ?? "");
  const [port, setPort] = useState(initial?.port ? String(initial.port) : "");
  const [known, setKnown] = useState<SshConfigHost[]>([]);
  const hostId = useId();
  const userId = useId();
  const portId = useId();

  useEffect(() => {
    let live = true;
    sshConfigHosts()
      .then((list) => live && setKnown(list))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);

  const portNumber = port.trim() === "" ? null : Number(port);
  const portValid = portNumber === null || (Number.isInteger(portNumber) && portNumber >= 1 && portNumber <= 65_535);
  const canConnect = host.trim() !== "" && portValid;

  return (
    <form
      className="flex flex-col gap-4"
      data-testid="remote-host-step"
      onSubmit={(event) => {
        event.preventDefault();
        if (!canConnect) return;
        onConnect({ host: host.trim(), user: user.trim() || null, port: portNumber });
      }}
    >
      <h3 className="text-sm font-semibold">{t(($) => $.library.remote.host.title)}</h3>
      {known.length > 0 ? (
        <div className="flex flex-col gap-1.5">
          <p className="text-xs text-muted-foreground">{t(($) => $.library.remote.host.known)}</p>
          <ul className="flex max-h-32 flex-wrap gap-1.5 overflow-y-auto">
            {known.map((item) => (
              <li key={item.alias}>
                <Button
                  type="button"
                  size="sm"
                  variant={host === item.alias ? "secondary" : "outline"}
                  title={details(item)}
                  aria-pressed={host === item.alias}
                  onClick={() => {
                    setHost(item.alias);
                    setUser("");
                    setPort("");
                  }}
                >
                  {item.alias}
                </Button>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
      <div className="grid gap-3 sm:grid-cols-[1fr_9rem_7rem]">
        <div className="flex flex-col gap-1.5">
          <label htmlFor={hostId} className="text-xs font-medium">
            {t(($) => $.library.remote.host.hostLabel)}
          </label>
          <Input
            id={hostId}
            value={host}
            autoFocus
            autoComplete="off"
            autoCapitalize="off"
            spellCheck={false}
            placeholder={HOST_EXAMPLE}
            data-testid="remote-host"
            onChange={(event) => setHost(event.target.value)}
          />
        </div>
        <div className="flex flex-col gap-1.5">
          <label htmlFor={userId} className="text-xs font-medium">
            {t(($) => $.library.remote.host.userLabel)}
          </label>
          <Input
            id={userId}
            value={user}
            autoComplete="off"
            autoCapitalize="off"
            spellCheck={false}
            data-testid="remote-user"
            onChange={(event) => setUser(event.target.value)}
          />
        </div>
        <div className="flex flex-col gap-1.5">
          <label htmlFor={portId} className="text-xs font-medium">
            {t(($) => $.library.remote.host.portLabel)}
          </label>
          <Input
            id={portId}
            value={port}
            inputMode="numeric"
            autoComplete="off"
            placeholder={PORT_EXAMPLE}
            aria-invalid={!portValid || undefined}
            data-testid="remote-port"
            onChange={(event) => setPort(event.target.value)}
          />
        </div>
      </div>
      <p className="text-xs leading-relaxed text-muted-foreground">{t(($) => $.library.remote.host.hint)}</p>
      <div className="flex justify-end">
        <Button type="submit" disabled={!canConnect} data-testid="remote-connect">
          {t(($) => $.library.remote.host.connect)}
        </Button>
      </div>
    </form>
  );
}
