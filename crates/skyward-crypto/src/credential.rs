//! Credential refusals kept distinct from Binder and storage faults.
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Incorrect,
    Retry { seconds: Option<u64>, incorrect: bool },
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incorrect => f.write_str("incorrect credential"),
            Self::Retry { seconds: Some(seconds), .. } => write!(f, "credential retries delayed by {seconds} seconds"),
            Self::Retry { seconds: None, .. } => f.write_str("credential retries delayed"),
        }
    }
}

impl std::error::Error for Rejection {}

pub fn rejection(error: &anyhow::Error) -> Option<Rejection> {
    error.downcast_ref::<Rejection>().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refusals_survive_context_and_untyped_errors_are_not_password_failures() {
        let error = anyhow::Error::new(Rejection::Incorrect).context("read the weaver").context("open user files");
        assert_eq!(rejection(&error), Some(Rejection::Incorrect));
        assert_eq!(rejection(&anyhow::anyhow!("the secure element does not recognise that credential")), None);
        assert_eq!(rejection(&anyhow::anyhow!("Binder transaction failed")), None);
    }
}
