//! The kernel's mount table: every filesystem mounted in this process's
//! namespace, where, and of what type.
//!
//! Three parts of Marcel ask it things. `browse::remoteness` asks what type
//! of filesystem serves a folder, to know whether reading it is a download.
//! The Trash asks where a path's mount starts, because a drive's Trash lives
//! at the top of the drive. The sidebar asks which drive or share a window is
//! on. They used to read the table three ways (two files, two parsers, four
//! copies of the unescaping); this is the one.
//!
//! `/proc/self/mountinfo` is the source rather than `/proc/self/mounts`
//! because it lists mounts in the order they were made, which is what tells
//! the mount on top of a point from the one under it.

use std::{
    ffi::OsString,
    io,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
};

/// A drive or share that Marcel unmounted, ejected, or disconnected, and
/// where it was. The stores send it once the call has succeeded, and every
/// window standing inside `root` goes home: moving only the window that
/// asked left the others on a folder that no longer existed, and moving it
/// before the call left it moved when the call was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountGone {
    pub root: PathBuf,
}

/// One line of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub point: PathBuf,
    /// The type as the kernel prints it: `ext4`, `nfs4`, `fuse.gvfsd-fuse`.
    pub filesystem: String,
}

/// The table as read at one moment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountTable {
    /// In the kernel's order, oldest first.
    mounts: Vec<Mount>,
}

impl MountTable {
    /// This process's mounts. A read of a kernel-generated file: cheap, and
    /// unable to stall on a network mount the way a `statfs` can.
    pub fn read() -> io::Result<Self> {
        std::fs::read("/proc/self/mountinfo").map(|table| Self::parse(&table))
    }

    /// A table in `mountinfo`'s format. Lines that do not parse are skipped.
    pub fn parse(table: &[u8]) -> Self {
        let mounts = table
            .split(|byte| *byte == b'\n')
            .filter_map(|line| {
                // Up to the ` - ` separator: id, parent, major:minor, root,
                // mount point, options, then any number of optional fields.
                // After it: the type, the source, the superblock options.
                let separator = line.windows(3).position(|window| window == b" - ")?;
                let (left, right) = (&line[..separator], &line[separator + 3..]);
                let point = left.split(|byte| *byte == b' ').nth(4)?;
                let filesystem = right.split(|byte| *byte == b' ').next()?;
                Some(Mount {
                    point: PathBuf::from(OsString::from_vec(unescape(point))),
                    filesystem: String::from_utf8_lossy(filesystem).into_owned(),
                })
            })
            .collect();
        Self { mounts }
    }

    /// The mount serving `path`: the deepest mount point that contains it,
    /// and of two on the same point, the later, which is the one on top. An
    /// automounted share is an `autofs` line followed by the `nfs4` or
    /// `cifs` line that actually serves it.
    ///
    /// Lexical: a symbolic link into a mount is not followed.
    pub fn serving(&self, path: &Path) -> Option<&Mount> {
        self.mounts
            .iter()
            .enumerate()
            .filter(|(_, mount)| path.starts_with(&mount.point))
            .max_by_key(|(order, mount)| (mount.point.as_os_str().len(), *order))
            .map(|(_, mount)| mount)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Mount> {
        self.mounts.iter()
    }
}

/// Undo the `\ooo` octal escapes the kernel's tables and fstab use for the
/// bytes that would break their whitespace-separated format: a disk mounted
/// at `/mnt/My Files` is listed as `/mnt/My\040Files`. A backslash that does
/// not start three octal digits is kept as itself.
pub fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let byte = field[index];
        let octal = field
            .get(index + 1..index + 4)
            .filter(|digits| digits.iter().all(|digit| (b'0'..=b'7').contains(digit)))
            .and_then(|digits| {
                digits
                    .iter()
                    .try_fold(0u8, |value, digit| value.checked_mul(8)?.checked_add(digit - b'0'))
            });
        match (byte, octal) {
            (b'\\', Some(value)) => {
                out.push(value);
                index += 4;
            }
            _ => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line per mount, in the shape the kernel writes.
    const TABLE: &[u8] = b"\
23 28 0:22 / /proc rw,relatime shared:12 - proc proc rw
28 1 254:2 / / rw,relatime shared:1 - ext4 /dev/root rw
41 28 0:39 / /home rw,relatime shared:25 - ext4 /dev/home rw
580 41 0:80 / /home/me/work rw,relatime shared:570 - nfs4 server:/export rw
590 41 0:90 / /home/me/My\\040Share rw,relatime shared:580 - cifs //nas/share rw
610 28 0:101 / /mnt/nas rw,relatime shared:600 - autofs systemd-1 rw,fd=45
615 610 0:102 / /mnt/nas rw,relatime shared:605 - nfs4 nas:/export rw
this line is not a mount
";

    fn serving(path: &str) -> String {
        MountTable::parse(TABLE).serving(Path::new(path)).unwrap().filesystem.clone()
    }

    #[test]
    fn the_deepest_mount_point_serves_a_path() {
        assert_eq!(serving("/home/me/notes.md"), "ext4");
        assert_eq!(serving("/home/me/work/report.pdf"), "nfs4");
        assert_eq!(serving("/etc/hosts"), "ext4");
    }

    #[test]
    fn of_two_mounts_on_one_point_the_later_is_on_top() {
        assert_eq!(serving("/mnt/nas/photos"), "nfs4");
    }

    #[test]
    fn escaped_mount_points_are_read_back_and_bad_lines_skipped() {
        assert_eq!(serving("/home/me/My Share/album"), "cifs");
        assert_eq!(MountTable::parse(TABLE).iter().count(), 7);
    }

    #[test]
    fn a_path_is_matched_by_component_not_by_prefix() {
        // `/home/me/workshop` is not inside `/home/me/work`.
        assert_eq!(serving("/home/me/workshop"), "ext4");
    }

    #[test]
    fn octal_escapes_are_undone_and_anything_else_kept() {
        assert_eq!(unescape(b"/mnt/My\\040Files"), b"/mnt/My Files");
        assert_eq!(unescape(b"/mnt/plain"), b"/mnt/plain");
        assert_eq!(unescape(b"/odd\\x"), b"/odd\\x");
        assert_eq!(unescape(b"/mnt/odd\\"), b"/mnt/odd\\");
        assert_eq!(unescape(b"tab\\011here"), b"tab\there");
    }
}
