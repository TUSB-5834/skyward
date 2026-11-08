//! Which of the users whose own files are still closed a recovery asks
//! about.
//!
//! Android gives each user — the owner, and each further one such as the
//! phone's second space or a cloned app's — their own files, each guarded by
//! that user's own screen lock. A recovery opens the partition, then asks
//! for the credential of whoever it needs, one user at a time.
//!
//! The owner is the one a recovery is for: it is the user whose storage the
//! pages show, whose screenshots are kept, and who is asked first. A further
//! user's files are worth opening too, but nobody asked the recovery to: a
//! phone with a second space set up would otherwise hold the recovery at a
//! password prompt for a user the owner never came to open, with the owner's
//! own files already open behind it. So the owner is asked for at once, and
//! a further user only when asked for by name.
//!
//! This is decided here, in code the computer builds, so that the choice can
//! be tested without a phone.

// The choice serves the phone's recovery alone, and the host tests.
#![cfg_attr(not(any(target_os = "android", test)), allow(dead_code))]

/// The phone's owner: the first of its users, whose storage the pages show.
pub const OWNER: u32 = 0;

/// A user whose own files are still closed, as much of one as the choice
/// between them needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiting {
    pub user: u32,
    /// Whether the phone made this user's credential up, keeping it behind
    /// the owner's: such a user is never asked for one.
    pub made_up: bool,
}

/// The user to ask for a credential, if any: the owner, where the owner's
/// own files are still closed and the phone did not make their credential
/// up. A further user is not asked for here — the owner came to open the
/// owner's files, and a second space holds nobody else's password in a
/// recovery's hands.
pub fn ask(waiting: &[Waiting]) -> Option<u32> {
    waiting.iter().find(|w| w.user == OWNER && !w.made_up).map(|w| w.user)
}

/// The users a page may offer to open by name: every one whose own files
/// are still closed and whose credential the phone did not make up, the
/// owner first, then by number. These are the ones a file manager can offer
/// to unlock when a folder of theirs is opened.
pub fn askable(waiting: &[Waiting]) -> Vec<u32> {
    let mut users: Vec<u32> = waiting.iter().filter(|w| !w.made_up).map(|w| w.user).collect();
    users.sort_unstable();
    users
}

/// The number of the user a folder belongs to, where that folder is one of
/// the phone's own user areas itself: `/data/user/<n>`, where a user's own
/// files live, and `/data/media/<n>`, where a user's shared storage does.
/// `None` for a folder inside one of those, which reads through the same
/// key and is not the area itself, and for every other folder.
pub fn user_of(path: &std::path::Path) -> Option<u32> {
    let parts: Vec<_> = path.components().skip(1).collect();
    let [first, area, number] = parts.as_slice() else { return None };
    let area = area.as_os_str().to_str()?;
    if first.as_os_str() != "data" || (area != "user" && area != "media") {
        return None;
    }
    number.as_os_str().to_str()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(user: u32, made_up: bool) -> Waiting {
        Waiting { user, made_up }
    }

    #[test]
    fn the_owner_is_asked_for_first_and_alone() {
        // What a phone with a second space set up keeps closed: the owner
        // and a cloned app's user, both with a screen lock of their own.
        let both = [w(0, false), w(999, false)];
        assert_eq!(ask(&both), Some(0), "the owner only: a further user does not hold the recovery here");
        assert_eq!(ask(&[w(0, false)]), Some(0));
        assert_eq!(ask(&[w(999, false)]), None, "no owner among them, so nobody is asked at once");
        assert_eq!(ask(&[]), None, "nothing closed");
    }

    #[test]
    fn a_user_the_phone_made_a_credential_up_for_is_never_asked() {
        // The owner's own files open behind a made-up credential: nothing to
        // ask, even with the owner in the list.
        assert_eq!(ask(&[w(0, true)]), None);
        assert_eq!(ask(&[w(0, true), w(999, false)]), None, "a further user alone is not asked for");
    }

    #[test]
    fn every_user_a_page_may_open_by_name_is_offered_owner_first() {
        let both = [w(999, false), w(0, false), w(10, true)];
        assert_eq!(askable(&both), vec![0, 999], "the owner first, then by number; a made-up one never");
        assert_eq!(askable(&[]), Vec::<u32>::new());
        assert_eq!(askable(&[w(999, false)]), vec![999]);
    }

    #[test]
    fn the_owners_files_open_behind_a_further_users_lock() {
        // The case that brought this about: the owner opens, a cloned app's
        // user stays closed, and the recovery is to be usable rather than
        // held at that user's prompt.
        let after_owner = [w(999, false)];
        assert_eq!(ask(&after_owner), None, "the owner's files are open, so nothing is asked");
        assert_eq!(askable(&after_owner), vec![999], "yet the file manager may offer to open it");
    }

    #[test]
    fn a_users_folder_is_named_by_the_area_and_the_number_under_data() {
        let at = |p: &str| user_of(std::path::Path::new(p));
        assert_eq!(at("/data/user/0"), Some(0));
        assert_eq!(at("/data/user/999"), Some(999));
        assert_eq!(at("/data/media/10"), Some(10));
        assert_eq!(at("/data/user/999/DCIM"), None, "a folder inside one is not the area itself");
        assert_eq!(at("/data/media"), None, "the area is not a user's own");
        assert_eq!(at("/data/system"), None);
        assert_eq!(at("/sdcard/DCIM"), None);
        assert_eq!(at("/"), None);
        assert_eq!(at("/data/user/notanumber"), None);
    }
}
