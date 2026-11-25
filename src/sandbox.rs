//! The built-in sandbox, with nothing to install: Landlock for files and a network
//! namespace for the network (Linux 5.13+).
//!
//! crack runs itself as a helper, `crack __sandbox-exec [options] -- bash -c <cmd>`.
//! The helper is a fresh single-threaded process, so it can safely lock itself down
//! (no threads to deadlock after fork) before replacing itself with the command:
//!
//! - files: everything readable except hidden paths (secrets), and only the project,
//!   /tmp, /dev and listed caches writable;
//! - network: with `--no-network`, a new network namespace with nothing in it.
//!
//! Landlock only grants access, so "all but ~/.ssh" is built by walking from `/`
//! toward each hidden path and granting every sibling on the way. The directories on
//! the way get listing rights only, so a hidden file's name may show, never its contents.

use std::path::{Path, PathBuf};

/// The argument that makes crack act as the sandbox helper.
pub const HELPER: &str = "__sandbox-exec";

/// Landlock works on this kernel.
pub fn native_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FOUND.get_or_init(|| {
            // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION) returns the ABI version.
            let version = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<u8>(),
                    0usize,
                    1u32,
                )
            };
            version >= 1
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The argv that runs `command` in the built-in sandbox.
pub fn command(
    command: &str,
    writable: &[PathBuf],
    hidden: &[PathBuf],
    network: bool,
) -> Option<Vec<String>> {
    let exe = std::env::current_exe().ok()?;
    let mut argv = vec![exe.display().to_string(), HELPER.to_owned()];
    for path in writable {
        argv.extend(["--write".into(), path.display().to_string()]);
    }
    for path in hidden {
        argv.extend(["--hide".into(), path.display().to_string()]);
    }
    if !network {
        argv.push("--no-network".into());
    }
    argv.extend(["--".into(), "bash".into(), "-c".into(), command.to_owned()]);
    Some(argv)
}

/// The helper: lock down, then become the command. Never returns.
pub fn run_helper(args: &[String]) -> ! {
    let mut writable = Vec::new();
    let mut hidden = Vec::new();
    let mut network = true;
    let mut rest = args.iter();
    let mut program: Vec<String> = Vec::new();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--write" => writable.extend(rest.next().map(PathBuf::from)),
            "--hide" => hidden.extend(rest.next().map(PathBuf::from)),
            "--no-network" => network = false,
            "--" => {
                program = rest.cloned().collect();
                break;
            }
            other => fail(&format!("unknown option {other}")),
        }
    }
    if program.is_empty() {
        fail("no command given");
    }
    if !network && let Err(error) = isolate_network() {
        fail(&format!(
            "couldn't block the network ({error}); turn sandbox network on, or the sandbox off"
        ));
    }
    if let Err(error) = restrict_files(&writable, &hidden) {
        fail(&format!("couldn't restrict file access: {error}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(&program[0])
            .args(&program[1..])
            .exec();
        fail(&format!("couldn't run {}: {error}", program[0]));
    }
    #[cfg(not(unix))]
    fail("the sandbox only runs on Linux");
}

fn fail(message: &str) -> ! {
    eprintln!("crack sandbox: {message}");
    std::process::exit(126);
}

/// A new user and network namespace: no interfaces but a downed loopback.
#[cfg(target_os = "linux")]
fn isolate_network() -> std::io::Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Keep our own uid and gid inside the namespace, so file ownership looks normal.
    std::fs::write("/proc/self/setgroups", "deny")?;
    std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
    std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn isolate_network() -> std::io::Result<()> {
    Err(std::io::Error::other("only on Linux"))
}

#[cfg(target_os = "linux")]
fn restrict_files(writable: &[PathBuf], hidden: &[PathBuf]) -> anyhow::Result<()> {
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };
    let abi = ABI::V5;
    let read = AccessFs::from_read(abi);
    let mut readable = Vec::new();
    let mut listable = Vec::new();
    grant_reads(Path::new("/"), hidden, &mut readable, &mut listable);

    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(&readable, read))?
        .add_rules(path_beneath_rules(&listable, AccessFs::ReadDir))?
        .add_rules(path_beneath_rules(writable, AccessFs::from_all(abi)))?
        .restrict_self()?;
    if status.ruleset == RulesetStatus::NotEnforced {
        anyhow::bail!("Landlock isn't enforced on this kernel");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn restrict_files(_: &[PathBuf], _: &[PathBuf]) -> anyhow::Result<()> {
    anyhow::bail!("only on Linux")
}

/// Grants read on everything under `dir` except `hidden` paths: children with a
/// hidden path inside are only listable, and are walked into.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn grant_reads(
    dir: &Path,
    hidden: &[PathBuf],
    readable: &mut Vec<PathBuf>,
    listable: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if hidden.contains(&path) {
            continue;
        }
        if hidden.iter().any(|h| h.starts_with(&path)) {
            listable.push(path.clone());
            grant_reads(&path, hidden, readable, listable);
        } else {
            readable.push(path);
        }
    }
    listable.push(dir.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_paths_are_walked_around() {
        let root = std::env::temp_dir().join(format!("crack-sandbox-{}", uuid::Uuid::new_v4()));
        for dir in ["a/secret", "a/open", "b"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let hidden = vec![root.join("a/secret")];
        let (mut readable, mut listable) = (Vec::new(), Vec::new());
        grant_reads(&root, &hidden, &mut readable, &mut listable);
        assert!(readable.contains(&root.join("a/open")));
        assert!(readable.contains(&root.join("b")));
        assert!(
            !readable
                .iter()
                .any(|p| p.starts_with(root.join("a/secret")))
        );
        assert!(listable.contains(&root.join("a")));
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// macOS: `sandbox-exec` (Seatbelt) is there to use.
pub fn seatbelt_available() -> bool {
    cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists()
}

/// A Seatbelt profile: writes only in `writable`, no reads in `hidden`, and no IP
/// networking unless `network` (local Unix sockets still work).
pub fn seatbelt_profile(writable: &[PathBuf], hidden: &[PathBuf], network: bool) -> String {
    // Seatbelt matches real paths (/tmp is /private/tmp on macOS).
    let real = |p: &PathBuf| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
    let quote = |p: &Path| {
        format!(
            "\"{}\"",
            p.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        )
    };
    let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    let mut allowed: Vec<String> = writable
        .iter()
        .map(|p| format!("(subpath {})", quote(&real(p))))
        .collect();
    allowed.push("(literal \"/dev/null\")".into());
    allowed.push("(literal \"/dev/tty\")".into());
    allowed.push("(literal \"/dev/dtracehelper\")".into());
    allowed.push("(regex #\"^/dev/fd/\")".into());
    profile.push_str(&format!("(allow file-write* {})\n", allowed.join(" ")));
    let denied: Vec<String> = hidden
        .iter()
        .filter(|p| p.exists())
        .map(|p| format!("(subpath {})", quote(&real(p))))
        .collect();
    if !denied.is_empty() {
        profile.push_str(&format!("(deny file-read* {})\n", denied.join(" ")));
    }
    if !network {
        profile
            .push_str("(deny network-outbound (remote ip))\n(deny network-inbound (local ip))\n");
    }
    profile
}

/// The argv that runs `command` under Seatbelt on macOS.
pub fn seatbelt_command(
    command: &str,
    writable: &[PathBuf],
    hidden: &[PathBuf],
    network: bool,
) -> Vec<String> {
    vec![
        "/usr/bin/sandbox-exec".into(),
        "-p".into(),
        seatbelt_profile(writable, hidden, network),
        "bash".into(),
        "-c".into(),
        command.to_owned(),
    ]
}

#[cfg(test)]
mod seatbelt_tests {
    use super::*;

    #[test]
    fn profile_limits_writes_reads_and_network() {
        let profile = seatbelt_profile(
            &[PathBuf::from("/work/project")],
            &[std::env::temp_dir()],
            false,
        );
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(subpath \"/work/project\")"));
        assert!(profile.contains("(deny file-read*"));
        assert!(profile.contains("(deny network-outbound (remote ip))"));
        assert!(!seatbelt_profile(&[], &[], true).contains("network"));
    }

    /// The real thing, on macOS (CI): writes inside the project work, outside don't.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_blocks_writes_outside_the_project() {
        let root = std::env::temp_dir().join(format!("crack-sb-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let outside = dirs::home_dir()
            .unwrap()
            .join(format!(".crack-sb-test-{}", std::process::id()));
        let run = |command: &str| {
            let argv = seatbelt_command(command, &[root.clone()], &[], false);
            std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .status()
                .unwrap()
                .success()
        };
        assert!(run(&format!("echo ok > {}/inside", root.display())));
        assert!(!run(&format!("echo no > {}", outside.display())));
        assert!(!outside.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
