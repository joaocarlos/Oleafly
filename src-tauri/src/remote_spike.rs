//! SPIKE (phase 1): run Oleafly's own Tauri commands without a window and
//! speak to them over stdio, so a client can reach them through
//! `ssh host oleafly-remoted`.
//!
//! One JSON object per line.
//!   in : {"id":1,"cmd":"term_open","args":{...},"channels":{"channel":7}}
//!   out: {"id":1,"ok":<value>} | {"id":1,"err":<value>}
//!        {"channel":7,"index":0,"message":<value>}   (Tauri Channel traffic)
//!
//! Commands run on `tauri::test::MockRuntime`, so only commands that are
//! generic over `R: Runtime` can be registered here.
use std::io::BufRead;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{json, Value};
use tauri::ipc::{CallbackFn, InvokeBody, InvokeError, InvokeResponse, InvokeResponseBody};
use tauri::test::{mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{Listener, Manager, WebviewWindowBuilder};

type Out = Arc<crate::remote_hub::Hub>;

static ATTACHED: Mutex<Vec<(String, std::path::PathBuf, std::path::PathBuf)>> =
    Mutex::new(Vec::new());

/// Folder and state dir a project id was attached to with `remote_attach`.
pub(crate) fn attached(project_id: &str) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let list = ATTACHED.lock().unwrap_or_else(|e| e.into_inner());
    list.iter()
        .find(|(id, _, _)| id == project_id)
        .map(|(_, root, state)| (root.clone(), state.clone()))
}

/// The project's own manifest lives next to the daemon's state, not in the folder.
fn seed_manifest(
    state: &std::path::Path,
    root: &std::path::Path,
    main_doc: Option<&str>,
    engine: Option<&str>,
) -> Result<(), String> {
    let manifest = state.join("project.json");
    if manifest.exists() {
        return Ok(());
    }
    let mut documents: Vec<String> = std::fs::read_dir(root)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            [".tex", ".typ", ".md"]
                .iter()
                .any(|ext| name.ends_with(ext))
        })
        .collect();
    documents.sort();
    let main = main_doc
        .map(str::to_owned)
        .or_else(|| {
            documents
                .iter()
                .find(|name| name.as_str() == "main.tex")
                .cloned()
        })
        .or_else(|| documents.first().cloned())
        .unwrap_or_else(|| "main.tex".to_owned());
    let name = root.file_name().map_or_else(
        || "Remote project".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    // The bundled Tectonic is not on a host; LaTeX there builds with the system's latexmk.
    let engine = engine
        .map(str::to_owned)
        .or_else(|| main.ends_with(".tex").then(|| "latexmk".to_owned()));
    let mut meta = json!({ "name": name, "main_doc": main });
    if let Some(engine) = engine {
        meta["engine"] = Value::String(engine);
    }
    std::fs::write(&manifest, meta.to_string()).map_err(|e| e.to_string())
}

fn attach(
    project_id: &str,
    path: &str,
    main_doc: Option<&str>,
    engine: Option<&str>,
) -> Result<serde_json::Value, String> {
    crate::paths::validate_project_id(project_id)?;
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => {
            std::path::PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is not set")?)
                .join(rest)
        }
        None => std::path::PathBuf::from(path),
    };
    if !expanded.is_absolute() {
        return Err("The folder must be an absolute path or start with ~/.".into());
    }
    let root = expanded
        .canonicalize()
        .map_err(|e| format!("{}: {e}", expanded.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a folder.", root.display()));
    }
    // Linked projects keep their data directly under the folder registry; the build,
    // manifest and trust code check that, so an attached folder gets its entry there.
    let state = crate::paths::linked_root()?.join(project_id);
    std::fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    seed_manifest(&state, &root, main_doc, engine)?;
    let mut list = ATTACHED.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(id, _, _)| id != project_id);
    list.push((project_id.to_owned(), root.clone(), state));
    Ok(json!({ "root": root }))
}

fn emit(out: &Out, value: &Value) {
    out.send(value);
}

/// Ids of the projects attached so far; a client that reconnects learns which it need not attach again.
pub(crate) fn attached_ids() -> Vec<String> {
    lock_attached()
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect()
}

fn lock_attached(
) -> std::sync::MutexGuard<'static, Vec<(String, std::path::PathBuf, std::path::PathBuf)>> {
    ATTACHED.lock().unwrap_or_else(|e| e.into_inner())
}

fn body_json(body: &InvokeResponseBody) -> Value {
    match body {
        InvokeResponseBody::Json(text) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        // Raw bytes cannot ride in a JSON line; the bridge turns this back into bytes.
        InvokeResponseBody::Raw(bytes) => {
            json!({ "base64": base64::engine::general_purpose::STANDARD.encode(bytes) })
        }
    }
}

/// The app the daemon runs commands on: no window, our channel sink, the shared command list.
fn build_app(out: &Out) -> Result<tauri::App<tauri::test::MockRuntime>, String> {
    let channel_out = out.clone();
    mock_builder()
        .manage(crate::state::AppState::default())
        .invoke_handler(tauri::generate_handler![
            crate::protocol::backend_protocol_info,
            crate::project::get_project,
            crate::project::list_file_tree,
            crate::project::list_files,
            crate::project::read_file,
            crate::project::write_file,
            crate::project::create_file,
            crate::project::delete_file,
            crate::project::rename_file,
            crate::project::copy_file,
            crate::commands::read_project_bytes,
            crate::commands::write_project_bytes,
            crate::project_file_sizes::project_file_sizes,
            crate::project_sources::read_project_sources,
            crate::project::project_manifest_home,
            crate::commands::compile_project,
            crate::commands::cancel_compile,
            crate::commands::read_compiled_pdf,
            crate::commands::read_build_artifact,
            crate::commands::validate_compile_fingerprint,
            crate::synctex::synctex_forward,
            crate::synctex::synctex_inverse,
            crate::synctex::synctex_map_line,
            crate::project::project_tex_status,
            crate::trust::revoke_folder_trust,
            crate::project::project_mutation_generation,
            crate::folder_status::project_folder_status,
            crate::trust::project_trust_state,
            crate::commands::project_engine,
            crate::project::project_compile_folder,
            crate::folder_watch::watch_project_folder,
            crate::folder_watch::unwatch_project_folder,
            crate::git::git_abort_merge,
            crate::git::git_ahead_behind,
            crate::git::git_checkout_branch,
            crate::git::git_clean_remote_credentials,
            crate::git::git_commit,
            crate::git::git_commit_amend,
            crate::git::git_continue_merge,
            crate::git::git_create_branch,
            crate::git::git_current_branch,
            crate::git::git_diff,
            crate::git::git_discard,
            crate::git::git_discard_paths,
            crate::git::git_fetch,
            crate::git::git_get_remote,
            crate::git::git_head_oid,
            crate::git::git_initialize,
            crate::git::git_is_initialized,
            crate::git::git_log,
            crate::git::git_prepare_publish,
            crate::git::git_publish_preflight,
            crate::git::git_pull,
            crate::git::git_push,
            crate::git::git_remote_credentials_need_cleanup,
            crate::git::git_remove_remote,
            crate::git::git_resolve_conflict,
            crate::git::git_restore,
            crate::git::git_set_remote,
            crate::git::git_show,
            crate::git::git_stage,
            crate::git::git_stage_all,
            crate::git::git_stage_paths,
            crate::git::git_stash_pop,
            crate::git::git_stash_push,
            crate::git::git_status,
            crate::git::git_unstage,
            crate::git::git_unstage_all,
            crate::git::git_unstage_paths,
            crate::git::git_workspace_snapshot,
            crate::terminal::term_ack,
            crate::terminal::term_foreground,
            crate::terminal::term_kill,
            crate::terminal::term_open,
            crate::terminal::term_resize,
            crate::terminal::term_shells,
            crate::terminal::term_write,
            crate::acp::commands::acp_agent_preferences,
            crate::acp::commands::acp_agent_sessions,
            crate::acp::commands::acp_authenticate,
            crate::acp::commands::acp_buffer_reply,
            crate::acp::commands::acp_cancel,
            crate::acp::commands::acp_cancel_start,
            crate::acp::commands::acp_catalog,
            crate::acp::commands::acp_check_agent,
            crate::acp::commands::acp_commit_turn,
            crate::acp::commands::acp_delegated_permission,
            crate::acp::commands::acp_disconnect,
            crate::acp::commands::acp_elicitation_respond,
            crate::acp::commands::acp_events,
            crate::acp::commands::acp_fork,
            crate::acp::commands::acp_hermes_set_option,
            crate::acp::commands::acp_hermes_settings,
            crate::acp::commands::acp_hermes_status,
            crate::acp::commands::acp_install,
            crate::acp::commands::acp_logout,
            crate::acp::commands::acp_permission,
            crate::acp::commands::acp_pick_agent_program,
            crate::acp::commands::acp_preferences,
            crate::acp::commands::acp_prompt,
            crate::acp::commands::acp_provider_choices,
            crate::acp::commands::acp_reconnect,
            crate::acp::commands::acp_refresh_plan_usage,
            crate::acp::commands::acp_register,
            crate::acp::commands::acp_registry_search,
            crate::acp::commands::acp_remove_agent,
            crate::acp::commands::acp_session_delete,
            crate::acp::commands::acp_session_events_all,
            crate::acp::commands::acp_session_export,
            crate::acp::commands::acp_session_import,
            crate::acp::commands::acp_session_rename,
            crate::acp::commands::acp_sessions,
            crate::acp::commands::acp_set_agent_preference,
            crate::acp::commands::acp_set_agent_preferences,
            crate::acp::commands::acp_set_agent_program,
            crate::acp::commands::acp_set_config_option,
            crate::acp::commands::acp_set_hermes_settings,
            crate::acp::commands::acp_set_mode,
            crate::acp::commands::acp_set_model,
            crate::acp::commands::acp_set_preferences,
            crate::acp::commands::acp_sign_in_command,
            crate::acp::commands::acp_snapshot,
            crate::acp::commands::acp_start,
            crate::acp::commands::acp_steer,
            crate::acp::commands::acp_test_remote_hermes,
        ])
        .channel_interceptor(move |_webview, callback, index, body| {
            emit(
                &channel_out,
                &json!({ "channel": callback.0, "index": index, "message": body_json(body) }),
            );
            true
        })
        .build(mock_context(noop_assets()))
        .map_err(|e| e.to_string())
}

type Mock = tauri::test::MockRuntime;

/// The app, the agent runtime and the forwarders: everything the daemon needs before it takes requests.
fn boot(out: &Out) -> Result<(tauri::App<Mock>, tauri::WebviewWindow<Mock>), String> {
    let app = build_app(out)?;
    // Agents: own the runtime here and forward its broadcast events as JSON
    // lines, instead of `acp::attach`, which emits them to local windows.
    let runtime = crate::acp::AcpRuntime::new(crate::paths::oleafly_root()?.join("acp"))?;
    let mut events = runtime.subscribe();
    let mut summaries = runtime.subscribe_summaries();
    app.manage(runtime);
    let event_out = out.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(event) = events.recv().await {
            emit(
                &event_out,
                &json!({ "event": "acp:event", "payload": event }),
            );
        }
    });
    let summary_out = out.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(summary) = summaries.recv().await {
            emit(
                &summary_out,
                &json!({ "event": "acp:session", "payload": summary }),
            );
        }
    });
    for name in crate::remote_commands::EVENTS
        .iter()
        .filter(|name| !name.starts_with("acp:"))
    {
        let event_out = out.clone();
        let name = (*name).to_owned();
        let forwarded = name.clone();
        app.listen_any(name, move |event| {
            let payload = serde_json::from_str::<Value>(event.payload()).unwrap_or(Value::Null);
            emit(
                &event_out,
                &json!({ "event": forwarded, "payload": payload }),
            );
        });
    }
    let window = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .map_err(|e| e.to_string())?;
    Ok((app, window))
}

/// One request line: attach, trust, or a Tauri command; the reply goes out through `out`.
fn handle_line(out: &Out, webview: &tauri::Webview<Mock>, line: &str) {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        emit(out, &json!({ "err": "unreadable request" }));
        return;
    };
    let id = message["id"].clone();
    let cmd = message["cmd"].as_str().unwrap_or_default().to_string();
    if cmd == "remote_trust" {
        // Not a Tauri command: the grant itself, after the user said yes on their computer.
        let a = message["args"].clone();
        let reply = out.clone();
        tauri::async_runtime::spawn(async move {
            let scope = serde_json::from_value(a["scope"].clone()).unwrap_or_default();
            let value = match crate::trust::grant_confirmed_remotely(
                a["projectId"].as_str().unwrap_or_default().to_owned(),
                scope,
            )
            .await
            {
                Ok(trust) => json!({ "id": id, "ok": trust }),
                Err(error) => json!({ "id": id, "err": error }),
            };
            emit(&reply, &value);
        });
        return;
    }
    if cmd == "remote_attach" {
        let a = &message["args"];
        let value = match attach(
            a["projectId"].as_str().unwrap_or_default(),
            a["path"].as_str().unwrap_or_default(),
            a["mainDoc"].as_str(),
            a["engine"].as_str(),
        ) {
            Ok(ok) => json!({ "id": id, "ok": ok }),
            Err(err) => json!({ "id": id, "err": err }),
        };
        emit(out, &value);
        return;
    }
    let mut args = message.get("args").cloned().unwrap_or_else(|| json!({}));
    if let Some(channels) = message["channels"].as_object() {
        for (key, channel_id) in channels {
            args[key] = Value::String(format!("__CHANNEL__:{channel_id}"));
        }
    }
    let request = InvokeRequest {
        cmd,
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "tauri://localhost".parse().expect("a fixed url"),
        body: InvokeBody::Json(args),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    };
    let reply = out.clone();
    webview.clone().on_message(
        request,
        Box::new(move |_webview, _cmd, response, _callback, _error| {
            let value = match response {
                InvokeResponse::Ok(InvokeResponseBody::Raw(bytes)) => {
                    json!({ "id": id, "okRaw": base64::engine::general_purpose::STANDARD.encode(bytes) })
                }
                InvokeResponse::Ok(body) => json!({ "id": id, "ok": body_json(&body) }),
                InvokeResponse::Err(InvokeError(error)) => json!({ "id": id, "err": error }),
            };
            emit(&reply, &value);
        }),
    );
}

/// Stdio mode: the process lives as long as the link does.
pub fn run_stdio() -> Result<(), String> {
    let out: Out = crate::remote_hub::Hub::with_stdout();
    let (_app, window) = boot(&out)?;
    let webview = AsRef::<tauri::Webview<_>>::as_ref(&window).clone();
    emit(
        &out,
        &json!({ "ready": true, "pid": std::process::id(), "attached": [] }),
    );
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if !line.trim().is_empty() {
            handle_line(&out, &webview, &line);
        }
    }
    Ok(())
}

/// Persistent mode: keep running after the link is gone, and be reachable again.
pub fn run_serve() -> Result<(), String> {
    let out: Out = crate::remote_hub::Hub::new();
    let (_app, window) = boot(&out)?;
    let webview = AsRef::<tauri::Webview<_>>::as_ref(&window).clone();
    let handler_out = out.clone();
    crate::remote_hub::serve(
        out,
        Arc::new(move |line: &str| handle_line(&handler_out, &webview, line)),
    )
}

/// The line `oleafly-remoted --version` prints, without starting anything.
pub fn version_line() -> String {
    crate::remote_commands::daemon_version_line()
}

pub fn run_attach() -> Result<(), String> {
    crate::remote_hub::attach()
}

pub fn run_stop() -> Result<(), String> {
    crate::remote_hub::stop()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke_error(webview: &tauri::WebviewWindow<tauri::test::MockRuntime>, cmd: &str) -> String {
        let request = InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::Json(json!({})),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        match tauri::test::get_ipc_response(webview, request) {
            Ok(_) => String::new(),
            Err(error) => error.to_string(),
        }
    }

    /// A command the daemon does not have answers "not found"; any other answer, even a refusal
    /// of the empty arguments, means it is registered. Keeps `remote_commands` and the daemon in step.
    #[test]
    fn every_routed_command_is_registered() {
        let out: Out = crate::remote_hub::Hub::with_stdout();
        let app = build_app(&out).unwrap();
        let window = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let missing: Vec<&str> = crate::remote_commands::FILES
            .iter()
            .chain(crate::remote_commands::GIT)
            .copied()
            .chain(["acp_catalog", "acp_start", "term_open", "term_shells"])
            .filter(|name| invoke_error(&window, name).contains("not found"))
            .collect();
        assert!(
            missing.is_empty(),
            "routed but not registered in the daemon: {missing:?}"
        );
        assert!(
            invoke_error(&window, "no_such_command_here").contains("not found"),
            "the check cannot tell a missing command"
        );
    }
}
