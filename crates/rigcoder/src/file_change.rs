//! File mutation preparation shared by the real write and edit tools.
//!
//! Preparation is read-only. Application consumes the prepared value, validates
//! its source again, and replaces the file with the exact prepared bytes. The
//! final check and rename are not a filesystem compare-and-swap: callers must
//! exclude concurrent external writers for an approval or exactly-once contract.

use std::{
    fs::{self, Metadata},
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::steer::Scope;

pub(crate) struct FileSource {
    requested: PathBuf,
    resolved: PathBuf,
    before: Option<(Vec<u8>, Metadata)>,
}

pub(crate) struct PreparedFileChange {
    source: FileSource,
    after: Vec<u8>,
}

fn canonical(path: &Path) -> io::Result<PathBuf> {
    Scope {
        root: std::env::current_dir()?,
        ..Default::default()
    }
    .canonical(path)
}

fn read_source(path: &Path) -> io::Result<Option<(Vec<u8>, Metadata)>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::other("mutation target must be a regular file"));
    }
    if metadata.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "mutation target is read-only",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(io::Error::other("mutation target must have a single link"));
        }
        if metadata.mode() & 0o6000 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "mutation target must not have set-user-ID or set-group-ID bits",
            ));
        }
    }
    validate_extended_metadata(path)?;
    Ok(Some((fs::read(path)?, metadata)))
}

// Atomic replacement cannot silently discard an ACL, extended attributes or
// ownership. Until those can be copied and verified, refuse such replacements.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn validate_extended_metadata(path: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    #[cfg(target_os = "macos")]
    let file = fs::File::open(path)?;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    #[cfg(target_os = "macos")]
    let size = unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0, 0) };
    #[cfg(target_os = "linux")]
    let size = unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    if size != 0 {
        return Err(io::Error::other(
            "mutation target has unsupported extended attributes",
        ));
    }
    #[cfg(target_os = "macos")]
    {
        // Darwin sys/acl.h: ACL_TYPE_EXTENDED = 0x100, ACL_FIRST_ENTRY = 0.
        unsafe extern "C" {
            fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut libc::c_void;
            fn acl_get_entry(
                acl: *mut libc::c_void,
                entry: libc::c_int,
                out: *mut *mut libc::c_void,
            ) -> libc::c_int;
            fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
        }
        use std::os::fd::AsRawFd;
        // SAFETY: file owns a valid descriptor; an acquired ACL is freed once.
        let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            // Darwin reports ENOENT for no extended ACL. A held descriptor
            // distinguishes that condition from a missing path.
            return if error.raw_os_error() == Some(libc::ENOENT) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let mut entry = std::ptr::null_mut();
        // Darwin's empty-ACL path may leave errno unchanged.
        unsafe { *libc::__error() = 0 };
        let found = unsafe { acl_get_entry(acl, 0, &mut entry) };
        let error = io::Error::last_os_error();
        unsafe { acl_free(acl) };
        if found == 0 {
            return Err(io::Error::other("mutation target has an unsupported ACL"));
        }
        if !matches!(error.raw_os_error(), Some(0) | Some(libc::EINVAL)) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn validate_extended_metadata(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic replacement metadata checks require macOS or Linux",
    ))
}

fn same_metadata(before: &Metadata, current: &Metadata) -> bool {
    if before.permissions() != current.permissions()
        || before.modified().ok() != current.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != current.dev()
            || before.ino() != current.ino()
            || before.uid() != current.uid()
            || before.gid() != current.gid()
            || before.ctime() != current.ctime()
            || before.ctime_nsec() != current.ctime_nsec()
        {
            return false;
        }
    }
    true
}

impl FileSource {
    pub(crate) fn read(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = path.as_os_str().as_bytes();
            if raw.ends_with(b"/") || raw.ends_with(b"/.") || raw == b"." {
                return Err(io::Error::other("mutation target requires a file name"));
            }
        }
        let requested = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let resolved = canonical(&requested)?;
        let before = read_source(&resolved)?;
        Ok(Self {
            requested,
            resolved,
            before,
        })
    }

    pub(crate) fn text(&self) -> io::Result<&str> {
        let (bytes, _) = self
            .before
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "edit target does not exist"))?;
        std::str::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn prepare(self, after: Vec<u8>) -> PreparedFileChange {
        PreparedFileChange {
            source: self,
            after,
        }
    }

    fn validate(&self) -> io::Result<()> {
        if canonical(&self.requested)? != self.resolved {
            return Err(io::Error::other("mutation path changed after preparation"));
        }
        let current = read_source(&self.resolved)?;
        let unchanged = match (&self.before, &current) {
            (None, None) => true,
            (Some((before, metadata)), Some((now, current_metadata))) => {
                before == now && same_metadata(metadata, current_metadata)
            }
            _ => false,
        };
        if unchanged {
            Ok(())
        } else {
            Err(io::Error::other(
                "mutation source changed after preparation",
            ))
        }
    }
}

impl PreparedFileChange {
    pub(crate) fn apply(self) -> io::Result<()> {
        self.source.validate()?;
        let path = &self.source.resolved;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("mutation target has no parent"))?;
        fs::create_dir_all(parent)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(&self.after)?;
        if let Some((_, metadata)) = &self.source.before {
            staged.as_file().set_permissions(metadata.permissions())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let replacement = staged.as_file().metadata()?;
                if metadata.uid() != replacement.uid() || metadata.gid() != replacement.gid() {
                    return Err(io::Error::other("replacement would change file ownership"));
                }
            }
        }
        validate_extended_metadata(staged.path())?;
        staged.as_file().sync_all()?;
        self.source.validate()?;
        if self.source.before.is_none() {
            staged
                .persist_noclobber(path)
                .map_err(|error| error.error)?;
        } else {
            staged.persist(path).map_err(|error| error.error)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_is_read_only_and_apply_uses_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("new/sub/file");
        let bytes = b"hello\0world\n";
        let prepared = FileSource::read(&path).unwrap().prepare(bytes.to_vec());
        assert!(!root.path().join("new").exists());
        prepared.apply().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn stale_edits_and_new_files_do_not_overwrite_external_changes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        for existing in [false, true] {
            if existing {
                fs::write(&path, "before").unwrap();
            }
            let prepared = FileSource::read(&path).unwrap().prepare(b"after".to_vec());
            fs::write(&path, "external").unwrap();
            assert!(prepared.apply().is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "external");
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn dropping_preparation_leaves_the_original_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        let source = FileSource::read(&path).unwrap();
        assert_eq!(source.text().unwrap(), "before");
        drop(source.prepare(b"after".to_vec()));
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn deleted_source_and_directory_target_are_refused() {
        let root = tempfile::tempdir().unwrap();
        assert!(FileSource::read(root.path()).is_err());
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        let prepared = FileSource::read(&path).unwrap().prepare(b"after".to_vec());
        fs::remove_file(&path).unwrap();
        assert!(prepared.apply().is_err());
        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn a_retargeted_parent_cannot_receive_a_prepared_new_file() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let link = root.path().join("link");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        symlink(&first, &link).unwrap();
        let prepared = FileSource::read(&link.join("new"))
            .unwrap()
            .prepare(b"after".to_vec());
        fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        assert!(prepared.apply().is_err());
        assert_eq!(fs::read_dir(&first).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&second).unwrap().count(), 0);
    }

    #[test]
    #[cfg(unix)]
    fn application_cannot_restore_old_permissions_over_a_changed_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let prepared = FileSource::read(&path).unwrap().prepare(b"after".to_vec());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(prepared.apply().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(FileSource::read(&path).is_err());
    }

    #[test]
    fn invalid_parent_traversal_cannot_be_normalized_into_a_valid_write() {
        let root = tempfile::tempdir().unwrap();
        let victim = root.path().join("victim");
        fs::write(root.path().join("blocker"), "regular file").unwrap();
        fs::write(&victim, "before").unwrap();
        for suffix in [
            "blocker/../victim",
            "blocker/.",
            "missing/../blocker/../victim",
        ] {
            assert!(
                FileSource::read(&root.path().join(suffix)).is_err(),
                "{suffix}"
            );
        }
        assert_eq!(fs::read_to_string(&victim).unwrap(), "before");
    }

    #[test]
    #[cfg(unix)]
    fn a_missing_directory_path_does_not_become_a_regular_file() {
        let root = tempfile::tempdir().unwrap();
        for suffix in ["missing/", "missing/."] {
            assert!(FileSource::read(&root.path().join(suffix)).is_err());
        }
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    #[cfg(unix)]
    fn symlink_retarget_and_same_content_replacement_are_stale() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        let link = root.path().join("link");
        fs::write(&first, "before").unwrap();
        fs::write(&second, "before").unwrap();
        symlink(&first, &link).unwrap();
        let prepared = FileSource::read(&link).unwrap().prepare(b"after".to_vec());
        fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        assert!(prepared.apply().is_err());
        let prepared = FileSource::read(&first).unwrap().prepare(b"after".to_vec());
        fs::rename(&second, &first).unwrap();
        assert!(prepared.apply().is_err());
        assert_eq!(fs::read_to_string(&first).unwrap(), "before");
    }

    #[test]
    #[cfg(unix)]
    fn replacements_preserve_permissions_and_refuse_hard_links() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
        FileSource::read(&path)
            .unwrap()
            .prepare(b"after".to_vec())
            .apply()
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
        fs::hard_link(&path, root.path().join("alias")).unwrap();
        assert!(FileSource::read(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "after");
    }

    #[test]
    #[cfg(unix)]
    fn privileged_permission_bits_cannot_be_copied_to_replacement_content() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        for mode in [0o4751, 0o2751] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(FileSource::read(&path).is_err());
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
    }

    #[test]
    #[cfg(unix)]
    fn changed_group_ownership_is_not_restored_over_a_new_decision() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::MetadataExt},
        };
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        let original = fs::metadata(&path).unwrap().gid();
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        assert!(count >= 0);
        let mut groups = vec![0; count as usize];
        assert!(unsafe { libc::getgroups(count, groups.as_mut_ptr()) } >= 0);
        let Some(group) = groups.into_iter().find(|group| *group != original) else {
            // Single-group hosts cannot perform an unprivileged chgrp probe.
            return;
        };
        let prepared = FileSource::read(&path).unwrap().prepare(b"after".to_vec());
        let raw = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chown(raw.as_ptr(), !0, group) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        assert!(prepared.apply().is_err());
        assert_eq!(fs::metadata(&path).unwrap().gid(), group);
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn extended_attributes_are_not_silently_discarded() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        let prepared = FileSource::read(&path).unwrap().prepare(b"after".to_vec());
        let raw = CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = c"user.rigcoder_test";
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::setxattr(raw.as_ptr(), name.as_ptr(), c"v".as_ptr().cast(), 1, 0, 0) };
        #[cfg(target_os = "linux")]
        let result =
            unsafe { libc::setxattr(raw.as_ptr(), name.as_ptr(), c"v".as_ptr().cast(), 1, 0) };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        assert!(prepared.apply().is_err());
        assert!(FileSource::read(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn an_acl_added_after_preparation_is_not_removed_by_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, "before").unwrap();
        let prepared = FileSource::read(&path).unwrap().prepare(b"after".to_vec());
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", "everyone deny read"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(prepared.apply().is_err());
        assert!(FileSource::read(&path).is_err());
        assert!(
            std::process::Command::new("/bin/chmod")
                .arg("-N")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
    }
}
