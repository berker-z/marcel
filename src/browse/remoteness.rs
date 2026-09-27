//! Whether a directory is on this machine or a network away.
//!
//! Marcel reaches a network share through the kernel, as an ordinary
//! directory: GVfs mounts one under `/run/user/<uid>/gvfs/`, sshfs and NFS
//! and SMB put one wherever they were mounted. Nothing in a `read_dir` or a
//! `stat` says which it is, so work that is free on an SSD — thumbnailing
//! every file in view, walking a tree to size it — becomes a download over
//! somebody's uplink without a single call site knowing it changed.
//!
//! The answer comes from `/proc/self/mountinfo` rather than `statfs`, for
//! two reasons. It names the filesystem *type* (`fuse.gvfsd-fuse`, `nfs4`,
//! `cifs`) where `statfs` gives only `FUSE_SUPER_MAGIC` for every FUSE
//! filesystem alike, which would put a local `gocryptfs` in the same bucket
//! as an SFTP share. And it is a read of a kernel-generated file, where
//! `statfs` on a FUSE path is a round trip to the daemon that owns it —
//! the very thing this module exists to avoid.

use std::path::Path;

use crate::mounts::MountTable;

/// Where the filesystem holding a path actually is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Locality {
    #[default]
    Local,
    Remote,
}

impl Locality {
    pub fn is_remote(self) -> bool {
        self == Self::Remote
    }
}

/// Filesystem types that are a network away, by the name the kernel prints.
///
/// `fuseblk` is deliberately absent: it is how NTFS-3G and friends mount a
/// local block device.
const REMOTE_TYPES: &[&str] = &[
    "9p",
    "afs",
    "ceph",
    "cifs",
    "coda",
    "davfs",
    "glusterfs",
    "lustre",
    "ncpfs",
    "nfs",
    "nfs4",
    "smb3",
    "smbfs",
];

/// FUSE subtypes that are a network away. The kernel prints these as
/// `fuse.<subtype>`, and everything else under `fuse.` — `gocryptfs`,
/// `mergerfs`, an AppImage's own mount — is local and stays that way.
const REMOTE_FUSE_SUBTYPES: &[&str] =
    &["curlftpfs", "davfs2", "ftpfs", "gvfsd-fuse", "rclone", "s3fs", "smbnetfs", "sshfs"];

/// Where `path` lives, as the kernel's mount table says.
///
/// A path under no known mount, or a mount table that cannot be read, is
/// treated as local: the cost of guessing wrong that way is a slow preview,
/// and the cost of guessing wrong the other way is a file manager that
/// silently stops showing thumbnails.
pub fn of(path: &Path) -> Locality {
    MountTable::read().map_or(Locality::Local, |table| in_table(path, &table))
}

/// `of`, against a mount table already in hand. The deepest mount holding
/// the path serves it, so a share mounted under the home directory reads as
/// a share.
pub fn in_table(path: &Path, table: &MountTable) -> Locality {
    table.serving(path).map_or(Locality::Local, |mount| classify(&mount.filesystem))
}

fn classify(filesystem: &str) -> Locality {
    let remote = match filesystem.split_once('.') {
        Some(("fuse", subtype)) => REMOTE_FUSE_SUBTYPES.contains(&subtype),
        _ => REMOTE_TYPES.contains(&filesystem),
    };
    if remote { Locality::Remote } else { Locality::Local }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// One line per mount, in the shape the kernel writes.
    const TABLE: &str = "\
23 28 0:22 / /proc rw,relatime shared:12 - proc proc rw
28 1 254:2 / / rw,relatime shared:1 - ext4 /dev/root rw
41 28 0:39 / /home rw,relatime shared:25 - ext4 /dev/home rw
564 98 0:63 / /run/user/1000/gvfs rw,nosuid,nodev,relatime shared:550 - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000
571 41 0:71 / /home/me/vault rw,nosuid,nodev,relatime shared:560 - fuse.gocryptfs gocryptfs rw,user_id=1000
580 41 0:80 / /home/me/work rw,relatime shared:570 - nfs4 server:/export rw
590 41 0:90 / /home/me/My\\040Share rw,relatime shared:580 - cifs //nas/share rw
600 28 0:99 / /mnt/win rw,relatime shared:590 - fuseblk /dev/sda3 rw
";

    fn locality(path: &str) -> Locality {
        in_table(&PathBuf::from(path), &MountTable::parse(TABLE.as_bytes()))
    }

    #[test]
    fn a_gvfs_share_is_remote_and_an_ordinary_directory_is_not() {
        assert_eq!(locality("/home/me/photos/cat.jpg"), Locality::Local);
        assert_eq!(locality("/run/user/1000/gvfs/sftp:host=wired/etc"), Locality::Remote);
    }

    #[test]
    fn the_deepest_mount_wins_so_a_share_under_a_local_tree_reads_remote() {
        // `/home` is ext4 and `/home/me/work` is NFS: the longer prefix has
        // to win, or every share mounted inside the home directory would be
        // thumbnailed and walked as if it were local.
        assert_eq!(locality("/home/me/work/report.pdf"), Locality::Remote);
        assert_eq!(locality("/home/me/notes.md"), Locality::Local);
    }

    /// `x-systemd.automount` leaves an `autofs` mount on the point and puts
    /// the real one on top of it once something looks inside.
    #[test]
    fn an_automounted_share_reads_as_the_share_on_top_not_the_autofs_below() {
        let table = "\
28 1 254:2 / / rw,relatime shared:1 - ext4 /dev/root rw
610 28 0:101 / /mnt/nas rw,relatime shared:600 - autofs systemd-1 rw,fd=45
615 610 0:102 / /mnt/nas rw,relatime shared:605 - nfs4 nas:/export rw
";
        let table = MountTable::parse(table.as_bytes());
        assert_eq!(in_table(&PathBuf::from("/mnt/nas/photos"), &table), Locality::Remote);
    }

    #[test]
    fn network_filesystems_are_named_by_type() {
        assert_eq!(locality("/home/me/My Share/album"), Locality::Remote);
    }

    #[test]
    fn a_local_fuse_filesystem_stays_local() {
        // The whole point of reading the type rather than the `statfs`
        // magic: both of these are FUSE, and only one is a network away.
        assert_eq!(locality("/home/me/vault/secret.txt"), Locality::Local);
        assert_eq!(locality("/mnt/win/games"), Locality::Local);
    }

    #[test]
    fn a_path_under_nothing_known_is_treated_as_local() {
        assert_eq!(in_table(&PathBuf::from("/srv/data"), &MountTable::default()), Locality::Local);
    }
}
