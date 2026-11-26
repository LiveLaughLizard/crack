//! Process trees across platforms: run a command in its own group and stop it with
//! everything it started (process groups on Unix, `taskkill /T` on Windows), and the
//! shell that runs commands (bash, or PowerShell on Windows without Git Bash).

/// Puts the command in its own process group, so stopping it stops its children.
pub fn own_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

/// Stops a process and everything it started. `gently` asks first (SIGTERM); otherwise
/// it's killed outright.
pub fn stop_tree(pid: u32, gently: bool) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(
            pid as i32,
            if gently { libc::SIGTERM } else { libc::SIGKILL },
        );
    }
    #[cfg(windows)]
    {
        let mut args = vec!["/PID".to_owned(), pid.to_string(), "/T".to_owned()];
        if !gently {
            args.push("/F".into());
        }
        let _ = std::process::Command::new("taskkill")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Git Bash on Windows. `C:\Windows\System32\bash.exe` is the WSL launcher, which
/// Windows would find first; it's skipped.
#[cfg(windows)]
fn git_bash() -> Option<std::path::PathBuf> {
    static FOUND: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let on_path = std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join("bash.exe"))
                .filter(|p| p.is_file())
                .find(|p| {
                    let lower = p.display().to_string().to_ascii_lowercase();
                    !lower.contains("\\system32\\") && !lower.contains("\\windowsapps\\")
                });
            on_path.or_else(|| {
                ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"]
                    .iter()
                    .filter_map(std::env::var_os)
                    .flat_map(|base| {
                        let base = std::path::PathBuf::from(base);
                        [
                            base.join("Git\\bin\\bash.exe"),
                            base.join("Programs\\Git\\bin\\bash.exe"),
                        ]
                    })
                    .find(|p| p.is_file())
            })
        })
        .clone()
}

/// The program and arguments that run `command` in a shell: Git Bash, else PowerShell.
#[cfg(windows)]
pub fn shell_argv(command: &str) -> Vec<String> {
    match git_bash() {
        Some(bash) => vec![bash.display().to_string(), "-c".into(), command.to_owned()],
        None => vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            command.to_owned(),
        ],
    }
}

/// The program and arguments that run `command` in a shell.
#[cfg(not(windows))]
pub fn shell_argv(command: &str) -> Vec<String> {
    vec!["bash".into(), "-c".into(), command.to_owned()]
}
