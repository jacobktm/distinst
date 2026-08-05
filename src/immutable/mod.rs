//! Immutable overlay layout support for the erase-and-install path.
//!
//! The immutable system installs onto a btrfs filesystem with a fixed set of
//! subvolumes instead of a flat root:
//!
//! - `@base`: locked, read-only OS template (extracted and configured here).
//! - `@data`: shared data/CLI/hooks payload, bind-mounted into overlays at boot.
//! - `@snapshots`: parent for overlay snapshots.
//! - `@overlay-init`: bootable snapshot of `@base`, the default boot overlay.
//! - `@overlay-recovery`: locked, read-only fallback snapshot of `@base`.
//!
//! The root btrfs partition is mounted with `subvol=@base` during install (see
//! `PartitionInfo::mount_options`); at boot the system mounts `subvol=@overlay-init`.
//! This module implements the subvolume creation, provisioning of `@data` + `@base`
//! from the `/usr/lib/immutable` bundle carried in the live image, the fstab/crypttab
//! rewrites, and the bootloader overlay snapshots.

use crate::external::exec;
use crate::installer::traits::InstallerDiskOps;
use std::{
    ffi::OsString,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use sys_mount::{Mount, MountFlags, Unmount, UnmountFlags};
use tempdir::TempDir;

pub const SUBVOL_BASE: &str = "@base";
pub const SUBVOL_DATA: &str = "@data";
pub const SUBVOL_SNAPSHOTS: &str = "@snapshots";
pub const SUBVOL_OVERLAY_INIT: &str = "@overlay-init";
pub const SUBVOL_OVERLAY_RECOVERY: &str = "@overlay-recovery";

/// The btrfs root of the filesystem, as a subvolume id.
const TOP_LEVEL: &str = "subvolid=5";

/// Kernel options applied to the booted overlay (mirrors install.sh).
const KERNEL_OPTIONS: &[&str] = &[
    "quiet",
    "loglevel=0",
    "systemd.show_status=false",
    "splash",
    "rootflags=subvol=@overlay-init",
];

fn io_err(msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.to_string())
}

/// Runs a `btrfs` sub-command against the given paths.
fn btrfs(args: &[&str]) -> io::Result<()> {
    let args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
    exec("btrfs", None, None, &args)
}

/// Returns the device path of the immutable btrfs root partition.
fn root_device<D: InstallerDiskOps>(disks: &D) -> io::Result<PathBuf> {
    disks
        .get_block_info_of("/")?
        .uid
        .get_device_path()
        .ok_or_else(|| io_err("unable to resolve the immutable root device path"))
}

/// True if the disk configuration uses the immutable overlay layout.
pub fn is_immutable<D: InstallerDiskOps>(disks: &D) -> bool { disks.has_immutable_root() }

/// The path to the pool (top-level btrfs) mount inside the install mount dir.
fn pool_path(mount_dir: &Path) -> PathBuf { mount_dir.join("pool") }

/// Mounts the top-level btrfs filesystem at `<mount_dir>/pool` so that `@data`
/// and the overlay snapshots can be reached during the install.
fn mount_pool(device: &Path, mount_dir: &Path) -> io::Result<Mount> {
    let pool = pool_path(mount_dir);
    fs::create_dir_all(&pool).map_err(|why| io_err(format!("creating pool dir: {}", why)))?;
    Mount::new(device, &pool, "btrfs", MountFlags::empty(), Some(TOP_LEVEL))
        .map_err(|why| io_err(format!("mounting pool at {}: {}", pool.display(), why)))
}

/// Recursively copies `src` into `dest` using `cp -a`.
fn copy_tree(src: &Path, dest: &Path) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|why| io_err(format!("creating {}: {}", parent.display(), why)))?;
    }
    fs::create_dir_all(dest).map_err(|why| io_err(format!("creating {}: {}", dest.display(), why)))?;

    let mut src_arg = src.as_os_str().to_owned();
    src_arg.push("/.");
    let dest_arg = dest.as_os_str().to_owned();
    exec("cp", None, None, &[OsString::from("-a"), src_arg, dest_arg])
        .map_err(|why| io_err(format!("copying {} to {}: {}", src.display(), dest.display(), why)))
}

/// Creates the immutable btrfs subvolumes on a freshly formatted root, and
/// seeds the initial `@data` directory structure.
///
/// This is a no-op when the layout is not immutable.
pub fn create_subvolumes<D: InstallerDiskOps>(disks: &D) -> io::Result<()> {
    if !is_immutable(disks) {
        return Ok(());
    }
    let device = root_device(disks)?;

    info!("creating immutable btrfs subvolumes on {}", device.display());

    let tmp = TempDir::new("distinst-subvol").map_err(|why| io_err(format!("creating temp dir: {}", why)))?;
    {
        let mount = Mount::new(&device, tmp.path(), "btrfs", MountFlags::empty(), Some(TOP_LEVEL))
            .map_err(|why| io_err(format!("mounting {} for subvolume creation: {}", device.display(), why)))?;

        for subvol in &[SUBVOL_DATA, SUBVOL_SNAPSHOTS, SUBVOL_BASE] {
            let path = tmp.path().join(subvol);
            btrfs(&["subvolume", "create", path.to_str().unwrap()])
                .map_err(|why| io_err(format!("creating {} subvolume: {}", subvol, why)))?;
        }

        // Seed @data with default user directories and dotfile stubs.
        let data = tmp.path().join(SUBVOL_DATA);
        for dir in &["Documents", "Downloads", "Pictures", "Videos", "Music"] {
            fs::create_dir_all(data.join(dir)).map_err(|why| {
                io_err(format!("seeding @data/{}: {}", dir, why))
            })?;
        }
        for dotfile in &[".bash_history", ".profile", ".bashrc", ".gitconfig"] {
            let _ = fs::OpenOptions::new().create(true).truncate(true).write(true).open(data.join(dotfile));
        }

        // Unmount the temporary top-level mount before the chroot mounts the
        // same device with `subvol=@base`. Leaving the top-level mount in place
        // makes the subsequent mount of the same device fail with ENOENT; this
        // mirrors install.sh, which unmounts between subvolume creation and the
        // `subvol=@base` mount.
        mount
            .unmount(UnmountFlags::empty())
            .map_err(|why| io_err(format!("unmounting {} after subvolume creation: {}", device.display(), why)))?;
    }

    info!("created btrfs subvolumes {}/{}/{}/{}", SUBVOL_DATA, SUBVOL_SNAPSHOTS, SUBVOL_BASE, TOP_LEVEL);
    Ok(())
}

/// Writes a file, creating parent directories, with the given mode.
fn write_file(dest: &Path, content: &str, mode: u32) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|why| io_err(format!("creating {}: {}", parent.display(), why)))?;
    }
    fs::write(dest, content)
        .map_err(|why| io_err(format!("writing {}: {}", dest.display(), why)))?;
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))
        .map_err(|why| io_err(format!("chmod {}: {}", dest.display(), why)))?;
    Ok(())
}

/// Copies a single file into place with the given mode, creating parents.
fn install_file(src: &Path, dest: &Path, mode: u32) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|why| io_err(format!("creating {}: {}", parent.display(), why)))?;
    }
    fs::copy(src, dest)
        .map_err(|why| io_err(format!("copying {} to {}: {}", src.display(), dest.display(), why)))?;
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))
        .map_err(|why| io_err(format!("chmod {}: {}", dest.display(), why)))?;
    Ok(())
}

/// The UUID of the immutable btrfs root filesystem.
fn root_uuid<D: InstallerDiskOps>(disks: &D) -> io::Result<String> {
    disks.get_block_info_of("/").map(|entry| entry.uid.id)
}

/// The `Pop_OS-<uuid>` directory on the ESP, creating it if needed.
fn esp_pop_dir(mount_dir: &Path, uuid: &str) -> io::Result<PathBuf> {
    let efi_base = mount_dir.join("boot/efi/EFI");
    fs::create_dir_all(&efi_base)
        .map_err(|why| io_err(format!("creating {}: {}", efi_base.display(), why)))?;
    if let Ok(rd) = fs::read_dir(&efi_base) {
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("Pop_OS-") {
                return Ok(entry.path());
            }
        }
    }
    let pop_dir = efi_base.join(format!("Pop_OS-{}", uuid));
    fs::create_dir_all(&pop_dir)
        .map_err(|why| io_err(format!("creating {}: {}", pop_dir.display(), why)))?;
    Ok(pop_dir)
}

/// Copies the newest kernel from the extracted root into the ESP so that the
/// initramfs hooks have something to update.
fn copy_kernel_to_esp(mount_dir: &Path, uuid: &str) -> io::Result<()> {
    let boot = mount_dir.join("boot");
    let kernel = fs::read_dir(&boot)
        .map(|rd| {
            rd.flatten()
                .find(|e| e.file_name().to_string_lossy().starts_with("vmlinuz-"))
        })
        .ok()
        .flatten();

    let kernel = match kernel {
        Some(k) => k.path(),
        None => {
            warn!("no kernel found in {} -- skipping ESP kernel copy", boot.display());
            return Ok(());
        }
    };

    let pop_dir = esp_pop_dir(mount_dir, uuid)?;
    install_file(&kernel, &pop_dir.join("vmlinuz.efi"), 0o644)
}

/// Applies the immutable fstab changes: rewrites the btrfs root entry to boot
/// into `@overlay-init` and adds the `/pool` (subvolid=5) entry.
fn immutable_fstab(mount_dir: &Path) -> io::Result<()> {
    let path = mount_dir.join("etc/fstab");
    let content = fs::read_to_string(&path)
        .map_err(|why| io_err(format!("reading {}: {}", path.display(), why)))?;

    let mut out = String::new();
    let mut root = None;

    for line in content.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 6 && fields[1] == "/" && fields[2] == "btrfs" {
            root = Some(fields[0].to_string());
            out.push_str(&format!(
                "{}  /  btrfs  defaults,noatime,compress=zstd:1,ssd,subvol={}  0  1\n",
                fields[0], SUBVOL_OVERLAY_INIT
            ));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }

    if let Some(uuid) = root {
        out.push_str(&format!(
            "{}  /pool  btrfs  defaults,noatime,subvolid=5  0  0\n",
            uuid
        ));
    } else {
        warn!("immutable fstab: could not locate the btrfs root entry in {}", path.display());
    }

    fs::write(&path, out)
        .map_err(|why| io_err(format!("writing {}: {}", path.display(), why)))
}

/// Provisions the immutable system from the `/usr/lib/immutable` bundle carried
/// in the live image. Populates both `@data/immutable` (the shared, bind-mounted
/// payload) and the `@base` paths (fallbacks + hooks + services).
pub fn provision<D: InstallerDiskOps>(disks: &D, mount_dir: &Path, username: Option<&str>) -> io::Result<()> {
    if !is_immutable(disks) {
        return Ok(());
    }
    let device = root_device(disks)?;
    let uuid = root_uuid(disks)?;

    let bundle = mount_dir.join("usr/lib/immutable");
    if !bundle.exists() {
        warn!(
            "immutable bundle not found at {} -- skipping immutable provisioning",
            bundle.display()
        );
        return Ok(());
    }

    info!("provisioning immutable overlay system on {}", device.display());
    let pool = mount_pool(&device, mount_dir)?;
    {
        let data_immutable = pool_path(mount_dir).join(SUBVOL_DATA).join("immutable");
        copy_tree(&bundle, &data_immutable)?;

        // @base fallbacks and hooks (bind-mounted @data overrides these at boot).
        let b = |p: &str| mount_dir.join(p);
        let src = |p: &str| bundle.join(p);

        install_file(&src("bin/immutable"), &b("usr/local/bin/immutable"), 0o755)?;
        install_file(&src("bash-completion/immutable"), &b("usr/share/bash-completion/completions/immutable"), 0o644)?;
        install_file(&src("man/immutable.1.gz"), &b("usr/share/man/man1/immutable.1.gz"), 0o644)?;
        install_file(&src("profile.d/immutable-prompt.sh"), &b("etc/profile.d/immutable-prompt.sh"), 0o644)?;
        install_file(&src("hooks/dpkg-immutable"), &b("etc/dpkg/dpkg.cfg.d/99-immutable"), 0o644)?;
        install_file(&src("hooks/immutable-hooks-apt-hook"), &b("etc/apt/apt.conf.d/99-immutable-hooks"), 0o644)?;
        install_file(&src("hooks/immutable-hooks-apt-hook"), &b("usr/lib/immutable/immutable-hooks-apt-hook"), 0o644)?;
        install_file(&src("hooks/apt-proxy-detect"), &b("usr/lib/immutable/apt-proxy-detect"), 0o755)?;
        install_file(&src("hooks/apt-proxy-detect.sh"), &b("usr/lib/immutable/apt-proxy-detect.sh"), 0o755)?;
        install_file(&src("immutable-boot-counter.sh"), &b("usr/lib/immutable/immutable-boot-counter.sh"), 0o755)?;
        install_file(&src("immutable-healthcheck.sh"), &b("usr/lib/immutable/immutable-healthcheck.sh"), 0o755)?;
        install_file(&src("services/immutable-data-mount.service"), &b("usr/lib/immutable/immutable-data-mount.service"), 0o644)?;

        install_file(&src("hooks/kernel-postinst.d/zz-kernelstub"), &b("etc/kernel/postinst.d/zz-kernelstub"), 0o755)?;
        install_file(&src("hooks/kernel-postinst.d/zz-systemd-boot"), &b("etc/kernel/postinst.d/zz-systemd-boot"), 0o755)?;
        install_file(&src("hooks/initramfs-post-update.d/zz-kernelstub"), &b("etc/initramfs/post-update.d/zz-kernelstub"), 0o755)?;
        install_file(&src("hooks/initramfs-post-update.d/systemd-boot"), &b("etc/initramfs/post-update.d/systemd-boot"), 0o755)?;

        for service in &[
            "immutable-data-mount",
            "fix-devpts",
            "immutable-boot-counter",
            "immutable-healthcheck",
        ] {
            let name = format!("{}.service", service);
            install_file(&src(&format!("services/{}", name)), &b(&format!("etc/systemd/system/{}", name)), 0o644)?;
        }

        // Systemd enablement (manual symlinks; systemctl enable fails in chroot).
        for (wants, service) in &[
            ("multi-user.target.wants/immutable-data-mount.service", "immutable-data-mount.service"),
            ("sysinit.target.wants/immutable-data-mount.service", "immutable-data-mount.service"),
            ("sysinit.target.wants/fix-devpts.service", "fix-devpts.service"),
            ("multi-user.target.wants/immutable-boot-counter.service", "immutable-boot-counter.service"),
            ("multi-user.target.wants/immutable-healthcheck.service", "immutable-healthcheck.service"),
        ] {
            let link = b(&format!("etc/systemd/system/{}", wants));
            fs::create_dir_all(link.parent().unwrap())
                .map_err(|why| io_err(format!("creating {}: {}", link.parent().unwrap().display(), why)))?;
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(format!("/etc/systemd/system/{}", service), &link)
                .map_err(|why| io_err(format!("symlinking {}: {}", link.display(), why)))?;
        }

        // Apt proxy auto-detect (takes precedence over any static proxy).
        write_file(
            &b("etc/apt/apt.conf.d/99-immutable-proxy"),
            "Acquire::http::ProxyAutoDetect \"/usr/lib/immutable/apt-proxy-detect\";\n",
            0o644,
        )?;

        // Immutable config used by the CLI for username resolution.
        let mut conf = String::from("# Immutable Pop!_OS configuration\n");
        if let Some(name) = username {
            conf.push_str(&format!("USERNAME={}\n", name));
        }
        write_file(&b("etc/immutable.conf"), &conf, 0o644)?;

        // Kernelstub configuration (the in-chroot kernelstub is skipped for the
        // immutable layout, so the booted system needs this pre-seeded).
        let options = KERNEL_OPTIONS
            .iter()
            .map(|o| format!("\"{}\"", o))
            .collect::<Vec<_>>()
            .join(", ");
        write_file(
            &b("etc/kernelstub/configuration"),
            &format!(
                "{{\n  \"default\": {{\n    \"kernel_options\": [{options}],\n    \"esp_path\": \"/boot/efi\",\n    \"setup_loader\": false,\n    \"manage_mode\": false,\n    \"force_update\": false,\n    \"live_mode\": false,\n    \"config_rev\": 3\n  }},\n  \"user\": {{\n    \"kernel_options\": [{options}],\n    \"esp_path\": \"/boot/efi\",\n    \"setup_loader\": true,\n    \"manage_mode\": true,\n    \"force_update\": false,\n    \"live_mode\": false,\n    \"config_rev\": 3\n  }}\n}}\n"
            ),
            0o644,
        )?;

        // Initramfs must load the btrfs module.
        let modules = b("etc/initramfs-tools/modules");
        let mut content = fs::read_to_string(&modules)
            .unwrap_or_default();
        if !content.lines().any(|l| l.trim() == "btrfs") {
            content.push_str("btrfs\n");
        }
        write_file(&modules, &content, 0o644)?;

        immutable_fstab(mount_dir)?;

        // Ensure the ESP has a Pop_OS-<uuid> directory and initial kernel so the
        // initramfs/kernelpostinst hooks have a target during the chroot install.
        copy_kernel_to_esp(mount_dir, &uuid)?;
    }
    drop(pool);
    Ok(())
}

/// Boot loader options for the immutable overlay entries.
fn kernel_options(uuid: &str, subvol: &str, nvidia: bool) -> String {
    let mut opts = format!(
        "root=UUID={} ro quiet loglevel=0 systemd.show_status=false splash rootflags=subvol={}",
        uuid, subvol
    );
    if nvidia {
        opts.push_str(" nvidia-drm.modeset=1");
    }
    opts
}

/// Writes the systemd-boot loader configuration and overlay entries.
fn write_boot_entries(mount_dir: &Path, uuid: &str) -> io::Result<()> {
    // The nvidia dkms tree lives in the configured @base rootfs, not in the
    // live session from which the installer runs.
    let has_nvidia = mount_dir.join("var/lib/dkms/nvidia").exists();
    let entries = mount_dir.join("boot/efi/loader/entries");
    fs::create_dir_all(&entries)
        .map_err(|why| io_err(format!("creating {}: {}", entries.display(), why)))?;

    let pop_dir_name = esp_pop_dir(mount_dir, uuid)?;
    let pop_dir = pop_dir_name
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("Pop_OS-{}", uuid));

    let linux = format!("/EFI/{}/vmlinuz.efi", pop_dir);
    let initrd = format!("/EFI/{}/initrd.img", pop_dir);
    let opts_init = kernel_options(uuid, SUBVOL_OVERLAY_INIT, has_nvidia);
    let opts_recovery = kernel_options(uuid, SUBVOL_OVERLAY_RECOVERY, has_nvidia);

    write_file(
        &entries.join("Pop_OS-current.conf"),
        &format!("title Pop!_OS\nlinux {}\ninitrd {}\noptions {}\n", linux, initrd, opts_init),
        0o644,
    )?;
    write_file(
        &entries.join("immutable.conf"),
        &format!("title Immutable (overlay-init)\nlinux {}\ninitrd {}\noptions {}\n", linux, initrd, opts_init),
        0o644,
    )?;
    write_file(
        &entries.join("recovery.conf"),
        &format!("title Pop!_OS Recovery\nlinux {}\ninitrd {}\noptions {}\n", linux, initrd, opts_recovery),
        0o644,
    )?;
    write_file(
        &entries.join("previous.conf"),
        &format!(
            "title Pop!_OS (previous kernel)\nlinux /EFI/{}/vmlinuz-previous.efi\ninitrd /EFI/{}/initrd.img-previous\noptions {}\n",
            pop_dir, pop_dir, opts_init
        ),
        0o644,
    )?;

    write_file(
        &mount_dir.join("boot/efi/loader/loader.conf"),
        "default Pop_OS-current.conf\ntimeout 0\nconsole-mode max\n",
        0o644,
    )?;

    Ok(())
}

/// Finalizes the immutable layout after the chroot configuration: creates the
/// overlay snapshots of the configured `@base`, locks `@base` read-only, seeds
/// the `@data` boot state, and mirrors the ESP into the overlays.
pub fn finalize<D: InstallerDiskOps>(disks: &D, mount_dir: &Path) -> io::Result<()> {
    if !is_immutable(disks) {
        return Ok(());
    }
    let device = root_device(disks)?;
    let uuid = root_uuid(disks)?;

    info!("finalizing immutable overlays on {}", device.display());

    write_boot_entries(mount_dir, &uuid)?;

    let pool = mount_pool(&device, mount_dir)?;
    {
        let base = pool_path(mount_dir).join(SUBVOL_BASE);
        let init = pool_path(mount_dir).join(SUBVOL_OVERLAY_INIT);
        let recovery = pool_path(mount_dir).join(SUBVOL_OVERLAY_RECOVERY);

        btrfs(&["subvolume", "snapshot", base.to_str().unwrap(), init.to_str().unwrap()])
            .map_err(|why| io_err(format!("snapshotting {}: {}", SUBVOL_OVERLAY_INIT, why)))?;
        btrfs(&["subvolume", "snapshot", base.to_str().unwrap(), recovery.to_str().unwrap()])
            .map_err(|why| io_err(format!("snapshotting {}: {}", SUBVOL_OVERLAY_RECOVERY, why)))?;

        // Mirror the ESP into each overlay so overlay shells and `immutable
        // switch` have a boot directory without touching the real ESP.
        copy_tree(&mount_dir.join("boot/efi"), &init.join("boot/efi"))?;
        copy_tree(&mount_dir.join("boot/efi"), &recovery.join("boot/efi"))?;

        btrfs(&["property", "set", "-ts", recovery.to_str().unwrap(), "ro", "true"])
            .map_err(|why| io_err(format!("locking {} read-only: {}", SUBVOL_OVERLAY_RECOVERY, why)))?;
        btrfs(&["property", "set", "-ts", base.to_str().unwrap(), "ro", "true"])
            .map_err(|why| io_err(format!("locking {} read-only: {}", SUBVOL_BASE, why)))?;

        // Initialize the boot counter in @data.
        let data = pool_path(mount_dir).join(SUBVOL_DATA);
        write_file(&data.join("boot-counter"), "0\n", 0o644)?;
        write_file(&data.join("boot-last-overlay"), &format!("{}\n", SUBVOL_OVERLAY_INIT), 0o644)?;
    }
    drop(pool);

    info!("created {} and {}; {} is read-only", SUBVOL_OVERLAY_INIT, SUBVOL_OVERLAY_RECOVERY, SUBVOL_BASE);
    Ok(())
}

