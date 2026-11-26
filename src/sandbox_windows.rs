//! The Windows sandbox: commands run as the user, but at low integrity, the level
//! browsers once gave their tabs. It needs no install and no administrator rights.
//!
//! Windows doesn't let a low-integrity process write anything labelled higher, and
//! everything is (files, folders and the registry are medium unless labelled
//! otherwise). Reading is unchanged.
//!
//! - files: everything the user can read stays readable, secrets included (Windows
//!   has no admin-free way to hide them without also hiding the tools in the
//!   profile). The project and listed caches are writable: each gets an inheritable
//!   low-integrity label, which stays on the folder, so it's only slow the first time.
//! - temporary files go to a shared `crack-sandbox` folder in the user's temp folder
//!   (`TEMP`, `TMP` and `TMPDIR` point there).
//! - network: Windows has no way to cut a process off the network without
//!   administrator rights, so `--no-network` is refused; crack asks before commands
//!   instead when sandbox network is blocked.
//! - privileges are dropped (only bypass-traverse is kept) and an administrator's
//!   Administrators group is for denying only (what the command creates belongs to
//!   the user, not to Administrators).
//! - Git Bash: MSYS2 keeps per-user state in shared memory that every Git Bash of
//!   the user opens. Made by a Git Bash the user has open, it's medium integrity, so
//!   a sandboxed one couldn't open it and wouldn't start. The helper relabels those
//!   objects low first. That means sandboxed commands share it with the user's Git
//!   Bash windows (it holds MSYS2's mount table, among other things).
//!
//! The command runs in a job object that kills it if the helper is killed. When it
//! finishes, anything it left running in the background is let go, as on Linux.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{NtOpenDirectoryObject, NtQueryDirectoryObject};
use windows_sys::Wdk::System::Memory::NtOpenSection;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, HANDLE, HANDLE_FLAG_INHERIT, LocalFree, OBJ_CASE_INSENSITIVE,
    SetHandleInformation, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE,
    SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SetEntriesInAclW, SetSecurityInfo,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, CreateRestrictedToken, DACL_SECURITY_INFORMATION,
    DISABLE_MAX_PRIVILEGE, GetLengthSid, GetSecurityDescriptorSacl, GetTokenInformation,
    InitializeAcl, LABEL_SECURITY_INFORMATION, LUA_TOKEN, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenDefaultDacl, TokenIntegrityLevel,
    TokenSessionId, TokenUser,
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
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, OpenProcessToken, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

const LOW_INTEGRITY: &str = "S-1-16-4096";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const SYSTEM: &str = "S-1-5-18";
const GENERIC_ALL: u32 = 0x1000_0000;
/// A low-integrity label that keeps lower processes from writing (none are lower).
const LOW_LABEL: &str = "S:(ML;;NW;;;LW)";

/// SE_GROUP_INTEGRITY.
const INTEGRITY: u32 = 0x20;
const READ_CONTROL: u32 = 0x0002_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
/// DIRECTORY_QUERY | DIRECTORY_TRAVERSE.
const DIRECTORY_LIST: u32 = 0x3;

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

/// A handle closed when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A SID from ConvertStringSidToSidW.
struct Sid(PSID);

impl Sid {
    fn parse(text: &str) -> io::Result<Sid> {
        let mut sid: PSID = null_mut();
        let text = wide(text);
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Sid(sid))
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

/// A token information class, in a buffer aligned for its struct.
fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut size = 0;
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut size) };
    let mut buffer = vec![0u64; (size as usize).div_ceil(8).max(1)];
    if unsafe { GetTokenInformation(token, class, buffer.as_mut_ptr().cast(), size, &mut size) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer)
}

/// This process's token.
fn own_token() -> io::Result<Handle> {
    let mut token: HANDLE = null_mut();
    let access = TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT;
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(token))
}

/// The token commands run with: this process's at low integrity, with privileges
/// dropped and the Administrators group (if any) only for denying.
fn sandbox_token(base: &Handle) -> io::Result<Handle> {
    let administrators = Sid::parse(ADMINISTRATORS)?;
    let disable = [SID_AND_ATTRIBUTES {
        Sid: administrators.0,
        Attributes: 0,
    }];
    let mut token: HANDLE = null_mut();
    if unsafe {
        CreateRestrictedToken(
            base.0,
            DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
            disable.len() as u32,
            disable.as_ptr(),
            0,
            null(),
            0,
            null(),
            &mut token,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let token = Handle(token);
    let low = Sid::parse(LOW_INTEGRITY)?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: low.0,
            Attributes: INTEGRITY,
        },
    };
    if unsafe {
        SetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            (&label as *const TOKEN_MANDATORY_LABEL).cast(),
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(low.0),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // What the command creates (its processes, pipes, the token itself) belongs to
    // the user. An administrator's default is the Administrators group, which the
    // command can only be denied by: it couldn't open its own pipes.
    let user_info = token_info(base.0, TokenUser)?;
    let user = unsafe { (*user_info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let system = Sid::parse(SYSTEM)?;
    let mut acl = vec![0u32; 128];
    let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
    if unsafe { InitializeAcl(acl_ptr, (acl.len() * 4) as u32, ACL_REVISION) } == 0
        || unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, user) } == 0
        || unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, GENERIC_ALL, system.0) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let dacl = TOKEN_DEFAULT_DACL {
        DefaultDacl: acl_ptr,
    };
    if unsafe {
        SetTokenInformation(
            token.0,
            TokenDefaultDacl,
            (&dacl as *const TOKEN_DEFAULT_DACL).cast(),
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    // The token object gets those permissions and a low label too, so the command
    // can adjust its own token (Git Bash sets its default permissions at startup).
    let mut owner: HANDLE = null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            token.0,
            GetCurrentProcess(),
            &mut owner,
            WRITE_OWNER | WRITE_DAC,
            0,
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let owner = Handle(owner);
    let error = match LowLabel::new() {
        Some(low) => unsafe {
            SetSecurityInfo(
                owner.0,
                SE_KERNEL_OBJECT,
                LABEL_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl_ptr,
                low.acl,
            )
        },
        None => 1,
    };
    if error != 0 {
        return Err(io::Error::other(format!(
            "couldn't label the token low (error {error})"
        )));
    }
    Ok(token)
}

/// The user's SID as text, and the logon session's number.
fn user_and_session(token: &Handle) -> io::Result<(String, u32)> {
    let info = token_info(token.0, TokenUser)?;
    let sid = unsafe { (*info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = unsafe { from_wide(text) };
    unsafe { LocalFree(text.cast()) };
    let session = token_info(token.0, TokenSessionId)?[0] as u32;
    Ok((user, session))
}

/// What NtQueryDirectoryObject returns for each entry.
#[repr(C)]
struct DirectoryEntry {
    name: UNICODE_STRING,
    kind: UNICODE_STRING,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Directory,
    Section,
    Mutant,
    Event,
    Semaphore,
}

impl Kind {
    /// A kind of object Git Bash shares, by the type name the object manager uses.
    fn shared(name: &str) -> Option<Kind> {
        match name {
            "Section" => Some(Kind::Section),
            "Mutant" => Some(Kind::Mutant),
            "Event" => Some(Kind::Event),
            "Semaphore" => Some(Kind::Semaphore),
            _ => None,
        }
    }
}

type NtOpen = unsafe extern "system" fn(*mut HANDLE, u32, *const OBJECT_ATTRIBUTES) -> i32;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtOpenMutant(handle: *mut HANDLE, access: u32, attributes: *const OBJECT_ATTRIBUTES) -> i32;
    fn NtOpenEvent(handle: *mut HANDLE, access: u32, attributes: *const OBJECT_ATTRIBUTES) -> i32;
    fn NtOpenSemaphore(
        handle: *mut HANDLE,
        access: u32,
        attributes: *const OBJECT_ATTRIBUTES,
    ) -> i32;
}

/// Opens a kernel object by name (relative to `root`, if given).
fn open_object(root: HANDLE, name: &str, kind: Kind, access: u32) -> Option<Handle> {
    let mut text: Vec<u16> = OsStr::new(name).encode_wide().collect();
    let name = UNICODE_STRING {
        Length: (text.len() * 2) as u16,
        MaximumLength: (text.len() * 2) as u16,
        Buffer: text.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root,
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut handle: HANDLE = null_mut();
    let open: NtOpen = match kind {
        Kind::Directory => NtOpenDirectoryObject,
        Kind::Section => NtOpenSection,
        Kind::Mutant => NtOpenMutant,
        Kind::Event => NtOpenEvent,
        Kind::Semaphore => NtOpenSemaphore,
    };
    let status = unsafe { open(&mut handle, access, &attributes) };
    (status >= 0 && !handle.is_null()).then_some(Handle(handle))
}

fn unicode(text: &UNICODE_STRING) -> String {
    if text.Buffer.is_null() {
        return String::new();
    }
    String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text.Buffer, text.Length as usize / 2)
    })
}

/// The names and types of the objects in an object directory.
fn list_directory(directory: &Handle) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut buffer = vec![0u64; 512];
    let mut context = 0u32;
    let mut first = true;
    loop {
        let mut length = 0;
        let status = unsafe {
            NtQueryDirectoryObject(
                directory.0,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                true,
                first,
                &mut context,
                &mut length,
            )
        };
        first = false;
        if status < 0 {
            break;
        }
        let entry = unsafe { &*buffer.as_ptr().cast::<DirectoryEntry>() };
        entries.push((unicode(&entry.name), unicode(&entry.kind)));
        if entries.len() > 100_000 {
            break;
        }
    }
    entries
}

/// Labels a kernel object low integrity, so low-integrity processes can open it to
/// write. Setting a label needs WRITE_OWNER; the owner can always change the
/// permissions, so if it isn't granted, it's added first.
fn label_low(root: HANDLE, name: &str, kind: Kind, label: *const ACL) -> bool {
    let object = open_object(root, name, kind, WRITE_OWNER).or_else(|| {
        let object = open_object(root, name, kind, READ_CONTROL | WRITE_DAC)?;
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe {
            GetSecurityInfo(
                object.0,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        } != 0
        {
            return None;
        }
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: WRITE_OWNER,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: owner.cast(),
            },
        };
        let mut updated: *mut ACL = null_mut();
        let mut done = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut updated) } == 0;
        if done {
            done = unsafe {
                SetSecurityInfo(
                    object.0,
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    updated,
                    null(),
                )
            } == 0;
            unsafe { LocalFree(updated.cast()) };
        }
        unsafe { LocalFree(descriptor) };
        if !done {
            return None;
        }
        open_object(root, name, kind, WRITE_OWNER)
    });
    let Some(object) = object else {
        return false;
    };
    let error = unsafe {
        SetSecurityInfo(
            object.0,
            SE_KERNEL_OBJECT,
            LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null(),
            label,
        )
    };
    error == 0
}

/// Lets sandboxed Git Bash (MSYS2, or Cygwin) start while the user has one open:
/// labels low the objects every one of them opens to write, where a Git Bash
/// running at normal integrity made them. Best effort.
fn share_msys_state(user: &str, session: u32) {
    let Some(low) = LowLabel::new() else {
        return;
    };
    let label = low.acl;
    {
        let own = format!("{user}.");
        for root in [
            "\\BaseNamedObjects".to_owned(),
            format!("\\Sessions\\{session}\\BaseNamedObjects"),
        ] {
            let Some(root) = open_object(null_mut(), &root, Kind::Directory, DIRECTORY_LIST) else {
                continue;
            };
            for (name, kind) in list_directory(&root) {
                let msys = name.starts_with("msys-") || name.starts_with("cygwin1");
                if kind != "Directory" || !msys {
                    continue;
                }
                label_low(root.0, &name, Kind::Directory, label);
                let Some(directory) = open_object(root.0, &name, Kind::Directory, DIRECTORY_LIST)
                else {
                    continue;
                };
                // Sections: the shared state, not other processes' own. Locks and
                // signals: all of them.
                for (object, kind) in list_directory(&directory) {
                    let Some(kind) = Kind::shared(&kind) else {
                        continue;
                    };
                    if kind != Kind::Section
                        || object.starts_with("shared.")
                        || object.starts_with(&own)
                    {
                        label_low(directory.0, &object, kind, label);
                    }
                }
            }
        }
    }
}

/// A low-integrity label, as a SACL for SetSecurityInfo.
struct LowLabel {
    descriptor: PSECURITY_DESCRIPTOR,
    acl: *mut ACL,
}

impl LowLabel {
    fn new() -> Option<LowLabel> {
        let sddl = wide(LOW_LABEL);
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return None;
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl: *mut ACL = null_mut();
        unsafe { GetSecurityDescriptorSacl(descriptor, &mut present, &mut acl, &mut defaulted) };
        let label = LowLabel { descriptor, acl };
        (!acl.is_null()).then_some(label)
    }
}

impl Drop for LowLabel {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor) };
    }
}

/// A file's integrity label in SDDL, e.g. `S:(ML;OICI;NW;;;LW)`.
fn sddl(path: &Path) -> io::Result<String> {
    let name = wide(path);
    let info = LABEL_SECURITY_INFORMATION;
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

/// The SDDL carries a low-integrity label that everything below inherits.
fn labelled_low(sddl: &str) -> bool {
    sddl.split('(')
        .skip(1)
        .map(|ace| ace.trim_end_matches(|c| c != ')').trim_end_matches(')'))
        .map(|ace| ace.split(';').collect::<Vec<_>>())
        .any(|f| {
            f.len() >= 6
                && f[0] == "ML"
                && f[1].contains("OI")
                && f[1].contains("CI")
                && f[2].contains("NW")
                && f[5] == "LW"
        })
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

/// Lets sandboxed commands write to `path` and everything in it (that the user can
/// write): labels it low integrity, inherited by everything in it.
fn share(path: &Path) -> io::Result<()> {
    if let Ok(current) = sddl(path)
        && labelled_low(&current)
    {
        return Ok(());
    }
    let label = if path.is_dir() { "(OI)(CI)L" } else { "L" };
    icacls(&[
        path.as_os_str(),
        "/setintegritylevel".as_ref(),
        label.as_ref(),
        "/Q".as_ref(),
    ])
}

/// The temporary folder sandboxed commands share.
fn sandbox_temp() -> Option<PathBuf> {
    let temp = std::env::temp_dir().join("crack-sandbox");
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

/// This process's environment with TEMP, TMP and TMPDIR replaced, as a CreateProcess
/// block.
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
        vars.retain(|(k, _)| {
            !["TEMP", "TMP", "TMPDIR"]
                .iter()
                .any(|v| k.eq_ignore_ascii_case(v))
        });
        let temp = temp.display().to_string();
        vars.push(("TEMP".into(), temp.clone()));
        vars.push(("TMP".into(), temp.clone()));
        // Git Bash's /tmp is shared with any Git Bash the user has open, which may
        // have set it up; mktemp and friends look here first.
        vars.push(("TMPDIR".into(), temp.replace('\\', "/")));
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

/// Runs `program` sandboxed and waits for it: write access only to `writable` (and
/// the shared temporary folder). Returns its exit code. The network can't be
/// blocked, so `network` off is refused rather than ignored.
pub fn run(writable: &[PathBuf], network: bool, program: &[String]) -> io::Result<u32> {
    if !network {
        return Err(io::Error::other(NO_NETWORK_BLOCKING));
    }
    let temp = sandbox_temp();
    for path in writable.iter().filter(|p| p.exists()).chain(temp.as_ref()) {
        share(path).map_err(|error| {
            io::Error::other(format!(
                "couldn't make {} writable: {error}",
                path.display()
            ))
        })?;
    }
    let base = own_token()?;
    if let Ok((user, session)) = user_and_session(&base) {
        share_msys_state(&user, session);
    }
    let token = sandbox_token(&base)
        .map_err(|error| io::Error::other(format!("couldn't make the sandbox token: {error}")))?;
    launch(&token, temp.as_deref(), program)
}

/// Why `--no-network` is refused.
pub const NO_NETWORK_BLOCKING: &str = "Windows can't block a sandboxed command's network without \
     administrator rights; allow sandbox network (ctrl-p), or turn the sandbox off";

fn launch(token: &Handle, temp: Option<&Path>, program: &[String]) -> io::Result<u32> {
    // The command shares the helper's stdin, stdout and stderr.
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    unsafe {
        startup.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
        startup.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
        startup.hStdError = GetStdHandle(STD_ERROR_HANDLE);
        for handle in [startup.hStdInput, startup.hStdOutput, startup.hStdError] {
            if !handle.is_null() {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            }
        }
    }

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
    // A restricted copy of this process's own token needs no special privilege.
    let created = unsafe {
        CreateProcessAsUserW(
            token.0,
            null(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            null(),
            &startup,
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
    // Finished: whatever it started in the background keeps running (still
    // restricted), as it would on Linux.
    limits.BasicLimitInformation.LimitFlags = 0;
    set_limits(&limits);
    Ok(code)
}

/// The sandbox works here: a command in it can write to a shared folder and not to
/// another one. Checked once, through the real helper, so a Windows where the token
/// or the shell doesn't work falls back to asking before commands.
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
    let Some(argv) = super::command(&command, std::slice::from_ref(&inside), &[], true) else {
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
    fn low_labels_are_read_from_sddl() {
        assert!(labelled_low("S:(ML;OICI;NW;;;LW)"));
        // Inherited from a labelled parent counts.
        assert!(labelled_low("D:(A;OICI;FA;;;SY)S:(ML;OICIID;NW;;;LW)"));
        // Not inherited, or not low, or no label: not enough.
        assert!(!labelled_low("S:(ML;;NW;;;LW)"));
        assert!(!labelled_low("S:(ML;OICI;NW;;;ME)"));
        assert!(!labelled_low("D:(A;OICI;FA;;;SY)"));
    }

    /// The real thing: the token can be made on the CI runner.
    #[test]
    fn sandbox_token_can_be_made() {
        let base = own_token().unwrap();
        sandbox_token(&base).unwrap();
        let (user, _) = user_and_session(&base).unwrap();
        assert!(user.starts_with("S-1-5-"), "{user}");
    }
}
