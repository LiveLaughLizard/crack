//! The Windows sandbox: commands run in an AppContainer, the isolation Windows gives
//! store apps and browser tabs. It needs no install and no administrator rights.
//!
//! - files: an AppContainer can read only what is shared with it. Windows and
//!   Program Files already are; the helper also shares (read-only) the folders of
//!   tools installed in the user's profile that are on PATH, so `cargo` or a per-user
//!   Git still run. The rest of the profile (keys, browser data, documents) can't be
//!   read. The project and listed caches are shared read-write: the container gets
//!   an inheritable "modify" entry on each, and the folder is labelled low integrity
//!   (AppContainers run at low integrity, and Windows stops those writing to normal
//!   folders). Both stay on the folders, so it's only slow the first time.
//! - temporary files go to the container's own folder (`TEMP` and `TMP` point there);
//! - network: allowed through the internetClient capabilities, or with `--no-network`
//!   none at all. AppContainers can't reach localhost either way.
//!
//! The command runs in a job object that kills it if the helper is killed. When it
//! finishes, anything it left running in the background is let go, as on Linux.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, LocalFree, SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSidToSidW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, FreeSid, LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// The AppContainer profile crack's commands run in.
const PROFILE: &str = "crack.sandbox";

/// internetClient, internetClientServer and privateNetworkClientServer.
const NETWORK_CAPABILITIES: [&str; 3] = ["S-1-15-3-1", "S-1-15-3-2", "S-1-15-3-3"];

/// SE_GROUP_ENABLED, for capability SIDs.
const ENABLED: u32 = 4;

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string.
unsafe fn from_wide(ptr: *const u16) -> String {
    let mut len = 0;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// A SID from the AppContainer functions (freed with FreeSid).
struct ContainerSid(PSID);

impl Drop for ContainerSid {
    fn drop(&mut self) {
        unsafe { FreeSid(self.0) };
    }
}

/// A handle closed when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// The container's SID, creating its profile the first time.
fn container_sid() -> io::Result<ContainerSid> {
    const ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;
    let name = wide(PROFILE);
    let description = wide("Shell commands run by crack");
    let mut sid: PSID = null_mut();
    let mut result = unsafe {
        CreateAppContainerProfile(
            name.as_ptr(),
            name.as_ptr(),
            description.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if result == ALREADY_EXISTS {
        result = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
    }
    if result < 0 || sid.is_null() {
        return Err(io::Error::other(format!(
            "couldn't create the AppContainer profile (error {result:#010x})"
        )));
    }
    Ok(ContainerSid(sid))
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe { from_wide(text) };
    unsafe { LocalFree(text.cast()) };
    Ok(result)
}

/// A file's DACL and integrity label in SDDL, e.g. `D:(A;OICI;FA;;;SY)S:(ML;;NW;;;LW)`.
fn sddl(path: &Path) -> io::Result<String> {
    let name = wide(path);
    let info = DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let error = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            info,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let mut text = null_mut();
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            info,
            &mut text,
            null_mut(),
        )
    };
    let result = if converted == 0 {
        Err(io::Error::last_os_error())
    } else {
        let text_string = unsafe { from_wide(text) };
        unsafe { LocalFree(text.cast()) };
        Ok(text_string)
    };
    unsafe { LocalFree(descriptor) };
    result
}

/// File access rights in an SDDL ACE: hex, or the FA/FR/FW/FX/GA/GR/GW/GX letters.
fn rights(field: &str) -> u32 {
    if let Some(hex) = field
        .strip_prefix("0x")
        .or_else(|| field.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16).unwrap_or(0);
    }
    let mut mask = 0;
    for pair in field.as_bytes().chunks(2) {
        mask |= match pair {
            b"FA" | b"GA" => 0x001F_01FF,
            b"FR" | b"GR" => 0x0012_0089,
            b"FW" | b"GW" => 0x0012_0116,
            b"FX" | b"GX" => 0x0012_00A0,
            b"SD" => 0x0001_0000,
            b"RC" => 0x0002_0000,
            b"WD" => 0x0004_0000,
            b"WO" => 0x0008_0000,
            _ => 0,
        };
    }
    mask
}

/// Read and execute (icacls `RX`).
const READ: u32 = 0x0012_00A9;
/// Modify (icacls `M`): read, write, execute and delete.
const MODIFY: u32 = 0x0013_01BF;

/// The SDDL already lets `sid` (or one of `also`) do `want` here and in everything
/// below, and, when `low` is set, carries an inheritable low-integrity label.
fn granted(sddl: &str, sids: &[&str], want: u32, low: bool) -> bool {
    let aces: Vec<Vec<&str>> = sddl
        .split('(')
        .skip(1)
        .map(|ace| ace.trim_end_matches(|c| c != ')').trim_end_matches(')'))
        .map(|ace| ace.split(';').collect())
        .filter(|fields: &Vec<&str>| fields.len() >= 6)
        .collect();
    let inherits = |flags: &str| flags.contains("OI") && flags.contains("CI");
    let allowed = aces.iter().any(|f| {
        f[0] == "A" && inherits(f[1]) && sids.contains(&f[5]) && rights(f[2]) & want == want
    });
    let labelled = !low
        || aces
            .iter()
            .any(|f| f[0] == "ML" && inherits(f[1]) && f[2].contains("NW") && f[5] == "LW");
    allowed && labelled
}

/// Runs icacls (in System32, whatever PATH says).
fn icacls(args: &[&OsStr]) -> io::Result<()> {
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let output = std::process::Command::new(PathBuf::from(system).join("System32\\icacls.exe"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
                + String::from_utf8_lossy(&output.stderr).trim(),
        ))
    }
}

/// Shares `path` (and everything in it) with the container: read-only, or writable.
fn share(path: &Path, sid: &str, writable: bool) -> io::Result<()> {
    let want = if writable { MODIFY } else { READ };
    // Program Files and Windows are shared with every AppContainer ("AC").
    if let Ok(current) = sddl(path)
        && granted(&current, &[sid, "AC"], want, writable)
    {
        return Ok(());
    }
    let inherit = if path.is_dir() { "(OI)(CI)" } else { "" };
    let grant = format!("*{sid}:{inherit}{}", if writable { "M" } else { "RX" });
    icacls(&[
        path.as_os_str(),
        "/grant".as_ref(),
        grant.as_ref(),
        "/Q".as_ref(),
    ])?;
    if writable {
        let label = format!("{inherit}L");
        icacls(&[
            path.as_os_str(),
            "/setintegritylevel".as_ref(),
            label.as_ref(),
            "/Q".as_ref(),
        ])?;
    }
    Ok(())
}

/// Folders in the user's profile a command needs to read to run its tools: the
/// install folder of the program and of each PATH entry there (`~/.cargo` for
/// `~/.cargo/bin`), and rustup's toolchains. Folders elsewhere are left alone.
fn tool_folders(program: &str) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(parent) = Path::new(program).parent() {
        dirs.push(parent.to_owned());
    }
    let mut folders: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let mut folder = dir;
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase());
        if matches!(name.as_deref(), Some("bin" | "shims"))
            && let Some(parent) = folder.parent()
        {
            folder = parent.to_owned();
        }
        let Ok(inside) = folder.strip_prefix(&home) else {
            continue;
        };
        let parts: Vec<String> = inside
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect();
        // Never the whole profile or AppData: only a tool's own folder.
        let broad = match parts.as_slice() {
            [] => true,
            [one] => !one.starts_with('.'),
            [first, _] => first == "appdata",
            _ => false,
        };
        if !broad && folder.is_dir() && !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    let rustup = home.join(".rustup");
    if rustup.is_dir() && !folders.contains(&rustup) {
        folders.push(rustup);
    }
    folders
}

/// The container's own folder (writable by it), for temporary files.
fn container_temp(sid: &str) -> Option<PathBuf> {
    let sid = wide(sid);
    let mut path = null_mut();
    if unsafe { GetAppContainerFolderPath(sid.as_ptr(), &mut path) } < 0 || path.is_null() {
        return None;
    }
    // Allocated with CoTaskMemAlloc; the helper exits soon, so it's left.
    let temp = PathBuf::from(unsafe { from_wide(path) }).join("Temp");
    std::fs::create_dir_all(&temp).ok()?;
    Some(temp)
}

/// One argument quoted for a Windows command line (the rules CommandLineToArgvW
/// and the C runtime use).
pub(crate) fn quote(arg: &str, line: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        let escaped = if c == '"' {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        line.extend(std::iter::repeat_n('\\', escaped));
        backslashes = 0;
        line.push(c);
    }
    line.extend(std::iter::repeat_n('\\', backslashes * 2));
    line.push('"');
}

/// This process's environment with TEMP and TMP replaced, as a CreateProcess block.
fn environment(temp: Option<&Path>) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    if let Some(temp) = temp {
        vars.retain(|(k, _)| !k.eq_ignore_ascii_case("TEMP") && !k.eq_ignore_ascii_case("TMP"));
        let temp = temp.display().to_string();
        vars.push(("TEMP".into(), temp.clone()));
        vars.push(("TMP".into(), temp));
    }
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// Runs `program` in the container and waits for it: write access only to
/// `writable`, network only if `network`. Returns its exit code.
pub fn run(writable: &[PathBuf], network: bool, program: &[String]) -> io::Result<u32> {
    let container = container_sid()?;
    let sid = sid_string(container.0)?;
    for path in writable.iter().filter(|p| p.exists()) {
        share(path, &sid, true).map_err(|error| {
            io::Error::other(format!(
                "couldn't make {} writable: {error}",
                path.display()
            ))
        })?;
    }
    // Best effort: a tool it can't read fails on its own, with a clear error.
    for folder in tool_folders(&program[0]) {
        let _ = share(&folder, &sid, false);
    }
    let temp = container_temp(&sid);

    // Network capabilities, when allowed.
    let mut capabilities: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    if network {
        for text in NETWORK_CAPABILITIES {
            let mut capability: PSID = null_mut();
            let text = wide(text);
            if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut capability) } == 0 {
                return Err(io::Error::last_os_error());
            }
            capabilities.push(SID_AND_ATTRIBUTES {
                Sid: capability,
                Attributes: ENABLED,
            });
        }
    }
    let result = launch(&container, &mut capabilities, temp.as_deref(), program);
    for capability in capabilities {
        unsafe { LocalFree(capability.Sid) };
    }
    result
}

fn launch(
    container: &ContainerSid,
    capabilities: &mut [SID_AND_ATTRIBUTES],
    temp: Option<&Path>,
    program: &[String],
) -> io::Result<u32> {
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: container.0,
        Capabilities: if capabilities.is_empty() {
            null_mut()
        } else {
            capabilities.as_mut_ptr()
        },
        CapabilityCount: capabilities.len() as u32,
        Reserved: 0,
    };

    // The attribute list that puts the process in the container.
    let mut size = 0;
    unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
    let mut buffer = vec![0u64; size.div_ceil(8)];
    let list: LPPROC_THREAD_ATTRIBUTE_LIST = buffer.as_mut_ptr().cast();
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    struct List(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for List {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _list = List(list);
    if unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            (&security as *const SECURITY_CAPABILITIES).cast(),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    // The command shares the helper's stdin, stdout and stderr.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    unsafe {
        startup.StartupInfo.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
        startup.StartupInfo.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
        startup.StartupInfo.hStdError = GetStdHandle(STD_ERROR_HANDLE);
        for handle in [
            startup.StartupInfo.hStdInput,
            startup.StartupInfo.hStdOutput,
            startup.StartupInfo.hStdError,
        ] {
            if !handle.is_null() {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
    }
    startup.lpAttributeList = list;

    let mut line = String::new();
    for (i, arg) in program.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        quote(arg, &mut line);
    }
    let mut line = wide(&line);
    let environment = environment(temp);

    // A job that kills the command if the helper is killed.
    let job = Handle(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set_limits = |limits: &JOBOBJECT_EXTENDED_LIMIT_INFORMATION| unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set_limits(&limits) == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessW(
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            null(),
            (&startup as *const STARTUPINFOEXW).cast(),
            &mut info,
        )
    };
    if created == 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::other(format!(
            "couldn't start {}: {error}",
            program[0]
        )));
    }
    let process = Handle(info.hProcess);
    let thread = Handle(info.hThread);
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        let error = io::Error::last_os_error();
        unsafe { TerminateProcess(process.0, 1) };
        return Err(error);
    }
    unsafe { ResumeThread(thread.0) };
    drop(thread);
    unsafe { WaitForSingleObject(process.0, INFINITE) };
    let mut code = 1;
    unsafe { GetExitCodeProcess(process.0, &mut code) };
    // Finished: whatever it started in the background keeps running (still in the
    // container), as it would on Linux.
    limits.BasicLimitInformation.LimitFlags = 0;
    set_limits(&limits);
    Ok(code)
}

/// The sandbox works here: a command in it can write to a shared folder and not to
/// another one. Checked once, through the real helper, so a Windows where
/// AppContainers or the shell don't work falls back to asking before commands.
pub fn available() -> bool {
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WORKS.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("crack-sandbox-check-{}", std::process::id()));
        let works = probe(&dir).unwrap_or(false);
        let _ = std::fs::remove_dir_all(&dir);
        works
    })
}

fn probe(dir: &Path) -> io::Result<bool> {
    let inside = dir.join("inside");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&inside)?;
    std::fs::create_dir_all(&outside)?;
    // Forward slashes and double quotes: the same in Git Bash and PowerShell.
    let path = |p: PathBuf| p.display().to_string().replace('\\', "/");
    let command = format!(
        "echo ok > \"{}\"; echo no > \"{}\"",
        path(inside.join("ok")),
        path(outside.join("no"))
    );
    let Some(argv) = super::command(&command, std::slice::from_ref(&inside), &[], false) else {
        return Ok(false);
    };
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&inside)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(inside.join("ok").exists() && !outside.join("no").exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_like_the_c_runtime_reads_them() {
        let line = |args: &[&str]| {
            let mut line = String::new();
            for arg in args {
                quote(arg, &mut line);
                line.push(' ');
            }
            line.trim_end().to_owned()
        };
        assert_eq!(line(&["bash", "-c", "ls"]), "bash -c ls");
        assert_eq!(line(&["echo a b"]), "\"echo a b\"");
        assert_eq!(line(&[""]), "\"\"");
        assert_eq!(line(&["say \"hi\""]), "\"say \\\"hi\\\"\"");
        assert_eq!(
            line(&["C:\\dir with space\\"]),
            "\"C:\\dir with space\\\\\""
        );
    }

    #[test]
    fn grants_are_read_from_sddl() {
        let sid = "S-1-15-2-1";
        let sddl = format!("D:(A;OICI;0x1301bf;;;{sid})(A;OICI;FA;;;SY)S:(ML;OICI;NW;;;LW)");
        assert!(granted(&sddl, &[sid], MODIFY, true));
        assert!(granted(&sddl, &[sid], READ, false));
        // Read-only, or not inherited, or no low label: not enough to write.
        assert!(!granted(
            &format!("D:(A;OICI;0x1200a9;;;{sid})S:(ML;OICI;NW;;;LW)"),
            &[sid],
            MODIFY,
            true
        ));
        assert!(!granted(
            &format!("D:(A;;0x1301bf;;;{sid})S:(ML;OICI;NW;;;LW)"),
            &[sid],
            MODIFY,
            true
        ));
        assert!(!granted(
            &format!("D:(A;OICI;0x1301bf;;;{sid})"),
            &[sid],
            MODIFY,
            true
        ));
        // Inherited from a shared parent counts; so does "all app packages".
        assert!(granted(
            &format!("D:(A;OICIID;FA;;;{sid})S:(ML;OICIID;NW;;;LW)"),
            &[sid],
            MODIFY,
            true
        ));
        assert!(granted("D:(A;OICIIO;FRFX;;;AC)", &[sid, "AC"], READ, false));
    }

    /// The real thing: the sandbox is available on the CI runner.
    #[test]
    fn container_profile_can_be_created() {
        let sid = container_sid().unwrap();
        assert!(sid_string(sid.0).unwrap().starts_with("S-1-15-2-"));
    }
}
