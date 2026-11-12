//! Output and lifecycle handling shared by the OTA engine and its tests.

use anyhow::{Context, Result};
use std::io::{self, ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const PROGRESS_EVERY: Duration = Duration::from_millis(100);
const LINGER: Duration = Duration::from_secs(1);

/// Observe process exit separately from pipe EOF. A descendant may retain
/// an output descriptor after the engine exits; allow its final output a
/// short drain period without waiting forever or terminating that process.
pub(crate) fn wait_engine<T>(child: &mut Child, rx: &Receiver<T>, mut receive: impl FnMut(T)) -> Result<ExitStatus> {
    let mut ended = None;
    loop {
        if ended.is_none() {
            if let Some(status) = child.try_wait().context("check whether update_engine exited")? {
                ended = Some((status, Instant::now()));
            }
        }
        match rx.recv_timeout(PROGRESS_EVERY) {
            Ok(line) => receive(line),
            Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => {
                // Closed output is not proof that the engine has finished.
                return child.wait().context("wait for update_engine");
            }
        }
        if let Some((status, at)) = ended {
            if at.elapsed() >= LINGER {
                return Ok(status);
            }
        }
    }
}

/// Read output with a cancellable poll so inherited pipe descriptors cannot
/// strand a reader thread, including a final line without a newline.
pub(crate) fn engine_lines(mut from: impl Read + AsRawFd, stop: &AtomicBool, mut each: impl FnMut(&str) -> io::Result<()>) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buf = [0; 4096];
    while !stop.load(Ordering::Relaxed) {
        let mut fd = libc::pollfd { fd: from.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let polled = unsafe { libc::poll(&mut fd, 1, 100) };
        if polled < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted { continue; }
            return Err(error);
        }
        if polled == 0 { continue; }
        let n = match from.read(&mut buf) {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 { break; }
        pending.extend_from_slice(&buf[..n]);
        let mut start = 0;
        for end in 0..pending.len() {
            if pending[end] == b'\n' {
                each(String::from_utf8_lossy(&pending[start..end]).trim_end_matches('\r'))?;
                start = end + 1;
            }
        }
        pending.drain(..start);
    }
    if !pending.is_empty() {
        each(String::from_utf8_lossy(&pending).trim_end_matches('\r'))?;
    }
    Ok(())
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use std::fs::File;
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::Stdio;
    use std::sync::{mpsc, Arc};
    use std::thread;

    fn pipe(_: libc::c_int) -> Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    #[test]
    fn retained_output_does_not_hide_engine_exit_or_kill_its_holder() {
        let (read, write) = pipe(0).unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30 & echo $!; exit 7"])
            .stdout(Stdio::from(write)).spawn().unwrap();
        let mut reader = BufReader::new(File::from(read));
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let holder: libc::pid_t = line.trim().parse().unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        let started = Instant::now();
        let status = wait_engine(&mut child, &rx, |_| {}).unwrap();
        let alive = unsafe { libc::kill(holder, 0) } == 0;
        unsafe { libc::kill(holder, libc::SIGKILL); }
        drop(tx);
        assert_eq!(status.code(), Some(7));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(alive, "waiting must not terminate a surviving snapshot helper");
    }

    #[test]
    fn closed_output_is_not_install_success_while_the_engine_runs() {
        let mut child = std::process::Command::new("/bin/sh").args(["-c", "sleep 0.2; exit 9"]).spawn().unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        assert_eq!(wait_engine(&mut child, &rx, |_| {}).unwrap().code(), Some(9));
    }

    #[test]
    fn a_reader_stops_with_an_open_pipe_and_delivers_its_partial_line() {
        let (read, write) = pipe(0).unwrap();
        let mut write = File::from(write);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            let mut lines = Vec::new();
            engine_lines(File::from(read), &stopping, |line| { lines.push(line.to_string()); Ok(()) }).unwrap();
            lines
        });
        write.write_all(b"complete\r\npartial").unwrap();
        thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        assert_eq!(thread.join().unwrap(), ["complete", "partial"]);
        drop(write);
    }
}
