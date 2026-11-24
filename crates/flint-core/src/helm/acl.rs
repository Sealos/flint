//! What a macOS access control list gives other accounts, which a file's mode does not show.
//!
//! On Linux a POSIX ACL's mask shows in the group bits, which the mode checks already refuse, so
//! there is nothing to read here.

// module-tests: ../helm_tests.rs

use std::path::Path;

/// What another account must not be allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reach {
    /// To write, delete, add entries, or change who may.
    Change,
    /// Anything at all, reading included.
    Any,
}

/// Whether an allow entry on `path` itself (links are not followed) gives an account other than
/// this one and root `reach`. Deny entries take nothing from the check. An entry whose account
/// can't be identified counts as another's, and so does a list that can't be read.
#[cfg(target_os = "macos")]
pub(super) fn lets_others(path: &Path, reach: Reach) -> bool {
    macos::lets_others(path, reach).unwrap_or(true)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn lets_others(_path: &Path, _reach: Reach) -> bool {
    false
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use super::Reach;

    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_int = 1;
    const ID_TYPE_UID: c_int = 0;

    // `sys/acl.h`: write or add, delete, append or add a folder, delete a child, write
    // attributes, write extended attributes, write the list, change the owner.
    const CHANGES: u64 =
        (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13);

    type Acl = *mut c_void;
    type Entry = *mut c_void;

    // SAFETY: as declared in `sys/acl.h` and `membership.h`, all in libSystem.
    unsafe extern "C" {
        fn acl_get_link_np(path: *const c_char, kind: c_int) -> Acl;
        fn acl_get_entry(acl: Acl, id: c_int, entry: *mut Entry) -> c_int;
        fn acl_get_tag_type(entry: Entry, tag: *mut c_int) -> c_int;
        fn acl_get_permset_mask_np(entry: Entry, mask: *mut u64) -> c_int;
        fn acl_get_qualifier(entry: Entry) -> *mut c_void;
        fn acl_free(object: *mut c_void) -> c_int;
        fn mbr_uuid_to_id(uuid: *const u8, id: *mut libc::id_t, kind: *mut c_int) -> c_int;
    }

    /// Frees what the ACL calls allocated.
    struct Owned(*mut c_void);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: a live object from `acl_get_link_np` or `acl_get_qualifier`, freed once.
            unsafe { acl_free(self.0) };
        }
    }

    /// Whether `entry`'s account is this one or root.
    fn is_this_account_or_root(entry: Entry) -> Option<bool> {
        // SAFETY: `entry` is live while its list is, and the result is checked before use.
        let qualifier = unsafe { acl_get_qualifier(entry) };
        if qualifier.is_null() {
            return None;
        }
        let qualifier = Owned(qualifier);
        let (mut id, mut kind) = (0, 0);
        // SAFETY: a qualifier is a 16-byte UUID, and both out-pointers are valid.
        if unsafe { mbr_uuid_to_id(qualifier.0.cast(), &raw mut id, &raw mut kind) } != 0 {
            return None;
        }
        // SAFETY: takes nothing and cannot fail.
        let me = unsafe { libc::geteuid() };
        Some(kind == ID_TYPE_UID && (id == 0 || id == me))
    }

    /// Whether this allow or deny `entry` gives another account `reach`.
    fn gives_others(entry: Entry, reach: Reach) -> Option<bool> {
        let (mut tag, mut mask) = (0, 0);
        // SAFETY: `entry` is live while its list is, and both out-pointers are valid.
        let read = unsafe {
            acl_get_tag_type(entry, &raw mut tag) == 0
                && acl_get_permset_mask_np(entry, &raw mut mask) == 0
        };
        if !read {
            return None;
        }
        let wanted = match reach {
            Reach::Change => CHANGES,
            Reach::Any => u64::MAX,
        };
        if tag != ACL_EXTENDED_ALLOW || mask & wanted == 0 {
            return Some(false);
        }
        is_this_account_or_root(entry).map(|safe| !safe)
    }

    /// `None` if the list could not be read to its end.
    pub(super) fn lets_others(path: &Path, reach: Reach) -> Option<bool> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `path` is a valid C string for the call.
        let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            // No list, or a volume that has none.
            let none = matches!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOENT | libc::ENOTSUP)
            );
            return none.then_some(false);
        }
        let acl = Owned(acl);
        let mut id = ACL_FIRST_ENTRY;
        let mut entry: Entry = std::ptr::null_mut();
        // SAFETY: `acl` is live, and `entry` is a valid out-pointer.
        while unsafe { acl_get_entry(acl.0, id, &raw mut entry) } == 0 {
            if gives_others(entry, reach)? {
                return Some(true);
            }
            id = ACL_NEXT_ENTRY;
        }
        Some(false)
    }
}
