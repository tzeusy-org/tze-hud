//! Windows adapter for the explicit helper. Its COM apartment, token handles,
//! protected journal lock, and UAC child all end before the early CLI returns.

use super::*;
use crate::firewall::{
    FirewallSnapshot, PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC, Rule, TailnetInbound, decide,
};
use ::windows::Win32::Foundation::*;
use ::windows::Win32::Globalization::CompareStringOrdinal;
use ::windows::Win32::NetworkManagement::WindowsFirewall::*;
use ::windows::Win32::Security::Authorization::*;
use ::windows::Win32::Security::*;
use ::windows::Win32::Storage::FileSystem::*;
use ::windows::Win32::System::Com::*;
use ::windows::Win32::System::Environment::ExpandEnvironmentStringsForUserW;
use ::windows::Win32::System::Ole::*;
use ::windows::Win32::System::Threading::*;
use ::windows::Win32::System::Variant::{VT_ARRAY, VT_BSTR, VT_DISPATCH, VT_EMPTY, VT_VARIANT};
use ::windows::Win32::UI::Shell::*;
use ::windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use ::windows::core::{BSTR, Interface, PCWSTR, PWSTR, VARIANT};
use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

const MAX_FILE: u64 = 16 * 1024 * 1024;
const ADMIN_ACL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

fn failure(context: &str, error: impl std::fmt::Display) -> String {
    format!("{context}: {error}")
}
fn wide(text: &str) -> Result<Vec<u16>, String> {
    if text.contains('\0') {
        return Err("embedded NUL in Windows value".into());
    }
    Ok(text.encode_utf16().chain(Some(0)).collect())
}
fn path_wide(path: &Path) -> Result<Vec<u16>, String> {
    let mut value: Vec<_> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err("embedded NUL in Windows path".into());
    }
    value.push(0);
    Ok(value)
}
fn ordinal_equal(a: &str, b: &str) -> bool {
    let a: Vec<_> = a.encode_utf16().collect();
    let b: Vec<_> = b.encode_utf16().collect();
    // SAFETY: counted UTF-16 slices are valid for the synchronous comparison.
    unsafe { CompareStringOrdinal(&a, &b, true).0 == 2 }
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: these wrappers own successful, non-pseudo API handles.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: only LocalAlloc-family API results are stored here.
        unsafe {
            let _ = LocalFree(HLOCAL(self.0));
        }
    }
}
struct ComAllocation(*mut c_void);
impl Drop for ComAllocation {
    fn drop(&mut self) {
        // SAFETY: SHGetKnownFolderPath returns task-allocator storage, including
        // on a failing HRESULT. CoTaskMemFree accepts a null pointer.
        unsafe { CoTaskMemFree(Some(self.0.cast_const())) };
    }
}

#[link(name = "shell32")]
unsafe extern "system" {
    #[link_name = "SHGetKnownFolderPath"]
    fn known_folder_path_raw(
        folder: *const ::windows::core::GUID,
        flags: u32,
        token: HANDLE,
        path: *mut PWSTR,
    ) -> ::windows::core::HRESULT;
}

fn program_data_path() -> Result<PathBuf, String> {
    let mut name = PWSTR::null();
    // Use the SDK's out-parameter interface so even a failing HRESULT releases
    // any returned allocation; the generated Result<PWSTR> hides that pointer.
    let result = unsafe {
        known_folder_path_raw(
            &FOLDERID_ProgramData,
            KF_FLAG_DEFAULT.0 as u32,
            HANDLE::default(),
            &mut name,
        )
    };
    let _memory = ComAllocation(name.0.cast());
    result.ok().map_err(|e| failure("ProgramData", e))?;
    Ok(PathBuf::from(
        unsafe { name.to_string() }.map_err(|e| failure("ProgramData path", e))?,
    ))
}
struct Com;
impl Com {
    fn init() -> Result<Self, String> {
        // SAFETY: the helper uses a dedicated thread; no caller apartment exists.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).ok() }
            .map_err(|e| failure("COM initialization", e))?;
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// Hold the invoked image read-only through UAC/child termination. Denying
/// write/delete sharing closes the rename/replacement gap between path checks
/// and Shell execution; an updater gets a bounded sharing conflict instead.
fn pin_current_image() -> Result<Handle, String> {
    let path = path_wide(&std::env::current_exe().map_err(|e| failure("current executable", e))?)?;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| failure("pin original executable image", e))?;
    Ok(Handle(handle))
}
fn canonical_file(path: &Path) -> Result<String, String> {
    let path = path_wide(path)?;
    // SAFETY: read-only file handle; every handle is closed by RAII.
    let handle = Handle(
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| failure("canonical executable file", e))?,
    );
    let mut buffer = vec![0u16; 32_768];
    let n = unsafe {
        GetFinalPathNameByHandleW(
            handle.0,
            &mut buffer,
            GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
        )
    } as usize;
    if n == 0 || n >= buffer.len() {
        return Err("canonical executable path unavailable/too long".into());
    }
    let path =
        String::from_utf16(&buffer[..n]).map_err(|_| "executable path is not valid UTF-16")?;
    Ok(if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
    })
}
fn process_created(process: HANDLE) -> Result<u64, String> {
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user) }
        .map_err(|e| failure("process creation identity", e))?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}
fn token(process: HANDLE) -> Result<Handle, String> {
    let mut handle = HANDLE::default();
    unsafe {
        OpenProcessToken(
            process,
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_IMPERSONATE,
            &mut handle,
        )
    }
    .map_err(|e| failure("original process token", e))?;
    Ok(Handle(handle))
}

/// Temporarily enable one privilege already assigned to this token. The saved
/// state contains only attributes actually changed by AdjustTokenPrivileges.
/// This never grants account policy or removes a privilege.
struct PrivilegeScope<'a> {
    token: &'a Handle,
    previous: TOKEN_PRIVILEGES,
    restored: bool,
}
impl<'a> PrivilegeScope<'a> {
    fn enable(token: &'a Handle, name: PCWSTR) -> Result<Self, String> {
        let mut luid = LUID::default();
        // SAFETY: the synchronous call writes one LUID for a valid constant.
        unsafe { LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) }
            .map_err(|e| failure("lookup original-token privilege", e))?;
        let requested = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let mut scope = Self {
            token,
            previous: TOKEN_PRIVILEGES::default(),
            restored: false,
        };
        // SAFETY: only one privilege can change, and previous has room for that
        // complete state. Capture last-error immediately: BOOL success alone
        // also represents ERROR_NOT_ALL_ASSIGNED and cannot authorize a retry.
        unsafe { SetLastError(ERROR_SUCCESS) };
        let adjusted = unsafe {
            AdjustTokenPrivileges(
                token.0,
                false,
                Some(&requested),
                std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
                Some(&mut scope.previous),
                None,
            )
        };
        let status = unsafe { GetLastError() };
        let enabled = adjusted
            .map_err(|e| failure("enable original-token privilege", e))
            .and_then(|()| {
                if status == ERROR_SUCCESS {
                    Ok(())
                } else {
                    Err(format!(
                        "original-token privilege not assigned/enabled (Windows error {})",
                        status.0
                    ))
                }
            });
        if let Err(error) = enabled {
            scope
                .restore()
                .map_err(|restore| format!("{error}; {restore}"))?;
            return Err(error);
        }
        Ok(scope)
    }

    fn restore(&mut self) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        if self.previous.PrivilegeCount != 0 {
            // SAFETY: restore only the exact previous state returned for our
            // single change; no other privilege is disabled or added.
            unsafe { SetLastError(ERROR_SUCCESS) };
            let adjusted = unsafe {
                AdjustTokenPrivileges(self.token.0, false, Some(&self.previous), 0, None, None)
            };
            let status = unsafe { GetLastError() };
            adjusted.map_err(|e| failure("restore original-token privilege", e))?;
            if status != ERROR_SUCCESS {
                return Err(format!(
                    "original-token privilege restoration failed (Windows error {})",
                    status.0
                ));
            }
        }
        self.restored = true;
        Ok(())
    }
}
impl Drop for PrivilegeScope<'_> {
    fn drop(&mut self) {
        // Explicit checked restoration controls the production result. Drop
        // additionally covers unwinding/early returns and retries a failed
        // restore, without ordinary file logging in this elevated path.
        let _ = self.restore();
    }
}

fn with_privilege<T>(
    token: &Handle,
    name: PCWSTR,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let mut scope = PrivilegeScope::enable(token, name)?;
    let result = operation();
    // An acquired token is not returned to the policy caller unless restoration
    // succeeded; on failure its RAII owner closes it before returning an error.
    match scope.restore() {
        Ok(()) => result,
        Err(restore) => Err(match result {
            Ok(_) => restore,
            Err(error) => format!("{error}; {restore}"),
        }),
    }
}

fn original_token(process: HANDLE, child: bool) -> Result<Handle, String> {
    match token(process) {
        Ok(token) => Ok(token),
        Err(error) if !child => Err(error),
        Err(first_error) => {
            // Identity/creation checks already succeeded in origin. Preserve
            // accessible same-account paths; a failed child open gets exactly
            // one retry under its already-assigned SeDebug privilege, as the
            // different-account OpenProcessToken contract requires.
            let mut current = HANDLE::default();
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES,
                    &mut current,
                )
            }
            .map_err(|e| failure("child privilege token", e))?;
            let current = Handle(current);
            with_privilege(&current, SE_DEBUG_NAME, || token(process)).map_err(|error| {
                format!("{first_error}; scoped child SeDebugPrivilege retry: {error}")
            })
        }
    }
}

fn token_sid(token: HANDLE) -> Result<String, String> {
    let mut required = 0;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut required) };
    if required == 0 || required > 65_536 {
        return Err("invalid token user length".into());
    }
    // usize allocation supplies TOKEN_USER alignment; the SID remains borrowed
    // until ConvertSidToStringSidW has allocated its independent string.
    let mut buffer = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required,
            &mut required,
        )
    }
    .map_err(|e| failure("token user", e))?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }
        .map_err(|e| failure("token SID", e))?;
    let _memory = LocalAllocation(text.0.cast());
    unsafe { text.to_string() }.map_err(|e| failure("SID text", e))
}
fn is_elevated() -> Result<bool, String> {
    let token = token(unsafe { GetCurrentProcess() })?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut length = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        )
    }
    .map_err(|e| failure("administrator token", e))?;
    Ok(elevation.TokenIsElevated != 0)
}
fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| failure("OS nonce", e))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn current_request(action: RemoteAction, ports: [u16; 2]) -> Result<ChildRequest, String> {
    let program =
        canonical_file(&std::env::current_exe().map_err(|e| failure("current executable", e))?)?;
    let request = ChildRequest {
        version: VERSION,
        action,
        program,
        ports,
        parent_pid: unsafe { GetCurrentProcessId() },
        parent_created: process_created(unsafe { GetCurrentProcess() })?,
        nonce: nonce()?,
    };
    request.validate()?;
    Ok(request)
}

/// The original owner is derived from a live, creation-time-checked process;
/// no caller-supplied SID or alternate administrator's APPDATA is trusted.
struct Origin {
    _image: Handle,
    _process: Option<Handle>,
    token: Handle,
    sid: String,
}
fn origin(request: &ChildRequest, child: bool) -> Result<Origin, String> {
    let image = pin_current_image()?;
    if !ordinal_equal(
        &canonical_file(&std::env::current_exe().map_err(|e| failure("current executable", e))?)?,
        &request.program,
    ) {
        return Err("helper executable differs from the original target".into());
    }
    let process = if child {
        Some(Handle(
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, request.parent_pid) }
                .map_err(|e| failure("original parent process", e))?,
        ))
    } else {
        None
    };
    let handle = process
        .as_ref()
        .map_or_else(|| unsafe { GetCurrentProcess() }, |p| p.0);
    if process_created(handle)? != request.parent_created {
        return Err("original parent creation identity changed".into());
    }
    let mut name = vec![0u16; 32_768];
    let mut length = name.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(name.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(|e| failure("original parent executable", e))?;
    let name = String::from_utf16(&name[..length as usize])
        .map_err(|_| "invalid parent executable UTF-16")?;
    if !ordinal_equal(&canonical_file(Path::new(&name))?, &request.program) {
        return Err("original parent executable mismatch".into());
    }
    let token = original_token(handle, child)?;
    let sid = token_sid(token.0)?;
    Ok(Origin {
        _image: image,
        _process: process,
        token,
        sid,
    })
}

pub(super) fn execute(action: RemoteAction, ports: [u16; 2]) -> Result<String, String> {
    let _image = pin_current_image()?;
    let request = current_request(action, ports)?;
    std::thread::spawn(move || {
        let _com = Com::init()?;
        let mut launcher = NativeLauncher { child: false };
        control(&mut launcher, &request, false)
    })
    .join()
    .map_err(|_| "firewall helper thread panicked")?
}
pub(super) fn execute_child(request: ChildRequest) -> Result<String, String> {
    std::thread::spawn(move || {
        let _com = Com::init()?;
        let mut launcher = NativeLauncher { child: true };
        control(&mut launcher, &request, true)
    })
    .join()
    .map_err(|_| "firewall child thread panicked")?
}
struct NativeLauncher {
    child: bool,
}
impl Launcher for NativeLauncher {
    fn elevated(&mut self) -> Result<bool, String> {
        is_elevated()
    }
    fn run_local(&mut self, request: &ChildRequest) -> Result<String, String> {
        let origin = origin(request, self.child)?;
        let (result, changes) = run_transaction(request, &origin);
        if self.child {
            let report = ChildResult {
                version: VERSION,
                request: request.clone(),
                original_sid: origin.sid.clone(),
                exit: u32::from(result.is_err()),
                message: match &result {
                    Ok(s) | Err(s) => s.clone(),
                },
                actual_changes: changes,
                classification: if result.is_ok() && request.action == RemoteAction::Allow {
                    "allowed"
                } else {
                    "not_evaluated_after_undo_or_error"
                }
                .into(),
                recovery_state: if result.is_ok() && request.action == RemoteAction::Disallow {
                    "undo_complete"
                } else {
                    "retain_journal_for_undo_or_error_recovery"
                }
                .into(),
            };
            if let Err(error) = write_result(&report) {
                return Err(format!(
                    "{}; protected result delivery failed: {error}; policy may have changed, retain the journal and retry this explicit command",
                    report.message
                ));
            }
        }
        result
    }
    fn launch_and_wait(&mut self, request: &ChildRequest) -> Result<String, String> {
        let executable = wide(&request.program)?;
        let payload = serde_json::to_string(request).map_err(|e| failure("child payload", e))?;
        let parameters = wide(&format!(
            "{} {}",
            quote_argument(CHILD_FLAG)?,
            quote_argument(&payload)?
        ))?;
        if parameters.len() > 32_767 {
            return Err("internal Windows command line exceeds the platform limit".into());
        }
        // Both buffers outlive ShellExecuteExW. No shell command, target option,
        // environment fallback, or recursive elevation is involved.
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: ::windows::core::w!("runas"),
            lpFile: PCWSTR(executable.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
            return Err(
                if error.code() == ::windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                    "UAC cancelled; no firewall changes".into()
                } else {
                    failure("UAC launch", error)
                },
            );
        }
        if info.hProcess.is_invalid() {
            return Err("UAC returned no child process handle; no success claimed".into());
        }
        let child = Handle(info.hProcess);
        if unsafe { WaitForSingleObject(child.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(
                "child wait failed; inspect the protected recovery journal before retry".into(),
            );
        }
        let mut exit = u32::MAX;
        unsafe { GetExitCodeProcess(child.0, &mut exit) }.map_err(|e| failure("child exit", e))?;
        let sid = token_sid(token(unsafe { GetCurrentProcess() })?.0)?;
        let report = read_result(request, &sid).map_err(|error| format!("child exited {exit} without a validated result: {error}; policy may have changed, retain the journal and retry this explicit command"))?;
        validate_child_result(report, request, &sid, exit)
    }
}

fn program_key(program: &str) -> String {
    let mut digest = Sha256::new();
    for unit in program.encode_utf16() {
        digest.update(unit.to_le_bytes());
    }
    format!("{:x}", digest.finalize())
}
fn run_transaction(
    request: &ChildRequest,
    origin: &Origin,
) -> (Result<String, String>, Vec<String>) {
    let mut store = match NativeStore::open(&request.program, origin) {
        Ok(store) => store,
        Err(error) => return (Err(error), Vec::new()),
    };
    cleanup_orphan_results(&store._root, &request.nonce);
    let key = program_key(&request.program);
    let spec = if request.action == RemoteAction::Allow {
        match RuleSpec::new(request.program.clone(), request.ports) {
            Ok(spec) => Some(spec),
            Err(error) => return (Err(error), Vec::new()),
        }
    } else {
        None
    };
    let result = transact(
        &mut store,
        request.action,
        &request.program,
        &origin.sid,
        &key,
        spec.as_ref(),
    )
    .map(|messages| messages.join("\n"))
    .map_err(|error| {
        if store.actual_changes.is_empty() {
            error
        } else {
            format!(
                "{error}\nActual successful COM writes, including compensation:\n{}",
                store.actual_changes.join("\n")
            )
        }
    });
    (result, store.actual_changes)
}

const DIRECTORY_ACL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;GRGX;;;BU)";
struct Descriptor {
    allocation: LocalAllocation,
}
impl Descriptor {
    fn new(sddl: &str) -> Result<Self, String> {
        let text = wide(sddl)?;
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|e| failure("protected descriptor", e))?;
        Ok(Self {
            allocation: LocalAllocation(descriptor.0),
        })
    }
    fn pointer(&self) -> PSECURITY_DESCRIPTOR {
        PSECURITY_DESCRIPTOR(self.allocation.0)
    }
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.allocation.0,
            bInheritHandle: false.into(),
        }
    }
}
fn descriptor_text(descriptor: PSECURITY_DESCRIPTOR) -> Result<String, String> {
    let mut text = PWSTR::null();
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut text,
            None,
        )
    }
    .map_err(|e| failure("descriptor verification", e))?;
    let _memory = LocalAllocation(text.0.cast());
    unsafe { text.to_string() }.map_err(|e| failure("descriptor text", e))
}
fn verify_file(file: &File, sddl: &str) -> Result<(), String> {
    let handle = HANDLE(file.as_raw_handle());
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(|e| failure("protected file attributes", e))?;
    if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err("helper path is a reparse point; no policy changes".into());
    }
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    }
    .ok()
    .map_err(|e| failure("protected file owner/DACL", e))?;
    let _memory = LocalAllocation(descriptor.0);
    // Compare normalized native descriptors, including the protected DACL flag.
    // Foreign or edited ACLs are refused rather than repaired in place.
    let normalized = |text: String| text.replace("D:PAI", "D:P").replace("D:PAR", "D:P");
    if normalized(descriptor_text(descriptor)?)
        != normalized(descriptor_text(Descriptor::new(sddl)?.pointer())?)
    {
        return Err("helper path has foreign owner/DACL; no policy changes".into());
    }
    Ok(())
}
fn open_file(
    path: &Path,
    access: u32,
    disposition: FILE_CREATION_DISPOSITION,
    sddl: &str,
    directory: bool,
    share: FILE_SHARE_MODE,
) -> Result<File, String> {
    let path = path_wide(path)?;
    let descriptor = Descriptor::new(sddl)?;
    let attributes = descriptor.attributes();
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access | READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0,
            share,
            Some(&attributes),
            disposition,
            flags,
            None,
        )
    }
    .map_err(|e| failure("protected helper file", e))?;
    // SAFETY: CreateFileW returned one owned handle, now transferred to File.
    let file = unsafe { File::from_raw_handle(handle.0) };
    verify_file(&file, sddl)?;
    Ok(file)
}
struct Root {
    path: PathBuf,
    _directories: Vec<File>,
}
fn protected_root(create: bool) -> Result<Root, String> {
    let mut path = program_data_path()?;
    // The system-owned ProgramData ancestor is opened without following a reparse
    // point. Do not impose our ACL on this shared Windows directory.
    let raw = path_wide(&path)?;
    let base = unsafe {
        CreateFileW(
            PCWSTR(raw.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|e| failure("ProgramData ancestor", e))?;
    let base = unsafe { File::from_raw_handle(base.0) };
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(base.as_raw_handle()),
            FileAttributeTagInfo,
            (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    }
    .map_err(|e| failure("ProgramData attributes", e))?;
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err("ProgramData is a reparse point".into());
    }
    let mut directories = vec![base];
    for component in ["tze_hud", "firewall"] {
        path.push(component);
        if create {
            let raw = path_wide(&path)?;
            let descriptor = Descriptor::new(DIRECTORY_ACL)?;
            let attributes = descriptor.attributes();
            if let Err(error) = unsafe { CreateDirectoryW(PCWSTR(raw.as_ptr()), Some(&attributes)) }
            {
                if error.code() != ::windows::core::HRESULT::from_win32(ERROR_ALREADY_EXISTS.0) {
                    return Err(failure("protected helper directory", error));
                }
            }
        }
        directories.push(open_file(
            &path,
            FILE_GENERIC_READ.0,
            OPEN_EXISTING,
            DIRECTORY_ACL,
            true,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )?);
    }
    Ok(Root {
        path,
        _directories: directories,
    })
}
fn bounded_read(mut file: &File) -> Result<Vec<u8>, String> {
    if file
        .metadata()
        .map_err(|e| failure("helper file size", e))?
        .len()
        > MAX_FILE
    {
        return Err("helper file exceeds size limit".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure("helper file read", e))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("helper file exceeds size limit".into());
    }
    Ok(bytes)
}
fn atomic_write(path: &Path, bytes: &[u8], sddl: &str) -> Result<(), String> {
    if bytes.len() as u64 > MAX_FILE {
        return Err("helper file exceeds size limit".into());
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            drop(open_file(
                path,
                FILE_GENERIC_READ.0,
                OPEN_EXISTING,
                sddl,
                false,
                FILE_SHARE_READ,
            )?);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(failure("journal replacement identity", error)),
    }
    let temp = path.with_extension(format!("tmp-{}", nonce()?));
    let outcome = (|| {
        let mut file = open_file(
            &temp,
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            CREATE_NEW,
            sddl,
            false,
            FILE_SHARE_MODE(0),
        )?;
        file.write_all(bytes)
            .map_err(|e| failure("helper file write", e))?;
        file.sync_all()
            .map_err(|e| failure("helper file flush", e))?;
        drop(file);
        let source = path_wide(&temp)?;
        let target = path_wide(path)?;
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|e| failure("durable journal replacement", e))
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    outcome
}
fn result_acl(sid: &str) -> String {
    format!("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRSD;;;{sid})")
}
fn result_path(root: &Root, request: &ChildRequest) -> PathBuf {
    root.path.join(format!("result-{}.json", request.nonce))
}
fn write_result(result: &ChildResult) -> Result<(), String> {
    let root = protected_root(true)?;
    let path = result_path(&root, &result.request);
    if path.exists() {
        return Err("child result nonce collision; recovery journal retained".into());
    }
    let bytes = serde_json::to_vec(result).map_err(|e| failure("child result encoding", e))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("child result exceeds size limit; recovery journal retained".into());
    }
    let mut file = open_file(
        &path,
        FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
        CREATE_NEW,
        &result_acl(&result.original_sid),
        false,
        FILE_SHARE_MODE(0),
    )?;
    file.write_all(&bytes)
        .map_err(|e| failure("child result write", e))?;
    file.sync_all()
        .map_err(|e| failure("child result flush", e))
}
fn read_result(request: &ChildRequest, sid: &str) -> Result<ChildResult, String> {
    let root = protected_root(false)?;
    let path = result_path(&root, request);
    let file = open_file(
        &path,
        FILE_GENERIC_READ.0 | DELETE.0,
        OPEN_EXISTING,
        &result_acl(sid),
        false,
        FILE_SHARE_MODE(0),
    )?;
    let bytes = bounded_read(&file)?;
    drop(file);
    let result = serde_json::from_slice(&bytes).map_err(|e| failure("child result decoding", e));
    // Delete only this nonce's result, never journal files or unrelated entries.
    std::fs::remove_file(path).map_err(|e| failure("child result cleanup", e))?;
    result
}

/// Decode only documented, one-dimensional string arrays. Never persist a raw
/// VARIANT, pointer, opaque COM object, or unrecognized array element.
fn interfaces(value: &VARIANT) -> Result<Option<Vec<String>>, String> {
    let raw = unsafe { &value.as_raw().Anonymous.Anonymous };
    if raw.vt == VT_EMPTY.0 {
        return Ok(None);
    }
    if raw.vt != (VT_ARRAY.0 | VT_VARIANT.0) && raw.vt != (VT_ARRAY.0 | VT_BSTR.0) {
        return Err("unsupported firewall Interfaces VARIANT type".into());
    }
    let array = unsafe { raw.Anonymous.parray }.cast::<SAFEARRAY>();
    if array.is_null() || unsafe { SafeArrayGetDim(array) } != 1 {
        return Err("unsupported firewall Interfaces array shape".into());
    }
    let vartype =
        unsafe { SafeArrayGetVartype(array) }.map_err(|e| failure("interface array type", e))?;
    if (vartype != VT_VARIANT && vartype != VT_BSTR) || raw.vt != (VT_ARRAY.0 | vartype.0) {
        return Err("unsupported firewall Interfaces element type".into());
    }
    let lower = unsafe { SafeArrayGetLBound(array, 1) }
        .map_err(|e| failure("interface array lower bound", e))?;
    let upper = unsafe { SafeArrayGetUBound(array, 1) }
        .map_err(|e| failure("interface array upper bound", e))?;
    if i64::from(upper) - i64::from(lower) > 4096 {
        return Err("too many firewall interfaces".into());
    }
    let mut names = Vec::new();
    for index in lower..=upper {
        if vartype == VT_BSTR {
            let mut text = BSTR::new();
            unsafe { SafeArrayGetElement(array, &index, (&mut text as *mut BSTR).cast()) }
                .map_err(|e| failure("interface string", e))?;
            names.push(text.to_string());
        } else {
            let mut element = VARIANT::new();
            unsafe { SafeArrayGetElement(array, &index, (&mut element as *mut VARIANT).cast()) }
                .map_err(|e| failure("interface variant", e))?;
            if unsafe { element.as_raw().Anonymous.Anonymous.vt } != VT_BSTR.0 {
                return Err("non-string firewall interface".into());
            }
            names.push(
                BSTR::try_from(&element)
                    .map_err(|e| failure("interface string conversion", e))?
                    .to_string(),
            );
        }
    }
    Ok(Some(names))
}
fn interface_variant(names: &[String]) -> Result<VARIANT, String> {
    let array = unsafe { SafeArrayCreateVector(VT_VARIANT, 0, names.len() as u32) };
    if array.is_null() {
        return Err("interface array allocation failed".into());
    }
    let mut raw: ::windows::core::imp::VARIANT = unsafe { std::mem::zeroed() };
    raw.Anonymous.Anonymous.vt = VT_ARRAY.0 | VT_VARIANT.0;
    raw.Anonymous.Anonymous.Anonymous.parray = array.cast();
    // Ownership transfers immediately, so errors below also free the array.
    let result = unsafe { VARIANT::from_raw(raw) };
    for (index, name) in names.iter().enumerate() {
        let element = VARIANT::from(name.as_str());
        unsafe { SafeArrayPutElement(array, &(index as i32), (&element as *const VARIANT).cast()) }
            .map_err(|e| failure("interface array insertion", e))?;
    }
    Ok(result)
}
fn image(rule: &INetFwRule) -> Result<RuleImage, String> {
    // Every getter and cast is mandatory. Diagnostic omission/empty substitution
    // would lose original policy and therefore cannot be used for undo.
    let read = || -> ::windows::core::Result<RuleImage> {
        unsafe {
            let rule2: INetFwRule2 = rule.cast()?;
            let rule3: INetFwRule3 = rule.cast()?;
            let protocol = rule.Protocol()?;
            let interfaces = interfaces(&rule.Interfaces()?)
                .map_err(|s| ::windows::core::Error::new(E_FAIL, s))?;
            Ok(RuleImage {
                name: rule.Name()?.to_string(),
                description: rule.Description()?.to_string(),
                application: rule.ApplicationName()?.to_string(),
                service: rule.ServiceName()?.to_string(),
                protocol,
                local_ports: if protocol == 6 || protocol == 17 {
                    Some(rule.LocalPorts()?.to_string())
                } else {
                    None
                },
                remote_ports: if protocol == 6 || protocol == 17 {
                    Some(rule.RemotePorts()?.to_string())
                } else {
                    None
                },
                icmp: if protocol == 1 || protocol == 58 {
                    Some(rule.IcmpTypesAndCodes()?.to_string())
                } else {
                    None
                },
                local_addresses: rule.LocalAddresses()?.to_string(),
                remote_addresses: rule.RemoteAddresses()?.to_string(),
                direction: rule.Direction()?.0,
                interfaces,
                interface_types: rule.InterfaceTypes()?.to_string(),
                enabled: rule.Enabled()?.as_bool(),
                grouping: rule.Grouping()?.to_string(),
                profiles: rule.Profiles()?,
                edge_traversal: rule.EdgeTraversal()?.as_bool(),
                action: rule.Action()?.0,
                edge_options: rule2.EdgeTraversalOptions()?,
                package_id: rule3.LocalAppPackageId()?.to_string(),
                user_owner: rule3.LocalUserOwner()?.to_string(),
                local_users: rule3.LocalUserAuthorizedList()?.to_string(),
                remote_users: rule3.RemoteUserAuthorizedList()?.to_string(),
                remote_machines: rule3.RemoteMachineAuthorizedList()?.to_string(),
                secure_flags: rule3.SecureFlags()?,
            })
        }
    };
    read().map_err(|e| failure("complete INetFwRule/2/3 snapshot", e))
}
fn create_rule(rule: &RuleImage) -> Result<INetFwRule, String> {
    if !rule.package_id.is_empty() {
        return Err(
            "LocalAppPackageId cannot be restored through INetFwRules::Add; no policy changes"
                .into(),
        );
    }
    if !(0..=256).contains(&rule.protocol)
        || ![1, 2].contains(&rule.direction)
        || ![0, 1].contains(&rule.action)
        || rule.profiles <= 0
        || ![0, 1, 2, 3].contains(&rule.edge_options)
    {
        return Err("unsupported firewall enum/property value; no policy changes".into());
    }
    for value in [
        &rule.name,
        &rule.description,
        &rule.application,
        &rule.service,
        &rule.local_addresses,
        &rule.remote_addresses,
        &rule.interface_types,
        &rule.grouping,
        &rule.user_owner,
        &rule.local_users,
        &rule.remote_users,
        &rule.remote_machines,
    ] {
        if value.contains('\0') {
            return Err("NUL in firewall property".into());
        }
    }
    let create = || -> ::windows::core::Result<INetFwRule> {
        unsafe {
            let object: INetFwRule = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)?;
            let rule2: INetFwRule2 = object.cast()?;
            let rule3: INetFwRule3 = object.cast()?;
            object.SetName(&BSTR::from(rule.name.as_str()))?;
            if !rule.description.is_empty() {
                object.SetDescription(&BSTR::from(rule.description.as_str()))?;
            }
            object.SetApplicationName(&BSTR::from(rule.application.as_str()))?;
            if !rule.service.is_empty() {
                object.SetServiceName(&BSTR::from(rule.service.as_str()))?;
            }
            // Protocol precedes ports/ICMP. Inapplicable properties are absent,
            // never probed or overwritten with made-up defaults.
            object.SetProtocol(rule.protocol)?;
            if let Some(value) = &rule.local_ports {
                object.SetLocalPorts(&BSTR::from(value.as_str()))?;
            }
            if let Some(value) = &rule.remote_ports {
                object.SetRemotePorts(&BSTR::from(value.as_str()))?;
            }
            if let Some(value) = &rule.icmp {
                object.SetIcmpTypesAndCodes(&BSTR::from(value.as_str()))?;
            }
            object.SetLocalAddresses(&BSTR::from(rule.local_addresses.as_str()))?;
            object.SetRemoteAddresses(&BSTR::from(rule.remote_addresses.as_str()))?;
            object.SetDirection(NET_FW_RULE_DIRECTION(rule.direction))?;
            if let Some(names) = &rule.interfaces {
                let value =
                    interface_variant(names).map_err(|s| ::windows::core::Error::new(E_FAIL, s))?;
                object.SetInterfaces(&value).map_err(|error| {
                    ::windows::core::Error::new(error.code(), format!("SetInterfaces: {error}"))
                })?;
            }
            object.SetInterfaceTypes(&BSTR::from(rule.interface_types.as_str()))?;
            object.SetProfiles(rule.profiles)?;
            object.SetAction(NET_FW_ACTION(rule.action))?;
            object.SetEnabled(VARIANT_BOOL::from(rule.enabled))?;
            if !rule.grouping.is_empty() {
                object.SetGrouping(&BSTR::from(rule.grouping.as_str()))?;
            }
            object.SetEdgeTraversal(VARIANT_BOOL::from(rule.edge_traversal))?;
            rule2.SetEdgeTraversalOptions(rule.edge_options)?;
            rule3.SetSecureFlags(rule.secure_flags)?;
            if !rule.user_owner.is_empty() {
                rule3.SetLocalUserOwner(&BSTR::from(rule.user_owner.as_str()))?;
            }
            if !rule.local_users.is_empty() {
                rule3.SetLocalUserAuthorizedList(&BSTR::from(rule.local_users.as_str()))?;
            }
            if !rule.remote_users.is_empty() {
                rule3.SetRemoteUserAuthorizedList(&BSTR::from(rule.remote_users.as_str()))?;
            }
            if !rule.remote_machines.is_empty() {
                rule3.SetRemoteMachineAuthorizedList(&BSTR::from(rule.remote_machines.as_str()))?;
            }
            Ok(object)
        }
    };
    create().map_err(|e| failure("standalone complete rule recreation", e))
}
fn standalone(rule: &RuleImage) -> Result<INetFwRule, String> {
    let object = create_rule(rule)?;
    if image(&object)? != *rule {
        return Err(
            "Windows cannot roundtrip every saved rule property exactly; no policy changes".into(),
        );
    }
    Ok(object)
}

fn prepare_owned_allow(rule: &RuleImage) -> Result<RuleImage, String> {
    let observed = image(&create_rule(rule)?)?;
    let mut comparable = observed.clone();
    if !ordinal_equal(&comparable.application, &rule.application) {
        return Err("native ALLOW retargeted the program".into());
    }
    comparable.application.clone_from(&rule.application);
    if !equivalent_allow(&comparable, rule) {
        return Err("native ALLOW changed its exact scope/properties; no policy changes".into());
    }
    // All future fingerprints use this actual native representation, including
    // Windows' subnet-mask/port formatting. Original backups stay byte-exact.
    standalone(&observed)?;
    Ok(observed)
}

struct NativeStore<'a> {
    policy: INetFwPolicy2,
    collection: INetFwRules,
    origin: &'a Origin,
    _root: Root,
    journal: PathBuf,
    _lock: File,
    actual_changes: Vec<String>,
    journal_digest: Option<String>,
}
impl<'a> NativeStore<'a> {
    fn open(program: &str, origin: &'a Origin) -> Result<Self, String> {
        let root = protected_root(true)?;
        let key = program_key(program);
        let lock_path = root.path.join(format!("{key}.lock"));
        // Share-zero serializes this program across users and terminal sessions;
        // Windows releases the handle after a crash. Busy is a non-blocking error.
        let lock = open_file(
            &lock_path,
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            OPEN_ALWAYS,
            ADMIN_ACL,
            false,
            FILE_SHARE_MODE(0),
        )
        .map_err(|e| format!("program firewall transaction busy or unsafe lock: {e}"))?;
        let policy: INetFwPolicy2 =
            unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| failure("firewall policy", e))?;
        let collection = unsafe { policy.Rules() }.map_err(|e| failure("firewall rules", e))?;
        let journal = root.path.join(format!("{key}.json"));
        Ok(Self {
            policy,
            collection,
            origin,
            _root: root,
            journal,
            _lock: lock,
            actual_changes: Vec::new(),
            journal_digest: None,
        })
    }
    fn ensure_modifiable(&self) -> Result<(), String> {
        if unsafe { self.policy.LocalPolicyModifyState() }
            .map_err(|e| failure("local policy authority", e))?
            != NET_FW_MODIFY_STATE_OK
        {
            return Err("local firewall policy changes are unavailable (for example Group Policy); no policy changes".into());
        }
        Ok(())
    }
    fn journal_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        match std::fs::symlink_metadata(&self.journal) {
            Ok(_) => {
                let file = open_file(
                    &self.journal,
                    FILE_GENERIC_READ.0,
                    OPEN_EXISTING,
                    ADMIN_ACL,
                    false,
                    FILE_SHARE_READ,
                )?;
                bounded_read(&file).map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(failure("journal identity", error)),
        }
    }
    fn verify_journal_digest(&self) -> Result<(), String> {
        let current = self
            .journal_bytes()?
            .map(|bytes| format!("{:x}", Sha256::digest(bytes)));
        if current != self.journal_digest {
            return Err(
                "protected journal changed externally; no overwrite, retain recovery evidence"
                    .into(),
            );
        }
        Ok(())
    }
    fn entries(&self) -> Result<Vec<(INetFwRule, RuleImage)>, String> {
        let expected =
            unsafe { self.collection.Count() }.map_err(|e| failure("firewall rule count", e))?;
        if !(0..=100_000).contains(&expected) {
            return Err("invalid firewall rule count".into());
        }
        let enumerator: IEnumVARIANT = unsafe { self.collection._NewEnum().and_then(|e| e.cast()) }
            .map_err(|e| failure("firewall rule enumeration", e))?;
        let mut result = Vec::new();
        loop {
            let mut items = [VARIANT::new()];
            let mut fetched = 0;
            let status = unsafe { enumerator.Next(&mut items, &mut fetched) };
            if status == S_FALSE && fetched == 0 {
                break;
            }
            if status != S_OK || fetched != 1 {
                return Err(failure("incomplete firewall enumeration", status));
            }
            let raw = unsafe { &items[0].as_raw().Anonymous.Anonymous };
            if raw.vt != VT_DISPATCH.0 {
                return Err("unexpected firewall enumeration variant".into());
            }
            let unknown =
                unsafe { ::windows::core::IUnknown::from_raw_borrowed(&raw.Anonymous.pdispVal) }
                    .ok_or("null firewall rule interface")?;
            let object: INetFwRule = unknown
                .cast()
                .map_err(|e| failure("firewall rule interface", e))?;
            let snapshot = image(&object)?;
            result.push((object, snapshot));
            if result.len() > 100_000 {
                return Err("firewall enumeration exceeds limit".into());
            }
        }
        if result.len() != expected as usize
            || unsafe { self.collection.Count() }.map_err(|e| failure("firewall rule count", e))?
                != expected
        {
            return Err("firewall rules changed during enumeration; retry explicit command".into());
        }
        Ok(result)
    }
    fn expanded(&self, application: &str) -> Result<String, String> {
        if application.is_empty() {
            return Ok(String::new());
        }
        let source = wide(application)?;
        let mut buffer = vec![0u16; 32_768];
        unsafe {
            ExpandEnvironmentStringsForUserW(
                self.origin.token.0,
                PCWSTR(source.as_ptr()),
                &mut buffer,
            )
        }
        .map_err(|e| failure("original-owner rule application expansion", e))?;
        let end = buffer
            .iter()
            .position(|c| *c == 0)
            .ok_or("expanded program path too long")?;
        let text = String::from_utf16(&buffer[..end])
            .map_err(|_| "expanded application is invalid UTF-16")?;
        if text.contains('%') {
            return Err(
                "rule application has an unresolved original-owner environment variable".into(),
            );
        }
        Ok(text)
    }
}
impl Store for NativeStore<'_> {
    fn rules(&mut self) -> Result<Vec<RuleImage>, String> {
        Ok(self
            .entries()?
            .into_iter()
            .map(|(_, image)| image)
            .collect())
    }
    fn load(&mut self) -> Result<Option<Journal>, String> {
        let Some(bytes) = self.journal_bytes()? else {
            self.journal_digest = None;
            return Ok(None);
        };
        let journal = serde_json::from_slice(&bytes)
            .map_err(|e| failure("malformed/unsupported protected journal", e))?;
        self.journal_digest = Some(format!("{:x}", Sha256::digest(&bytes)));
        Ok(Some(journal))
    }
    fn save(&mut self, journal: &Journal) -> Result<(), String> {
        self.verify_journal_digest()?;
        let bytes = serde_json::to_vec(journal).map_err(|e| failure("journal encoding", e))?;
        atomic_write(&self.journal, &bytes, ADMIN_ACL)?;
        self.journal_digest = Some(format!("{:x}", Sha256::digest(&bytes)));
        self.verify_journal_digest()
    }
    fn remove_journal(&mut self) -> Result<(), String> {
        self.verify_journal_digest()?;
        drop(open_file(
            &self.journal,
            FILE_GENERIC_READ.0 | DELETE.0,
            OPEN_EXISTING,
            ADMIN_ACL,
            false,
            FILE_SHARE_MODE(0),
        )?);
        std::fs::remove_file(&self.journal).map_err(|e| failure("journal removal", e))?;
        self.journal_digest = None;
        Ok(())
    }
    fn preflight_rule(&mut self, rule: &RuleImage) -> Result<(), String> {
        self.ensure_modifiable()?;
        standalone(rule).map(|_| ())
    }
    fn prepare_allow(&mut self, spec: &RuleSpec, key: &str) -> Result<RuleImage, String> {
        self.ensure_modifiable()?;
        prepare_owned_allow(&spec.rule(key))
    }
    fn same_program(&mut self, application: &str, program: &str) -> Result<bool, String> {
        let expanded = self.expanded(application)?;
        if expanded.is_empty() {
            return Ok(false);
        }
        if ordinal_equal(&expanded, program) {
            return Ok(true);
        }
        // Missing foreign executable files are not this running executable.
        // Existing aliases are resolved through a file handle before comparison.
        match std::fs::metadata(&expanded) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(failure("rule application identity", error)),
        }
        Ok(ordinal_equal(
            &canonical_file(Path::new(&expanded))?,
            program,
        ))
    }
    fn apply(&mut self, change: &Change) -> Result<(), String> {
        let entries = self.entries()?;
        let mut expected: Vec<_> = entries.iter().map(|(_, image)| image.clone()).collect();
        if let Some(before) = &change.before {
            let index = expected
                .iter()
                .position(|r| r == before)
                .ok_or("missing exact rule before write")?;
            expected.remove(index);
        }
        if let Some(after) = &change.after {
            expected.push(after.clone());
        }
        let mutation = match (&change.before, &change.after) {
            (Some(before), Some(after)) => {
                let selected: Vec<_> = entries
                    .iter()
                    .filter(|(_, image)| image == before)
                    .collect();
                if selected.len() != 1
                    || (after.name.starts_with("tze_hud-undo-")
                        && entries.iter().any(|(_, image)| image.name == after.name))
                    || entries.iter().any(|(_, image)| image == after)
                {
                    return Err("exact rule/rename alias changed externally".into());
                }
                let mut renamed = before.clone();
                renamed.name.clone_from(&after.name);
                if renamed != *after {
                    return Err("only a full-image-preserving rename is allowed".into());
                }
                unsafe { selected[0].0.SetName(&BSTR::from(after.name.as_str())) }
                    .map_err(|e| failure("exact firewall rule rename", e))
            }
            (Some(before), None) => {
                if entries.iter().filter(|(_, image)| image == before).count() != 1
                    || entries
                        .iter()
                        .filter(|(_, image)| image.name == before.name)
                        .count()
                        != 1
                {
                    return Err("rule name is not unique; Remove(name) was refused".into());
                }
                unsafe { self.collection.Remove(&BSTR::from(before.name.as_str())) }
                    .map_err(|e| failure("unique firewall rule removal", e))
            }
            (None, Some(after)) => {
                if entries.iter().any(|(_, image)| image.name == after.name) {
                    return Err("rule add name collision".into());
                }
                let object = standalone(after)?;
                unsafe { self.collection.Add(&object) }
                    .map_err(|e| failure("unique firewall rule addition", e))
            }
            _ => Err("empty firewall write".into()),
        };
        mutation?;
        self.actual_changes.push(match (&change.before, &change.after) {
            (Some(before), Some(after)) => format!("Renamed {} -> {}", before.name, after.name),
            (Some(before), None) => format!("Removed {} (program {}, protocol {}, ports {:?}, remotes {}, profiles {}, enabled {})", before.name, before.application, before.protocol, before.local_ports, before.remote_addresses, before.profiles, before.enabled),
            (None, Some(after)) => format!("Added {}", after.name),
            _ => unreachable!(),
        });
        let mut expected: Vec<_> = expected
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(|e| failure("rule verification encoding", e))?;
        let mut actual: Vec<_> = self
            .rules()?
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(|e| failure("rule verification encoding", e))?;
        expected.sort();
        actual.sort();
        if actual != expected {
            return Err("full rule multiset changed unexpectedly; external policy was not overwritten by recovery".into());
        }
        Ok(())
    }
    fn allowed(&mut self, spec: &RuleSpec) -> Result<bool, String> {
        let current = unsafe { self.policy.CurrentProfileTypes() }
            .map_err(|e| failure("current firewall profiles", e))? as u32;
        let mut snapshot = FirewallSnapshot {
            current,
            ..Default::default()
        };
        for bit in [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC] {
            let profile = NET_FW_PROFILE_TYPE2(bit as i32);
            if unsafe { self.policy.get_FirewallEnabled(profile) }
                .map_err(|e| failure("firewall profile enablement", e))?
                .as_bool()
            {
                snapshot.enabled |= bit;
                if current & bit != 0
                    && unsafe { self.policy.get_BlockAllInboundTraffic(profile) }
                        .map_err(|e| failure("block-all-inbound policy", e))?
                        .as_bool()
                {
                    return Ok(false);
                }
            }
            if unsafe { self.policy.get_DefaultInboundAction(profile) }
                .map_err(|e| failure("default inbound policy", e))?
                == NET_FW_ACTION_BLOCK
            {
                snapshot.default_block |= bit;
            }
        }
        for rule in self.rules()? {
            let application = if rule.application.is_empty() {
                None
            } else if self.same_program(&rule.application, &spec.program)? {
                Some(spec.program.clone())
            } else {
                Some(self.expanded(&rule.application)?)
            };
            snapshot.rules.push(Rule {
                name: rule.name,
                enabled: rule.enabled,
                inbound: rule.direction == 1,
                allow: rule.action == 1,
                profiles: rule.profiles as u32,
                application,
                protocol: rule.protocol,
                local_ports: rule.local_ports,
                remote_addresses: rule.remote_addresses,
            });
        }
        Ok(matches!(
            decide(true, &Ok(snapshot), Path::new(&spec.program), &spec.ports),
            TailnetInbound::Allowed
        ))
    }
}

/// Best-effort, bounded cleanup during an explicit operation only. A result is
/// deleted only after age, dead original parent, strict schema, nonce-name and
/// exact protected owner/DACL verification. No journal/lock/foreign file is used.
fn cleanup_orphan_results(root: &Root, current_nonce: &str) {
    let Ok(entries) = std::fs::read_dir(&root.path) else {
        return;
    };
    let mut removed = 0;
    for entry in entries.take(64).flatten() {
        if removed >= 8 {
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(nonce) = name
            .strip_prefix("result-")
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if nonce == current_nonce
            || nonce.len() != 32
            || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file()
            || metadata
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age.as_secs() < 86_400)
        {
            continue;
        }
        let Ok(path) = path_wide(&entry.path()) else {
            continue;
        };
        let Ok(handle) = (unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                FILE_GENERIC_READ.0 | READ_CONTROL.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }) else {
            continue;
        };
        let file = unsafe { File::from_raw_handle(handle.0) };
        let Ok(bytes) = bounded_read(&file) else {
            continue;
        };
        let Ok(report) = serde_json::from_slice::<ChildResult>(&bytes) else {
            continue;
        };
        if report.version != VERSION
            || report.request.nonce != nonce
            || report.request.validate().is_err()
            || verify_file(&file, &result_acl(&report.original_sid)).is_err()
        {
            continue;
        }
        let gone = match unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                report.request.parent_pid,
            )
        } {
            Ok(handle) => process_created(Handle(handle).0)
                .is_ok_and(|created| created != report.request.parent_created),
            Err(error) => {
                error.code() == ::windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0)
            }
        };
        if !gone {
            continue;
        }
        drop(file);
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_backup_roundtrips_through_windows_com() {
        // Standalone COM objects only. No INetFwPolicy2, INetFwRules::Add/Remove,
        // elevation, UAC prompt, or machine policy mutation occurs. Privilege
        // checks below modify only private duplicate tokens, never a live
        // process/thread token or an account's assigned privileges.
        std::thread::spawn(|| {
            let _com = Com::init().unwrap();
            fn duplicate_fixture_token(access: TOKEN_ACCESS_MASK) -> Handle {
                let source = token(unsafe { GetCurrentProcess() }).unwrap();
                let mut duplicate = HANDLE::default();
                unsafe {
                    DuplicateTokenEx(
                        source.0,
                        access,
                        None,
                        SecurityImpersonation,
                        TokenImpersonation,
                        &mut duplicate,
                    )
                }
                .unwrap();
                Handle(duplicate)
            }
            // Use the already-assigned change-notification privilege on an
            // isolated duplicate: CI needs no administrator/SeDebug assignment.
            // These calls exercise the production guard and checked restore,
            // while the original cross-account UAC remains an owner exercise.
            let duplicate = duplicate_fixture_token(TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES);
            let mut luid = LUID::default();
            unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_CHANGE_NOTIFY_NAME, &mut luid) }
                .unwrap();
            let disabled = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: TOKEN_PRIVILEGES_ATTRIBUTES(0),
                }],
            };
            unsafe {
                SetLastError(ERROR_SUCCESS);
                AdjustTokenPrivileges(duplicate.0, false, Some(&disabled), 0, None, None).unwrap();
            }
            assert_eq!(unsafe { GetLastError() }, ERROR_SUCCESS);
            {
                let mut scope = PrivilegeScope::enable(&duplicate, SE_CHANGE_NOTIFY_NAME).unwrap();
                assert_eq!(scope.previous.PrivilegeCount, 1);
                assert!(
                    !scope.previous.Privileges[0]
                        .Attributes
                        .contains(SE_PRIVILEGE_ENABLED)
                );
                // An already-enabled privilege has no changed previous state;
                // its restoration must leave the outer scope enabled.
                let mut enabled =
                    PrivilegeScope::enable(&duplicate, SE_CHANGE_NOTIFY_NAME).unwrap();
                assert_eq!(enabled.previous.PrivilegeCount, 0);
                enabled.restore().unwrap();
                scope.restore().unwrap();
            }
            for outcome in [Ok(()), Err("injected token-open failure".into())] {
                assert_eq!(
                    with_privilege(&duplicate, SE_CHANGE_NOTIFY_NAME, || outcome.clone()),
                    outcome
                );
                let mut restored =
                    PrivilegeScope::enable(&duplicate, SE_CHANGE_NOTIFY_NAME).unwrap();
                assert_eq!(restored.previous.PrivilegeCount, 1);
                assert!(
                    !restored.previous.Privileges[0]
                        .Attributes
                        .contains(SE_PRIVILEGE_ENABLED)
                );
                restored.restore().unwrap();
            }
            assert!(
                std::panic::catch_unwind(|| {
                    let _: Result<(), String> =
                        with_privilege(&duplicate, SE_CHANGE_NOTIFY_NAME, || {
                            panic!("injected acquisition unwind")
                        });
                })
                .is_err()
            );
            {
                let mut restored =
                    PrivilegeScope::enable(&duplicate, SE_CHANGE_NOTIFY_NAME).unwrap();
                assert_eq!(restored.previous.PrivilegeCount, 1);
                assert!(
                    !restored.previous.Privileges[0]
                        .Attributes
                        .contains(SE_PRIVILEGE_ENABLED)
                );
                restored.restore().unwrap();
            }
            let read_only = duplicate_fixture_token(TOKEN_QUERY);
            assert!(PrivilegeScope::enable(&read_only, SE_CHANGE_NOTIFY_NAME).is_err());
            // Removing a privilege from this disposable duplicate makes the
            // real success-with-NOT_ALL_ASSIGNED case deterministic. Nothing
            // can grant it back, and the guarded operation must not be called.
            let removed = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_REMOVED,
                }],
            };
            unsafe {
                SetLastError(ERROR_SUCCESS);
                AdjustTokenPrivileges(duplicate.0, false, Some(&removed), 0, None, None).unwrap();
            }
            assert_eq!(unsafe { GetLastError() }, ERROR_SUCCESS);
            let called = std::cell::Cell::new(false);
            assert!(
                with_privilege(&duplicate, SE_CHANGE_NOTIFY_NAME, || {
                    called.set(true);
                    Ok(())
                })
                .is_err()
            );
            assert!(!called.get());

            // A read-only folder query exercises the production task-allocation
            // lifetime without opening helper files or obtaining machine policy.
            assert!(program_data_path().unwrap().is_absolute());
            let program = canonical_file(&std::env::current_exe().unwrap()).unwrap();
            let base = prepare_owned_allow(
                &RuleSpec::new(program, [9090, 50051])
                    .unwrap()
                    .rule("native-fixture"),
            )
            .unwrap();
            for protocol in [6, 17, 256, 1, 58] {
                let mut rule = base.clone();
                rule.protocol = protocol;
                rule.action = 0;
                rule.name = format!("standalone fixture {protocol}");
                rule.description = "Unicode Δ backup".into();
                rule.enabled = false;
                rule.profiles = 2;
                rule.grouping = "fixture group".into();
                rule.local_ports = matches!(protocol, 6 | 17).then(|| "9090,50051".into());
                rule.remote_ports = matches!(protocol, 6 | 17).then(|| "*".into());
                rule.icmp = matches!(protocol, 1 | 58).then(|| "*".into());
                let saved = image(&create_rule(&rule).unwrap()).unwrap();
                assert_eq!(saved.name, rule.name);
                assert_eq!(saved.description, rule.description);
                assert_eq!(
                    (saved.protocol, saved.enabled, saved.profiles),
                    (rule.protocol, false, 2)
                );
                assert_eq!(saved.interfaces, rule.interfaces);
                assert_eq!(image(&standalone(&saved).unwrap()).unwrap(), saved);
            }
            for edge_options in [0, 1, 2, 3] {
                let mut rule = base.clone();
                rule.name = format!("standalone edge fixture {edge_options}");
                rule.edge_options = edge_options;
                let saved = image(&create_rule(&rule).unwrap()).unwrap();
                assert_eq!(saved.edge_options, edge_options);
                assert_eq!(image(&standalone(&saved).unwrap()).unwrap(), saved);
            }
            let mut secured = base.clone();
            secured.name = "standalone secured fixture".into();
            secured.secure_flags = NET_FW_AUTHENTICATE_WITH_INTEGRITY.0;
            secured.remote_users = "D:(A;;CC;;;WD)".into();
            secured.remote_machines = "D:(A;;CC;;;WD)".into();
            secured.user_owner = "S-1-5-18".into();
            let saved = image(&create_rule(&secured).unwrap()).unwrap();
            assert_eq!(saved.secure_flags, secured.secure_flags);
            assert!(
                !saved.remote_users.is_empty()
                    && !saved.remote_machines.is_empty()
                    && !saved.user_owner.is_empty()
            );
            assert_eq!(image(&standalone(&saved).unwrap()).unwrap(), saved);
            let mut unsupported = base.clone();
            unsupported.package_id = "unsupported package".into();
            assert!(standalone(&unsupported).is_err());
            // Native Interfaces setters resolve real adapter friendly names.
            // A fabricated name is a refusal case, not a valid host fixture.
            // Keep typed multi-name/Unicode codec coverage independent of host
            // adapters; restoration still requires native preflight of all saved
            // properties before any policy mutation.
            let mut missing_interface = base.clone();
            missing_interface.interfaces = Some(vec!["fixture interface".into()]);
            let error = match create_rule(&missing_interface) {
                Ok(_) => panic!("a nonexistent native interface was accepted"),
                Err(error) => error,
            };
            assert!(error.contains("SetInterfaces:"), "{error}");
            let names = vec!["Ethernet".into(), "Unicode Δ".into()];
            assert_eq!(
                interfaces(&interface_variant(&names).unwrap()).unwrap(),
                Some(names)
            );
            assert!(interfaces(&VARIANT::from(42i32)).is_err());

            // Exercise the native parser rather than reimplementing its inverse.
            for argument in [
                "",
                "space path",
                r"C:\HUD Δ\tze_hud.exe",
                "quoted\"value",
                "trailing\\",
                "\\\"",
                "尾",
            ] {
                let command = wide(&format!(
                    "fixture.exe {}",
                    quote_argument(argument).unwrap()
                ))
                .unwrap();
                let mut count = 0;
                let argv = unsafe { CommandLineToArgvW(PCWSTR(command.as_ptr()), &mut count) };
                assert!(!argv.is_null());
                let _memory = LocalAllocation(argv.cast());
                assert_eq!(count, 2);
                assert_eq!(unsafe { (*argv.add(1)).to_string() }.unwrap(), argument);
            }
        })
        .join()
        .unwrap();
    }
}
