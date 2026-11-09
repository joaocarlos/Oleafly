//! The commands a remote workspace runs on its own host. One list for the
//! daemon, the bridge and the frontend (the bridge hands `FILES` to the
//! frontend); `remote_spike::tests::every_routed_command_is_registered` checks
//! the daemon answers each of them.
#![cfg_attr(not(feature = "remote-bridge"), allow(dead_code))]

/// Agent and terminal commands are routed by prefix.
pub const PREFIXES: &[&str] = &["acp_", "term_"];

/// Project file commands, listed one by one.
pub const FILES: &[&str] = &[
    "cancel_compile",
    "compile_project",
    "copy_file",
    "create_file",
    "delete_file",
    "get_project",
    "list_file_tree",
    "list_files",
    "project_compile_folder",
    "project_engine",
    "project_file_sizes",
    "project_folder_status",
    "project_manifest_home",
    "project_mutation_generation",
    "project_tex_status",
    "project_trust_state",
    "read_build_artifact",
    "read_compiled_pdf",
    "read_file",
    "read_project_bytes",
    "read_project_sources",
    "rename_file",
    "revoke_folder_trust",
    "synctex_forward",
    "synctex_inverse",
    "synctex_map_line",
    "unwatch_project_folder",
    "validate_compile_fingerprint",
    "watch_project_folder",
    "write_file",
    "write_project_bytes",
];

/// Git commands of a project, run by the host's own `git`. `git_clone_project` is not here: it makes
/// a new local project. Credentials stay on the host: its ssh keys and its Oleafly accounts.
pub const GIT: &[&str] = &[
    "git_abort_merge",
    "git_ahead_behind",
    "git_checkout_branch",
    "git_clean_remote_credentials",
    "git_commit",
    "git_commit_amend",
    "git_continue_merge",
    "git_create_branch",
    "git_current_branch",
    "git_diff",
    "git_discard",
    "git_discard_paths",
    "git_fetch",
    "git_get_remote",
    "git_head_oid",
    "git_initialize",
    "git_is_initialized",
    "git_log",
    "git_prepare_publish",
    "git_publish_preflight",
    "git_pull",
    "git_push",
    "git_remote_credentials_need_cleanup",
    "git_remove_remote",
    "git_resolve_conflict",
    "git_restore",
    "git_set_remote",
    "git_show",
    "git_stage",
    "git_stage_all",
    "git_stage_paths",
    "git_stash_pop",
    "git_stash_push",
    "git_status",
    "git_unstage",
    "git_unstage_all",
    "git_unstage_paths",
    "git_workspace_snapshot",
];

/// Commands the app answers itself before involving the host: `trust_folder`
/// asks the user on this computer, then applies the grant on the host.
pub const ASKED_HERE: &[&str] = &["trust_folder"];

/// Events the daemon sends that the app re-emits unchanged.
pub const EVENTS: &[&str] = &[
    "acp:event",
    "acp:session",
    "compile:log",
    "project-folder-changed",
];

pub fn routable(command: &str) -> bool {
    PREFIXES.iter().any(|prefix| command.starts_with(prefix))
        || FILES.contains(&command)
        || GIT.contains(&command)
}
