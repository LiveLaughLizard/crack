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

/// The program and arguments that run `command` in a shell.
pub fn shell_argv(command: &str) -> Vec<String> {
    #[cfg(windows)]
    if crate::policy::which("bash").is_none() {
        return vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            command.to_owned(),
        ];
    }
    vec!["bash".into(), "-c".into(), command.to_owned()]
}
