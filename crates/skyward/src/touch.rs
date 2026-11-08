//! Native Xiaomi double-tap control. This is the common-data ABI used by
//! dada's driver, not the older integer-array Xiaomi touch ioctl.

use anyhow::{ensure, Context, Result};
use skyward_config::XiaomiDoubleTap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

#[repr(C)]
struct CommonData {
    touch_id: i8,
    cmd: u8,
    mode: u16,
    data_len: u16,
    data: [i32; 256],
}

const _: () = assert!(std::mem::size_of::<CommonData>() == 1032);
const COMMON: u32 = (3 << 30) | (1032 << 16) | ((b'T' as u32) << 8);
const SELECT: u32 = ((b'T' as u32) << 8) | 3;
const DOUBLE_TAP: u16 = 14;

pub struct Gesture {
    file: File,
    touch_id: i8,
    original: bool,
    applied: bool,
}

impl Gesture {
    pub fn open(cfg: &XiaomiDoubleTap) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(&cfg.path)
            .with_context(|| format!("open native double tap at {}", cfg.path))?;
        if unsafe { libc::ioctl(file.as_raw_fd(), SELECT as _, cfg.touch_id as libc::c_ulong) } < 0 {
            return Err(io::Error::last_os_error()).context("select Xiaomi touch controller");
        }
        let mut gesture = Self { file, touch_id: cfg.touch_id, original: false, applied: false };
        let value = gesture.command(1, DOUBLE_TAP, 0)?;
        ensure!(matches!(value, 0 | 1), "native double tap returned an invalid mode value: {value}");
        // Non-UI mode suppresses double tap. Do not overwrite another touch
        // client's mode in an environment that did not start in normal UI.
        ensure!(gesture.command(1, 17, 0)? == 0, "native double tap is blocked by non-UI touch mode");
        gesture.original = value != 0;
        gesture.applied = gesture.original;
        Ok(gesture)
    }

    fn command(&self, cmd: u8, mode: u16, value: i32) -> Result<i32> {
        let mut data = CommonData { touch_id: self.touch_id, cmd, mode, data_len: 1, data: [0; 256] };
        data.data[0] = value;
        if unsafe { libc::ioctl(self.file.as_raw_fd(), COMMON as _, &mut data) } < 0 {
            return Err(io::Error::last_os_error()).context("Xiaomi common-data touch ioctl");
        }
        Ok(data.data[0])
    }

    pub fn set(&mut self, enabled: bool) -> Result<()> {
        if enabled != self.applied {
            self.command(0, DOUBLE_TAP, i32::from(enabled))?;
            self.applied = enabled;
            ensure!(self.command(1, DOUBLE_TAP, 0)? == i32::from(enabled), "native double tap did not accept its new mode");
            eprintln!("native double tap enabled={enabled}");
        }
        Ok(())
    }
}

impl Drop for Gesture {
    fn drop(&mut self) {
        if self.applied != self.original {
            if let Err(error) = self.command(0, DOUBLE_TAP, i32::from(self.original)) {
                eprintln!("restore native double tap: {error:#}");
            }
        }
    }
}
