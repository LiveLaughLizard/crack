//! The Windows sandbox, through the real helper (`crack __sandbox-exec`): commands
//! write inside the shared folder and nowhere else, Git Bash runs in it (beside an
//! open one too), and `--no-network` is refused because Windows can't honour it.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crack-sbx-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const GIT_BASH: &str = "C:\\Program Files\\Git\\bin\\bash.exe";

/// Git Bash, as on the CI runner; PowerShell otherwise.
fn shell(command: &str) -> Vec<String> {
    if Path::new(GIT_BASH).is_file() {
        vec![GIT_BASH.into(), "-c".into(), command.into()]
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
    // Readable in a CI annotation.
    let text = text.replace(|c: char| c.is_control() && c != '\n', "");
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

/// Ordinary commands work: other programs, pipes, `NUL`, temporary files.
#[test]
fn ordinary_commands_work() {
    let project = scratch("ordinary");
    std::fs::write(project.join("a.txt"), "hello\n").unwrap();
    let (code, output) = sandboxed(
        &project,
        true,
        &shell(
            "ls | wc -l; cat a.txt; echo x > /dev/null; echo y > NUL; t=$(mktemp) && echo tmp > \"$t\" && cat \"$t\" && rm \"$t\"",
        ),
    );
    assert_eq!(code, 0, "{output}");
    assert!(
        output.contains("hello") && output.contains("tmp"),
        "{output}"
    );
    let (code, output) = sandboxed(
        &project,
        true,
        &[
            "cmd".into(),
            "/c".into(),
            "echo hi> NUL && echo cmd-ok".into(),
        ],
    );
    assert_eq!(code, 0, "{output}");
    assert!(output.contains("cmd-ok"), "{output}");
    let _ = std::fs::remove_dir_all(&project);
}

/// A Git Bash the user has open (not sandboxed, started first) doesn't stop one
/// starting in the sandbox, nor do two sandboxed ones at once.
#[test]
fn git_bash_starts_beside_others() {
    if !Path::new(GIT_BASH).is_file() {
        return;
    }
    let project = scratch("beside");
    let mut open = Command::new(GIT_BASH)
        .args(["-c", "sleep 20"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (code, output) = sandboxed(&project, true, &shell("ls | wc -l; echo first"));
    let other = project.clone();
    let second = std::thread::spawn(move || {
        sandboxed(&other, true, &shell("sleep 2; ls | wc -l; echo second"))
    });
    let (code3, output3) = sandboxed(&project, true, &shell("sleep 1; ls | wc -l; echo third"));
    let (code2, output2) = second.join().unwrap();
    let _ = open.kill();
    let _ = open.wait();
    assert_eq!(code, 0, "beside an open Git Bash: {output}");
    assert_eq!(code2, 0, "two at once: {output2}");
    assert_eq!(code3, 0, "two at once: {output3}");
    let _ = std::fs::remove_dir_all(&project);
}

/// Windows has no admin-free way to cut a process off the network, so the helper
/// refuses `--no-network` rather than run the command connected (crack asks before
/// commands instead when sandbox network is blocked). With the network allowed,
/// commands connect.
#[test]
fn no_network_is_refused_not_ignored() {
    let project = scratch("network");
    let marker = project.join("ran.txt");
    let (code, output) = sandboxed(
        &project,
        false,
        &shell(&format!("echo ran > \"{}\"", slashes(&marker))),
    );
    assert_ne!(code, 0, "{output}");
    assert!(!marker.exists(), "ran with --no-network: {output}");
    assert!(output.contains("can't block"), "{output}");

    let curl = PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32\\curl.exe");
    let fetch = [
        curl.display().to_string(),
        "-sS".into(),
        "-m".into(),
        "15".into(),
        "-o".into(),
        "NUL".into(),
        "https://1.1.1.1".into(),
    ];
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
