//! Careful file-system primitives: never follow symlinks, write atomically, keep mtimes,
//! clone instead of copy where the file system allows it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Prefix of our temporary files. Desktop ignores anything not named `local_*`/`deleted_*`.
pub const TMP_PREFIX: &str = ".cc-same-tmp-";

pub fn to_ns(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

pub fn from_ns(ns: i128) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs() as u64)
    }
}

pub fn mtime_ns(md: &fs::Metadata) -> i128 {
    md.modified().map(to_ns).unwrap_or(0)
}

pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

#[cfg(unix)]
pub fn nlink(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.nlink()
}

#[cfg(not(unix))]
pub fn nlink(_md: &fs::Metadata) -> u64 {
    1
}

#[cfg(unix)]
pub fn mode(md: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o777
}

#[cfg(not(unix))]
pub fn mode(_md: &fs::Metadata) -> u32 {
    0o644
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(other(format!("refusing to read through a symlink: {}", path.display())));
    }
    opts.open(path)
}

/// Read a regular file without following a symlink at the leaf, up to `limit` bytes.
pub fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let f = open_nofollow(path)?;
    let len = f.metadata()?.len();
    if len > limit {
        return Err(other("file too large"));
    }
    let mut buf = Vec::with_capacity(len as usize + 1);
    f.take(limit + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > limit {
        return Err(other("file too large"));
    }
    Ok(buf)
}

pub fn read_json(path: &Path, limit: u64) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::from_slice(&read_limited(path, limit)?)?)
}

pub fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

fn set_mtime(file: &File, mtime_ns: i128) -> io::Result<()> {
    file.set_modified(from_ns(mtime_ns))
}

#[cfg(unix)]
fn set_private(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Open a log file for appending: created 0600, never through a symlink. The log names
/// accounts, orgs and file names, and on macOS it lives outside the private state folder.
pub fn open_private_append(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if is_symlink(path) {
        return Err(other(format!("refusing to write through a symlink: {}", path.display())));
    }
    let f = opts.open(path)?;
    // Logs written by older versions were created with the default 0644.
    set_private(&f)?;
    Ok(f)
}

/// Write `payload` to `path` through a temporary file in the same folder, fsync it, give it
/// `mtime_ns`, then rename it into place. With `no_clobber` an existing file is left alone and
/// `Ok(false)` is returned. Never writes through a symlink.
pub fn atomic_write(path: &Path, payload: &[u8], mtime_ns: Option<i128>, no_clobber: bool) -> io::Result<bool> {
    if is_symlink(path) {
        return Err(other(format!("refusing to write through a symlink: {}", path.display())));
    }
    let dir = path.parent().ok_or_else(|| other("no parent directory"))?;
    let mut tmp = tempfile::Builder::new().prefix(TMP_PREFIX).tempfile_in(dir)?;
    tmp.write_all(payload)?;
    tmp.as_file().sync_all()?;
    set_private(tmp.as_file())?;
    if let Some(ns) = mtime_ns {
        set_mtime(tmp.as_file(), ns)?;
    }
    if no_clobber {
        match tmp.persist_noclobber(path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e.error),
        }
    } else {
        tmp.persist(path).map_err(|e| e.error)?;
    }
    // Windows may bump the time when the handle closes; set it again on the final file.
    #[cfg(windows)]
    if let Some(ns) = mtime_ns {
        set_mtime(&OpenOptions::new().write(true).open(path)?, ns)?;
    }
    Ok(true)
}

#[cfg(unix)]
fn mkdir_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn mkdir_private(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    match mkdir_private(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// `mkdir -p` below `root` that refuses symlinked or non-directory components.
pub fn ensure_real_dir(path: &Path, root: &Path) -> io::Result<()> {
    let rel = path.strip_prefix(root).map_err(|_| other(format!("path escapes its root: {}", path.display())))?;
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(md) if md.file_type().is_symlink() || !md.is_dir() => {
                return Err(other(format!("not a real directory: {}", cur.display())));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => mkdir_private(&cur)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Copy-on-write clone (APFS, Btrfs, XFS, ReFS) or a plain copy, then restore mode and mtime.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(src)?;
    if !md.is_file() {
        return Err(other(format!("not a regular file: {}", src.display())));
    }
    // A copy follows a symlink at `dst`; every caller copies into a fresh path.
    if fs::symlink_metadata(dst).is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("already exists: {}", dst.display())));
    }
    reflink_copy::reflink_or_copy(src, dst)?;
    let f = OpenOptions::new().write(true).open(dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(mode(&md)))?;
    }
    set_mtime(&f, mtime_ns(&md))
}

/// Recursively clone `src` into `dst` (which must not exist). Symlinks inside are recreated
/// as symlinks on Unix and skipped elsewhere; they are never followed.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    mkdir_private(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ft.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &to)?;
        } else if ft.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if ft.is_file() {
            clone_file(&entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Move a file or folder, falling back to copy + delete across volumes.
pub fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        create_private_dir_all(parent)?;
    }
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            let md = fs::symlink_metadata(src)?;
            if md.is_dir() {
                copy_tree(src, dst)?;
                fs::remove_dir_all(src)
            } else {
                clone_file(src, dst)?;
                fs::remove_file(src)
            }
        }
        Err(e) => Err(e),
    }
}

fn is_cross_device(e: &io::Error) -> bool {
    #[cfg(unix)]
    return e.raw_os_error() == Some(libc::EXDEV);
    #[cfg(windows)]
    return e.raw_os_error() == Some(17); // ERROR_NOT_SAME_DEVICE
    #[allow(unreachable_code)]
    false
}

/// A path that does not exist yet: `base`, `base.1`, `base.2`, …
pub fn unique_path(base: PathBuf) -> PathBuf {
    if fs::symlink_metadata(&base).is_err() {
        return base;
    }
    for n in 1.. {
        let mut s = base.clone().into_os_string();
        s.push(format!(".{n}"));
        let candidate = PathBuf::from(s);
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    unreachable!()
}
