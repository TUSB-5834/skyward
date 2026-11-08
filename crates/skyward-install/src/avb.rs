//! The top-level vbmeta flags used by avbctl and TWRP's DisableAvb2.
//! Only the four flag bytes change; descriptors and authentication remain
//! as they were. An unlocked bootloader is required to use the result.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const VERITY_DISABLED: u32 = 1;
pub const VERIFICATION_DISABLED: u32 = 2;
const HEADER_LEN: usize = 256;
const FLAGS_AT: u64 = 120;
const FOOTER_LEN: u64 = 64;
static WRITING: Mutex<()> = Mutex::new(());

/// Checks to disable. TWRP defaults to verity alone unless its build
/// enables TW_AVB_VBMETA_FLAGS_ALL_DISABLED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checks {
    pub verity: bool,
    pub verification: bool,
}

impl Default for Checks {
    fn default() -> Self {
        Self { verity: true, verification: false }
    }
}

impl Checks {
    pub fn mask(self) -> u32 {
        (u32::from(self.verity) * VERITY_DISABLED) | (u32::from(self.verification) * VERIFICATION_DISABLED)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub partition: String,
    pub flags: u32,
}

impl State {
    pub fn verity_enabled(&self) -> bool {
        self.flags & VERITY_DISABLED == 0
    }

    pub fn verification_enabled(&self) -> bool {
        self.flags & VERIFICATION_DISABLED == 0
    }
}

fn invalid(why: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, why.into())
}

fn about(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

/// A vbmeta partition, or the vbmeta image carried by boot's AVB footer.
fn location(dir: &Path, suffix: &str) -> io::Result<(PathBuf, u64)> {
    if !["", "_a", "_b"].contains(&suffix) {
        return Err(invalid("invalid slot suffix"));
    }
    let vbmeta = dir.join(format!("vbmeta{suffix}"));
    if vbmeta.exists() {
        return Ok((vbmeta, 0));
    }
    let boot = dir.join(format!("boot{suffix}"));
    let mut file = File::open(&boot).map_err(|e| about(&boot, e))?;
    let len = file.seek(SeekFrom::End(0)).map_err(|e| about(&boot, e))?;
    let end = len.checked_sub(FOOTER_LEN).ok_or_else(|| invalid("boot is shorter than an AVB footer"))?;
    let mut footer = [0; FOOTER_LEN as usize];
    file.seek(SeekFrom::Start(end))?;
    file.read_exact(&mut footer)?;
    if !footer.starts_with(b"AVBf") {
        return Err(invalid(format!("there is no {}, and {} does not end in a vbmeta footer", vbmeta.display(), boot.display())));
    }
    let at = u64::from_be_bytes(footer[20..28].try_into().expect("eight bytes"));
    let size = u64::from_be_bytes(footer[28..36].try_into().expect("eight bytes"));
    if size < HEADER_LEN as u64 || at.checked_add(size).is_none_or(|n| n > end) {
        return Err(invalid(format!("{} has a vbmeta image outside its AVB footer bounds", boot.display())));
    }
    Ok((boot, at))
}

fn flags(file: &mut File, path: &Path, at: u64) -> io::Result<u32> {
    let mut header = [0; HEADER_LEN];
    file.seek(SeekFrom::Start(at)).and_then(|_| file.read_exact(&mut header)).map_err(|e| about(path, e))?;
    if !header.starts_with(b"AVB0") {
        return Err(invalid(format!("{} holds no vbmeta header {at} bytes in", path.display())));
    }
    Ok(u32::from_be_bytes(header[FLAGS_AT as usize..FLAGS_AT as usize + 4].try_into().expect("four bytes")))
}

fn state(path: &Path, flags: u32) -> State {
    State { partition: path.file_name().expect("a partition has a name").to_string_lossy().into_owned(), flags }
}

pub fn read(dir: &Path, suffix: &str) -> io::Result<State> {
    let (path, at) = location(dir, suffix)?;
    let mut file = File::open(&path).map_err(|e| about(&path, e))?;
    Ok(state(&path, flags(&mut file, &path, at)?))
}

struct Patch {
    file: File,
    path: PathBuf,
    at: u64,
    before: u32,
    after: u32,
}

impl Patch {
    fn open(dir: &Path, suffix: &str, mask: u32, enabled: bool) -> io::Result<Self> {
        let (path, at) = location(dir, suffix)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path).map_err(|e| about(&path, e))?;
        #[cfg(target_os = "android")]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(about(&path, io::Error::last_os_error()));
            }
        }
        let before = flags(&mut file, &path, at)?;
        let after = if enabled { before & !mask } else { before | mask };
        Ok(Self { file, path, at, before, after })
    }

    fn write(&mut self, value: u32) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(self.at + FLAGS_AT))?;
        self.file.write_all(&value.to_be_bytes()).map_err(|e| about(&self.path, e))?;
        self.file.sync_all().map_err(|e| about(&self.path, e))?;
        if flags(&mut self.file, &self.path, self.at)? != value {
            return Err(invalid(format!("{}: vbmeta flags did not read back as written", self.path.display())));
        }
        Ok(())
    }
}

/// Read and open every target before changing any of them. If one write
/// fails, try to restore every flag word touched, and report failed restores.
fn change(dir: &Path, suffixes: &[&str], mask: u32, enabled: bool) -> io::Result<Vec<State>> {
    if mask == 0 || mask & !(VERITY_DISABLED | VERIFICATION_DISABLED) != 0 || suffixes.is_empty() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "choose verity or verification and at least one slot"));
    }
    let _writing = WRITING.lock().map_err(|_| io::Error::other("vbmeta writer was interrupted"))?;
    let mut patches = Vec::new();
    for suffix in suffixes {
        if suffixes.iter().filter(|s| *s == suffix).count() != 1 {
            return Err(invalid("a slot was named more than once"));
        }
        patches.push(Patch::open(dir, suffix, mask, enabled)?);
    }
    commit(patches)
}

fn commit(mut patches: Vec<Patch>) -> io::Result<Vec<State>> {
    for i in 0..patches.len() {
        if patches[i].before == patches[i].after {
            continue;
        }
        let after = patches[i].after;
        if let Err(error) = patches[i].write(after) {
            let mut failed = Vec::new();
            for p in patches[..=i].iter_mut().rev() {
                if p.before != p.after && flags(&mut p.file, &p.path, p.at).ok() != Some(p.before) {
                    if let Err(e) = p.write(p.before) {
                        failed.push(e.to_string());
                    }
                }
            }
            let restored = if failed.is_empty() { "original flags restored".into() } else { format!("could not restore: {}", failed.join("; ")) };
            return Err(io::Error::new(error.kind(), format!("{error}; {restored}")));
        }
    }
    Ok(patches.iter().map(|p| state(&p.path, p.after)).collect())
}

pub fn set(dir: &Path, suffix: &str, mask: u32, enabled: bool) -> io::Result<State> {
    Ok(change(dir, &[suffix], mask, enabled)?.remove(0))
}

pub fn disable(dir: &Path, suffixes: &[&str], checks: Checks) -> io::Result<Vec<State>> {
    change(dir, suffixes, checks.mask(), false)
}

/// Set dm-verity on every target without changing boot verification.
pub fn set_verity(dir: &Path, suffixes: &[&str], enabled: bool) -> io::Result<Vec<State>> {
    change(dir, suffixes, VERITY_DISABLED, enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn image(flags: u32) -> Vec<u8> {
        let mut bytes = vec![0x5a; 8192];
        bytes[..4].copy_from_slice(b"AVB0");
        bytes[120..124].copy_from_slice(&flags.to_be_bytes());
        bytes
    }

    #[test]
    fn both_slots_change_only_the_requested_flag_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let original = image(0x80);
        for slot in ["_a", "_b"] {
            fs::write(dir.path().join(format!("vbmeta{slot}")), &original).unwrap();
        }
        let states = disable(dir.path(), &["_a", "_b"], Checks::default()).unwrap();
        assert!(states.iter().all(|s| s.flags == 0x81));
        let mut expected = original.clone();
        expected[120..124].copy_from_slice(&0x81u32.to_be_bytes());
        for slot in ["_a", "_b"] {
            assert_eq!(fs::read(dir.path().join(format!("vbmeta{slot}"))).unwrap(), expected);
        }
        disable(dir.path(), &["_a", "_b"], Checks { verity: true, verification: true }).unwrap();
        assert_eq!(read(dir.path(), "_a").unwrap().flags, 0x83);
        set(dir.path(), "_a", VERITY_DISABLED, true).unwrap();
        assert_eq!(read(dir.path(), "_a").unwrap().flags, 0x82);
        assert_eq!(read(dir.path(), "_b").unwrap().flags, 0x83);
    }

    #[test]
    fn the_verity_switch_enables_and_disables_both_slots_without_changing_verification() {
        let dir = tempfile::tempdir().unwrap();
        let originals = [("_a", image(0x82)), ("_b", image(0x80))];
        for (slot, bytes) in &originals {
            fs::write(dir.path().join(format!("vbmeta{slot}")), bytes).unwrap();
        }
        for enabled in [false, true, false, true] {
            set_verity(dir.path(), &["_a", "_b"], enabled).unwrap();
            for (slot, bytes) in &originals {
                let mut expected = bytes.clone();
                let original_flags = u32::from_be_bytes(bytes[120..124].try_into().unwrap());
                let flags = if enabled { original_flags } else { original_flags | VERITY_DISABLED };
                expected[120..124].copy_from_slice(&flags.to_be_bytes());
                assert_eq!(fs::read(dir.path().join(format!("vbmeta{slot}"))).unwrap(), expected);
            }
        }
    }

    #[test]
    fn a_bad_second_slot_prevents_writing_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let original = image(0);
        fs::write(dir.path().join("vbmeta_a"), &original).unwrap();
        fs::write(dir.path().join("vbmeta_b"), b"not vbmeta").unwrap();
        assert!(disable(dir.path(), &["_a", "_b"], Checks::default()).is_err());
        assert_eq!(fs::read(dir.path().join("vbmeta_a")).unwrap(), original);
    }

    #[test]
    fn a_second_write_failure_restores_the_first_slot() {
        let dir = tempfile::tempdir().unwrap();
        let original = image(0);
        for slot in ["_a", "_b"] {
            fs::write(dir.path().join(format!("vbmeta{slot}")), &original).unwrap();
        }
        let a = Patch::open(dir.path(), "_a", VERITY_DISABLED, false).unwrap();
        let mut b = Patch::open(dir.path(), "_b", VERITY_DISABLED, false).unwrap();
        // Simulate a target that became unwritable after both were checked.
        b.file = File::open(&b.path).unwrap();
        let error = commit(vec![a, b]).unwrap_err();
        assert!(error.to_string().contains("original flags restored"));
        for slot in ["_a", "_b"] {
            assert_eq!(fs::read(dir.path().join(format!("vbmeta{slot}"))).unwrap(), original);
        }
    }

    #[test]
    fn a_vbmeta_carried_by_boot_changes_only_its_flags() {
        let dir = tempfile::tempdir().unwrap();
        let mut boot = vec![0x11; 4096];
        boot.extend(image(0));
        let at = 4096u64;
        let mut footer = [0; 64];
        footer[..4].copy_from_slice(b"AVBf");
        footer[20..28].copy_from_slice(&at.to_be_bytes());
        footer[28..36].copy_from_slice(&8192u64.to_be_bytes());
        boot.extend(footer);
        let path = dir.path().join("boot");
        fs::write(&path, &boot).unwrap();
        disable(dir.path(), &[""], Checks::default()).unwrap();
        boot[4096 + 120..4096 + 124].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(fs::read(path).unwrap(), boot);
        assert_eq!(read(dir.path(), "").unwrap().partition, "boot");
    }

    #[test]
    fn an_out_of_bounds_footer_is_rejected_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mut boot = vec![0x11; 4096];
        let mut footer = [0; 64];
        footer[..4].copy_from_slice(b"AVBf");
        footer[20..28].copy_from_slice(&u64::MAX.to_be_bytes());
        footer[28..36].copy_from_slice(&256u64.to_be_bytes());
        boot.extend(footer);
        fs::write(dir.path().join("boot"), &boot).unwrap();
        assert!(disable(dir.path(), &[""], Checks::default()).is_err());
        assert_eq!(fs::read(dir.path().join("boot")).unwrap(), boot);
    }

    #[test]
    fn empty_checks_and_invalid_slots_never_write() {
        let dir = tempfile::tempdir().unwrap();
        let original = image(0);
        fs::write(dir.path().join("vbmeta"), &original).unwrap();
        assert!(disable(dir.path(), &[""], Checks { verity: false, verification: false }).is_err());
        assert!(disable(dir.path(), &["../vbmeta"], Checks::default()).is_err());
        assert!(disable(dir.path(), &["", ""], Checks::default()).is_err());
        assert_eq!(fs::read(dir.path().join("vbmeta")).unwrap(), original);
    }
}
