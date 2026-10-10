use crate::{with_manager, PluginManager};
#[cfg(windows)]
pub(crate) use buzz_desktop::process::windows_job;
#[cfg(unix)]
pub(crate) use buzz_desktop::process::ProcessGroupGuard;
pub(crate) use buzz_desktop::process::{effective_path, resolve_program, run_output};
use buzz_desktop::process::run_output_with_input;
use buzzodz_plugins::{
    HostCommand, DEFAULT_HOST_COMMAND_OUTPUT_BYTES, MAX_HOST_COMMAND_OUTPUT_BYTES,
};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(5);

#[tauri::command]
pub(crate) async fn plugin_host_run_command(
    manager: tauri::State<'_, PluginManager>,
    id: String,
    revision: String,
    command_id: String,
    input: Option<String>,
) -> Result<Option<String>, String> {
    let operation = async {
        let command = with_manager(manager, move |manager| {
            manager
                .host_grants(&id, &revision)?
                .commands
                .into_iter()
                .find(|command| command.id == command_id)
                .ok_or_else(|| "Command is not declared".into())
        })
        .await
        .ok()?;
        run_command(&command, input.as_deref(), DEADLINE).await
    };
    Ok(tokio::time::timeout(DEADLINE, operation)
        .await
        .ok()
        .flatten())
}

async fn run_command(
    command: &HostCommand,
    input: Option<&str>,
    deadline: Duration,
) -> Option<String> {
    if command
        .max_input_bytes
        .is_some_and(|limit| !(1..=buzzodz_plugins::MAX_HOST_COMMAND_INPUT_BYTES).contains(&limit))
        || input.is_some_and(|value| {
            command
                .max_input_bytes
                .map_or(true, |limit| value.len() as u64 > limit)
        })
    {
        return None;
    }
    let path = effective_path();
    let max_output_bytes = command
        .max_output_bytes
        .unwrap_or(DEFAULT_HOST_COMMAND_OUTPUT_BYTES);
    if !(1..=MAX_HOST_COMMAND_OUTPUT_BYTES).contains(&max_output_bytes) {
        return None;
    }
    run(
        &resolve_program(&command.program, &path),
        &command.args,
        deadline,
        &path,
        max_output_bytes,
        input,
    )
    .await
}

async fn run(
    executable: &Path,
    args: &[String],
    deadline: Duration,
    path: &OsStr,
    max_output_bytes: u64,
    input: Option<&str>,
) -> Option<String> {
    let (output, status) =
        run_output_with_input(executable, args, deadline, path, max_output_bytes, input).await?;
    status.success().then_some(output)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{effective_path, run as run_with_path};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    async fn run(executable: &Path, args: &[String], deadline: Duration) -> Option<String> {
        run_with_path(
            executable,
            args,
            deadline,
            &effective_path(),
            super::DEFAULT_HOST_COMMAND_OUTPUT_BYTES,
            None,
        )
        .await
    }

    fn executable(script: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tool");
        crate::test_executable::write_executable(&path, format!("#!/bin/sh\n{script}\n"));
        (directory, path)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn per_user_installs_come_before_system_wide_ones() {
        let home = std::env::var_os("HOME").unwrap();
        let path = effective_path();
        let directories = std::env::split_paths(&path).collect::<Vec<_>>();
        let first = |directory: &Path| directories.iter().position(|item| item == directory);
        assert_eq!(first(&Path::new(&home).join(".local/bin")), Some(0));
        assert!(first(Path::new("/usr/local/bin")) > Some(0));
    }

    #[tokio::test]
    async fn larger_output_requires_a_bounded_manifest_opt_in() {
        let (_directory, path) =
            executable("/usr/bin/head -c 5000 /dev/zero | /usr/bin/tr '\\000' x");
        let mut command = super::HostCommand {
            id: "inventory".into(),
            program: "env".into(),
            args: vec![path.to_string_lossy().into_owned()],
            max_output_bytes: None,
            max_input_bytes: None,
        };
        assert_eq!(
            super::run_command(&command, None, Duration::from_secs(2)).await,
            None
        );
        command.max_output_bytes = Some(5000);
        assert_eq!(
            super::run_command(&command, None, Duration::from_secs(2)).await,
            Some("x".repeat(5000))
        );
        command.max_output_bytes = Some(4999);
        assert_eq!(
            super::run_command(&command, None, Duration::from_secs(2)).await,
            None
        );
        for limit in [0, super::MAX_HOST_COMMAND_OUTPUT_BYTES + 1] {
            command.max_output_bytes = Some(limit);
            assert_eq!(
                super::run_command(&command, None, Duration::from_secs(2)).await,
                None
            );
        }
    }

    #[tokio::test]
    async fn stdin_is_exact_data_and_output_is_drained_concurrently() {
        let (_directory, path) = executable("printf 'prefix'; cat");
        let input = "$(touch injected)\nreview decision";
        assert_eq!(
            run_with_path(
                &path,
                &[],
                Duration::from_secs(1),
                &effective_path(),
                65536,
                Some(input)
            )
            .await,
            Some(format!("prefix{input}"))
        );
        let (_directory, path) = executable("head -c 16384 /dev/zero; cat");
        let input = "x".repeat(16384);
        let output = run_with_path(
            &path,
            &[],
            Duration::from_secs(2),
            &effective_path(),
            65536,
            Some(&input),
        )
        .await
        .unwrap();
        assert_eq!(output.len(), 32768);
    }

    #[tokio::test]
    async fn undeclared_and_oversized_input_do_not_launch_the_command() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("executed");
        let mut command: buzzodz_plugins::HostCommand = serde_json::from_value(serde_json::json!({
            "id": "review", "program": "touch", "args": [marker]
        }))
        .unwrap();
        assert_eq!(
            super::run_command(&command, Some("data"), Duration::from_secs(1)).await,
            None
        );
        command.max_input_bytes = Some(1);
        assert_eq!(
            super::run_command(&command, Some("data"), Duration::from_secs(1)).await,
            None
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn passes_exact_args_without_shell_interpretation() {
        let (_directory, path) = executable(
            "[ \"$#\" -eq 2 ] && [ \"$1\" = status ] && [ \"$2\" = '--mode=ready;echo injected' ] || exit 1\nprintf 'ready \\n'",
        );
        assert_eq!(
            run(
                &path,
                &["status".into(), "--mode=ready;echo injected".into()],
                Duration::from_secs(1)
            )
            .await,
            Some("ready \n".into())
        );
    }

    #[tokio::test]
    async fn env_shebang_uses_the_path_that_resolved_the_command() {
        let directory = tempfile::tempdir().unwrap();
        let interpreter = directory.path().join("fixture-runtime");
        crate::test_executable::write_executable(&interpreter, "#!/bin/sh\nexec /bin/sh \"$@\"\n");
        let tool = directory.path().join("tool");
        crate::test_executable::write_executable(
            &tool,
            "#!/usr/bin/env fixture-runtime\nprintf 'ready\\n'\n",
        );
        let path =
            std::env::join_paths([Path::new("/usr/bin"), Path::new("/bin"), directory.path()])
                .unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(super::resolve_program("tool", &path), tool);
        assert_eq!(
            run_with_path(
                &tool,
                &[],
                Duration::from_secs(5),
                &path,
                super::DEFAULT_HOST_COMMAND_OUTPUT_BYTES,
                None
            )
            .await,
            Some("ready\n".into())
        );
    }

    #[tokio::test]
    async fn returns_none_for_failures_and_invalid_utf8() {
        let (_directory, path) = executable("echo 'private stderr' >&2; exit 1");
        assert_eq!(run(&path, &[], Duration::from_secs(1)).await, None);
        let (_directory, path) = executable("printf '\\377'");
        assert_eq!(run(&path, &[], Duration::from_secs(1)).await, None);
        assert_eq!(
            run(
                PathBuf::from("missing-tool-executable").as_path(),
                &[],
                Duration::from_secs(1)
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn rejects_large_output_without_waiting_for_the_process() {
        let (_directory, path) = executable("yes x");
        let started = Instant::now();
        assert_eq!(run(&path, &[], Duration::from_secs(1)).await, None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn kills_a_stalled_command_at_the_deadline() {
        let (_directory, path) = executable("sleep 10");
        let started = Instant::now();
        assert_eq!(run(&path, &[], Duration::from_millis(50)).await, None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn aborting_the_read_kills_the_cli_process() {
        let (_directory, path) = executable("printf '%s' \"$$\" > \"$0.pid\"\nexec sleep 10");
        let marker = path.with_extension("pid");
        let reader = tokio::spawn(async move { run(&path, &[], Duration::from_secs(5)).await });
        let process_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(process_id) = fs::read_to_string(&marker) {
                    if let Ok(process_id) = process_id.parse::<i32>() {
                        break process_id;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake command did not start");

        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        let exited = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let result =
                    unsafe { libc::waitpid(process_id, std::ptr::null_mut(), libc::WNOHANG) };
                if result == process_id
                    || (result == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
                {
                    break;
                }
                assert_eq!(result, 0);
                tokio::task::yield_now().await;
            }
        })
        .await;
        if exited.is_err() {
            unsafe { libc::kill(-process_id, libc::SIGKILL) };
            unsafe { libc::waitpid(process_id, std::ptr::null_mut(), 0) };
        }
        assert!(exited.is_ok(), "aborted command left the child running");
    }

    #[tokio::test]
    async fn aborting_the_read_kills_descendants() {
        let (_directory, path) =
            executable("sleep 10 &\nprintf '%s' \"$!\" > \"$0.childpid\"\nwait");
        let marker = path.with_extension("childpid");
        let reader = tokio::spawn(async move { run(&path, &[], Duration::from_secs(5)).await });
        let process_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(process_id) = fs::read_to_string(&marker) {
                    if let Ok(process_id) = process_id.parse::<i32>() {
                        break process_id;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("descendant did not start");

        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        let exited = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if unsafe { libc::kill(process_id, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if exited.is_err() {
            unsafe { libc::kill(process_id, libc::SIGKILL) };
        }
        assert!(exited.is_ok(), "aborted command left a descendant running");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{effective_path, run, windows_job::WindowsJob};
    use std::fs;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;
    use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, ResumeThread, SuspendThread, TerminateProcess, WaitForSingleObject,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    fn command_script(directory: &Path) -> (PathBuf, PathBuf) {
        let script = directory.join("spawn-child.ps1");
        let marker = directory.join("child.pid");
        let marker_literal = marker.to_string_lossy().replace('\'', "''");
        fs::write(
            &script,
            format!(
                "$child = Start-Process -FilePath ping.exe -ArgumentList '-n 30 127.0.0.1' -PassThru -WindowStyle Hidden\n[System.IO.File]::WriteAllText('{marker_literal}', [string]$child.Id)\n$child.WaitForExit()\n"
            ),
        )
        .unwrap();
        (script, marker)
    }

    async fn descendant_id(marker: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Ok(process_id) = fs::read_to_string(marker) {
                    if let Ok(process_id) = process_id.parse::<u32>() {
                        break process_id;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant did not start")
    }

    fn assert_descendant_exited(process_id: u32) {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, process_id) };
        if handle.is_null() {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(ERROR_INVALID_PARAMETER as i32),
                "failed to inspect descendant"
            );
            return;
        }
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        let result = unsafe { WaitForSingleObject(process.as_raw_handle(), 3_000) };
        if result != WAIT_OBJECT_0 {
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
        }
        assert_eq!(result, WAIT_OBJECT_0, "command left a descendant running");
    }

    #[tokio::test]
    async fn command_starts_only_after_job_assignment() {
        let directory = tempfile::tempdir().unwrap();
        let (script, marker) = command_script(directory.path());
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                script.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let job = WindowsJob::new().unwrap();
        let child = job
            .spawn_with_check(&mut command, 0, |_, thread| {
                let previous_count = unsafe { SuspendThread(thread.as_raw_handle()) };
                let restored_count = unsafe { ResumeThread(thread.as_raw_handle()) };
                assert_eq!(previous_count, 1, "command ran before job assignment");
                assert_eq!(restored_count, 2);
                assert!(!marker.exists(), "command ran before job assignment");
            })
            .unwrap();
        let process_id = descendant_id(&marker).await;
        drop(job);
        assert_descendant_exited(process_id);
        drop(child);
    }

    #[tokio::test]
    async fn cancellation_kills_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let (script, marker) = command_script(directory.path());
        let reader = tokio::spawn(async move {
            run(
                Path::new("powershell.exe"),
                &[
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                    script.to_string_lossy().into_owned(),
                ],
                Duration::from_secs(10),
                &effective_path(),
                super::DEFAULT_HOST_COMMAND_OUTPUT_BYTES,
                None,
            )
            .await
        });
        let process_id = descendant_id(&marker).await;
        reader.abort();
        assert!(reader.await.unwrap_err().is_cancelled());
        assert_descendant_exited(process_id);
    }

    #[tokio::test]
    async fn timeout_kills_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let (script, marker) = command_script(directory.path());
        let reader = tokio::spawn(async move {
            run(
                Path::new("powershell.exe"),
                &[
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                    script.to_string_lossy().into_owned(),
                ],
                Duration::from_secs(5),
                &effective_path(),
                super::DEFAULT_HOST_COMMAND_OUTPUT_BYTES,
                None,
            )
            .await
        });
        let process_id = descendant_id(&marker).await;
        assert_eq!(reader.await.unwrap(), None);
        assert_descendant_exited(process_id);
    }
}
