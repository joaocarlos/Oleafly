# Spike: headless command host for remote use

Branch `spike/remote-daemon`. Not product code.

## What it is

`oleafly-remoted` (feature `remote-spike`) runs Oleafly's own Tauri commands on
Tauri's `MockRuntime`, with no window, and speaks JSON lines on stdio. A client
reaches it with `ssh host oleafly-remoted`, so no port, token or tunnel is needed.
Tauri `Channel` traffic is captured with `Builder::channel_interceptor`.

```bash
cargo build -p oleafly --features remote-spike --bin oleafly-remoted
python3 scripts/spike/remote-client.py src-tauri/target/debug/oleafly-remoted
# over ssh: python3 scripts/spike/remote-client.py ssh host /path/to/oleafly-remoted
```

## Result

- Works: `backend_protocol_info`, `term_shells`, `term_open`, `term_write`,
  `term_resize`, `term_kill`. A real PTY shell opened in the project folder,
  output streamed through the intercepted Channel, and the exit event arrived.
- Did not compile as is: commands that take a bare `AppHandle`, `Webview` or
  `Window` (e.g. `acp_start`) are bound to the `Wry` runtime. About a third of
  the 736 commands are (approximate count). The ACP ones are now generic, see below.
- Not tried: file watching, Git, compile, the other ~200 Wry-bound commands.

## Over ssh (tested)

Same client, daemon started as `ssh -T host env OLEAFLY_DATA_DIR=... oleafly-remoted`,
against a temporary loopback sshd with a throwaway key (macOS Remote Login was
off). Same result as the local run: protocol info, shell list, PTY terminal with
streamed output, resize, kill and exit event. Nothing but JSON on stdout, and the
daemon exited when ssh closed stdin. Not tested: a Linux host, a slow or dropped
link, reconnecting to a live terminal.

## Linux host (tested)

Built on the host (Ubuntu 24.04, x86_64, 24 cores): `cargo build -p oleafly
--features remote-spike --bin oleafly-remoted`, 1 min 59 s in debug, after
installing the Tauri `-dev` packages from CI plus `libdbus-1-dev` and
`libssl-dev`. The client on a Mac started the daemon with `ssh -T host env
OLEAFLY_DATA_DIR=... oleafly-remoted` and got the same result as before: a bash
PTY in the project folder on the host, streamed output, resize, kill, exit
event. No orphan daemon afterwards.

## Agents (ACP), generic over Runtime (tested on the Linux host)

Made `acp/commands.rs` generic (`AppHandle<R>`, `Webview<R>`, 35 signatures,
mechanical) plus the helpers they call: `research_mcp::{start,start_restricted}`,
`skills_pack::pack_root`, `acp::skill_block::prompt_skill`,
`acp::setup::{pick_program,confirm_program}`. The app's own build still checks.

The daemon owns the `AcpRuntime` (instead of `acp::attach`) and forwards its
`subscribe()` / `subscribe_summaries()` streams as `{"event":"acp:event"|"acp:session"}`
lines, so no Tauri `emit` is needed for agent traffic.

Result against `example-host`: `acp_catalog` lists the agents installed on the
host; `acp_start` with `opencode` 1.17.9 started a session whose project path is on
the host; `acp_prompt` ("reply with exactly the word OK") streamed thought chunks,
the message chunk (text `OK`), usage and `turn_complete`; `acp_disconnect` ended it
and no agent process was left behind. The model was the host's local Ollama one, so
no paid API was used.

Not done: Codex, the pick-program / confirm dialogs (need a client-side answer),
the `acp:buffer-request` round trip (open editor buffers), reconnecting a live session.

## Claude Code on the host (tested)

Data dir on the host: `~/.oleafly-spike` (set with `OLEAFLY_DATA_DIR`, kept between runs).

1. `acp_install {"agentId":"claude"}` installed `@agentclientprotocol/claude-agent-acp@0.95.0`
   under `~/.oleafly-spike/acp` (358 MB). npm runs with piped stdout/stderr, null stdin and
   `--ignore-scripts`, so it cannot corrupt the JSON channel. The catalog then reported
   `claude installed = True`.
2. `acp_start` + `acp_prompt` ("reply with exactly OK"): the agent ran under the host's own
   claude.ai login (one small prompt against that plan) and answered `OK`.
3. Permission loop: prompt "create hello.txt containing REMOTE_EDIT_OK". The daemon sent a
   `permission` event (title "Write hello.txt", kind `edit`, options `allow-once` /
   `allow-with-updates` / `reject`); the client answered `acp_permission` with `allow-once`;
   `hello.txt` appeared on the host with exactly `REMOTE_EDIT_OK` (this run also used the
   claude.ai plan). The client never printed the reply to `acp_permission`; the evidence is the
   `permission_resolved` event and the file. Events seen: `tool_call`,
   `tool_call_update`, `permission_resolved`, `turn_changes`, `turn_complete`.

Side effects on the host: Claude Code wrote session transcripts (about 660 KB) to the real
`~/.claude/projects/-home-user--oleafly-spike-projects-spk1`.
No agent or daemon process of the spike was left running. The agent probably ran with the
host user's own `~/.claude` settings (hooks, MCP servers, CLAUDE.md); not checked.

## Wiring to the frontend (phase 2)

A project can be bound to a folder on another machine. The calls of its terminal and agent panels
are relayed there; the rest of the app (editor, files, compile, Git) is still local. The panels
themselves were not driven in the real app, so "works in the UI" is unverified.

```
 UI (acp.ts, terminal-ipc.ts) -- routedInvoke --> remote_call / remote_call_channel (Tauri, feature remote-bridge)
   -> ssh -T host env OLEAFLY_DATA_DIR=... oleafly-remoted  (JSON lines on stdio)
   -> daemon: remote_attach(projectId, folder), term_*, acp_*  (MockRuntime, no window)
 daemon events acp:event / acp:session  --> re-emitted locally under the same names
```

Pieces:
- `src-tauri/src/remote_bridge.rs` (feature `remote-bridge`): one ssh child per binding, built with
  the research runs' `SshContext` + `transport::ssh_args` (BatchMode, ServerAlive, known hosts
  strict). Relays only `acp_*` and `term_*`. A closed link fails every pending call and emits
  `remote:state`.
- `src-tauri/src/remote_spike.rs` (feature `remote-spike`): the daemon now registers all 48 `acp_*`
  and 7 `term_*` commands and a `remote_attach` request. `project_location::locate` has a
  feature-gated hook, so an attached id points at any host folder; its state lives in the daemon's
  data dir (`remote-state/<id>`), nothing is written into the code folder.
- `src/lib/remote-workspace.ts`: `routedInvoke` replaces `invoke` in `acp.ts` and `terminal-ipc.ts`.
  Calls without a project (agent catalog, preferences, shells) follow the project last used.
  `acp_pick_agent_program`, `acp_buffer_reply`, `acp_session_export` and the external-terminal calls
  stay local. With no binding, or without `remote-bridge`, every call goes to the local backend as before.

Binding file, `<Oleafly data folder>/remote-workspaces.json` (`~/.oleafly/` by default):

```json
{ "projects": { "<local project id>": {
    "host": "example-host", "path": "~/work/analysis",
    "binary": "/home/me/Developer/oleafly/src-tauri/target/debug/oleafly-remoted",
    "dataDir": "/home/me/.oleafly-spike" } } }
```

Verified: the bridge against `example-host` from a Rust test (`cargo test -p oleafly --features
remote-bridge --lib remote_bridge -- --ignored --nocapture` with `REMOTE_BRIDGE_*` set): shells, a PTY
terminal through a `Channel`, the agent catalog, an `opencode` session started on the attached folder
with its `acp:event` (2 seen) and `acp:session` (1 seen) re-emitted locally via `listen_any`, the code
folder left untouched, refusal of a non-relayed command. Read, not run: the terminal pane sends a
project-relative `cwd` (resolved inside the attached folder on the host), and project ids
(`linked-<hex>` or words joined by `-`) fit what `remote_attach` accepts. On the host, with Node
24: `tsc --noEmit` clean, biome clean, vitest `acp`/`terminal` 859 tests in 55 files pass, including 7 new
routing tests.

Real app (macOS, `pnpm tauri dev --features remote-bridge`, Node 22 through npx, isolated
`OLEAFLY_DATA_DIR`, project `remote-demo` bound to `~/oleafly-scratch/codeproj` on the host): the user opened the
terminal panel and reported it worked. Seen from outside: an `ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes ...`
child on the Mac, and `oleafly-remoted` with a `bash -l` child on the host. In the agent panel (CLI Agent tab) the user then started `opencode`, whose model picker showed the host's
"Ollama on AI Workstation" provider, sent "Responda OK se funcionar." and got `OK` back (screenshot from the
user); on the host an `opencode acp` process appeared under `oleafly-remoted` at the same time.
The local compile of `main.tex` fails there because the Tectonic/Typst/Pandoc sidecars are empty placeholders.

Nothing here drove the Tauri window; the terminal and agent results come from the user. Not tried in the UI:
permission prompts, prompt attachments, the turn-changes view, Claude Code, reconnecting. Known mismatches while the editor is
local: prompt attachments (`files`, `selections`) and the turn-changes view carry local paths while the agent
edits files on the host. Not done: remote file editing, compile and
Git; reconnecting after the link drops (the pending calls fail, the sessions end); a Settings screen for
bindings; remote export of a conversation; Codex; the 200 commands bound to `Wry`.

## Text files on the host (phase 3)

The project's files now live on the host: the editor reads and writes them there, and the folder watcher
tells the app when something (an agent, a script) changes them.

What changed:
- `src-tauri/src/remote_commands.rs`: the single list of routed commands (`FILES`, agent/terminal by prefix,
  `ASKED_HERE`, `EVENTS`). The bridge relays only these, hands `FILES` to the frontend through
  `remote_bindings`, and `remote_spike::tests::every_routed_command_is_registered` fails if the daemon lacks one.
- The daemon registers 22 project commands (`get_project`, `list_file_tree`, `read_file`, `write_file`,
  `create_file`, `delete_file`, `rename_file`, `copy_file`, `read_project_bytes`, `write_project_bytes`,
  `project_*` status/engine/manifest/trust calls, `watch_project_folder`, `revoke_folder_trust`, ...). Almost
  all were already runtime-agnostic; `rename_file` needed `<R: Runtime>`. It manages `AppState`, and forwards
  `project-folder-changed` (and the agent events) as JSON lines.
- Raw replies (`read_project_bytes`) travel as base64 (`okRaw`) and come back as a `tauri::ipc::Response`.
- Attaching a folder seeds `project.json` in the daemon's state dir (`remote-state/<id>`), from the folder name and
  the binding's optional `mainDoc`; the code folder is not written to.
- Trust: a host folder is a linked folder, so agents refuse to start (`trust.agents`) until the user trusts it.
  `trust_folder` is answered on this computer (native dialog) by `remote_trust_folder`, and the grant is applied on
  the host (`remote_trust`). `revoke_folder_trust` is relayed.
- `src/lib/tauri.ts` now goes through `routedInvoke`; it routes only agent/terminal commands and the `FILES`
  list. A projectless call follows the last project only for agent/terminal commands. Everything else, including
  `compile_project`, `git_*`, `collab_*`, stays on this computer.
- `traced_commands` (feature `remote-bridge`) appends, once per command, every call that names a remote-bound
  project but ran locally to `<data>/remote-trace.log`: the list of what is left to port.

Verified (Rust, against `example-host`, `remote_bridge` tests with `--ignored`): `get_project` (name from the
folder, `paper.tex` as main), a tree with only the host's files, `read_file` returning a text that exists only
there, `write_file`, a raw `read_project_bytes`, a `project-folder-changed` event after a change made over ssh,
`delete_file`; an untrusted folder refusing `acp_start`, then starting after `remote_trust`. Frontend on the host
(Node 24): `tsc` clean, vitest 24,308 tests pass and 2 fail (`typst-figure.compile`, which runs the empty Typst
placeholder in `src-tauri/binaries`), 12 routing tests.

Known breaks, not fixed: compile on a bound project builds the local stub (no TeX on the host was checked);
`git_*`, `collab_*`, skills, Zotero and research calls act on the local stub; responses with absolute host paths
(project root, compile folder) are shown as if local (reveal in Finder, external terminal); a second window.

Real app, text on the host (macOS dev app, `remote-demo` bound to `~/oleafly-scratch/codeproj`, local stub emptied): the file
tree shows the host's `analysis.py` and `paper.tex` (user's report). The host's `paper.tex` then held a line typed in the editor
("Teste manual."), so a save from the UI landed on the host; the trust flow went through `remote_trust_folder`.
Still running locally for the bound project, from `remote-trace.log`: `compile_project`, `read_compiled_pdf`,
`git_status`, `git_is_initialized`, `skills_list`, `zotero_project_links`, `get_research_workspace`,
`research_root_health`, `overleaf_sync_status`, `mcp_set_active_project`, `collab_*` (4). Host tools (read-only check):
`latexmk`, `pdflatex`, `xelatex`, `lualatex` and `git` are installed; Tectonic, Typst and Pandoc are not.

## Git on the host (phase 4)

`remote_commands::GIT` lists 38 `git_*` commands (all but `git_clone_project`, which makes a new local project). Ten were
bound to `Wry` (`restore`, `pull`, `discard`, `discard_paths`, `checkout_branch`, `stash_push/pop`, `resolve_conflict`,
`continue_merge`, `abort_merge`) and only needed `<R: Runtime>`; nothing cascaded. The daemon registers all 38, the consistency
test covers them, the bridge hands them to the frontend, so `routedInvoke` sends a bound project's source control to the host's own `git`.

Verified against `example-host` (`remote_bridge::tests::runs_git_on_the_host`, `--ignored`): an untrusted host folder refuses
git (`trust.git`, same gate as agents); after the grant, `git_is_initialized` (false), `git_initialize`, `git_status` (the host's
untracked file), `git_stage_all`, `git_commit`, `git_log`, `git_current_branch`, `git_diff` after a change made over ssh.
`git -C <folder> log` on the host shows the commit (author from the host's git config), and the folder holds only the project file and `.git`.
Frontend (host, Node 24): routing tests 14 in total, `tauri.*` and `acp.ipc` suites 268 tests pass, `tsc` and biome clean; the full
suite was not rerun after this step.

Credentials stay on the host by design: pushes use its ssh keys or its own Oleafly accounts, not the Mac's tokens. Not tried: push,
pull and clone against a real remote, merge conflicts, the Source Control panel in the app.

Real app, Source Control panel on the bound project (user's screenshot): branch `master`, `analysis.py` listed as untracked, a commit
"Teste de commit" in the graph with the host's git author, and the host repository really holds it (`git log` over ssh: `63c372a Teste de commit`).
There is no Push because the host repository has no remote: "Commit and push" / "Commit and sync" are disabled without `snapshot.remote`, and the
Publish action sits in the panel's "..." menu. The host authenticates to GitHub over ssh (`ssh -T git@github.com` greets the user), so an ssh remote
should work from there; an HTTPS remote needs credentials the host does not have. Publish and push were not tried.

## Compile on the host (phase 5)

LaTeX projects now build with the host's own `latexmk` (TeX Live), and the PDF is read back from there. Tectonic, Typst and Pandoc are not on the host, so this is
LaTeX only.

- `compile_project` is generic over `Runtime`: `CompileRequest<'a, AppRt = Wry>` and 27 helpers in `document_engine.rs` (the app's own callers still infer `Wry`; 20 test
  call sites that pass `None` for the log emitter got `::<tauri::Wry>`). `compile_project` skips the collaboration upload of a shared project's PDF on a non-desktop
  runtime (a `downcast` to the Wry handle), because that module is not generic.
- Routed: `compile_project`, `cancel_compile`, `read_compiled_pdf`, `read_build_artifact`, `validate_compile_fingerprint`, `synctex_forward/inverse/map_line`,
  `project_tex_status`; the `compile:log` event is forwarded. `cancel_compile` has no project, so it follows the last project like agent commands.
- The seeded manifest sets `"engine": "latexmk"` for a `.tex` main document (the binding can set `engine`). The attached state moved to
  `<data>/linked/<id>` because the build and manifest code insist that a linked project's data is a direct child of the folder registry; the old
  `remote-state/<id>` is no longer used.
- **Trust changes the engine.** `trust::restrict_compile_meta` swaps an untrusted project's engine for the bundled Tectonic. On a host without Tectonic an
  untrusted folder therefore fails with "no compiler output" (found with `strace`: it tried to exec the empty placeholder `target/debug/tectonic`). After the trust
  grant, latexmk runs. The message is misleading for a host; not fixed.

Verified against `example-host` (`remote_bridge::tests::compiles_on_the_host`, `--ignored`): after `remote_trust`, `compile_project` returns `ok` and `has_pdf` (pdfTeX, TeX
Live 2023 Debian), `read_compiled_pdf` returns 15,097 raw bytes starting `%PDF-1.5`, 18 `compile:log` events reach the app, the code folder holds only `main.tex`, and the PDF sits
under the daemon's `linked/<id>/build`. `document_engine` unit tests: 269 pass, 28 fail; 27 call the real Pandoc and one the bundled Typst, which are empty placeholders here
(one inspected fails on "No such file or directory"; not compared against an unmodified run). Frontend routing tests: 16.
Not done: the PDF preview in the app, SyncTeX in the UI, Typst/Markdown on the host, a bibliography (biber) run, a host without latexmk.

Real app, compile on the host (user's report): compiling `paper.tex` in the bound project produced the PDF. On the host the PDF is
`~/.oleafly-spike/linked/remote-demo/build/_oleafly_entry.pdf`, the code folder holds only `analysis.py`, `.git` and `paper.tex`, and the seeded manifest is
`{"name":"codeproj","main_doc":"paper.tex","engine":"latexmk"}`. `remote-trace.log` no longer lists `compile_project`, `read_compiled_pdf` or any `git_*` as local; what still runs
on this computer for the bound project: `collab_*` (4), `get_research_workspace`, `research_root_health`, `mcp_set_active_project`, `zotero_project_links`,
`overleaf_sync_status`, `skills_list`, `pdf_sync_status`.

## Reconnecting after a dropped link (phase 6)

Before: the daemon was a child of the ssh session, so a dropped link ended every terminal and agent on the host (baseline recorded: after killing the local ssh,
`term_write` answered "terminal session is not open" and only `TICK1` and `TICK2` of 8 had arrived). Now a binding with `"persistent": true` keeps the daemon on the host
between connections, like VS Code's server.

```
 app -- ssh -T host env OLEAFLY_DATA_DIR=... oleafly-remoted --attach --> relay (stdio <-> unix socket)
                                                                           |
                       persistent daemon (--serve), started by the first --attach, setsid, log in remoted.log
                       <data>/remoted.sock (0600), remoted.pid, remoted.lock (flock: one daemon)
```

- `src-tauri/src/remote_hub.rs`: the `Hub` (one client at a time; a new one replaces the old; a stalled one, a 10 s write timeout, is dropped and what it missed is kept),
  a replay ring per channel (4 MiB, keyed by Tauri's message index), the socket server, the relay, `--stop`, and an idle exit (`OLEAFLY_REMOTED_IDLE_SECS`, default 2 h
  with no client; agents and terminals end then). 6 unit tests cover replay order, no repeats, the gap marker, stalled clients, replacement and the session count.
- Hello: the bridge sends the last index it saw per channel; the daemon answers `ready` (pid, attached projects), replays the rest (or a `gap` marker), and sends a `resync`
  event if events (agent updates, folder changes) were missed. Those are not kept: the bridge re-emits `acp:resync` and a `project-folder-changed` with `rescan` per attached
  project, so the agent panel and file tree refetch.
- Stale binary: the relay probes the daemon with its own executable's identity (path, size, mtime). A stale daemon with no open terminals or live agents is replaced; one with
  sessions keeps serving and the app says so (`stale`). Rebuilding the daemon on the host therefore does not silently leave old code running.
- Bridge: request and channel ids are global, so a late reply from a dead link cannot answer a new call; channels survive the connection (dedupe by index); ssh is tuned with
  `ServerAliveInterval=10`, `ServerAliveCountMax=3` placed before the defaults (the first `-o` value wins); a closed link starts a backoff loop (1, 2, 4, 8, 15, then 30 s, 24 tries)
  and `remote:state` events (`reconnecting`, `connected`, `lost`, `gap`, `stale`) drive a toast in `remote-workspace.ts`. `remote_disconnect` is now a detach; `remote_stop_host` ends the daemon.

Verified against `example-host`, killing the local ssh mid-run (`survives_a_dropped_link`, persistent): same shell pid before and after, `TICK1`..`TICK8` once each and in order
(the ticks printed while detached included), a write after the drop works, exactly one daemon on the host, the app saw disconnected, reconnecting, connected; then an idle daemon on an
older build was replaced (pid changed). `an_agent_session_survives_a_dropped_link`: an `opencode` session started, a prompt cut by the drop (the call fails with "connection closed"),
the same agent process still running, the session `running` after the reconnect, and the turn finished on the host (`turn_complete` and the answer are in `acp_events`).
All six ignored bridge tests pass in persistent mode (the agent-survival one needs it); in stdio mode the other five pass. Fixtures: each test now builds its own host folder, since
trust is remembered per folder and the editor had rewritten the shared `paper.tex`. Frontend (host, Node 24): 15 routing and notice tests, `tsc` and biome clean.

What this does not prove: a stalled network (a link that stays open but carries nothing) and sleep/wake were not exercised, only a clean kill of the local ssh; the write-timeout path is
covered by a unit test with a failing writer. Input typed during the gap is lost (the terminal prints its usual error once). While detached, agents keep running and spending the
user's plan until the idle exit. `acp:resync` did not fire in the host test because the reconnect (about 1 s) came before the model produced events.

Real app, dropped link (macOS dev app, `remote-demo` with `"persistent": true`): with a counting loop running in the app's terminal (`for i in $(seq 1 40); do echo $i; sleep 1; done`), the app's ssh
child was killed at 18:14:10. A new ssh appeared about a second later, the host's daemon kept the same pid (321105) and the terminal's shell (pid 321208) kept starting its `sleep 1`
without a break; the user reported the count continued without skipping a number. Not recorded from the UI: the exact wording of the notices, input typed during the gap.

## Setting a remote workspace up from the app (phase 7)

Until now a binding was a JSON file written by hand, with the helper built on the host. The new-project chooser has a fourth card, **Remote workspace**, that does it: host, then the helper on it, then a folder, then the project.
The card appears only when the backend answers `remote_bindings` (a build with `remote-bridge`), so the default build shows the same three cards as before.

```
 host key (ComputeHostKeyDialog) -> probe -> helper on the host -> folder -> project
                                    sh -s     sha256, --version    list/make   stub + binding, then a real get_project
```

Where the code is:
- `src-tauri/src/remote_bridge/setup.rs` (a child of `remote_bridge`, so it reaches the private binding file and `connect`): `remote_probe_host`, `remote_trust_host`, `remote_install_helper`, `remote_cancel_setup`, `remote_list_dir`, `remote_make_dir`, `remote_create_project`. All refuse any window but `main`.
  The host name, user and port are validated before ssh sees them, every path is quoted with `sh_quote`, and the frontend never supplies a path to install to: the backend probes the host itself.
- `remote_bridge.rs`: `update_config` writes `remote-workspaces.json` under a lock, through a temporary file and a rename, and refuses to write when the existing file cannot be parsed (reading treats it as empty, which would have erased the user's other entries on a write). `Config` is a `BTreeMap` now, so the file keeps a stable order.
- `project.rs`: `create_remote_stub` writes only `project.json` (no `main.tex`, no `git init`), and `discard_remote_stub` takes it back while it holds nothing else. The library lists the stub.
- `remote_commands::daemon_version_line` and `oleafly-remoted --version`: `oleafly-remoted <version> protocol <n>`. The helper runs the app's own commands, so one is usable only when `--version` prints exactly what the app expects.
- `src/lib/remote-setup.ts`, `src/components/library/remote/*`, `ProjectKindChooser` and `NewProjectDialog`. `refreshRemoteWorkspaces()` reads the bindings again before the new project opens: `initRemoteWorkspaces` runs once at boot, and without this the new project's first `list_file_tree` would have gone to the empty local stub.

What is installed, and where (shown to the user before anything is copied): `~/.oleafly/remoted/<app version>/oleafly-remoted` on the host, with its data in `~/.oleafly/remote-data`. Nothing outside `~/.oleafly` is touched.
The copy goes over the user's own ssh (`SshInput::Reader`, with progress events `remote:setup`), into a temporary file that is made executable, checked against its SHA-256 on the host, and only then renamed. Install is idempotent: the same bytes already there copy nothing, a different or damaged file is replaced.

Where the helper comes from on this computer, in order: `<data folder>/remote-daemons/<target>/oleafly-remoted`, then `oleafly-remoted` next to the app's own program when the host is of this computer's kind (a development build, a Mac host from a Mac). A file whose header is not an executable for the host's target is ignored. **No release job builds Linux helpers yet**, so an installed app has none to offer; when it has none for a host, the wizard says so and names the path to put one at. That is the open distribution question (bundle per target, download on first use, or build on the host).

Found while building it:
- The daemon links `webkit2gtk` and `gtk` on Linux (`cargo tree -i webkit2gtk --target x86_64-unknown-linux-gnu` shows both, through `tauri`), so a prebuilt Linux helper needs `libwebkit2gtk-4.1` and `libgtk-3` on the host at run time, which a server often lacks. The spike never saw this because it built the helper on a host that had the `-dev` packages. The probe looks for both libraries with `ldconfig -p` and the wizard shows the `apt` command; after a copy, `--version` is the real test, and a loader error is explained (libraries, a C library that is too old, a `noexec` home).
- The daemon did not create its data folder, so a host that had never run it failed at `remoted.lock`. It does now (`remote_hub::data_file`, mode 0700), with a test.
- The helper's unix socket lives under the data folder, and a socket path has a limit (104 bytes on macOS, 108 on Linux). A home folder with a very long path now makes the probe say so before anything is copied.
- A helper that could not start made `connect` wait its whole 30 seconds, and then the reader's end-of-stream branch started `reconnect_loop` for a link that had never come up, with "reconnecting" notices and about ten minutes of retries against a binding the setup had already removed. Now the end of the stream gathers ssh's last words, releases the wait at once, and brings a link back only if it had said `ready`; `reconnect_loop` also stops when no project is bound to that host and helper any more (`still_bound`). This changes the bridge for hand-written bindings too: a first connection that fails is reported, not retried.
- Helpers are built against glibc, so a musl host (Alpine) is refused with that reason. Their glibc floor is that of the machine that builds them.

Verified:
- Rust: 29 unit tests in `setup.rs` (probe parsing and its refusals, the install script's quoting and order, helper selection by header, folder listing, the broad-folder refusals, config writing without losing entries, a broken file left alone, eight writers at once, the stub and its removal) and one in `remote_hub`. `every_routed_command_is_registered` still passes. `cargo clippy -p oleafly --all-targets -D warnings` is clean with `remote-spike,remote-bridge`, with `remote-bridge` alone and with the default features.
- End to end, over real ssh (`scripts/spike/remote-setup-e2e.sh`, which starts a throwaway sshd on 127.0.0.1:22022 with a scratch `$HOME`; the "host" is this Mac): an unknown host key is reported and then trusted by fingerprint; the probe reads `Darwin arm64`; the 178 MB debug helper is copied, checked and started, with `upload` and `done` progress events; a second call copies nothing; appending a byte to the installed helper makes the next call replace it; folders are listed and made; the home folder and a missing folder are refused and leave no project and no binding behind; a helper that cannot start (the host's data folder made read-only) fails the creation in about 14 seconds, mostly the probe's checksum of the helper, with the host's own "Permission denied" in the message, the binding file back to empty, the project list unchanged and no `reconnecting` event in the following four seconds; then the project is created and a real `list_file_tree` and `get_project` through the persistent daemon return the host's `paper.tex` as the main document; the local project folder holds only `project.json`.
- Frontend: `remote-workspace.test.ts` (4 new: availability, a new binding routes at once, a failed read keeps what is known, nothing when the backend lacks the commands), `remote-setup.test.ts`, `RemoteProjectDialog.test.tsx` (15: ssh hosts, straight to the folder when the helper is there, the consent text and Install, missing libraries, no helper for the host, a failed copy, a new host key and a changed one, three failure kinds, a failed create keeps the form, a missing and a read-only folder, a copy stopped when the dialog goes away) and 2 new `ProjectKindChooser` tests (no remote card without the backend, a fourth card with it). `tsc` and `biome lint` are clean (two older warnings in test files). The 53 new keys exist in all 22 locales; `pnpm i18n:validate` reports 515 errors here, the same as the parent commit (Node 26's plural data for `vi`), none from the new keys.
  Full `vitest run` here: 52 failed, 24,290 passed, 7 skipped. The 12 files that fail (Typst, `ChatComposer`, `SettingsModal.search`, `mermaid-diagram`, `split-resizer` and others, which need `localStorage` or the real sidecars) fail the same 52 tests on the parent commit, so the change added none; none of them touches the library, the new project dialog, the tours or the catalogs.
  The wizard was also looked at in a browser with the Tauri calls faked (light and dark): the chooser with four cards, the host, install (with the progress bar), missing-libraries and folder steps. A dialog taller than the window now scrolls.

Not verified:
- A real **Linux** host. The Linux-only paths (`ldconfig` and the library check, glibc and musl detection, `aarch64`) are covered by tests of the parsers and messages, not by a run. The loopback host is a Mac.
- The wizard in the running Tauri window: its tests drive it with the backend mocked. The progress bar against a real transfer was not seen.
- The app started from the Finder or the Dock has no `SSH_AUTH_SOCK` unless launchd provides one; keys protected by a passphrase and not in an agent fail with the "keys or agent" message. Not tested.
- `ProxyJump` and other `~/.ssh/config` features: the wizard passes the config alias as the host, so they should apply as they do for Research compute targets, but no run used one.
- Cancelling a copy: the host's script removes its temporary file on a hangup, but a cancel after the file has been renamed leaves a complete helper there, which the next install reuses. Neither case was run.
- After an app update a binding still points at the older helper. `ensure_helper` is idempotent and could run on connect; it does not yet.
- Host-side messages (a probe failure, the reason a helper is unavailable, a missing folder) are English text from the backend, like the link notices, and are not in the catalogs.
- The chooser reuses the open-folder picture for the new card until it has art of its own.

To try it on a Linux host today (the first real run of the Linux-only paths): a helper from before this phase has no `--version`, so the wizard does not count it. Build one from this branch on the host and put it where the wizard looks:

```bash
cargo build -p oleafly --features remote-spike --bin oleafly-remoted
install -D target/debug/oleafly-remoted ~/.oleafly/remoted/0.4.11/oleafly-remoted
```

The wizard then sees a current helper and goes straight to the folder. Without it the wizard reports that this app has no helper for the host.
