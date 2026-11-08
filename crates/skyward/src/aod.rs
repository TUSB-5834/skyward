//! Carries out the panel's own doze that `skyward_ui::aod` decides on.
//!
//! Nothing here touches the phone until a panel first goes dark: the driver's
//! folder and the node a command set is sent through are opened then, not on
//! the way up. A recovery that reads a proc node before its first frame is a
//! recovery whose frame may never come.
//!
//! Every write here is best-effort, and one the driver refuses is not the end
//! of the panel's doze: the display driver answers EAGAIN while it is still
//! sending the frame the recovery just handed it. A set is offered again, and
//! only a panel that refuses several times in a row is put out the ordinary
//! way for the rest of the run, with why said once in the kernel log.

use skyward_config::Aod;
use skyward_ui::aod::{Doze, Driver, Feature, Plan, Shown, Want};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// How many times one command set is offered to a driver that says it is
/// busy, and how long between the offers: the driver is sending a frame, so
/// this is the length of a frame or two.
const SEND_ATTEMPTS: u32 = 6;
const SEND_RETRY: Duration = Duration::from_millis(12);

/// How many times in a row the panel may refuse a command set before the
/// recovery stops asking for the rest of the run.
const REFUSALS_BEFORE_GIVING_UP: u32 = 3;

/// The panel's doze as this run of the recovery has it.
pub struct Panel {
    doze: Doze,
    dir: PathBuf,
    node: PathBuf,
    /// What the driver said of itself, read once the first panel went dark.
    driver: Option<Option<Driver>>,
    shown: Shown,
    /// Set once the driver has refused too many times, so the recovery stops
    /// asking.
    given_up: bool,
    /// The command sets refused in a row.
    refusals: u32,
}

impl Panel {
    pub fn new(aod: &Aod) -> Self {
        let feature = |f: &skyward_config::Feature| Feature { id: f.id, value: f.value.clone() };
        let doze = Doze {
            enter: aod.enter.iter().map(feature).collect(),
            enter_cmds: aod.enter_cmds.clone(),
            leave: aod.leave.iter().map(feature).collect(),
            leave_cmds: aod.leave_cmds.clone(),
        };
        let (dir, node) = (PathBuf::from(&aod.dir), PathBuf::from(&aod.node));
        Panel { doze, dir, node, driver: None, shown: Shown::Lit, given_up: false, refusals: 0 }
    }

    /// Whether a dark panel is to show the face rather than be blacked out.
    pub fn usable(&self) -> bool {
        !self.given_up
    }

    pub fn is_dozing(&self) -> bool {
        self.shown == Shown::Dozing
    }

    /// Tell the panel what the pages want of it, and give what it is doing
    /// once told.
    pub fn want(&mut self, want: Want) -> Shown {
        if self.driver.is_none() {
            self.driver = Some(read_driver(&self.dir, &self.node));
        }
        let driver = self.driver.as_ref().and_then(Option::as_ref);
        let decision = self.doze.plan(self.shown, want, driver);
        if let Some(refusal) = decision.refusal {
            self.give_up(refusal.why());
        }
        self.shown = decision.shown;
        let (writes, cmds) = match decision.plan {
            Plan::Ordinary => return self.shown,
            Plan::Doze { writes, cmds } | Plan::Wake { writes, cmds } => (writes, cmds),
        };
        for (name, value) in writes {
            let path = self.dir.join(&name);
            if let Err(e) = fs::write(&path, &value) {
                let _ = skyward_sys::kmsg(&format!("the panel's {name} is not set to {value}: {e}"));
            }
        }
        for cmd in cmds {
            if let Err(e) = self.send(cmd) {
                // A panel that did not go into its doze is blacked out by the
                // caller; one that did not come out is lit regardless.
                if self.shown == Shown::Dozing {
                    self.shown = Shown::Blank;
                }
                // One refused set is not the end of the panel's doze: the
                // display driver answers EAGAIN while it is still sending the
                // frame the recovery just handed it, and a set sent then is
                // simply not taken. Only a panel that has refused several
                // times in a row is given up on, and then for the whole run.
                self.refusals += 1;
                if self.refusals >= REFUSALS_BEFORE_GIVING_UP {
                    self.give_up(&format!(
                        "the panel did not take command set {cmd} {REFUSALS_BEFORE_GIVING_UP} times in a row, so it is put out the ordinary way: {e}"
                    ));
                }
                break;
            }
            self.refusals = 0;
        }
        self.shown
    }

    /// Send one command set to the panel, waiting out a driver that is still
    /// busy with the frame it was just handed rather than giving up on it.
    ///
    /// The number goes with a newline after it, as a shell's `echo` writes
    /// it: the driver reads the node a line at a time, and a write that ends
    /// without one is answered EAGAIN and dropped, measured on haotian.
    fn send(&self, cmd: u32) -> std::io::Result<()> {
        let mut left = SEND_ATTEMPTS;
        loop {
            match fs::write(&self.node, format!("{cmd}\n")) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && left > 1 => {
                    left -= 1;
                    std::thread::sleep(SEND_RETRY);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn give_up(&mut self, why: &str) {
        if !self.given_up {
            let _ = skyward_sys::kmsg(why);
        }
        self.given_up = true;
        self.driver = Some(None);
    }
}

/// What the display driver says of itself: the features `disp_param` lists
/// as `name[id]: value`, and whether the node that sends a command set opens
/// for writing. A driver with neither is no driver at all.
fn read_driver(dir: &std::path::Path, node: &std::path::Path) -> Option<Driver> {
    let sending = fs::OpenOptions::new().write(true).open(node).is_ok();
    let features = fs::read_to_string(dir.join("disp_param"))
        .map(|text| {
            text.lines()
                .filter_map(|line| {
                    let (head, _) = line.split_once(':')?;
                    let (name, id) = head.trim().strip_suffix(']')?.split_once('[')?;
                    Some((id.to_string(), name.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    (sending || !features.is_empty()).then_some(Driver { features, sending })
}
