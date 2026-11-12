//! avbctl as install scripts run it before they write a system that
//! verified boot would turn away: to tell, and to turn on or off,
//! dm-verity, which checks each block of a partition as it is read, and
//! verification, the bootloader's checking of the partitions at all. Each
//! is a flag in the header of the top-level vbmeta image of the slot the
//! phone runs from, which an unlocked bootloader honours. What it prints
//! and how it ends are what those scripts read.

use super::Io;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// How avbctl ends, numbered as sysexits.h numbers them.
const DONE: i32 = 0;
const MISUSED: i32 = 64;
const FAILED: i32 = 70;
const REFUSED: i32 = 77;

/// A vbmeta image's header: 256 bytes starting with its magic, its flags a
/// big-endian 32-bit word 120 bytes in.
const HEADER_LEN: usize = 256;
const MAGIC: &[u8] = b"AVB0";
const FLAGS_AT: usize = 120;

/// The footer ending a partition whose image carries its vbmeta image
/// along: 64 bytes starting with its magic, where the vbmeta image starts
/// in the partition a big-endian 64-bit word 20 bytes in.
const FOOTER_LEN: usize = 64;
const FOOTER_MAGIC: &[u8] = b"AVBf";
const VBMETA_AT: usize = 20;

const USAGE: &str = "\
avbctl - command-line tool for AVB.

Usage:
  avbctl [--force] COMMAND

Commands:
  avbctl get-verity           - Prints whether verity is enabled in current slot.
  avbctl disable-verity       - Disable verity in current slot.
  avbctl enable-verity        - Enable verity in current slot.
  avbctl get-verification     - Prints whether verification is enabled in current slot.
  avbctl disable-verification - Disable verification in current slot.
  avbctl enable-verification  - Enable verification in current slot.
";

const LOCKED: &str = "\
avbctl: the bootloader is locked, and a locked bootloader does not boot a slot
whose vbmeta image has been changed. Run with --force to change it all the same.
";

/// What avbctl turns on and off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Check {
    Verity,
    Verification,
}

impl Check {
    fn name(self) -> &'static str {
        match self {
            Check::Verity => "verity",
            Check::Verification => "verification",
        }
    }

    /// The flag in the header that turns it off.
    fn off(self) -> u32 {
        match self {
            Check::Verity => 1 << 0,
            Check::Verification => 1 << 1,
        }
    }
}

/// What avbctl is asked: whether a check is on, or to turn it on or off.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Command {
    Get(Check),
    Set(Check, bool),
}

/// The phone avbctl works on: the folder its partitions are named in, the
/// suffix of the slot it runs from, empty on a phone with one slot, and
/// whether its bootloader is locked.
struct Phone<'a> {
    by_name: &'a Path,
    suffix: &'a str,
    locked: bool,
}

pub(super) fn run(args: &[OsString], io: &mut Io) -> i32 {
    let suffix = skyward_sys::prop("ro.boot.slot_suffix").unwrap_or_default();
    let locked = skyward_sys::prop("ro.boot.vbmeta.device_state").as_deref() == Some("locked");
    avbctl(args, io, &Phone { by_name: Path::new(skyward_block::BY_NAME), suffix: &suffix, locked })
}

fn avbctl(args: &[OsString], io: &mut Io, phone: &Phone) -> i32 {
    let (mut force, mut command) = (false, None);
    // Of the commands given the last counts, and anything else is passed
    // over.
    for arg in args {
        match arg.to_str() {
            Some("--force") => force = true,
            Some("get-verity") => command = Some(Command::Get(Check::Verity)),
            Some("disable-verity") => command = Some(Command::Set(Check::Verity, false)),
            Some("enable-verity") => command = Some(Command::Set(Check::Verity, true)),
            Some("get-verification") => command = Some(Command::Get(Check::Verification)),
            Some("disable-verification") => command = Some(Command::Set(Check::Verification, false)),
            Some("enable-verification") => command = Some(Command::Set(Check::Verification, true)),
            _ => {}
        }
    }
    let Some(command) = command else {
        let _ = io.errors.write_all(USAGE.as_bytes());
        return MISUSED;
    };
    let (Command::Get(check) | Command::Set(check, _)) = command;
    let on = match is_on(phone, check) {
        Ok(on) => on,
        Err(e) => {
            let _ = writeln!(io.errors, "avbctl: {e}");
            let _ = writeln!(io.errors, "Error getting whether {} is enabled.", check.name());
            return FAILED;
        }
    };
    let slot = match phone.suffix {
        "" => String::new(),
        suffix => format!(" on slot with suffix {suffix}"),
    };
    let state = |on: bool| if on { "enabled" } else { "disabled" };
    match command {
        Command::Get(_) => say(io, &format!("{} is {}{slot}.\n", check.name(), state(on))),
        Command::Set(_, want) if want == on => say(io, &format!("{} is already {}{slot}.\n", check.name(), state(on))),
        Command::Set(_, want) => {
            if phone.locked && !force {
                let _ = io.errors.write_all(LOCKED.as_bytes());
                return REFUSED;
            }
            if let Err(e) = set(phone, check, want) {
                let _ = writeln!(io.errors, "avbctl: {e}");
                let _ = writeln!(io.errors, "Error setting {}.", check.name());
                return FAILED;
            }
            say(io, &format!("Successfully {} {}{slot}. Reboot the device for changes to take effect.\n", state(want), check.name()))
        }
    }
}

fn say(io: &mut Io, text: &str) -> i32 {
    match io.output.write_all(text.as_bytes()).and_then(|()| io.output.flush()) {
        Ok(()) => DONE,
        Err(_) => FAILED,
    }
}

/// Whether `check` is on in the slot's top-level vbmeta image.
fn is_on(phone: &Phone, check: Check) -> io::Result<bool> {
    Ok(crate::avb::read(phone.by_name, phone.suffix)?.flags & check.off() == 0)
}

/// Turn `check` on or off in the slot's top-level vbmeta image, leaving the
/// rest of it as it was.
fn set(phone: &Phone, check: Check, on: bool) -> io::Result<()> {
    crate::avb::set(phone.by_name, phone.suffix, check.off(), on).map(|_| ())
}

/// Where the slot's top-level vbmeta image starts: at the start of its
/// vbmeta partition, or, on a phone without one, in its boot partition
/// where the footer ending it says.
fn top_level(phone: &Phone) -> io::Result<(PathBuf, u64)> {
    let vbmeta = phone.by_name.join(format!("vbmeta{}", phone.suffix));
    if vbmeta.exists() {
        return Ok((vbmeta, 0));
    }
    let boot = phone.by_name.join(format!("boot{}", phone.suffix));
    let mut file = File::open(&boot).map_err(|e| about(&boot, e))?;
    let mut footer = [0; FOOTER_LEN];
    let footer_at = file.seek(SeekFrom::End(-(FOOTER_LEN as i64))).map_err(|e| about(&boot, e))?;
    file.read_exact(&mut footer).map_err(|e| about(&boot, e))?;
    if !footer.starts_with(FOOTER_MAGIC) {
        let why = format!("there is no {}, and {} does not end in a vbmeta footer", vbmeta.display(), boot.display());
        return Err(io::Error::new(ErrorKind::InvalidData, why));
    }
    let at = u64::from_be_bytes(footer[VBMETA_AT..VBMETA_AT + 8].try_into().expect("eight bytes"));
    let size = u64::from_be_bytes(footer[28..36].try_into().expect("eight bytes"));
    if size < HEADER_LEN as u64 || at.checked_add(size).is_none_or(|end| end > footer_at) {
        return Err(io::Error::new(ErrorKind::InvalidData, format!("{} has a vbmeta image outside its AVB footer bounds", boot.display())));
    }
    Ok((boot, at))
}

fn flags(header: &[u8; HEADER_LEN]) -> u32 {
    u32::from_be_bytes(header[FLAGS_AT..FLAGS_AT + 4].try_into().expect("four bytes"))
}

/// `e`, saying which partition it came from.
fn about(path: &Path, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// What the pages and the installer use, as against what a script runs.
// ---------------------------------------------------------------------------

/// What the phone's vbmeta says now: the slot it was read for, the flags it
/// holds, whether the bootloader is unlocked, and a hash of the whole image.
///
/// The hash is what makes a change safe: the page shows a state, the owner
/// confirms it, and the write refuses unless the partition still hashes to
/// what was shown. Nothing else on the phone writes vbmeta, but a page left
/// open while a zip flashes one would otherwise overwrite the newer image
/// with flags read from the older.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub suffix: String,
    pub flags: u32,
    pub unlocked: bool,
    pub digest: [u8; 32],
}

impl State {
    /// Whether `check` is turned off in this state.
    pub fn disabled(&self, check: Check) -> bool {
        self.flags & check.off() != 0
    }
}

/// The current A/B slot and whether the bootloader is unlocked, as the
/// bootloader itself reported them at boot.
fn current_phone() -> io::Result<(String, bool)> {
    let suffix = skyward_sys::prop("ro.boot.slot_suffix").unwrap_or_default();
    if !matches!(suffix.as_str(), "_a" | "_b") {
        return Err(io::Error::other("the current A/B slot is unknown"));
    }
    let unlocked = skyward_sys::prop("ro.boot.vbmeta.device_state").as_deref() == Some("unlocked");
    Ok((suffix, unlocked))
}

/// Read the state of the slot the phone runs from, as it is now.
pub fn current() -> io::Result<State> {
    let (suffix, unlocked) = current_phone()?;
    read_state(Path::new(skyward_block::BY_NAME), &suffix, unlocked)
}

/// Turn both checks off in `slot`, after an install that asked for it.
///
/// This is the recovery's own act, not the package's: an install script is an
/// arbitrary program and may ask for anything, so whether a flashed ROM leaves
/// the phone bootable is decided here, from the option the owner set on the
/// install page, and from the slot the boot control service positively
/// reports. `Ok(None)` where the slot was already clear.
pub fn disable_slot(slot: skyward_vab::Slot) -> io::Result<Option<PathBuf>> {
    let (_, unlocked) = current_phone()?;
    if !unlocked {
        return Err(io::Error::other("an unlocked bootloader is required to change vbmeta"));
    }
    let by_name = Path::new(skyward_block::BY_NAME);
    let expected = read_state(by_name, slot.suffix(), unlocked)?;
    if expected.flags & BOTH == BOTH {
        return Ok(None);
    }
    let (_, backup) = change_flags_at(by_name, &backups(), &expected, unlocked, expected.flags | BOTH)?;
    Ok(Some(backup))
}

/// The flags that turn both checks off.
const BOTH: u32 = (1 << 0) | (1 << 1);

/// Where a vbmeta image is kept before it is changed, so that a phone that
/// will not boot afterwards can be put back. `/tmp` is memory in a recovery,
/// so this is a convenience for the session, not a lasting backup: the copy
/// the owner should keep is the one on their computer.
fn backups() -> PathBuf {
    PathBuf::from("/skyward/run")
}

/// Read the state of `suffix`'s vbmeta image, given whether the bootloader is
/// unlocked.
fn read_state(by_name: &Path, suffix: &str, unlocked: bool) -> io::Result<State> {
    // The suffix names a partition, so nothing but a slot's own name may
    // reach the path.
    if !matches!(suffix, "" | "_a" | "_b") {
        return Err(io::Error::other(format!("{suffix:?} is not a slot suffix")));
    }
    let phone = Phone { by_name, suffix, locked: !unlocked };
    let (path, at) = top_level(&phone)?;
    let mut file = File::open(&path).map_err(|e| about(&path, e))?;
    let image = read_vbmeta(&mut file, &path, at)?;
    Ok(State {
        suffix: suffix.to_string(),
        flags: flags(&image[..HEADER_LEN].try_into().expect("a header was read")),
        unlocked,
        digest: Sha256::digest(&image).into(),
    })
}

/// The whole vbmeta image a partition holds at `at`, bounded so that a
/// corrupt footer cannot make this read a gigabyte of somebody's data.
fn read_vbmeta(file: &mut File, path: &Path, at: u64) -> io::Result<Vec<u8>> {
    /// The most a vbmeta image can be. The largest this project has seen is
    /// tens of kilobytes; a megabyte is already generous for a phone that
    /// carries several chained descriptors.
    const MOST: usize = 1 << 20;
    file.seek(SeekFrom::Start(at)).map_err(|e| about(path, e))?;
    let mut image = Vec::new();
    Read::by_ref(file).take(MOST as u64 + 1).read_to_end(&mut image).map_err(|e| about(path, e))?;
    if image.len() > MOST {
        return Err(io::Error::new(ErrorKind::InvalidData, format!("{} exceeds the vbmeta backup size limit", path.display())));
    }
    // A vbmeta partition is usually bigger than the image in it, so the end
    // is not known: what bounds it is the header's own descriptor offset and
    // sizes. This only checks that a header is there and is not absurd.
    if image.len() < HEADER_LEN || !image.starts_with(MAGIC) {
        return Err(io::Error::new(ErrorKind::InvalidData, format!("{} holds no vbmeta header {at} bytes in", path.display())));
    }
    Ok(image)
}

/// Change one flag of the current slot's vbmeta, after confirming that it
/// still holds what the page showed.
pub fn change(expected: &State, check: Check, disabled: bool) -> io::Result<(State, PathBuf)> {
    let (suffix, unlocked) = current_phone()?;
    if suffix != expected.suffix {
        return Err(io::Error::other("the phone now starts from the other slot; read it again"));
    }
    let wanted = if disabled { expected.flags | check.off() } else { expected.flags & !check.off() };
    change_flags_at(Path::new(skyward_block::BY_NAME), &backups(), expected, unlocked, wanted)
}

/// Write `wanted` into the confirmed slot's vbmeta, keeping the image as it
/// was beside it first, and checking what was written by reading it back.
///
/// The image is re-read and re-hashed before anything is written, so a
/// `State` that no longer describes the partition is refused rather than used
/// to overwrite a newer one.
fn change_flags_at(by_name: &Path, backups: &Path, expected: &State, unlocked: bool, wanted: u32) -> io::Result<(State, PathBuf)> {
    if !expected.unlocked || !unlocked {
        return Err(io::Error::other("an unlocked bootloader is required to change vbmeta"));
    }
    let now = read_state(by_name, &expected.suffix, unlocked)?;
    if now != *expected {
        return Err(io::Error::other("vbmeta changed since the page read it; read it again before changing it"));
    }
    let phone = Phone { by_name, suffix: &expected.suffix, locked: !unlocked };
    let (path, at) = top_level(&phone)?;
    let mut file = OpenOptions::new().read(true).write(true).open(&path).map_err(|e| about(&path, e))?;
    let original = read_vbmeta(&mut file, &path, at)?;
    if <[u8; 32]>::from(Sha256::digest(&original)) != expected.digest {
        return Err(io::Error::other(format!("{} cannot be read back as it was", path.display())));
    }
    // The backup is written and flushed before the change, so that a phone
    // that will not boot afterwards has something to be put back with in this
    // session.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let name = match expected.suffix.as_str() {
        "" => format!("skyward-vbmeta-{stamp}.img"),
        suffix => format!("skyward-vbmeta{suffix}-{stamp}.img"),
    };
    let backup = backups.join(name);
    let mut saved = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&backup).map_err(|e| about(&backup, e))?;
    saved.write_all(&original).and_then(|()| saved.sync_all()).map_err(|e| about(&backup, e))?;
    let mut wanted_bytes = original.clone();
    wanted_bytes[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&wanted.to_be_bytes());
    let written = (|| -> io::Result<()> {
        file.seek(SeekFrom::Start(at + FLAGS_AT as u64)).map_err(|e| about(&path, e))?;
        file.write_all(&wanted.to_be_bytes()).map_err(|e| about(&path, e))?;
        file.sync_all().map_err(|e| about(&path, e))?;
        let read_back = read_vbmeta(&mut file, &path, at)?;
        if read_back != wanted_bytes {
            return Err(io::Error::other(format!("{} did not read back as it was written", path.display())));
        }
        Ok(())
    })();
    written.map_err(|e| io::Error::other(format!("{e}; the image as it was is kept at {}", backup.display())))?;
    Ok((State { flags: wanted, digest: Sha256::digest(&wanted_bytes).into(), ..expected.clone() }, backup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A vbmeta image whose flags are `flags`: its header, and the
    /// authentication block after it, which avbctl leaves as it is.
    fn vbmeta(flags: u32) -> Vec<u8> {
        let mut image = vec![0u8; 256 + 64];
        image[..4].copy_from_slice(b"AVB0");
        image[120..124].copy_from_slice(&flags.to_be_bytes());
        image[128..141].copy_from_slice(b"avbtool 1.3.0");
        for (i, b) in image[256..].iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        image
    }

    fn ran(args: &[&str], phone: &Phone) -> (i32, String, String) {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let code = avbctl(&args, &mut Io { input: &mut io::empty(), output: &mut output, errors: &mut errors }, phone);
        (code, String::from_utf8(output).unwrap(), String::from_utf8(errors).unwrap())
    }

    #[test]
    fn verity_and_verification_are_flags_in_the_header_of_the_slot_s_vbmeta_image() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("vbmeta_a"), dir.path().join("vbmeta_b"));
        fs::write(&a, vbmeta(0)).unwrap();
        fs::write(&b, vbmeta(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false };
        assert_eq!(ran(&["get-verity"], &phone), (0, "verity is enabled on slot with suffix _a.\n".into(), String::new()));
        assert_eq!(
            ran(&["--force", "disable-verity"], &phone),
            (0, "Successfully disabled verity on slot with suffix _a. Reboot the device for changes to take effect.\n".into(), String::new())
        );
        assert_eq!(fs::read(&a).unwrap(), vbmeta(1), "the verity flag set and nothing else");
        assert_eq!(ran(&["disable-verity"], &phone), (0, "verity is already disabled on slot with suffix _a.\n".into(), String::new()));
        assert_eq!(ran(&["disable-verification", "--force"], &phone).0, 0, "the command and --force in either order");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(3));
        assert_eq!(ran(&["get-verification"], &phone).1, "verification is disabled on slot with suffix _a.\n");
        assert_eq!(ran(&["get-verity"], &phone).1, "verity is disabled on slot with suffix _a.\n");
        assert_eq!(
            ran(&["enable-verity"], &phone).1,
            "Successfully enabled verity on slot with suffix _a. Reboot the device for changes to take effect.\n"
        );
        assert_eq!(fs::read(&a).unwrap(), vbmeta(2));
        assert_eq!(ran(&["enable-verification"], &phone).0, 0);
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0));
        assert_eq!(fs::read(&b).unwrap(), vbmeta(0), "the other slot's as it was");
    }

    #[test]
    fn a_phone_with_one_slot_names_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("vbmeta"), vbmeta(2)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "", locked: false };
        assert_eq!(ran(&["get-verification"], &phone).1, "verification is disabled.\n");
        assert_eq!(ran(&["enable-verification"], &phone).1, "Successfully enabled verification. Reboot the device for changes to take effect.\n");
        assert_eq!(fs::read(dir.path().join("vbmeta")).unwrap(), vbmeta(0));
    }

    #[test]
    fn a_locked_bootloader_s_flags_change_only_when_forced() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("vbmeta_a");
        fs::write(&a, vbmeta(2)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: true };
        assert_eq!(ran(&["disable-verity"], &phone), (77, String::new(), LOCKED.into()));
        assert_eq!(fs::read(&a).unwrap(), vbmeta(2), "nothing written");
        assert_eq!(ran(&["get-verity"], &phone).0, 0, "telling needs no force");
        assert_eq!(ran(&["disable-verification"], &phone).1, "verification is already disabled on slot with suffix _a.\n");
        assert_eq!(ran(&["--force", "disable-verity"], &phone).0, 0);
        assert_eq!(fs::read(&a).unwrap(), vbmeta(3));
    }

    #[test]
    fn without_a_vbmeta_partition_the_image_is_where_the_boot_partition_s_footer_says() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot_b");
        let partition = |flags: u32| {
            let (kernel, image) = (vec![0x11u8; 4096], vbmeta(flags));
            let mut boot = kernel.clone();
            boot.extend_from_slice(&image);
            boot.resize(4096 * 3, 0);
            let mut footer = b"AVBf".to_vec();
            footer.extend_from_slice(&1u32.to_be_bytes());
            footer.extend_from_slice(&0u32.to_be_bytes());
            footer.extend_from_slice(&(kernel.len() as u64).to_be_bytes());
            footer.extend_from_slice(&4096u64.to_be_bytes());
            footer.extend_from_slice(&(image.len() as u64).to_be_bytes());
            footer.resize(64, 0);
            let end = boot.len() - 64;
            boot[end..].copy_from_slice(&footer);
            boot
        };
        fs::write(&boot, partition(0)).unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_b", locked: false };
        assert_eq!(ran(&["get-verity"], &phone).1, "verity is enabled on slot with suffix _b.\n");
        assert_eq!(ran(&["disable-verity"], &phone).0, 0);
        assert_eq!(fs::read(&boot).unwrap(), partition(1));
    }

    #[test]
    fn what_holds_no_vbmeta_image_fails_and_stays_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false };
        let (code, output, errors) = ran(&["get-verity"], &phone);
        assert_eq!((code, output.as_str()), (70, ""), "no partition to look in");
        assert!(errors.ends_with("Error getting whether verity is enabled.\n"), "{errors}");
        fs::write(dir.path().join("boot_a"), vec![0u8; 8192]).unwrap();
        assert_eq!(ran(&["disable-verity"], &phone).0, 70, "a boot partition without a footer");
        let a = dir.path().join("vbmeta_a");
        let other = b"ANDROID!".repeat(64);
        fs::write(&a, &other).unwrap();
        let (code, _, errors) = ran(&["disable-verification"], &phone);
        assert_eq!(code, 70);
        assert!(errors.contains("holds no vbmeta header"), "{errors}");
        assert_eq!(fs::read(&a).unwrap(), other, "nothing written");
    }

    #[test]
    fn without_a_command_it_tells_how_it_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let phone = Phone { by_name: dir.path(), suffix: "_a", locked: false };
        for args in [&[][..], &["--force"], &["disable-dm-verity"]] {
            assert_eq!(ran(args, &phone), (64, String::new(), USAGE.into()), "{args:?}");
        }
    }

    // -- what the pages and the installer use -------------------------------

    #[test]
    fn changing_a_flag_backs_the_image_up_first_and_writes_only_that_flag() {
        let dir = tempfile::tempdir().unwrap();
        let original = vbmeta(0x80);
        let (a, b) = (dir.path().join("vbmeta_a"), dir.path().join("vbmeta_b"));
        fs::write(&a, &original).unwrap();
        fs::write(&b, &original).unwrap();
        let state = read_state(dir.path(), "_a", true).unwrap();
        assert!(!state.disabled(Check::Verity) && !state.disabled(Check::Verification));

        let (after, backup) = change_flags_at(dir.path(), dir.path(), &state, true, state.flags | Check::Verity.off()).unwrap();
        assert_eq!(fs::read(&backup).unwrap(), original, "the image as it was, before anything was written");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0x81), "only the verity flag");
        assert_eq!(fs::read(&b).unwrap(), original, "and the other slot untouched");
        assert!(after.disabled(Check::Verity));
        assert!(!after.disabled(Check::Verification), "the other check is left as it was");

        // Back again, from a fresh reading of what is there now.
        let now = read_state(dir.path(), "_a", true).unwrap();
        assert_eq!(now, after);
        let (back, _) = change_flags_at(dir.path(), dir.path(), &now, true, now.flags & !Check::Verity.off()).unwrap();
        assert_eq!(fs::read(&a).unwrap(), original);
        assert_eq!(back.flags, state.flags);
    }

    #[test]
    fn a_state_that_no_longer_describes_the_partition_is_refused() {
        // What stops a page left open from overwriting an image a zip wrote
        // in the meantime: the same flags are not enough, the bytes must be
        // the ones the page was shown.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("vbmeta_a");
        fs::write(&a, vbmeta(0)).unwrap();
        let state = read_state(dir.path(), "_a", true).unwrap();
        let mut newer = vbmeta(0);
        newer[HEADER_LEN] ^= 1;
        fs::write(&a, &newer).unwrap();
        let refused = change_flags_at(dir.path(), dir.path(), &state, true, state.flags | 3);
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(fs::read(&a).unwrap(), newer, "nothing written");
    }

    #[test]
    fn a_locked_bootloader_changes_nothing_through_the_pages_either() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("vbmeta_a");
        fs::write(&a, vbmeta(0)).unwrap();
        let state = read_state(dir.path(), "_a", false).unwrap();
        assert!(!state.unlocked);
        let refused = change_flags_at(dir.path(), dir.path(), &state, false, 3);
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0), "nothing written");
    }

    #[test]
    fn a_backup_that_cannot_be_written_stops_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("vbmeta_a");
        fs::write(&a, vbmeta(0)).unwrap();
        let state = read_state(dir.path(), "_a", true).unwrap();
        let absent = dir.path().join("no such folder");
        assert!(change_flags_at(dir.path(), &absent, &state, true, state.flags | 3).is_err());
        assert_eq!(fs::read(&a).unwrap(), vbmeta(0), "nothing written without a backup");
    }

    #[test]
    fn a_slot_suffix_that_is_not_one_names_no_partition() {
        // The suffix goes into a path, so only a slot's own name may reach it.
        // The empty suffix is the phone with one slot, which is allowed and
        // simply finds nothing in this folder.
        let dir = tempfile::tempdir().unwrap();
        for suffix in ["../../vbmeta", "_c", "a", "_A"] {
            let refused = read_state(dir.path(), suffix, true);
            assert!(refused.is_err(), "{suffix:?} is not a slot suffix, and reached a path anyway");
        }
        assert!(read_state(dir.path(), "", true).is_err(), "an empty folder holds no vbmeta image");
    }

    #[test]
    fn what_holds_no_vbmeta_header_is_not_read_as_a_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("vbmeta_a"), b"not a vbmeta image at all").unwrap();
        assert!(read_state(dir.path(), "_a", true).is_err());
    }

    #[test]
    fn an_oversized_backup_is_refused_before_writing_any_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vbmeta_a");
        let mut original = vbmeta(0);
        original.resize((1 << 20) + 1, 0);
        fs::write(&path, &original).unwrap();
        assert!(read_state(dir.path(), "_a", true).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn a_footer_cannot_point_at_an_image_that_overlaps_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boot_a");
        let mut original = vbmeta(0);
        original.resize(8192, 0);
        let footer = &mut original[8192 - FOOTER_LEN..];
        footer[..4].copy_from_slice(FOOTER_MAGIC);
        footer[20..28].copy_from_slice(&0u64.to_be_bytes());
        footer[28..36].copy_from_slice(&8192u64.to_be_bytes());
        fs::write(&path, &original).unwrap();
        assert!(read_state(dir.path(), "_a", true).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}
