use std::{path::Path, sync::Arc, time::Duration};

use fs::{FakeFs, Fs};
use gpui::{BackgroundExecutor, FutureExt as _, TestAppContext};
use pretty_assertions::assert_eq;
use project::resolve_shell_environment_directory_for_tests;
use serde_json::json;
use util::path;

const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

async fn resolve_directory(
    abs_path: &Path,
    fs: Option<Arc<dyn fs::Fs>>,
    executor: &BackgroundExecutor,
) -> Result<Arc<Path>, String> {
    resolve_shell_environment_directory_for_tests(Arc::from(abs_path), fs)
        .with_timeout(RESOLVE_TIMEOUT, executor)
        .await
        .map_err(|_| "timed out resolving shell environment directory".to_string())?
        .map_err(|error| error.to_string())
}

#[gpui::test]
async fn test_shell_environment_directory_uses_fs_for_virtual_directory(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/project"), json!({})).await;

    let resolved = resolve_directory(
        Path::new(path!("/project")),
        Some(fs.clone()),
        &cx.executor(),
    )
    .await;

    assert_eq!(
        resolved.as_ref().map(|path| path.as_ref()),
        Ok(Path::new(path!("/project"))),
        "a FakeFs directory is not on the host disk; smol::fs::metadata cannot see it and must not be used to decide whether the worktree root is a directory"
    );
}

#[gpui::test]
async fn test_shell_environment_directory_uses_parent_for_virtual_file(cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/project"),
        json!({
            "readme.md": "# hi\n"
        }),
    )
    .await;

    let resolved = resolve_directory(
        Path::new(path!("/project/readme.md")),
        Some(fs.clone()),
        &cx.executor(),
    )
    .await;

    assert_eq!(
        resolved.as_ref().map(|path| path.as_ref()),
        Ok(Path::new(path!("/project"))),
        "a FakeFs file is not on the host disk; the directory used for direnv must be its parent, not a smol::fs::metadata error"
    );
}

#[gpui::test]
async fn test_shell_environment_directory_follows_virtual_symlink_to_directory(
    cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/project"),
        json!({
            "real": {}
        }),
    )
    .await;
    fs.create_symlink(
        Path::new(path!("/project/link")),
        path!("/project/real").into(),
    )
    .await
    .expect("create symlink");

    let resolved = resolve_directory(
        Path::new(path!("/project/link")),
        Some(fs.clone()),
        &cx.executor(),
    )
    .await;

    assert_eq!(
        resolved.as_ref().map(|path| path.as_ref()),
        Ok(Path::new(path!("/project/link"))),
        "a symlink to a directory must be treated as a directory, not rejected as missing because smol::fs::metadata cannot see FakeFs"
    );
}

#[gpui::test]
async fn test_shell_environment_directory_missing_virtual_path_is_an_error(
    cx: &mut TestAppContext,
) {
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/project"), json!({})).await;

    let resolved = resolve_directory(
        Path::new(path!("/missing")),
        Some(fs.clone()),
        &cx.executor(),
    )
    .await;

    assert_eq!(
        resolved,
        Err(format!("stat {:?}", Path::new(path!("/missing")))),
        "a missing path must stay an error; treating it as a file would use its parent and silently run direnv in the wrong directory"
    );
}

#[test]
fn test_shell_environment_directory_real_directory_without_fs() {
    let temp_directory = tempfile::TempDir::new().expect("temp directory");
    let abs_path = temp_directory.path().to_path_buf();
    let resolved = smol::block_on(resolve_shell_environment_directory_for_tests(
        abs_path.clone().into(),
        None,
    ))
    .map_err(|error| error.to_string());

    assert_eq!(
        resolved.as_ref().map(|path| path.as_ref()),
        Ok(abs_path.as_path()),
        "native resolution without an Fs still uses the host filesystem and must keep treating a real directory as a directory"
    );
}

#[test]
fn test_shell_environment_directory_real_file_uses_parent_without_fs() {
    let temp_directory = tempfile::TempDir::new().expect("temp directory");
    let file_path = temp_directory.path().join("file.txt");
    std::fs::write(&file_path, "hi\n").expect("write file");

    let resolved = smol::block_on(resolve_shell_environment_directory_for_tests(
        file_path.into(),
        None,
    ))
    .map_err(|error| error.to_string());

    assert_eq!(
        resolved.as_ref().map(|path| path.as_ref()),
        Ok(temp_directory.path()),
        "native resolution without an Fs must keep using the parent of a real file, matching smol::fs::metadata"
    );
}

#[test]
fn wasm_path_lookup_does_not_return_a_host_git_binary() {
    let cwd = std::env::current_dir().expect("cwd");
    let path = std::env::var_os("PATH");
    let found = project::lookup_system_binary_impl_for_tests("git", path.as_deref(), &cwd, true);
    assert_eq!(
        found, None,
        "actual {found:?}; wasm must not search PATH because std::env::split_paths panics. correct: None"
    );
}

#[test]
fn native_path_lookup_still_finds_git_when_it_is_on_path() {
    let cwd = std::env::current_dir().expect("cwd");
    let path = std::env::var_os("PATH");
    let found = project::lookup_system_binary_impl_for_tests("git", path.as_deref(), &cwd, false);
    assert!(
        found.is_some(),
        "native which must still find git on PATH; actual {found:?}"
    );
}

#[test]
fn wasm_direnv_is_spawned_by_name_without_path_lookup() {
    let program = project::direnv_spawn_path_for_tests(true);
    assert_eq!(
        program,
        Some(std::path::PathBuf::from("direnv")),
        "actual {program:?}; wasm must pass the program name so Process::output resolves PATH on the host. correct: Some(\"direnv\")"
    );
}

#[test]
fn native_direnv_lookup_matches_path_search() {
    let looked_up =
        project::lookup_system_binary_impl_for_tests("direnv", None, Path::new("."), false);
    let found = project::direnv_spawn_path_for_tests(false);
    assert_eq!(
        found, looked_up,
        "native direnv spawn path must stay the PATH lookup result; actual {found:?}, correct {looked_up:?}"
    );
}

#[test]
fn wasm_adapter_which_must_not_call_which_in() {
    let cwd = std::env::current_dir().expect("cwd");
    let path = std::env::var_os("PATH");
    let actual = project::lookup_adapter_binary_for_tests(
        std::ffi::OsStr::new("git"),
        path.as_deref(),
        &cwd,
        true,
    );
    let correct = Some(std::path::PathBuf::from("git"));
    assert_eq!(
        actual, correct,
        "actual {actual:?}; after direnv, LSP which() is called with the captured PATH. wasm must not run which::which_in (host absolute path + split_paths panics). correct: Some(\"git\") so Process::output resolves PATH on the host, matching direnv_spawn_path"
    );
}

#[test]
fn native_adapter_which_still_finds_git_on_path() {
    let cwd = std::env::current_dir().expect("cwd");
    let path = std::env::var_os("PATH");
    let actual = project::lookup_adapter_binary_for_tests(
        std::ffi::OsStr::new("git"),
        path.as_deref(),
        &cwd,
        false,
    );
    let correct = which::which_in("git", path.as_deref(), &cwd).ok();
    assert_eq!(
        actual, correct,
        "native adapter which must stay which::which_in; actual {actual:?}, correct {correct:?}"
    );
}

#[test]
fn wasm_adapter_which_rejects_an_empty_program_name() {
    let cwd = std::env::current_dir().expect("cwd");
    let actual =
        project::lookup_adapter_binary_for_tests(std::ffi::OsStr::new(""), None, &cwd, true);
    assert_eq!(
        actual, None,
        "actual {actual:?}; an empty program name must not become a spawnable path. correct: None"
    );
}
