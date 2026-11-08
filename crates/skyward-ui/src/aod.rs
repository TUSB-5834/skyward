//! The panel's own doze: a little kept on the glass with the backlight out,
//! as the stock system keeps its always-on display.
//!
//! On Xiaomi's phones the panel is put into a doze by sending it command sets
//! of its own through Xiaomi's `mi_disp` driver, which lives inside
//! `msm_drm` — the same module that drives the pages. The driver lists the
//! features it offers, each under a number, in `disp_param` beside the node
//! that sends a command set. What glows is decided by the frame, not by the
//! light: an OLED lights exactly the subpixels the buffer lights, so the face
//! drawn here is all that is on, and everything else must be black.
//!
//! The whole of the decision — what to send, and whether to fall back to the
//! recovery's own blanking — is [`Doze::plan`], a function with no state and
//! no paths in it, so that a computer can test every way it can go. The code
//! that carries it out is in the recovery, and every write there is
//! best-effort: a phone whose driver does not answer is put out the way the
//! recovery has always put it out.

use crate::canvas::Canvas;
use crate::color::Rgba;
use crate::font::Fonts;

/// The black the rest of the face is: on an OLED, the pixels that are not
/// lit, which is all but a few hundred of them.
const BLACK: Rgba = Rgba::rgb(0, 0, 0);
/// The clock's ink. White keeps the face readable at low brightness.
const INK: Rgba = Rgba::rgb(255, 255, 255);
/// The mark's ink, dimmer, so that it is not the brightest thing left on the
/// glass through the night.
const MARK: Rgba = Rgba::rgb(96, 96, 96);

/// The clock's size in dp, and how far down the screen its middle sits as a
/// share of the screen's height.
const CLOCK_DP: f32 = 64.0;
const CLOCK_AT: f32 = 0.22;
/// The mark under the clock: how far below its middle it sits, how wide it
/// is, and how thick, all in dp.
const MARK_DROP_DP: f32 = 56.0;
const MARK_WIDE_DP: f32 = 72.0;
const MARK_THICK_DP: f32 = 2.0;

/// The line of readings under the mark: the battery, the slot the recovery
/// runs from, and whether the data partition is open. Its size in dp, and how
/// far below the clock's middle it sits.
const FACTS_DP: f32 = 15.0;
const FACTS_DROP_DP: f32 = 84.0;
/// The date above the clock, its size in dp and how far above the clock's
/// middle its baseline sits.
const DATE_DP: f32 = 17.0;
const DATE_RISE_DP: f32 = 78.0;

/// What the face says beside the time: the readings a person glancing at the
/// phone wants, and nothing that has to be read closely.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The date, as the phone's locale writes it, such as `2026年11月7日`.
    pub date: String,
    /// The battery's level, as a share of a full charge, and whether it is
    /// charging.
    pub battery: Option<(u8, bool)>,
    /// The slot the recovery runs from, as its letter: `A` or `B`.
    pub slot: Option<String>,
    /// Whether the data partition is open, as the decrypt page names it.
    pub data: String,
}

impl Facts {
    /// The readings as one line, in the order they are drawn: the battery,
    /// the slot, and the data partition. A reading the phone does not give is
    /// left out, and a face with nothing to say draws no line at all.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if let Some((level, charging)) = self.battery {
            parts.push(if charging { format!("{level}% +") } else { format!("{level}%") });
        }
        if let Some(slot) = &self.slot {
            parts.push(slot.clone());
        }
        if !self.data.is_empty() {
            parts.push(self.data.clone());
        }
        parts.join("   ")
    }
}


/// One of the display driver's features and the value it is to be given, as
/// the phone's configuration names it: the number `disp_param` prints in the
/// brackets after the feature's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feature {
    /// The number in the brackets, read as the configuration writes it.
    pub id: u32,
    /// What is written to the node the driver names that feature by.
    pub value: String,
}

/// The panel's own doze as the phone's configuration names it: the feature
/// values written and the command sets sent, on the way in and on the way
/// out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doze {
    /// The values written as the panel goes into its doze.
    pub enter: Vec<Feature>,
    /// The command sets sent as it goes in, by the number the driver's own
    /// listing gives each.
    pub enter_cmds: Vec<u32>,
    /// The values written as it comes back out.
    pub leave: Vec<Feature>,
    /// The command sets sent as it comes back out.
    pub leave_cmds: Vec<u32>,
}

/// What the pages want of the panel just now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// The pages are lit and the panel is to show them.
    Lit,
    /// The pages have gone dark: nothing on the glass needs reading.
    Dark,
}

/// What the panel is doing, as the recovery last left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    /// Showing the pages.
    Lit,
    /// Put out the way the recovery has always put it out: the frame blacked
    /// and the backlight at 0.
    Blank,
    /// In the panel's own doze, with the face on the glass.
    Dozing,
}

/// What the display driver answered about itself when it was asked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Driver {
    /// The features `disp_param` lists, each with the number the driver
    /// prints in the brackets after its name, as it prints it, and the name
    /// it gives it.
    pub features: Vec<(String, String)>,
    /// Whether the node a command set's number is sent through could be
    /// opened for writing.
    pub sending: bool,
}

impl Driver {
    /// The name the driver gives feature `id`, where it lists one under it.
    ///
    /// The brackets are read the way the driver prints them, which is
    /// hexadecimal on the phones this is known for; a build that prints them
    /// as decimal numbers is read as well, rather than quietly skipped.
    pub fn name(&self, id: u32) -> Option<&str> {
        self.features.iter().find(|(n, _)| reads_as(n, id)).map(|(_, name)| name.as_str())
    }

    /// What is written for each of `features`, in order: the node the driver
    /// names the feature by, and the value. A feature the driver does not
    /// list is left out, and so is one whose name could not be a file of its
    /// own — neither is written blind.
    fn writes(&self, features: &[Feature]) -> Vec<(String, String)> {
        features
            .iter()
            .filter_map(|f| {
                let name = self.name(f.id)?;
                let usable = !name.is_empty() && !name.contains('/') && !name.contains(char::is_whitespace);
                usable.then(|| (name.to_string(), f.value.clone()))
            })
            .collect()
    }
}

/// Whether the number a driver printed in the brackets is `id`, read either
/// way a driver of this generation has printed them.
fn reads_as(printed: &str, id: u32) -> bool {
    let printed = printed.trim();
    u32::from_str_radix(printed, 16).ok() == Some(id) || printed.parse::<u32>().ok() == Some(id)
}

/// What is to be done to the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// The recovery's own way, unchanged: the pages shown, or the frame
    /// blacked out with the backlight at 0.
    Ordinary,
    /// Into the panel's own doze: these values written first, then these
    /// command sets sent.
    Doze { writes: Vec<(String, String)>, cmds: Vec<u32> },
    /// Out of the doze and back to the pages.
    Wake { writes: Vec<(String, String)>, cmds: Vec<u32> },
}

/// Why the panel is left the recovery's own way where its doze was wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The configuration names no command set to put the panel into its doze
    /// with, so there would be nothing to send.
    NothingToSend,
    /// The display driver was not reached: the phone has none, or its folder
    /// is not where the recovery can open it.
    NoDriver,
    /// The driver offers no node that sends a command set to the panel.
    NotSending,
}

impl Refusal {
    /// The words that go to the kernel log, once, where a dark panel is put
    /// out the ordinary way instead of dozing.
    pub fn why(self) -> &'static str {
        match self {
            Refusal::NothingToSend => {
                "the configuration names no command set that puts the panel into its own doze, so the panel is put out the ordinary way"
            }
            Refusal::NoDriver => "the display driver says nothing of itself, so the panel is put out the ordinary way",
            Refusal::NotSending => "the display driver sends no command set to the panel, so it is put out the ordinary way",
        }
    }
}

/// What the panel is to be told, and what it is doing once it has been.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub plan: Plan,
    /// What the panel is doing once the plan is carried out. A step that the
    /// panel refuses leaves the recovery to set this itself.
    pub shown: Shown,
    /// Why the panel's own doze is not used, where it was wanted and not
    /// given.
    pub refusal: Option<Refusal>,
}

impl Doze {
    /// What the panel is to be told, given what it is doing now, what the
    /// pages want of it, and what the display driver answered.
    ///
    /// This is the whole of the decision, with nothing of the phone in it, so
    /// that a computer can test every way it goes: the caller only carries it
    /// out. Where the panel's doze cannot be had, the answer is the recovery
    /// as it was before any of this — the ordinary blanking — and the reason
    /// for a log line.
    pub fn plan(&self, shown: Shown, want: Want, driver: Option<&Driver>) -> Decision {
        match (shown, want) {
            // The pages came back: out of the doze, and the page over it.
            (Shown::Dozing, Want::Lit) => self.leave(driver),
            // The pages went dark: into the doze, where it can be had.
            (Shown::Lit, Want::Dark) => self.enter(driver),
            // Already as it should be, and nothing to tell the panel.
            (Shown::Lit, Want::Lit) | (Shown::Blank, Want::Dark) | (Shown::Dozing, Want::Dark) => {
                Decision { plan: Plan::Ordinary, shown, refusal: None }
            }
            (Shown::Blank, Want::Lit) => Decision { plan: Plan::Ordinary, shown: Shown::Lit, refusal: None },
        }
    }

    /// Into the panel's own doze, where there is a command set to send and the
    /// driver takes one. Anything short of that is the ordinary blanking.
    fn enter(&self, driver: Option<&Driver>) -> Decision {
        let ordinary = |refusal| Decision { plan: Plan::Ordinary, shown: Shown::Blank, refusal: Some(refusal) };
        let Some(driver) = driver else { return ordinary(Refusal::NoDriver) };
        if !driver.sending {
            return ordinary(Refusal::NotSending);
        }
        // A doze with nothing sent is no doze at all: the panel would stay
        // black with the backlight at 0, which is the ordinary blanking with
        // more ways to fail.
        if self.enter_cmds.is_empty() {
            return ordinary(Refusal::NothingToSend);
        }
        let plan = Plan::Doze { writes: driver.writes(&self.enter), cmds: self.enter_cmds.clone() };
        Decision { plan, shown: Shown::Dozing, refusal: None }
    }

    /// Out of the panel's own doze and back to the pages. A panel that cannot
    /// be brought out is given up on for the rest of the run rather than sent
    /// another command set every frame.
    fn leave(&self, driver: Option<&Driver>) -> Decision {
        let ordinary = |refusal| Decision { plan: Plan::Ordinary, shown: Shown::Lit, refusal: Some(refusal) };
        let Some(driver) = driver else { return ordinary(Refusal::NoDriver) };
        if !driver.sending {
            return ordinary(Refusal::NotSending);
        }
        if self.leave_cmds.is_empty() {
            return ordinary(Refusal::NothingToSend);
        }
        let plan = Plan::Wake { writes: driver.writes(&self.leave), cmds: self.leave_cmds.clone() };
        Decision { plan, shown: Shown::Lit, refusal: None }
    }
}

/// Draw the face the panel keeps on the glass while it dozes: the date, the
/// time, the readings a glance wants, and a small mark under them, on black.
///
/// The frame is what glows, so everything but what is drawn here is black: a
/// page's worth of light would burn the panel and drain the battery with the
/// backlight never coming on. The face stands a little to one side or the
/// other of the middle, a different place each minute, so that the same
/// subpixels are not lit for hours on end.
pub fn face(out: &mut Canvas, f: &Fonts, clock: &str, facts: &Facts, px_per_dp: f32) {
    out.clear(BLACK);
    let (w, h) = (out.w as f32, out.h as f32);
    let (cx, cy) = (w / 2.0 + drift(clock, px_per_dp), h * CLOCK_AT);
    if !facts.date.is_empty() {
        out.text_center(&f.body, &facts.date, cx, cy - DATE_RISE_DP * px_per_dp, DATE_DP * px_per_dp, MARK);
    }
    out.text_center(&f.display, clock, cx, cy, CLOCK_DP * px_per_dp, INK);
    let (wide, thick) = (MARK_WIDE_DP * px_per_dp, (MARK_THICK_DP * px_per_dp).max(1.0));
    out.fill_rect(cx - wide / 2.0, cy + MARK_DROP_DP * px_per_dp, wide, thick, MARK);
    let line = facts.line();
    if !line.is_empty() {
        out.text_center(&f.body, &line, cx, cy + FACTS_DROP_DP * px_per_dp, FACTS_DP * px_per_dp, MARK);
    }
}

/// How far, in pixels, the face stands from the middle of the screen for the
/// minute it shows: five places, a step apart, so that no subpixel of it is
/// lit for more than a few minutes together.
fn drift(clock: &str, px_per_dp: f32) -> f32 {
    const STEPS: [f32; 5] = [-16.0, -8.0, 0.0, 8.0, 16.0];
    let minute = clock.rsplit(':').next().and_then(|m| m.trim().parse::<usize>().ok());
    minute.map_or(0.0, |m| STEPS[m % STEPS.len()] * px_per_dp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver as `haotian`'s says itself to be: the features it lists, and
    /// a node that takes a command set.
    fn haotian() -> Driver {
        let features = [
            ("00", "dimming"),
            ("01", "hbm"),
            ("03", "doze_brightness"),
            ("06", "flat_mode"),
            ("36", "apl_dimming"),
        ];
        Driver { features: features.iter().map(|(n, name)| (n.to_string(), name.to_string())).collect(), sending: true }
    }

    /// The doze `haotian`'s configuration names: the low-brightness doze in,
    /// and the switch out of the low-power mode.
    fn doze() -> Doze {
        Doze { enter: Vec::new(), enter_cmds: vec![45], leave: Vec::new(), leave_cmds: vec![18] }
    }

    #[test]
    fn a_dark_panel_goes_into_its_own_doze_when_the_phone_names_one() {
        let d = doze().plan(Shown::Lit, Want::Dark, Some(&haotian()));
        assert_eq!(d, Decision { plan: Plan::Doze { writes: vec![], cmds: vec![45] }, shown: Shown::Dozing, refusal: None });
        // And coming back out is the command set the way out names.
        let d = doze().plan(Shown::Dozing, Want::Lit, Some(&haotian()));
        assert_eq!(d, Decision { plan: Plan::Wake { writes: vec![], cmds: vec![18] }, shown: Shown::Lit, refusal: None });
    }

    #[test]
    fn a_panel_with_no_driver_of_its_own_is_put_out_the_ordinary_way() {
        // The folder is not there, or nothing could be opened: the panel goes
        // out as it always did, and the reason is one line of the log.
        let d = doze().plan(Shown::Lit, Want::Dark, None);
        assert_eq!(d.plan, Plan::Ordinary, "no command set is sent to nothing");
        assert_eq!(d.shown, Shown::Blank, "the frame is blacked out and the backlight put to 0");
        assert_eq!(d.refusal, Some(Refusal::NoDriver));
        assert!(!Refusal::NoDriver.why().is_empty());
    }

    #[test]
    fn a_driver_with_no_command_set_node_is_put_out_the_ordinary_way() {
        let mut driver = haotian();
        driver.sending = false;
        let d = doze().plan(Shown::Lit, Want::Dark, Some(&driver));
        assert_eq!((d.plan, d.shown, d.refusal), (Plan::Ordinary, Shown::Blank, Some(Refusal::NotSending)));
    }

    #[test]
    fn a_configuration_naming_no_command_set_sends_nothing_and_falls_back() {
        // The nodes are all there and the driver answers, but there is no
        // set to send: a doze that sends nothing would leave the panel black
        // with the backlight at 0, which is the blanking with more ways to
        // fail.
        let empty = Doze { leave_cmds: vec![18], ..Doze::default() };
        let d = empty.plan(Shown::Lit, Want::Dark, Some(&haotian()));
        assert_eq!((d.plan, d.shown, d.refusal), (Plan::Ordinary, Shown::Blank, Some(Refusal::NothingToSend)));
        let empty = Doze { enter_cmds: vec![45], ..Doze::default() };
        let d = empty.plan(Shown::Dozing, Want::Lit, Some(&haotian()));
        assert_eq!((d.plan, d.shown, d.refusal), (Plan::Ordinary, Shown::Lit, Some(Refusal::NothingToSend)));
    }

    #[test]
    fn a_panel_already_as_the_pages_want_is_told_nothing() {
        // Nothing changes on a page left lit, and nothing changes on a panel
        // already dark: no write is made on every frame.
        for (shown, want) in [(Shown::Lit, Want::Lit), (Shown::Blank, Want::Dark), (Shown::Dozing, Want::Dark)] {
            let d = doze().plan(shown, want, Some(&haotian()));
            assert_eq!(d, Decision { plan: Plan::Ordinary, shown, refusal: None }, "{shown:?} {want:?}");
        }
        // A blanked panel lit again is only the page over it.
        let d = doze().plan(Shown::Blank, Want::Lit, Some(&haotian()));
        assert_eq!(d, Decision { plan: Plan::Ordinary, shown: Shown::Lit, refusal: None });
    }

    #[test]
    fn a_feature_is_written_only_under_the_name_the_driver_gives_it() {
        let doze = Doze {
            enter: vec![Feature { id: 3, value: "1".into() }, Feature { id: 41, value: "1".into() }],
            enter_cmds: vec![45],
            ..Doze::default()
        };
        let d = doze.plan(Shown::Lit, Want::Dark, Some(&haotian()));
        let Plan::Doze { writes, cmds } = d.plan else { panic!("the doze is wanted") };
        assert_eq!(cmds, vec![45]);
        assert_eq!(writes, vec![("doze_brightness".to_string(), "1".to_string())], "the feature the driver does not list is left alone");
    }

    #[test]
    fn the_numbers_the_driver_prints_are_read_as_it_prints_them() {
        let driver = haotian();
        // `disp_param` prints `doze_brightness[03]` and `apl_dimming[36]`.
        assert_eq!(driver.name(3), Some("doze_brightness"));
        assert_eq!(driver.name(0x36), Some("apl_dimming"), "the brackets are hexadecimal, as the driver prints them");
        assert_eq!(driver.name(41), None, "a feature this driver does not offer");
        // A build that prints decimal numbers is read too, rather than
        // silently skipped: the same node answers either way.
        let decimal = Driver { features: vec![("36".into(), "apl_dimming".into())], sending: true };
        assert_eq!(decimal.name(36), Some("apl_dimming"), "read as decimal where it is not a hexadecimal number of the driver's");
    }

    #[test]
    fn a_feature_named_as_no_file_of_its_own_is_not_written() {
        let driver = Driver { features: vec![("03".into(), "doze/brightness".into())], sending: true };
        let doze = Doze { enter: vec![Feature { id: 3, value: "1".into() }], enter_cmds: vec![45], ..Doze::default() };
        let Plan::Doze { writes, .. } = doze.plan(Shown::Lit, Want::Dark, Some(&driver)).plan else { panic!("the doze is wanted") };
        assert!(writes.is_empty(), "a name that is no file of the driver's folder is not written blind");
    }

    /// A panel the size of `haotian`'s, at its density.
    fn screen() -> (Canvas, Fonts) {
        (Canvas::filled(1440, 3200, Rgba::rgb(9, 9, 9)), crate::font::bundled())
    }

    /// The readings a face on `haotian` is given: a date, a battery at 48%
    /// that is charging, slot A, and the data partition still closed.
    fn facts() -> Facts {
        Facts { date: "2026年11月7日".into(), battery: Some((48, true)), slot: Some("A".into()), data: "未解密".into() }
    }

    #[test]
    fn the_face_lights_a_clock_and_its_readings_on_black_and_nothing_else() {
        let (mut out, f) = screen();
        face(&mut out, &f, "03:27", &facts(), 3.75);
        let lit = out.data.as_chunks::<4>().0.iter().filter(|p| p[0] > 0).count();
        assert!(lit > 0, "the clock is on the glass");
        // A page's worth of light would burn the panel: a few thousand
        // pixels out of four and a half million is all the face lights.
        assert!(lit < 90_000, "{lit} pixels lit of {}", out.data.len() / 4);
        // Everything that is not the face is black; its edges are
        // anti-aliased, so a little more than its ink is not.
        let black = out.data.as_chunks::<4>().0.iter().filter(|p| p[..3] == [0, 0, 0]).count();
        assert!(black > out.data.len() / 4 * 97 / 100, "{} of {} pixels are black", black, out.data.len() / 4);
    }

    #[test]
    fn the_face_shows_the_readings_the_phone_gives_and_leaves_out_the_rest() {
        // A whole line: the battery, its charging, the slot and the data
        // partition, each where a glance finds it.
        assert_eq!(facts().line(), "48% +   A   未解密");
        assert_eq!(Facts { battery: Some((48, false)), ..facts() }.line(), "48%   A   未解密");
        // A phone that gives none of them draws no line at all, and the face
        // is still the clock over its mark.
        let bare = Facts::default();
        assert_eq!(bare.line(), "");
        let (mut out, f) = screen();
        face(&mut out, &f, "03:27", &bare, 3.75);
        assert!(out.data.as_chunks::<4>().0.iter().any(|p| p[0] > 0), "the clock is still on the glass");
    }

    #[test]
    fn the_face_lights_no_channel_the_glass_would_mangle() {
        // Every lit pixel is neutral grey or white, so the face does not
        // depend on a panel's treatment of individual colour channels.
        let (mut out, f) = screen();
        face(&mut out, &f, "03:27", &facts(), 3.75);
        let wrong: Vec<[u8; 4]> = out.data.as_chunks::<4>().0.iter().copied().filter(|p| p[0] != p[2]).collect();
        assert!(wrong.is_empty(), "{} pixels depend on a channel the glass mangles: {:?}", wrong.len(), wrong.first());
    }

    #[test]
    fn the_face_moves_with_the_minute_so_no_pixel_is_lit_for_long() {
        let (mut one, f) = screen();
        let (mut two, f2) = screen();
        face(&mut one, &f, "03:20", &facts(), 3.75);
        face(&mut two, &f2, "03:21", &facts(), 3.75);
        assert_ne!(one.data, two.data, "the next minute's face stands somewhere else");
        // And it stays on the glass, whatever minute it is.
        for minute in 0..60 {
            let mut out = one.clone();
            face(&mut out, &f, &format!("03:{minute:02}"), &facts(), 3.75);
            assert!(!out.data.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]), "minute {minute} is on the glass");
            assert_eq!(out.w, 1440, "the face does not resize the frame");
        }
        // The odd clock a phone whose RTC never set gives, without a minute,
        // stands in the middle rather than panicking.
        let mut out = one.clone();
        face(&mut out, &f, "", &facts(), 3.75);
        assert_eq!(drift("", 3.75), 3.75 * 0.0);
    }
}
