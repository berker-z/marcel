//! Block devices with a filesystem on them, read from UDisks2 over the
//! system bus: what is plugged in, what is mounted where, and the three
//! calls that change that.
//!
//! Which volumes are worth showing follows GVfs's UDisks2 volume monitor
//! (`monitor/udisks2/gvfsudisks2volumemonitor.c`, `should_include_volume`),
//! which is what Nautilus displays, so a sidebar here and one there agree.
//! UDisks2 itself already hides the partitions no user wants (`HintIgnore`
//! on the EFI system partition, Windows recovery, swap); what is left to
//! decide is where a mount point counts as the system's business rather than
//! the user's, and that is GLib's exact-match list of system directories plus
//! the rule that a mount outside `/media`, `/run/media/<user>`, and `$HOME`
//! needs `x-gvfs-show` in fstab to be listed.
//!
//! Nothing here knows about GPUI. `VolumeStore` in `crate::volumes` owns the
//! monitor and turns its snapshots into state windows observe.

use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow};
use zbus::{
    fdo::{ManagedObjects, ObjectManagerProxy},
    zvariant::{OwnedObjectPath, OwnedValue},
};

const UDISKS: &str = "org.freedesktop.UDisks2";
const UDISKS_PATH: &str = "/org/freedesktop/UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const LOOP: &str = "org.freedesktop.UDisks2.Loop";

/// One filesystem UDisks2 knows about and a user might want to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// The block object, which the mount, unmount, and eject calls address.
    pub block: OwnedObjectPath,
    pub drive: Option<OwnedObjectPath>,
    /// The device node, `/dev/sda1`.
    pub device: PathBuf,
    /// The label, the fstab `x-gvfs-name`, or "268 GB Volume".
    pub name: String,
    /// `vfat`, `ntfs`, `ext4`.
    pub filesystem: String,
    pub size: u64,
    pub mount_point: Option<PathBuf>,
    /// A drive that can come and go: a stick, a card, a disc.
    pub removable: bool,
    /// The drive has a tray or the equivalent (`Drive.Eject`).
    pub ejectable: bool,
    /// The drive can be told to power down (`Drive.PowerOff`), which is how a
    /// USB stick is made safe to pull.
    pub can_power_off: bool,
    pub read_only: bool,
}

impl Volume {
    pub fn is_mounted(&self) -> bool {
        self.mount_point.is_some()
    }

    /// Whether Eject means anything for this drive: unmount, then either open
    /// the tray or power the device down. An internal disk has neither.
    pub fn can_eject(&self) -> bool {
        self.removable && (self.ejectable || self.can_power_off)
    }
}

/// A connection to UDisks2, or the reason there is none.
pub struct VolumeMonitor {
    connection: zbus::Connection,
    objects: ObjectManagerProxy<'static>,
    user: String,
    home: PathBuf,
}

impl VolumeMonitor {
    /// Reach UDisks2 on the system bus. Fails when there is no system bus or
    /// nothing answers on it, which is what a container looks like.
    pub async fn connect(user: String, home: PathBuf) -> Result<Self> {
        let connection = zbus::Connection::system().await.context("No system bus")?;
        let objects = ObjectManagerProxy::builder(&connection)
            .destination(UDISKS)?
            .path(UDISKS_PATH)?
            .build()
            .await
            .context("Could not address UDisks2")?;
        // Prove something is there before claiming a Devices section exists.
        objects.get_managed_objects().await.context("UDisks2 is not on the system bus")?;
        Ok(Self { connection, objects, user, home })
    }

    /// Every volume worth listing, sorted by device node.
    pub async fn snapshot(&self) -> Result<Vec<Volume>> {
        let objects = self.objects.get_managed_objects().await.context("Could not list devices")?;
        Ok(volumes_from(&objects, &self.user, &self.home))
    }

    /// Resolve once anything UDisks2 publishes has changed: a device added
    /// or removed, or a property (a mount point, a label) updated. The next
    /// `snapshot` says what changed; the signal payloads are not inspected,
    /// because re-reading a few dozen objects is cheaper than tracking them.
    pub async fn changed(&self) -> Result<()> {
        use smol::stream::StreamExt as _;

        let added = self.objects.receive_interfaces_added().await?.map(|_| ());
        let removed = self.objects.receive_interfaces_removed().await?.map(|_| ());
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(UDISKS)?
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .build();
        let properties =
            zbus::MessageStream::for_match_rule(rule, &self.connection, None).await?.map(|_| ());
        let mut any = added.race(removed).race(properties);
        any.next().await.ok_or_else(|| anyhow!("UDisks2 stopped sending changes"))
    }

    /// Mount a volume as the calling user, returning where it landed and
    /// whether it had to be mounted read-only.
    ///
    /// UDisks2 picks the mount point (`/run/media/<user>/<label>`) unless
    /// fstab names one, and asks polkit whether this user may: a removable
    /// drive on a local session mounts without a prompt, an internal
    /// partition prompts for a password unless fstab covers it.
    ///
    /// A read-write mount that fails is retried read-only. The case this is
    /// for is the Windows partition of a dual-boot machine: Fast Startup or
    /// hibernation leaves NTFS dirty, and the kernel's `ntfs3` refuses a
    /// dirty volume for writing ("volume is dirty and "force" flag is not
    /// set") but takes it read-only. UDisks2 picks `ntfs3` first and never
    /// falls back, and GVfs makes the same call with no retry, so Nautilus
    /// fails here too; the files are readable regardless, and reading them
    /// is most of what a dual-booter wants. The user is told what happened
    /// and what fixes the write side.
    pub async fn mount(&self, volume: &Volume) -> Result<Mounted> {
        let proxy = self.filesystem(volume).await?;
        let call = |options: &'static str| async {
            let mut arguments: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
            if !options.is_empty() {
                arguments.insert("options", options.into());
            }
            proxy.call::<_, _, String>("Mount", &(arguments,)).await
        };
        match call("").await {
            Ok(mount_point) => {
                Ok(Mounted { mount_point: PathBuf::from(mount_point), read_only: false })
            }
            // Polkit said no, or the user dismissed the prompt: asking again
            // read-only would prompt again for the same answer.
            Err(error) if is_authorization_error(&error) => {
                Err(describe_udisks_error(&error, "mount", volume))
            }
            Err(error) => match call("ro").await {
                Ok(mount_point) => {
                    Ok(Mounted { mount_point: PathBuf::from(mount_point), read_only: true })
                }
                Err(_) => Err(describe_udisks_error(&error, "mount", volume)),
            },
        }
    }

    pub async fn unmount(&self, volume: &Volume) -> Result<()> {
        let proxy = self.filesystem(volume).await?;
        let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        proxy
            .call::<_, _, ()>("Unmount", &(options,))
            .await
            .map_err(|error| describe_udisks_error(&error, "unmount", volume))
    }

    /// Unmount, then make the drive safe to pull: open the tray when there is
    /// one, otherwise power the device down. This is what GVfs does for its
    /// Eject (`gvfsudisks2drive.c`), and what makes a stick's light go out.
    pub async fn eject(&self, volume: &Volume) -> Result<()> {
        if volume.is_mounted() {
            self.unmount(volume).await?;
        }
        let Some(drive) = &volume.drive else {
            return Ok(());
        };
        let proxy = zbus::Proxy::new(&self.connection, UDISKS, drive.clone(), DRIVE).await?;
        let options: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let method = if volume.ejectable { "Eject" } else { "PowerOff" };
        proxy
            .call::<_, _, ()>(method, &(options,))
            .await
            .map_err(|error| describe_udisks_error(&error, "eject", volume))
    }

    async fn filesystem(&self, volume: &Volume) -> Result<zbus::Proxy<'static>> {
        zbus::Proxy::new(&self.connection, UDISKS, volume.block.clone(), FILESYSTEM)
            .await
            .context("Could not address the volume")
    }
}

/// Where a mount landed, and whether the read-only retry was what got it there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mounted {
    pub mount_point: PathBuf,
    pub read_only: bool,
}

fn is_authorization_error(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _) if name.as_str().contains("NotAuthorized"))
}

/// UDisks2's errors name D-Bus error types; the part after the last dot is
/// the readable one, and polkit's refusal deserves a sentence of its own.
fn describe_udisks_error(error: &zbus::Error, action: &str, volume: &Volume) -> anyhow::Error {
    let detail = match error {
        zbus::Error::MethodError(name, description, _) => {
            let kind = name.as_str().rsplit('.').next().unwrap_or(name.as_str());
            match kind {
                "NotAuthorized" | "NotAuthorizedCanObtain" | "NotAuthorizedDismissed" => {
                    "not permitted: this drive needs an administrator to mount it, or an fstab entry"
                        .to_string()
                }
                _ => match description {
                    Some(text) => text.trim_end_matches('.').to_string(),
                    None => kind.to_string(),
                },
            }
        }
        other => other.to_string(),
    };
    anyhow!("Could not {action} “{}”: {detail}", volume.name)
}

// ---------------------------------------------------------------------------
// Reading the object tree.

/// What one `Block` object says, once the variants are unpacked.
#[derive(Debug, Default)]
struct BlockFacts {
    device: PathBuf,
    drive: Option<OwnedObjectPath>,
    label: String,
    id_type: String,
    id_usage: String,
    size: u64,
    read_only: bool,
    hint_ignore: bool,
    crypto_backing_device: Option<OwnedObjectPath>,
    /// Mount points, present only when the object has a `Filesystem`.
    mount_points: Option<Vec<PathBuf>>,
    fstab: Option<FstabEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FstabEntry {
    dir: PathBuf,
    options: String,
}

#[derive(Debug, Default)]
struct DriveFacts {
    removable: bool,
    ejectable: bool,
    can_power_off: bool,
    /// Which user set up a loop device, for hiding other users' images.
    setup_by_uid: Option<u32>,
}

type Properties = HashMap<String, OwnedValue>;

fn volumes_from(objects: &ManagedObjects, user: &str, home: &Path) -> Vec<Volume> {
    let drives: HashMap<&OwnedObjectPath, DriveFacts> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            interfaces.get(DRIVE).map(|properties| (path, drive_facts(properties)))
        })
        .collect();
    let mut volumes: Vec<Volume> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let block = interfaces.get(BLOCK)?;
            let mut facts = block_facts(block);
            facts.mount_points = interfaces.get(FILESYSTEM).map(|filesystem| {
                property::<Vec<Vec<u8>>>(filesystem, "MountPoints")
                    .unwrap_or_default()
                    .into_iter()
                    .map(|bytes| PathBuf::from(OsString::from_vec(trim_nul(bytes))))
                    .collect()
            });
            let loop_owner = interfaces
                .get(LOOP)
                .and_then(|properties| property::<u32>(properties, "SetupByUID"));
            let drive = facts.drive.as_ref().and_then(|drive| drives.get(drive));
            let drive_facts = DriveFacts {
                setup_by_uid: loop_owner,
                ..drive
                    .map(|d| DriveFacts {
                        removable: d.removable,
                        ejectable: d.ejectable,
                        can_power_off: d.can_power_off,
                        setup_by_uid: None,
                    })
                    .unwrap_or_default()
            };
            if !should_list(&facts, &drive_facts, user, home, current_uid()) {
                return None;
            }
            let mount_point =
                facts.mount_points.as_ref().and_then(|points| points.first().cloned());
            Some(Volume {
                block: path.clone(),
                drive: facts.drive.clone(),
                name: display_name(&facts),
                device: facts.device,
                filesystem: facts.id_type,
                size: facts.size,
                mount_point,
                removable: drive_facts.removable,
                ejectable: drive_facts.ejectable,
                can_power_off: drive_facts.can_power_off,
                read_only: facts.read_only,
            })
        })
        .collect();
    volumes.sort_by(|a, b| a.device.cmp(&b.device));
    volumes
}

fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// GVfs's `should_include_volume`, minus the encrypted-volume morphing that
/// this sprint does not show.
fn should_list(block: &BlockFacts, drive: &DriveFacts, user: &str, home: &Path, uid: u32) -> bool {
    // Block:HintIgnore trumps everything.
    if block.hint_ignore {
        return false;
    }
    // Another user's disk image is theirs.
    if drive.setup_by_uid.is_some_and(|owner| owner != 0 && owner != uid) {
        return false;
    }
    // An unlocked LUKS volume shows as its cleartext block, which has a
    // backing device; the locked one is what a later sprint will list.
    if block.id_usage == "crypto" {
        return false;
    }
    if block.crypto_backing_device.as_ref().is_some_and(|backing| backing.as_str() != "/") {
        return false;
    }
    let Some(mount_points) = &block.mount_points else {
        return false;
    };
    let fstab_options = block.fstab.as_ref().map(|entry| entry.options.as_str());
    if mount_points.is_empty() {
        // Not mounted: an fstab entry pointing somewhere the user would not
        // look means the system owns this one.
        return block
            .fstab
            .as_ref()
            .is_none_or(|entry| should_show_path(&entry.dir, Some(&entry.options), user, home));
    }
    mount_points.iter().any(|point| should_show_path(point, fstab_options, user, home))
}

/// GVfs's `should_include`: where a mount point counts as the user's.
///
/// `x-gvfs-show` and `x-gvfs-hide` in the fstab options decide outright.
/// Otherwise the path must not be a system directory or hidden under a dot,
/// and must be under `$HOME`, a direct child of `/media` or `/run/media`, or
/// anywhere under `/media/<user>` or `/run/media/<user>`.
fn should_show_path(path: &Path, fstab_options: Option<&str>, user: &str, home: &Path) -> bool {
    if let Some(options) = fstab_options {
        if has_fstab_option(options, "x-gvfs-show") {
            return true;
        }
        if has_fstab_option(options, "x-gvfs-hide") {
            return false;
        }
    }
    if is_system_internal_path(path) {
        return false;
    }
    let text = path.to_string_lossy();
    if text.contains("/.") {
        return false;
    }
    if path.starts_with(home) && path != home {
        return true;
    }
    let under_media = text.strip_prefix("/run").unwrap_or(&text);
    if let Some(rest) = under_media.strip_prefix("/media/") {
        return rest.strip_prefix(user).is_some_and(|tail| tail.starts_with('/'))
            || !rest.contains('/');
    }
    false
}

/// GLib's `g_unix_is_mount_path_system_internal`: the exact list from
/// `gunixmounts-private.h`, plus everything under `/dev`, `/proc`, and `/sys`.
fn is_system_internal_path(path: &Path) -> bool {
    const SYSTEM: &[&str] = &[
        "/",
        "/bin",
        "/boot",
        "/compat/linux/proc",
        "/compat/linux/sys",
        "/dev",
        "/etc",
        "/home",
        "/lib",
        "/lib64",
        "/libexec",
        "/live/cow",
        "/live/image",
        "/media",
        "/mnt",
        "/net",
        "/opt",
        "/proc",
        "/rescue",
        "/root",
        "/run",
        "/sbin",
        "/srv",
        "/sys",
        "/tmp",
        "/usr",
        "/usr/X11R6",
        "/usr/local",
        "/usr/obj",
        "/usr/ports",
        "/usr/src",
        "/usr/xobj",
        "/var",
        "/var/crash",
        "/var/local",
        "/var/log",
        "/var/log/audit",
        "/var/mail",
        "/var/run",
        "/var/tmp",
    ];
    let text = path.to_string_lossy();
    SYSTEM.contains(&text.as_ref())
        || text.starts_with("/dev/")
        || text.starts_with("/proc/")
        || text.starts_with("/sys/")
        || text.ends_with("/.gvfs")
}

/// Whether a comma-separated fstab option list carries `name` or `name=…`.
fn has_fstab_option(options: &str, name: &str) -> bool {
    options.split(',').any(|option| {
        option == name || option.strip_prefix(name).is_some_and(|rest| rest.starts_with('='))
    })
}

fn fstab_option_value<'a>(options: &'a str, name: &str) -> Option<&'a str> {
    options.split(',').find_map(|option| {
        option.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')).filter(|v| !v.is_empty())
    })
}

/// GVfs's naming: the label, else the fstab name, else the size and the word
/// Volume, which is what "268 GB Volume" in every GNOME sidebar is.
fn display_name(block: &BlockFacts) -> String {
    if let Some(name) =
        block.fstab.as_ref().and_then(|entry| fstab_option_value(&entry.options, "x-gvfs-name"))
    {
        return unescape_fstab(name);
    }
    if !block.label.is_empty() {
        return block.label.clone();
    }
    if block.size > 0 {
        return format!("{} Volume", size_for_display(block.size));
    }
    "Volume".to_string()
}

/// Decimal units, one decimal below ten, like `udisks_client_get_size_for_display`.
fn size_for_display(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = "bytes";
    for candidate in UNITS {
        if value < 1000.0 {
            break;
        }
        value /= 1000.0;
        unit = candidate;
    }
    if unit == "bytes" {
        format!("{bytes} bytes")
    } else if value < 10.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}

/// fstab escapes spaces as `\040`; `x-gvfs-name=My\040Disk` should read back
/// as "My Disk".
fn unescape_fstab(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('\\') {
        output.push_str(&rest[..index]);
        let escape = &rest[index + 1..];
        match escape.get(..3).and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
            Some(byte) => {
                output.push(byte as char);
                rest = &escape[3..];
            }
            None => {
                output.push('\\');
                rest = escape;
            }
        }
    }
    output.push_str(rest);
    output
}

fn block_facts(properties: &Properties) -> BlockFacts {
    let path_property = |name: &str| -> Option<OwnedObjectPath> {
        property::<OwnedObjectPath>(properties, name).filter(|path| path.as_str() != "/")
    };
    BlockFacts {
        device: PathBuf::from(OsString::from_vec(trim_nul(
            property::<Vec<u8>>(properties, "Device").unwrap_or_default(),
        ))),
        drive: path_property("Drive"),
        label: property(properties, "IdLabel").unwrap_or_default(),
        id_type: property(properties, "IdType").unwrap_or_default(),
        id_usage: property(properties, "IdUsage").unwrap_or_default(),
        size: property(properties, "Size").unwrap_or_default(),
        read_only: property(properties, "ReadOnly").unwrap_or_default(),
        hint_ignore: property(properties, "HintIgnore").unwrap_or_default(),
        crypto_backing_device: path_property("CryptoBackingDevice"),
        mount_points: None,
        fstab: fstab_entry(properties),
    }
}

/// The `fstab` item of `Block.Configuration`, `a(sa{sv})` with byte-string
/// values for `dir` and `opts`.
fn fstab_entry(properties: &Properties) -> Option<FstabEntry> {
    let configuration =
        property::<Vec<(String, HashMap<String, OwnedValue>)>>(properties, "Configuration")?;
    configuration.into_iter().find(|(kind, _)| kind == "fstab").map(|(_, values)| {
        let text = |name: &str| {
            property::<Vec<u8>>(&values, name)
                .map(|bytes| String::from_utf8_lossy(&trim_nul(bytes)).into_owned())
                .unwrap_or_default()
        };
        FstabEntry { dir: PathBuf::from(text("dir")), options: text("opts") }
    })
}

fn drive_facts(properties: &Properties) -> DriveFacts {
    DriveFacts {
        removable: property(properties, "Removable").unwrap_or_default()
            || property(properties, "MediaRemovable").unwrap_or_default(),
        ejectable: property(properties, "Ejectable").unwrap_or_default(),
        can_power_off: property(properties, "CanPowerOff").unwrap_or_default(),
        setup_by_uid: None,
    }
}

fn property<T: TryFrom<OwnedValue>>(properties: &Properties, name: &str) -> Option<T> {
    properties.get(name).and_then(|value| T::try_from(value.clone()).ok())
}

/// UDisks2 byte strings carry their C terminator.
fn trim_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/test")
    }

    #[test]
    fn mount_points_the_user_would_look_at_are_shown() {
        let shown = |path: &str| should_show_path(Path::new(path), None, "test", &home());
        assert!(shown("/run/media/test/STICK"));
        assert!(shown("/run/media/test/deeper/STICK"));
        assert!(shown("/media/STICK"));
        assert!(shown("/media/test/STICK"));
        assert!(shown("/home/test/mnt/backup"));
        assert!(!shown("/media/other/STICK"), "another user's mount under /media");
        assert!(!shown("/run/media/other/STICK"));
        assert!(!shown("/home/test"), "the home itself is not a volume");
        assert!(!shown("/home/test/.hidden/mount"), "a dot path is a request to hide");
        assert!(!shown("/mnt/windows"), "outside the user's areas without x-gvfs-show");
        assert!(!shown("/"), "system-internal");
        assert!(!shown("/boot"));
        assert!(!shown("/nix/store"), "not in the list, not in the user's areas");
        assert!(!shown("/dev/shm"));
    }

    #[test]
    fn fstab_options_decide_outright() {
        let shown = |path: &str, options: &str| {
            should_show_path(Path::new(path), Some(options), "test", &home())
        };
        assert!(shown("/mnt/windows", "uid=1000,x-gvfs-show,nofail"));
        assert!(shown("/mnt/windows", "x-gvfs-show=1"));
        assert!(!shown("/run/media/test/STICK", "x-gvfs-hide"));
        assert!(!shown("/mnt/windows", "x-gvfs-showoff"), "a prefix is not the option");
    }

    #[test]
    fn listing_follows_gvfs() {
        let user = "test";
        let facts = |mount_points: Option<Vec<&str>>, fstab: Option<(&str, &str)>| BlockFacts {
            mount_points: mount_points
                .map(|points| points.into_iter().map(PathBuf::from).collect()),
            fstab: fstab.map(|(dir, options)| FstabEntry {
                dir: PathBuf::from(dir),
                options: options.to_string(),
            }),
            ..BlockFacts::default()
        };
        let drive = DriveFacts::default();
        let list = |block: &BlockFacts| should_list(block, &drive, user, &home(), 1000);

        assert!(list(&facts(Some(vec![]), None)), "an unmounted stick");
        assert!(list(&facts(Some(vec!["/run/media/test/STICK"]), None)));
        assert!(!list(&facts(None, None)), "no filesystem, nothing to open");
        assert!(!list(&facts(Some(vec!["/"]), Some(("/", "x-initrd.mount")))), "the root");
        assert!(!list(&facts(Some(vec![]), Some(("/mnt/windows", "uid=1000")))));
        assert!(list(&facts(Some(vec![]), Some(("/mnt/windows", "uid=1000,x-gvfs-show")))));
        assert!(list(&facts(Some(vec!["/mnt/windows"]), Some(("/mnt/windows", "x-gvfs-show")))));

        let mut ignored = facts(Some(vec![]), None);
        ignored.hint_ignore = true;
        assert!(!list(&ignored), "HintIgnore trumps everything");

        let mut locked = facts(Some(vec![]), None);
        locked.id_usage = "crypto".to_string();
        assert!(!list(&locked));

        let others_image = DriveFacts { setup_by_uid: Some(1001), ..DriveFacts::default() };
        assert!(!should_list(&facts(Some(vec![]), None), &others_image, user, &home(), 1000));
        let root_image = DriveFacts { setup_by_uid: Some(0), ..DriveFacts::default() };
        assert!(should_list(&facts(Some(vec![]), None), &root_image, user, &home(), 1000));
    }

    #[test]
    fn names_follow_gvfs() {
        let named = |label: &str, size: u64, fstab: Option<&str>| {
            display_name(&BlockFacts {
                label: label.to_string(),
                size,
                fstab: fstab.map(|options| FstabEntry {
                    dir: PathBuf::from("/mnt/x"),
                    options: options.to_string(),
                }),
                ..BlockFacts::default()
            })
        };
        assert_eq!(named("GPARTED-LIV", 8_000_000_000, None), "GPARTED-LIV");
        assert_eq!(named("", 268_435_456_000, None), "268 GB Volume");
        assert_eq!(named("", 8_002_000_000, None), "8.0 GB Volume");
        assert_eq!(named("", 512, None), "512 bytes Volume");
        assert_eq!(named("", 0, None), "Volume");
        assert_eq!(named("C", 1, Some("uid=1000,x-gvfs-name=Windows\\040Disk")), "Windows Disk");
    }

    #[test]
    fn udisks_byte_strings_lose_their_terminator() {
        assert_eq!(trim_nul(b"/dev/sda1\0".to_vec()), b"/dev/sda1");
        assert_eq!(trim_nul(b"".to_vec()), b"");
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Talks to the real UDisks2, so it is opt-in: `cargo test -- --ignored
    /// live_snapshot --nocapture` prints what the sidebar would list.
    #[test]
    #[ignore = "needs UDisks2 on the system bus"]
    fn live_snapshot() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let user = std::env::var("USER").unwrap_or_default();
        let volumes = smol::block_on(async {
            let monitor = VolumeMonitor::connect(user, home).await?;
            monitor.snapshot().await
        })
        .unwrap();
        for volume in &volumes {
            eprintln!("{volume:#?}");
        }
    }
}
