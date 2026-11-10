import { useCallback, useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowUp, Folder, FolderPlus } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Spinner } from "@/components/ui/spinner";
import { describeError } from "@/lib/app-error";
import {
  folderName,
  hostLabel,
  isTooBroad,
  listRemoteDir,
  makeRemoteDir,
  type DirListing,
  type HostProbe,
  type HostRef,
} from "@/lib/remote-setup";

const joinPath = (folder: string, name: string) => `${folder.replace(/\/+$/, "")}/${name}`;
const validFolderName = (name: string) => name !== "" && name !== "." && name !== ".." && !/[/\\\0\n\r]/.test(name);

/** The folder on the host, and the project's name. Nothing is created until the last button. */
export function RemoteFolderStep({
  host,
  probe,
  busy,
  error,
  onCreate,
  onBack,
}: Readonly<{
  host: HostRef;
  probe: HostProbe;
  busy: boolean;
  error: string | null;
  onCreate: (request: { path: string; name: string }) => void;
  onBack: () => void;
}>) {
  const { t } = useTranslation(["library", "common"]);
  const label = hostLabel(host);
  const [typed, setTyped] = useState("~");
  const [listing, setListing] = useState<DirListing | null>(null);
  const [loading, setLoading] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [newFolder, setNewFolder] = useState("");
  const latest = useRef(0);
  const pathId = useId();
  const nameId = useId();
  const newId = useId();

  const load = useCallback(
    async (path: string) => {
      const request = ++latest.current;
      setLoading(true);
      setFailure(null);
      try {
        const next = await listRemoteDir(host, path);
        if (request !== latest.current) return;
        setListing(next);
        setTyped(next.path);
      } catch (cause) {
        if (request !== latest.current) return;
        setFailure(describeError(cause));
      } finally {
        if (request === latest.current) setLoading(false);
      }
    },
    [host],
  );

  useEffect(() => {
    void load("~");
  }, [load]);

  const ok = listing?.state === "ok";
  const path = listing?.path ?? "";
  useEffect(() => {
    if (ok && !nameTouched) setName(folderName(path));
  }, [ok, path, nameTouched]);

  const broad = ok && isTooBroad(path, probe.home);
  const canCreate = ok && !broad && listing.writable && !loading && !busy && name.trim() !== "";

  const makeFolder = async (target: string) => {
    setLoading(true);
    setFailure(null);
    try {
      const made = await makeRemoteDir(host, target);
      setNewFolder("");
      await load(made);
    } catch (cause) {
      setFailure(describeError(cause));
      setLoading(false);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-testid="remote-folder-step">
      <div className="flex flex-col gap-1">
        <h3 className="text-sm font-semibold">{t(($) => $.library.remote.folder.title)}</h3>
        <p className="text-xs text-muted-foreground">{t(($) => $.library.remote.folder.body, { host: label })}</p>
      </div>

      <form
        className="flex flex-col gap-1.5"
        onSubmit={(event) => {
          event.preventDefault();
          void load(typed.trim() || "~");
        }}
      >
        <label htmlFor={pathId} className="text-xs font-medium">
          {t(($) => $.library.remote.folder.pathLabel)}
        </label>
        <div className="flex gap-2">
          <Button
            type="button"
            variant="outline"
            size="icon"
            disabled={!listing?.parent || loading || busy}
            aria-label={t(($) => $.library.remote.folder.up)}
            onClick={() => listing?.parent && void load(listing.parent)}
          >
            <ArrowUp aria-hidden />
          </Button>
          <Input
            id={pathId}
            value={typed}
            spellCheck={false}
            autoCapitalize="off"
            autoComplete="off"
            className="font-mono text-xs"
            data-testid="remote-path"
            disabled={busy}
            onChange={(event) => setTyped(event.target.value)}
          />
        </div>
      </form>

      <div
        className="flex h-40 flex-col overflow-y-auto rounded-md border bg-muted/20"
        aria-busy={loading || undefined}
        data-testid="remote-folder-list"
      >
        {loading && !listing ? (
          <div className="flex flex-1 items-center justify-center">
            <Spinner />
          </div>
        ) : ok && listing.dirs.length > 0 ? (
          <ul>
            {listing.dirs.map((dir) => (
              <li key={dir}>
                <button
                  type="button"
                  disabled={loading || busy}
                  className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm hover:bg-accent focus-visible:bg-accent disabled:opacity-60"
                  onClick={() => void load(joinPath(listing.path, dir))}
                >
                  <Folder aria-hidden className="size-4 shrink-0 text-muted-foreground" />
                  <span className="truncate">{dir}</span>
                </button>
              </li>
            ))}
          </ul>
        ) : ok ? (
          <p className="m-auto text-xs text-muted-foreground">{t(($) => $.library.remote.folder.empty)}</p>
        ) : null}
      </div>

      {listing && listing.state !== "ok" ? (
        <div role="alert" className="flex items-center justify-between gap-2 rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive">
          <span>
            {listing.state === "missing"
              ? t(($) => $.library.remote.folder.missing)
              : t(($) => $.library.remote.folder.denied)}
          </span>
          {listing.state === "missing" ? (
            <Button type="button" size="xs" variant="outline" disabled={loading} onClick={() => void makeFolder(listing.path)}>
              {t(($) => $.library.remote.folder.createMissing)}
            </Button>
          ) : null}
        </div>
      ) : null}

      {ok ? (
        <ul className="flex flex-col gap-0.5 text-xs text-muted-foreground" data-testid="remote-folder-facts">
          <li>{t(($) => $.library.remote.folder.items, { count: listing.entries })}</li>
          {listing.hasTex ? <li>{t(($) => $.library.remote.folder.hasTex)}</li> : null}
          {listing.hasGit ? <li>{t(($) => $.library.remote.folder.hasGit)}</li> : null}
          {!listing.writable ? (
            <li className="text-destructive">{t(($) => $.library.remote.folder.notWritable)}</li>
          ) : null}
          {broad ? (
            <li className="text-destructive">{t(($) => $.library.remote.folder.tooBroad, { home: probe.home })}</li>
          ) : null}
        </ul>
      ) : null}

      {ok ? (
        <form
          className="flex items-end gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (validFolderName(newFolder.trim())) void makeFolder(joinPath(listing.path, newFolder.trim()));
          }}
        >
          <div className="flex flex-1 flex-col gap-1.5">
            <label htmlFor={newId} className="text-xs font-medium">
              {t(($) => $.library.remote.folder.newFolder)}
            </label>
            <Input
              id={newId}
              value={newFolder}
              spellCheck={false}
              autoComplete="off"
              placeholder={t(($) => $.library.remote.folder.newFolderPlaceholder)}
              disabled={loading || busy || !listing.writable}
              onChange={(event) => setNewFolder(event.target.value)}
            />
          </div>
          <Button type="submit" variant="outline" disabled={loading || busy || !validFolderName(newFolder.trim())}>
            <FolderPlus aria-hidden />
            {t(($) => $.library.remote.folder.createFolder)}
          </Button>
        </form>
      ) : null}

      <div className="flex flex-col gap-1.5">
        <label htmlFor={nameId} className="text-xs font-medium">
          {t(($) => $.library.remote.folder.nameLabel)}
        </label>
        <Input
          id={nameId}
          value={name}
          maxLength={80}
          autoComplete="off"
          data-testid="remote-project-name"
          disabled={busy}
          onChange={(event) => {
            setName(event.target.value);
            setNameTouched(true);
          }}
        />
      </div>

      {failure || error ? (
        <p role="alert" className="select-text rounded-md bg-destructive/10 px-3 py-2 text-xs text-destructive">
          {failure ?? error}
        </p>
      ) : null}
      {busy ? (
        <p className="flex items-center gap-2 text-xs text-muted-foreground" aria-live="polite">
          <Spinner size="sm" aria-hidden />
          {t(($) => $.library.remote.creating, { host: label })}
        </p>
      ) : null}

      <div className="flex justify-between gap-2">
        <Button type="button" variant="outline" disabled={busy} onClick={onBack}>
          {t(($) => $.common.actions.back)}
        </Button>
        <Button
          type="button"
          disabled={!canCreate}
          data-testid="remote-create"
          onClick={() => onCreate({ path, name: name.trim() })}
        >
          {t(($) => $.library.remote.folder.create)}
        </Button>
      </div>
    </div>
  );
}
