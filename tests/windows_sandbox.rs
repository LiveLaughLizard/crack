//! The Windows sandbox, through the real helper (`crack __sandbox-exec`): commands
//! write inside the shared folder and nowhere else, and `--no-network` cuts them off.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crack-sbx-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Git Bash, as on the CI runner; PowerShell otherwise.
fn shell(command: &str) -> Vec<String> {
    let bash = Path::new("C:\\Program Files\\Git\\bin\\bash.exe");
    if bash.is_file() {
        vec![bash.display().to_string(), "-c".into(), command.into()]
    } else {
        vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            command.into(),
        ]
    }
}

/// Runs `program` sandboxed with `writable` shared; its exit code and output.
fn sandboxed(writable: &Path, network: bool, program: &[String]) -> (i32, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crack"));
    command.arg("__sandbox-exec").arg("--write").arg(writable);
    if !network {
        command.arg("--no-network");
    }
    let output = command
        .arg("--")
        .args(program)
        .current_dir(writable)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    (output.status.code().unwrap_or(-1), text)
}

fn slashes(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[test]
fn writes_stay_in_the_shared_folder() {
    let project = scratch("project");
    let inside = project.join("made-inside.txt");
    let (code, output) = sandboxed(
        &project,
        true,
        &shell(&format!("echo ok > \"{}\"", slashes(&inside))),
    );
    assert_eq!(code, 0, "writing inside failed: {output}");
    assert!(inside.exists(), "{output}");

    // Files that were there before the folder was shared are writable too.
    let fresh = scratch("existing");
    std::fs::write(fresh.join("old.txt"), "old").unwrap();
    std::fs::create_dir_all(fresh.join("deep")).unwrap();
    std::fs::write(fresh.join("deep/old.txt"), "old").unwrap();
    let (code, output) = sandboxed(
        &fresh,
        true,
        &shell(&format!(
            "echo new > \"{0}/old.txt\"; echo new > \"{0}/deep/old.txt\"",
            slashes(&fresh)
        )),
    );
    assert_eq!(code, 0, "rewriting existing files failed: {output}");
    assert_eq!(
        std::fs::read_to_string(fresh.join("deep/old.txt"))
            .unwrap()
            .trim(),
        "new"
    );
    let _ = std::fs::remove_dir_all(&fresh);

    // Not into the user's profile, nor another folder in temp.
    let home = std::env::var("USERPROFILE").unwrap();
    let outside = PathBuf::from(home).join(format!(".crack-sbx-outside-{}", std::process::id()));
    let (code, output) = sandboxed(
        &project,
        true,
        &shell(&format!("echo no > \"{}\"", slashes(&outside))),
    );
    let escaped = outside.exists();
    let _ = std::fs::remove_file(&outside);
    assert!(!escaped, "wrote outside the sandbox: {output}");
    assert_ne!(code, 0, "{output}");

    let other = scratch("other");
    let (_, output) = sandboxed(
        &project,
        true,
        &shell(&format!("echo no > \"{}/x.txt\"", slashes(&other))),
    );
    assert!(!other.join("x.txt").exists(), "{output}");

    let _ = std::fs::remove_dir_all(&project);
    let _ = std::fs::remove_dir_all(&other);
}

#[test]
fn no_network_blocks_connections() {
    let project = scratch("network");
    let curl = PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32\\curl.exe");
    let curl = curl.display().to_string();
    // curl itself runs in the sandbox...
    let (code, output) = sandboxed(&project, false, &[curl.clone(), "--version".into()]);
    assert_eq!(code, 0, "curl didn't run in the sandbox: {output}");
    // ...but can't connect without the network (by address, so DNS isn't the test).
    let fetch = [
        curl.clone(),
        "-sS".into(),
        "-m".into(),
        "15".into(),
        "-o".into(),
        "NUL".into(),
        "https://1.1.1.1".into(),
    ];
    let (code, output) = sandboxed(&project, false, &fetch);
    assert_ne!(code, 0, "connected with the network blocked: {output}");
    // With the network allowed, it connects.
    let (code, output) = sandboxed(&project, true, &fetch);
    assert_eq!(
        code, 0,
        "couldn't connect with the network allowed: {output}"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn exit_codes_pass_through() {
    let project = scratch("exit");
    let (code, output) = sandboxed(&project, true, &shell("exit 7"));
    assert_eq!(code, 7, "{output}");
    let _ = std::fs::remove_dir_all(&project);
}
