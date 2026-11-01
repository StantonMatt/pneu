//! Local Windows filesystems only. All reparse points (including OneDrive
//! placeholders) fail closed; cloud compatibility needs separate acceptance.
//! This includes user-chosen received-file folders and input files: mapped remote
//! drives are unsupported even when hidden behind a local drive letter or subst.
//! Private objects have a protected owner + SYSTEM DACL, never Administrators.
//! Candidate durability: write-through create, flush data, relative POSIX rename,
//! flush file and target directory before receipt. Crash acceptance is pending.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom},
    mem::{offset_of, size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileExt, OpenOptionsExt},
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
    sync::Arc,
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::*,
        System::SystemServices::{FILE_FS_DEVICE_INFORMATION, FILE_REMOTE_DEVICE},
    },
    Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{
            Ioctl::{
                FILE_DEVICE_NETWORK, FILE_DEVICE_NETWORK_FILE_SYSTEM,
                FILE_DEVICE_NETWORK_REDIRECTOR,
            },
            Threading::*,
        },
    },
};
const SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const READ: u32 = GENERIC_READ | READ_CONTROL | SYNCHRONIZE;
const WRITE: u32 = READ | GENERIC_WRITE;
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}
fn win(ok: i32) -> std::io::Result<()> {
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn nt(status: i32) -> std::io::Result<()> {
    if status < 0 {
        // SAFETY: pure NTSTATUS-to-Win32 conversion.
        Err(std::io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ))
    } else {
        Ok(())
    }
}
struct Local(*mut std::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: this allocation came from a LocalAlloc-family security API.
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn token_sid(class: TOKEN_INFORMATION_CLASS) -> Result<String> {
    // SAFETY: buffers are sized by GetTokenInformation, u64-aligned, and kept
    // alive until the embedded SID has been converted. File owns the token.
    unsafe {
        let mut token = null_mut();
        win(OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY,
            &mut token,
        ))?;
        let token = File::from_raw_handle(token);
        let mut len = 0;
        GetTokenInformation(token.as_raw_handle(), class, null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        win(GetTokenInformation(
            token.as_raw_handle(),
            class,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        ))?;
        // TokenUser and TokenOwner both start with a SID pointer.
        sid_string(*(buf.as_ptr().cast::<PSID>()))
    }
}
fn own_sid() -> Result<String> {
    token_sid(TokenUser)
}
unsafe fn sid_string(sid: PSID) -> Result<String> {
    // SAFETY: caller supplies a valid SID retained by the token/descriptor.
    unsafe {
        let mut text = null_mut();
        win(ConvertSidToStringSidW(sid, &mut text))?;
        let _allocation = Local(text.cast());
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, len,
        )))
    }
}
// One policy supplies both the creation descriptor and the exact checker.
// In particular a private directory must propagate both full-control grants to
// files and subdirectories; a safe ACL for the directory alone is insufficient.
fn private_grants(own: &str) -> [(&str, u32, u32); 2] {
    let flags = OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
    [
        (own, FILE_ALL_ACCESS, flags),
        ("S-1-5-18", FILE_ALL_ACCESS, flags),
    ]
}
fn security() -> Result<Local> {
    let sid = own_sid()?;
    // Inheritable grants keep SQLite-created sidecars private from birth.
    // Objects explicitly created here also set SE_DACL_PROTECTED.
    let mut sddl = format!("O:{sid}D:P");
    for (principal, mask, flags) in private_grants(&sid) {
        let inheritance: String = [(OBJECT_INHERIT_ACE, "OI"), (CONTAINER_INHERIT_ACE, "CI")]
            .into_iter()
            .filter_map(|(flag, text)| (flags & flag != 0).then_some(text))
            .collect();
        sddl.push_str(&format!("(A;{inheritance};0x{mask:x};;;{principal})"));
    }
    let sddl = wide(OsStr::new(&sddl));
    let mut sd = null_mut();
    // SAFETY: NUL-terminated SDDL, valid output pointer; Local frees result.
    unsafe {
        win(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            null_mut(),
        ))?;
    }
    Ok(Local(sd))
}
pub(crate) mod private {
    use super::*;
    pub(crate) fn created(file: &File) -> Result<()> {
        check(file)
    }
    pub(crate) fn check(file: &File) -> Result<()> {
        check_policy(file, false)
    }
    // SQLite can recreate WAL/SHM/journal using the OS inherited descriptor.
    // Only this contextual check permits inheritance: the caller retains and
    // verifies the protected parent and opens the child relative to it. No ACL
    // is repaired, and generic private-object checks remain strict.
    pub(crate) fn check_in(parent: &File, name: &OsStr) -> Result<()> {
        check(parent)?;
        let file = open_read_at(parent, name)?;
        ensure!(file.metadata()?.is_file(), "private child must be regular");
        if check(&file).is_ok() {
            return Ok(());
        }
        check_policy(&file, true)
    }
    fn check_policy(file: &File, inherited: bool) -> Result<()> {
        let own = own_sid()?;
        // Elevated tokens may assign their default owner (Administrators),
        // rather than TokenUser, to OS-created children. Accept it only for
        // exact inherited user/SYSTEM grants in a checked private parent.
        let expected_owner = if inherited { "S-1-5-32-544" } else { &own };
        // SAFETY: GetSecurityInfo owns the returned descriptor; owner, ACL and
        // ACE pointers are borrowed from it until Local frees it below.
        unsafe {
            let (mut owner, mut acl, mut sd) = (null_mut(), null_mut(), null_mut());
            let code = GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut sd,
            );
            if code != 0 {
                return Err(std::io::Error::from_raw_os_error(code as i32).into());
            }
            let _allocation = Local(sd);
            let (mut control, mut revision) = (0, 0);
            win(GetSecurityDescriptorControl(
                sd,
                &mut control,
                &mut revision,
            ))?;
            ensure!(
                sid_string(owner)? == expected_owner || (inherited && sid_string(owner)? == own),
                "private object has foreign owner"
            );
            ensure!(
                !acl.is_null() && (inherited || control & SE_DACL_PROTECTED != 0),
                "private DACL must be protected"
            );
            ensure!(
                !inherited || control & SE_DACL_PROTECTED == 0,
                "inherited private DACL must be unprotected"
            );
            let expected = private_grants(&own);
            ensure!(
                (*acl).AceCount as usize == expected.len(),
                "private DACL must have exactly user and SYSTEM grants"
            );
            let mut seen = [false; 2];
            for index in 0..(*acl).AceCount as u32 {
                let mut raw = null_mut();
                win(GetAce(acl, index, &mut raw))?;
                let header = &*raw.cast::<windows_sys::Win32::Security::ACE_HEADER>();
                ensure!(
                    header.AceType == 0
                        && header.AceSize as usize >= size_of::<ACCESS_ALLOWED_ACE>(),
                    "private DACL has an unexpected ACE"
                );
                let ace = &*raw.cast::<ACCESS_ALLOWED_ACE>();
                let sid = sid_string((&ace.SidStart as *const u32).cast_mut().cast())?;
                let grant = expected
                    .iter()
                    .position(|(principal, _, _)| *principal == sid)
                    .context("private DACL grants another principal")?;
                let (_, mask, flags) = expected[grant];
                ensure!(
                    !seen[grant]
                        && ace.Mask == mask
                        && u32::from(header.AceFlags)
                            == if inherited { INHERITED_ACE } else { flags },
                    "private DACL has an unexpected grant or inheritance policy"
                );
                seen[grant] = true;
            }
            ensure!(seen.into_iter().all(|grant| grant), "missing private grant");
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn check_path(path: &Path) -> Result<()> {
        check(&open_read(path)?)
    }
    #[cfg(test)]
    pub(crate) fn open(path: &Path) -> Result<File> {
        open_lock(path)
    }
}
// Paths handed to SQLite/std/desktop APIs must name the same objects as NT
// relative opens. Validate before any creation; never silently trim or map a
// configured path. 8.3 aliases do not change NT vs Win32 resolution; containment
// uses directory identities below, so aliases cannot cross its boundary.
pub(crate) fn validate_name(name: &OsStr) -> Result<()> {
    let text = name
        .to_str()
        .context("Windows paths must contain valid Unicode")?;
    ensure!(
        crate::storage::windows_names::is_safe(text),
        "ambiguous Windows path component: {name:?} (trailing dots/spaces, device names and Win32 special characters are forbidden)"
    );
    Ok(())
}
fn drive_root(path: &Path) -> Result<PathBuf> {
    let mut parts = path.components();
    let Some(Component::Prefix(prefix)) = parts.next() else {
        anyhow::bail!("Windows path must be an absolute local drive path")
    };
    ensure!(
        matches!(prefix.kind(), Prefix::Disk(_)) && parts.next() == Some(Component::RootDir),
        "Windows path must be an absolute local drive path; UNC, verbatim and device namespaces are forbidden"
    );
    Ok(path.components().take(2).collect())
}
pub(crate) fn validate_path(path: &Path) -> Result<()> {
    drive_root(path)?;
    for part in path.components().skip(2) {
        match part {
            Component::Normal(name) => validate_name(name)?,
            Component::CurDir | Component::ParentDir => {}
            _ => anyhow::bail!("unsafe Windows path component"),
        }
    }
    Ok(())
}
fn open_at(
    parent: &File,
    name: &OsStr,
    access: u32,
    create: bool,
    dir: Option<bool>,
) -> Result<File> {
    validate_name(name)?;
    native_open_at(parent, name, access, create, dir)
}
fn native_open_at(
    parent: &File,
    name: &OsStr,
    access: u32,
    create: bool,
    dir: Option<bool>,
) -> Result<File> {
    native_open_at_shared(parent, name, access, create, dir, SHARE)
}
fn native_open_at_shared(
    parent: &File,
    name: &OsStr,
    access: u32,
    create: bool,
    dir: Option<bool>,
    share: u32,
) -> Result<File> {
    let mut n = wide(name);
    let sd = if create { Some(security()?) } else { None };
    let u = UNICODE_STRING {
        Length: ((n.len() - 1) * 2) as u16,
        MaximumLength: (n.len() * 2) as u16,
        Buffer: n.as_mut_ptr(),
    };
    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &u,
        Attributes: 0x40,
        SecurityDescriptor: sd.as_ref().map_or(null(), |s| s.0.cast()),
        SecurityQualityOfService: null(),
    };
    // SAFETY: borrowed root, Unicode and security descriptor outlive the
    // synchronous call. A successful new handle is transferred once to File.
    let file = unsafe {
        let mut handle = null_mut();
        let mut ios = zeroed();
        nt(NtCreateFile(
            &mut handle,
            access,
            &oa,
            &mut ios,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            if create { FILE_CREATE } else { FILE_OPEN },
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | if create { FILE_WRITE_THROUGH } else { 0 }
                | if dir == Some(true) {
                    FILE_DIRECTORY_FILE
                } else {
                    0
                },
            null(),
            0,
        ))?;
        File::from_raw_handle(handle)
    };
    inspect(&file)?;
    if let Some(directory) = dir {
        let meta = file.metadata()?;
        if directory && !meta.is_dir() {
            return Err(std::io::Error::from_raw_os_error(ERROR_DIRECTORY as i32).into());
        }
        ensure!(directory || meta.is_file(), "unexpected file type");
    }
    if create {
        private::created(&file)?;
    }
    Ok(file)
}
fn inspect(file: &File) -> Result<()> {
    let info: FILE_ATTRIBUTE_TAG_INFO = query(file, FileAttributeTagInfo)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::from_raw_os_error(ERROR_CANT_ACCESS_FILE as i32).into());
    }
    // Disk handles only: reject pipes, devices and other special streams.
    // SAFETY: borrowed live handle.
    ensure!(
        unsafe { GetFileType(file.as_raw_handle()) } == FILE_TYPE_DISK,
        "only disk files are supported"
    );
    // FILE_TYPE_DISK also describes SMB redirector handles. Query the actual
    // opened device, not GetDriveType/path syntax, so mapped and subst drive
    // aliases cannot turn remote storage into an admitted local root. Query
    // failure is fatal. Inspecting relative opens also covers every sibling API.
    let mut device: FILE_FS_DEVICE_INFORMATION = unsafe { zeroed() };
    let mut ios = unsafe { zeroed() };
    // SAFETY: live borrowed handle and correctly sized/aligned POD outputs.
    unsafe {
        nt(NtQueryVolumeInformationFile(
            file.as_raw_handle(),
            &mut ios,
            (&mut device as *mut FILE_FS_DEVICE_INFORMATION).cast(),
            size_of::<FILE_FS_DEVICE_INFORMATION>() as u32,
            FileFsDeviceInformation,
        ))?;
    }
    ensure!(
        device.Characteristics & FILE_REMOTE_DEVICE == 0
            && !matches!(
                device.DeviceType,
                FILE_DEVICE_NETWORK
                    | FILE_DEVICE_NETWORK_FILE_SYSTEM
                    | FILE_DEVICE_NETWORK_REDIRECTOR
            ),
        "remote storage is unsupported; only local filesystems are supported"
    );
    Ok(())
}
fn query<T: Copy>(file: &File, class: FILE_INFO_BY_HANDLE_CLASS) -> Result<T> {
    // SAFETY: call sites pair the documented info class with its POD struct;
    // the API initializes the output before it is read on success.
    unsafe {
        let mut info: T = zeroed();
        win(GetFileInformationByHandleEx(
            file.as_raw_handle(),
            class,
            (&mut info as *mut T).cast(),
            size_of::<T>() as u32,
        ))?;
        Ok(info)
    }
}
fn root_file(root: &Path, access: u32) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .access_mode(access)
        .share_mode(SHARE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(root)
}
fn walk(path: &Path, create: bool, private_leaf: bool, leaf_access: u32) -> Result<File> {
    walk_components(path, create, private_leaf, leaf_access, false)
}
fn walk_components(
    path: &Path,
    create: bool,
    private_leaf: bool,
    leaf_access: u32,
    parents: bool,
) -> Result<File> {
    validate_path(path)?;
    let root = drive_root(path)?;
    let parts: Vec<_> = path.components().skip(2).collect();
    let first = root_file(&root, if parts.is_empty() { leaf_access } else { READ })?;
    inspect(&first)?;
    physical_chain(&first)?;
    // Retain every validated ancestor. NT rejects a literal ".." in a relative
    // NtCreateFile name. Pop to the already validated parent instead, without
    // lexically erasing a reparse point encountered before that component.
    let mut chain = vec![first];
    let mut names: Vec<OsString> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let name = match part {
            Component::Normal(name) => *name,
            Component::ParentDir if parents => {
                if chain.len() > 1 {
                    chain.pop();
                    names.pop();
                }
                continue;
            }
            Component::CurDir => continue,
            _ => anyhow::bail!("unsafe folder component"),
        };
        let current = chain.last().expect("drive root");
        let last = i + 1 == parts.len();
        let access = if last { leaf_access } else { READ };
        let next = match open_at(current, name, access, false, Some(true)) {
            Ok(file) => file,
            Err(e) if create && is_missing(&e) => {
                // Increase access through a retained ancestor, never a display
                // path; verify identity before creating the new link.
                let parent = if chain.len() > 1 {
                    open_at(
                        &chain[chain.len() - 2],
                        names.last().expect("parent name"),
                        WRITE,
                        false,
                        Some(true),
                    )?
                } else {
                    reopen(current, WRITE)?
                };
                inspect(&parent)?;
                ensure!(
                    identity(&parent)? == identity(current)?,
                    "parent was replaced"
                );
                let file = match open_at(&parent, name, WRITE, true, Some(true)) {
                    Ok(file) => file,
                    Err(e) if is_exists(&e) => open_at(&parent, name, WRITE, false, Some(true))?,
                    Err(e) => return Err(e),
                };
                parent.sync_all()?;
                file.sync_all()?;
                file
            }
            Err(e) => return Err(e),
        };
        names.push(name.to_os_string());
        chain.push(next);
    }
    let current = chain.pop().expect("drive root");
    physical_chain(&current)?;
    if private_leaf {
        private::check(&current)?;
    }
    Ok(current)
}
// Resolve each existing component without following reparses. The identities
// detect case, 8.3 and drive-letter aliases. Preserve missing suffixes so config
// admission also covers destinations not yet created. ".." never skips an open.
fn folder_location(path: &Path) -> Result<(Vec<File>, Vec<OsString>)> {
    validate_path(path)?;
    let first = root_file(&drive_root(path)?, READ)?;
    inspect(&first)?;
    physical_chain(&first)?;
    let mut chain = vec![first];
    let mut missing = Vec::new();
    for part in path.components().skip(2) {
        match part {
            Component::Normal(name) => {
                if missing.is_empty() {
                    match open_at(
                        chain.last().expect("drive root"),
                        name,
                        READ,
                        false,
                        Some(true),
                    ) {
                        Ok(file) => {
                            chain.push(file);
                            continue;
                        }
                        Err(e) if is_missing(&e) => {}
                        Err(e) => return Err(e),
                    }
                }
                missing.push(name.to_os_string());
            }
            Component::ParentDir => {
                if !missing.is_empty() {
                    missing.pop();
                } else if chain.len() > 1 {
                    chain.pop();
                }
            }
            Component::CurDir => {}
            _ => anyhow::bail!("unsafe folder component"),
        }
    }
    Ok((physical_chain(chain.last().expect("drive root"))?, missing))
}
pub(crate) fn folder_overlaps_cache(save: &Path, cache: &Path) -> Result<bool> {
    location_overlaps(folder_location(save)?, folder_location(cache)?)
}
/// Admission may retain an unavailable user destination for later recovery.
/// Resolve private state first: only save-folder availability is provisional.
pub(crate) fn configured_folder_overlaps_cache(save: &Path, cache: &Path) -> Result<bool> {
    validate_path(save).context("invalid save folder")?;
    let cache = folder_location(cache).context("cannot validate cache directory")?;
    let save = match folder_location(save) {
        Ok(location) => location,
        Err(error) if save_folder_unavailable(&error) => return Ok(false),
        Err(error) => return Err(error.context("cannot validate save folder")),
    };
    location_overlaps(save, cache)
}
fn save_folder_unavailable(error: &anyhow::Error) -> bool {
    // Do not use ErrorKind::PermissionDenied: reparse rejection and other
    // policy failures must never be mistaken for unavailable storage. Unknown
    // errors, remote devices and identity mismatches still fail admission.
    error.chain().any(|cause| {
        matches!(
            code(cause),
            Some(
                ERROR_FILE_NOT_FOUND
                    | ERROR_PATH_NOT_FOUND
                    | ERROR_NOT_READY
                    | ERROR_DEVICE_NOT_CONNECTED
                    | ERROR_INVALID_DRIVE
                    | ERROR_ACCESS_DENIED
                    | ERROR_BAD_NETPATH
            )
        )
    })
}
pub(crate) fn retained_folder_overlaps_cache(save: &Folder, cache: &Path) -> Result<bool> {
    location_overlaps(
        (physical_chain(&save.file)?, Vec::new()),
        folder_location(cache)?,
    )
}
fn location_overlaps(
    (save, save_missing): (Vec<File>, Vec<OsString>),
    (cache, cache_missing): (Vec<File>, Vec<OsString>),
) -> Result<bool> {
    let cache_id = identity(cache.last().expect("drive root"))?;
    if cache_missing.is_empty() {
        for directory in &save {
            if identity(directory)? == cache_id {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if identity(save.last().expect("drive root"))? != cache_id
        || save_missing.len() < cache_missing.len()
    {
        return Ok(false);
    }
    for (save, cache) in save_missing.iter().zip(&cache_missing) {
        let save = wide(save);
        let cache = wide(cache);
        // SAFETY: counted UTF-16 buffers; ordinal Windows case comparison is
        // conservative admission for names which have no identity yet.
        let comparison = unsafe {
            windows_sys::Win32::Globalization::CompareStringOrdinal(
                save.as_ptr(),
                (save.len() - 1) as i32,
                cache.as_ptr(),
                (cache.len() - 1) as i32,
                1,
            )
        };
        ensure!(comparison != 0, "cannot compare Windows path components");
        if comparison != 2 {
            return Ok(false);
        }
    }
    Ok(true)
}
#[derive(Clone, Debug)]
pub(crate) struct Folder {
    pub file: Arc<File>,
}
impl Folder {
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        Ok(Self {
            file: Arc::new(walk(path, create, false, WRITE)?),
        })
    }
    pub fn open_private(path: &Path) -> Result<Self> {
        Ok(Self {
            file: Arc::new(walk(path, true, true, WRITE)?),
        })
    }
    pub fn path(&self) -> Result<PathBuf> {
        directory_path(&self.file)
    }
    pub fn identity(&self) -> Result<Identity> {
        identity(&self.file)
    }
    pub fn space(&self) -> Result<u64> {
        #[repr(C)]
        struct SizeInfo {
            total: i64,
            available: i64,
            actual_available: i64,
            sectors: u32,
            bytes: u32,
        }
        // SAFETY: live directory and correctly sized/aligned output structures.
        unsafe {
            let mut info: SizeInfo = zeroed();
            let mut ios = zeroed();
            nt(NtQueryVolumeInformationFile(
                self.file.as_raw_handle(),
                &mut ios,
                (&mut info as *mut SizeInfo).cast(),
                size_of::<SizeInfo>() as u32,
                FileFsFullSizeInformation,
            ))?;
            Ok((info.available.max(0) as u64)
                .saturating_mul(info.sectors as u64)
                .saturating_mul(info.bytes as u64))
        }
    }
}
// Only handle recovery constructs this type. Public/config paths still reject
// verbatim namespaces. GUID strings locate a volume; file IDs prove identity.
struct CanonicalPath(PathBuf);
impl CanonicalPath {
    fn from_handle(file: &File) -> Result<Self> {
        let mut buf = vec![0u16; 32768];
        // SAFETY: live handle and writable counted UTF-16 output.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_GUID,
            )
        } as usize;
        ensure!(
            len != 0 && len < buf.len(),
            "cannot recover canonical volume GUID path: {}",
            std::io::Error::last_os_error()
        );
        let path = PathBuf::from(OsString::from_wide(&buf[..len]));
        let text = path.to_str().context("canonical path is not Unicode")?;
        ensure!(
            text.starts_with(r"\\?\Volume{") && text.contains(r"}\"),
            "canonical path is not a local volume GUID path"
        );
        Ok(Self(path))
    }
    fn root_and_names(&self) -> Result<(PathBuf, Vec<OsString>)> {
        let text = self.0.to_str().context("canonical path is not Unicode")?;
        let end = text.find(r"}\").context("missing volume GUID root")? + 2;
        let root = PathBuf::from(&text[..end]);
        let names = text[end..]
            .split('\\')
            .filter(|n| !n.is_empty())
            .map(OsString::from)
            .collect();
        Ok((root, names))
    }
}
fn physical_chain(file: &File) -> Result<Vec<File>> {
    let canonical = CanonicalPath::from_handle(file)?;
    let (root, names) = canonical.root_and_names()?;
    let first = root_file(&root, READ)?;
    inspect(&first)?;
    let mut chain = vec![first];
    for name in names {
        chain.push(open_at(
            chain.last().expect("volume root"),
            &name,
            READ,
            false,
            Some(true),
        )?);
    }
    ensure!(
        identity(chain.last().expect("volume root"))? == identity(file)?,
        "canonical directory was replaced during physical ancestry validation"
    );
    Ok(chain)
}

// Far beyond SQLite's PENDING/RESERVED/SHARED lock bytes at 0x40000000.
const STORE_LOCK_OFFSET: u64 = 1 << 63;
fn database_lock(file: &File) -> Result<()> {
    let mut overlapped = windows_sys::Win32::System::IO::OVERLAPPED::default();
    overlapped.Anonymous.Anonymous.Offset = STORE_LOCK_OFFSET as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (STORE_LOCK_OFFSET >> 32) as u32;
    // SAFETY: synchronous one-byte lock; OVERLAPPED only supplies the offset.
    unsafe {
        win(LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        ))
    }
    .context("data store is already owned by another daemon")
}
fn single_link(file: &File) -> Result<()> {
    let info: FILE_STANDARD_INFO = query(file, FileStandardInfo)?;
    ensure!(
        info.NumberOfLinks == 1 && !info.DeletePending,
        "database and sidecars must have exactly one link and must not be delete-pending"
    );
    Ok(())
}
fn database_path_fits(path: &Path) -> Result<()> {
    // Check before any private precreation, including room for SQLite sidecars.
    // SAFETY: static terminated VFS name; bundled SQLite registers this VFS.
    let vfs = unsafe { rusqlite::ffi::sqlite3_vfs_find(c"win32-longpath".as_ptr()) };
    ensure!(!vfs.is_null(), "SQLite win32-longpath VFS is unavailable");
    let limit = unsafe { (*vfs).mxPathname } as usize;
    ensure!(
        path.to_str().context("database path is not Unicode")?.len() + 8 < limit
            && path.as_os_str().encode_wide().count() + 8 < 32767,
        "canonical database path exceeds SQLite win32-longpath limit"
    );
    Ok(())
}
fn reconcile_database(conn: &rusqlite::Connection, locked: &File) -> Result<()> {
    let mut raw: HANDLE = null_mut();
    // SAFETY: live connection; SQLite writes its borrowed main-db HANDLE.
    let code = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
            (&mut raw as *mut HANDLE).cast(),
        )
    };
    ensure!(
        code == rusqlite::ffi::SQLITE_OK && !raw.is_null() && raw != INVALID_HANDLE_VALUE,
        "cannot reconcile SQLite main database handle (code {code})"
    );
    // Borrow only; SQLite owns and closes this handle.
    let sqlite_file = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(raw) });
    inspect(&sqlite_file)?;
    ensure!(
        identity(&sqlite_file)? == identity(locked)?,
        "SQLite database identity changed before open"
    );
    Ok(())
}
pub(crate) fn open_database(path: &Path) -> Result<(rusqlite::Connection, File, Folder)> {
    open_database_inner(path, || Ok(()), false)
}
fn open_database_inner(
    path: &Path,
    after_precreate: impl FnOnce() -> Result<()>,
    configured_sqlite_path: bool,
) -> Result<(rusqlite::Connection, File, Folder)> {
    validate_path(path).context("invalid database path")?;
    let parent = Folder::open(path.parent().context("database parent")?, false)?;
    private::check(&parent.file)?;
    let name = path.file_name().context("database name")?;
    let parent_path = CanonicalPath::from_handle(&parent.file)?;
    database_path_fits(&parent_path.0.join(name))?;
    // A separate private writer is closed before retaining a read-only lock
    // handle. DELETE access would conflict with SQLite's read/write sharing.
    match create_private_at(&parent.file, name, Access::ReadWrite) {
        Ok(file) => {
            file.sync_all()?;
            parent.file.sync_all()?;
        }
        Err(e) if is_exists(&e) => {}
        Err(e) => return Err(e),
    }
    let locked = open_read_at(&parent.file, name)?;
    ensure!(locked.metadata()?.is_file(), "database must be regular");
    private::check(&locked)?;
    single_link(&locked)?;
    database_lock(&locked)?;
    let canonical = CanonicalPath::from_handle(&locked)?;
    database_path_fits(&canonical.0)?;
    let (root, mut names) = canonical.root_and_names()?;
    let leaf = names.pop().context("database canonical leaf")?;
    // Re-walk the canonical parent and prove that SQLite's sidecar namespace
    // belongs to the checked parent, including when the input used an 8.3 leaf.
    let mut canonical_parent = root_file(&root, READ)?;
    inspect(&canonical_parent)?;
    for name in names {
        canonical_parent = open_at(&canonical_parent, &name, READ, false, Some(true))?;
    }
    ensure!(
        identity(&canonical_parent)? == parent.identity()?,
        "database parent identity changed"
    );
    ensure!(
        identity_at(&parent.file, &leaf)? == identity(&locked)?,
        "database leaf identity changed"
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = leaf.clone();
        sidecar.push(suffix);
        match open_read_at(&parent.file, &sidecar) {
            Ok(file) => {
                private::check_in(&parent.file, &sidecar)?;
                single_link(&file)?;
            }
            Err(e) if is_missing(&e) => {
                let file = create_private_at(&parent.file, &sidecar, Access::ReadWrite)?;
                single_link(&file)?;
            }
            Err(e) => return Err(e),
        }
    }
    after_precreate()?;
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    // Never CREATE here: a changed namespace must not create an unverified DB.
    // The test-only alternative demonstrates that reconciliation fails closed.
    let sqlite_path = if configured_sqlite_path {
        path
    } else {
        &canonical.0
    };
    let conn = rusqlite::Connection::open_with_flags_and_vfs(sqlite_path, flags, "win32-longpath")?;
    reconcile_database(&conn, &locked)?;
    Ok((conn, locked, parent))
}
pub(crate) fn directory_path(file: &File) -> Result<PathBuf> {
    let mut buf = vec![0u16; 32768];
    // SAFETY: live handle and writable UTF-16 buffer of the advertised size.
    let len = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    } as usize;
    ensure!(len != 0 && len < buf.len(), "cannot recover directory path");
    let path = PathBuf::from(OsString::from_wide(&buf[..len]));
    let path = path
        .to_str()
        .and_then(|s| s.strip_prefix(r"\\?\"))
        .map(PathBuf::from)
        .unwrap_or(path);
    ensure!(
        identity(file)? == identity(&walk(&path, false, false, READ)?)?,
        "folder was deleted or replaced"
    );
    Ok(path)
}
fn path_open(path: &Path, access: u32) -> Result<File> {
    validate_path(path)?;
    let parent = walk_components(
        path.parent().context("file parent")?,
        false,
        false,
        READ,
        true,
    )?;
    open_at(
        &parent,
        path.file_name().context("file name")?,
        access,
        false,
        None,
    )
}
/// Read a private entry using the caller's retained parent throughout.
pub(crate) fn private_bytes_at(
    parent: &File,
    name: &OsStr,
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let file = match open_read_at(parent, name) {
        Ok(file) => file,
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) => return Err(e),
    };
    ensure!(file.metadata()?.is_file(), "private entry must be regular");
    private::check(&file)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "private entry exceeds bounded read limit"
    );
    Ok(Some(bytes))
}
pub(crate) fn open_regular_file(path: &Path) -> Result<File> {
    let file = path_open(path, READ)?;
    ensure!(
        file.metadata()?.is_file(),
        "only regular files can be opened"
    );
    Ok(file)
}
pub(crate) fn open_read(path: &Path) -> Result<File> {
    path_open(path, READ)
}
#[cfg(test)]
pub(crate) fn open_secret(path: &Path) -> std::io::Result<File> {
    open_read(path).map_err(io_error)
}
pub(crate) fn open_read_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    open_at(parent, name.as_ref(), READ, false, None)
}
pub(crate) fn open_journal_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    open_read_at(parent, name)
}
pub(crate) fn metadata_file(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    open_read_at(parent, name)
}
pub(crate) fn open_directory_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    open_at(parent, name.as_ref(), WRITE, false, Some(true))
}
pub(crate) fn create_private_directory_at(parent: &File, name: impl AsRef<OsStr>) -> Result<()> {
    open_at(parent, name.as_ref(), WRITE, true, Some(true)).map(|_| ())
}
#[derive(Clone, Copy)]
pub(crate) enum Access {
    Write,
    ReadWrite,
}
pub(crate) fn create_private_at(
    parent: &File,
    name: impl AsRef<OsStr>,
    access: Access,
) -> Result<File> {
    let access = match access {
        Access::Write => GENERIC_WRITE | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
        Access::ReadWrite => WRITE,
    };
    open_at(parent, name.as_ref(), access | DELETE, true, Some(false))
}
pub(crate) fn open_lock_at(parent: &File, name: impl AsRef<OsStr>) -> Result<File> {
    let name = name.as_ref();
    validate_name(name)?;
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    let file = match native_open_at_shared(parent, name, WRITE, true, Some(false), share) {
        Ok(file) => file,
        Err(e) if is_exists(&e) => {
            native_open_at_shared(parent, name, WRITE, false, Some(false), share)?
        }
        Err(e) => return Err(e),
    };
    private::check(&file)?;
    Ok(file)
}
pub(crate) fn open_lock(path: &Path) -> Result<File> {
    validate_path(path)?;
    let parent = walk(path.parent().context("lock parent")?, false, false, WRITE)?;
    open_lock_at(&parent, path.file_name().context("lock name")?)
}
pub(crate) fn rename(
    parent: &File,
    name: impl AsRef<OsStr>,
    target: &File,
    destination: impl AsRef<OsStr>,
    replace: bool,
) -> Result<()> {
    validate_name(destination.as_ref())?;
    let file = open_at(
        parent,
        name.as_ref(),
        DELETE | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
        false,
        None,
    )?;
    let n: Vec<_> = destination.as_ref().encode_wide().collect();
    let size =
        size_of::<FILE_RENAME_INFO>().max(offset_of!(FILE_RENAME_INFO, FileName) + n.len() * 2);
    let mut buf = vec![0u64; size.div_ceil(8)];
    // SAFETY: aligned variable-length FILE_RENAME_INFO, including complete name;
    // synchronous NtSetInformationFile borrows both handles and this buffer.
    unsafe {
        let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.Flags = 2 | u32::from(replace); // POSIX_SEMANTICS / REPLACE_IF_EXISTS
        (*info).RootDirectory = target.as_raw_handle();
        (*info).FileNameLength = (n.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(n.as_ptr(), (*info).FileName.as_mut_ptr(), n.len());
        let mut ios = zeroed();
        nt(NtSetInformationFile(
            file.as_raw_handle(),
            &mut ios,
            info.cast(),
            size as u32,
            FileRenameInformationEx,
        ))?;
    }
    // Writers must flush data before publishing (Stage::place and atomic
    // private writes do so). A DELETE-only rename handle also works for frozen
    // read-only outbox files; FlushFileBuffers would require data-write access.
    // Flush the namespace on both sides, including cross-directory placement.
    target.sync_all()?;
    parent.sync_all()?;
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Identity(u64, [u8; 16]);
pub(crate) fn identity(file: &File) -> Result<Identity> {
    let info: FILE_ID_INFO = query(file, FileIdInfo)?;
    Ok(Identity(info.VolumeSerialNumber, info.FileId.Identifier))
}
pub(crate) fn identity_at(parent: &File, name: impl AsRef<OsStr>) -> Result<Identity> {
    identity(&metadata_file(parent, name)?)
}
#[derive(PartialEq, Eq)]
pub(crate) struct Version(Identity, i64, i64, i64, i64);
impl Version {
    /// Finish a Pneu writer after its last write, before retaining ownership.
    pub fn record_written(file: &File) -> Result<Self> {
        file.sync_all()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            / 100
            + 116_444_736_000_000_000;
        let now = i64::try_from(now)?;
        let basic = FILE_BASIC_INFO {
            CreationTime: 0,
            LastAccessTime: 0,
            LastWriteTime: now,
            ChangeTime: now,
            FileAttributes: 0,
        };
        // NTFS can defer write/change times until the writer's final close.
        // Explicit times finalize this file object's writes (including clones)
        // before capture; no writes may follow. Independent later writers still
        // update both times. Do not freeze timestamps with the -1 sentinel.
        // SAFETY: live writable handle and a complete borrowed FILE_BASIC_INFO.
        unsafe {
            win(SetFileInformationByHandle(
                file.as_raw_handle(),
                FileBasicInfo,
                (&basic as *const FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            ))?;
        }
        file.sync_all()?;
        Self::read_regular(&reopen(file, READ)?)
    }
    #[cfg(test)]
    pub fn read(file: &File) -> Result<Self> {
        Self::read_regular(file)
    }
    pub fn read_regular(file: &File) -> Result<Self> {
        ensure!(file.metadata()?.is_file(), "placed file must be regular");
        let basic: FILE_BASIC_INFO = query(file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(file, FileStandardInfo)?;
        Ok(Self(
            identity(file)?,
            standard.EndOfFile,
            basic.CreationTime,
            basic.LastWriteTime,
            basic.ChangeTime,
        ))
    }
}
fn dispose(file: &File) -> Result<()> {
    let flags = FILE_DISPOSITION_FLAG_DELETE
        | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
        | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE;
    // SAFETY: live DELETE-capable handle; FileDispositionInfoEx takes a u32.
    unsafe {
        win(SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            (&flags as *const u32).cast(),
            4,
        ))?;
    }
    Ok(())
}
fn remove_owned(parent: &File, name: &OsStr, retained: &File, dir: bool) -> Result<()> {
    let named = match open_at(parent, name, READ | DELETE, false, None) {
        Ok(file) => file,
        Err(e) if is_missing(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    if identity(&named)? == identity(retained)? {
        ensure!(named.metadata()?.is_dir() == dir, "unexpected cleanup type");
        dispose(&named)?;
    }
    Ok(())
}
pub(crate) fn remove_file(parent: &File, name: impl AsRef<OsStr>, file: &File) -> Result<()> {
    remove_owned(parent, name.as_ref(), file, false)
}
pub(crate) fn remove_directory(parent: &File, name: impl AsRef<OsStr>, file: &File) -> Result<()> {
    remove_owned(parent, name.as_ref(), file, true)
}
pub(crate) fn unlink_directory(parent: &File, name: impl AsRef<OsStr>) -> Result<()> {
    dispose(&open_at(
        parent,
        name.as_ref(),
        READ | DELETE,
        false,
        Some(true),
    )?)
}
pub(crate) fn is_directory_at(parent: &File, name: impl AsRef<OsStr>) -> Result<bool> {
    Ok(metadata_file(parent, name)?.metadata()?.is_dir())
}
pub(crate) fn is_private_directory(file: &File) -> Result<bool> {
    Ok(file.metadata()?.is_dir() && private::check(file).is_ok())
}
pub(crate) fn is_owned_private(file: &File) -> Result<bool> {
    Ok(private::check(file).is_ok())
}
pub(crate) fn is_private_file(file: &File) -> Result<bool> {
    Ok(file.metadata()?.is_file() && private::check(file).is_ok())
}
// Existing objects are checked, never repaired after possible exposure.
#[cfg(test)]
pub(crate) fn set_private_file(path: &Path) -> std::io::Result<()> {
    private::check_path(path).map_err(io_error)
}
pub(crate) fn set_private_directory(file: &File) -> std::io::Result<()> {
    private::check(file).map_err(io_error)
}
pub(crate) fn set_read_only(file: &File) -> std::io::Result<()> {
    let mut p = file.metadata()?.permissions();
    p.set_readonly(true);
    file.set_permissions(p)
}
pub(crate) fn received_reader(file: &File) -> std::io::Result<File> {
    reopen(file, READ)
}
fn reopen(file: &File, access: u32) -> std::io::Result<File> {
    // SAFETY: ReOpenFile returns a new independent file object (unlike dup),
    // SHARE keeps retained identities usable across rename/delete.
    let handle = unsafe {
        ReOpenFile(
            file.as_raw_handle(),
            access,
            SHARE,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_handle(handle) })
    } // SAFETY: unique successful handle.
}
pub(crate) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<usize> {
    reopen(file, READ)?.seek_read(bytes, offset)
}
pub(crate) fn copy_prefix(source: &File, destination: &File, prefix: u64) -> Result<()> {
    let mut source = reopen(source, READ)?;
    (&*destination).seek(SeekFrom::Start(0))?;
    let copied = std::io::copy(&mut Read::take(&mut source, prefix), &mut &*destination)?;
    ensure!(copied == prefix, "source shortened during copy");
    (&*destination).seek(SeekFrom::Start(prefix))?;
    Ok(())
}
pub(crate) fn clipboard_file(cache: &Path) -> Result<File> {
    let folder = Folder::open_private(cache)?;
    let name = format!(".pneu-clipboard-{}", crate::random_hex(16));
    let file = create_private_at(&folder.file, &name, Access::ReadWrite)?;
    let retained = reopen(&file, WRITE)?;
    dispose(&file)?;
    drop(file);
    Ok(retained)
}
pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    validate_path(path)?;
    walk(path.parent().context("no parent")?, false, false, WRITE)?.sync_all()?;
    Ok(())
}
pub(crate) fn entries(parent: &File) -> Result<impl Iterator<Item = Result<OsString>>> {
    // Serialize this backend's shared directory cursors. Collect before
    // returning so recursive cleanup never holds the enumeration lock.
    static ENUMERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ENUMERATION
        .lock()
        .map_err(|_| anyhow::anyhow!("directory enumeration poisoned"))?;
    let mut names = Vec::new();
    let mut buf = vec![0u64; 8192];
    let mut first = true;
    loop {
        // SAFETY: aligned writable buffer; success initializes chained records.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                parent.as_raw_handle(),
                if first {
                    FileIdBothDirectoryRestartInfo
                } else {
                    FileIdBothDirectoryInfo
                },
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
            )
        };
        first = false;
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(e.into());
        }
        let mut offset = 0;
        loop {
            // SAFETY: kernel-provided record offsets, bounded against buffer.
            let info = unsafe {
                &*(buf
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>())
            };
            let len = info.FileNameLength as usize / 2;
            ensure!(
                offset + offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) + len * 2 <= buf.len() * 8,
                "invalid directory record"
            );
            // SAFETY: counted UTF-16 name fits the checked record buffer.
            let name = OsString::from_wide(unsafe {
                std::slice::from_raw_parts(info.FileName.as_ptr(), len)
            });
            if name != "." && name != ".." {
                names.push(Ok(name));
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            offset += info.NextEntryOffset as usize;
            ensure!(
                offset + size_of::<FILE_ID_BOTH_DIR_INFO>() <= buf.len() * 8 && offset % 8 == 0,
                "invalid directory offset"
            );
        }
    }
    Ok(names.into_iter())
}
fn io_error(e: anyhow::Error) -> std::io::Error {
    match e.downcast::<std::io::Error>() {
        Ok(e) => e,
        Err(e) => std::io::Error::other(e),
    }
}
fn code(cause: &(dyn std::error::Error + 'static)) -> Option<u32> {
    cause
        .downcast_ref::<std::io::Error>()?
        .raw_os_error()
        .map(|v| v as u32)
}
pub(crate) fn is_missing(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|e| matches!(code(e), Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND)))
}
pub(crate) fn is_exists(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|e| matches!(code(e), Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS)))
}
pub(crate) fn is_link(e: &anyhow::Error) -> bool {
    e.chain().any(|e| code(e) == Some(ERROR_CANT_ACCESS_FILE))
}
pub(crate) fn is_not_directory(e: &anyhow::Error) -> bool {
    e.chain().any(|e| code(e) == Some(ERROR_DIRECTORY))
}
pub(crate) fn permission_denied() -> anyhow::Error {
    std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32).into()
}
pub(crate) fn no_space() -> std::io::Error {
    std::io::Error::from_raw_os_error(ERROR_DISK_FULL as i32)
}
pub(crate) fn storage_failure(
    e: &(dyn std::error::Error + 'static),
) -> Option<pneu_proto::NackCode> {
    use pneu_proto::NackCode;
    match code(e)? {
        ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL | ERROR_DISK_QUOTA_EXCEEDED => {
            Some(NackCode::DiskFull)
        }
        ERROR_ACCESS_DENIED
        | ERROR_WRITE_PROTECT
        | ERROR_FILE_NOT_FOUND
        | ERROR_PATH_NOT_FOUND
        | ERROR_DIRECTORY
        | ERROR_ALREADY_EXISTS
        | ERROR_FILE_EXISTS
        | ERROR_SHARING_VIOLATION => Some(NackCode::NotWritable),
        _ => None,
    }
}
pub(crate) fn lock_error(e: std::fs::TryLockError) -> anyhow::Error {
    match e {
        std::fs::TryLockError::WouldBlock => {
            std::io::Error::from(std::io::ErrorKind::WouldBlock).into()
        }
        std::fs::TryLockError::Error(e) => e.into(),
    }
}
pub(crate) fn known_folder(id: &windows_sys::core::GUID) -> Result<PathBuf> {
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath},
    };
    // SAFETY: known folder GUID and output pointer are valid. The returned
    // terminated string is read before its matching CoTaskMemFree.
    unsafe {
        let mut raw = null_mut();
        let status = SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY as u32, null_mut(), &mut raw);
        ensure!(status >= 0, "known folder lookup failed: {status:#x}");
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, len)));
        CoTaskMemFree(raw.cast());
        Ok(path)
    }
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
#[cfg(test)]
pub(crate) use tests::TestDrive;
