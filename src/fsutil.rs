//! Small filesystem predicates shared by scanners and the deletion engine.
//!
//! On Windows, junctions and other reparse points are not ordinary symbolic
//! links. Treating them as directories can make a recursive walk leave the
//! intended tree, so they are handled as leaf links instead.

use std::fs::Metadata;

/// Windows' FILE_ATTRIBUTE_REPARSE_POINT.
#[cfg(windows)]
const REPARSE_POINT: u32 = 0x0400;

pub fn is_reparse_point(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

/// True for links on Unix and for links/reparse points on Windows.
pub fn is_link_or_reparse(meta: &Metadata) -> bool {
    meta.file_type().is_symlink() || is_reparse_point(meta)
}
