//! Codex's version, as `codex --version` prints it.

use std::fmt;

/// A release version `major.minor.patch`, optionally a pre-release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// Ordered after `pre_release`: a pre-release comes before its release.
    release: bool,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            release: true,
        }
    }

    /// Parses the output of `codex --version`, such as
    /// `codex-cli 0.167.1` or `codex-cli 0.169.0-alpha.9`.
    pub fn from_version_output(output: &str) -> Option<Self> {
        let mut words = output.split_whitespace();
        let _program = words.next()?;
        let version = words.next()?;
        let (numbers, pre_release) = match version.split_once('-') {
            Some((numbers, pre_release)) => (numbers, Some(pre_release)),
            None => (version, None),
        };
        let mut parts = numbers.split('.').map(str::parse::<u64>);
        let (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch)), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        Some(Self {
            major,
            minor,
            patch,
            release: pre_release.is_none(),
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.release {
            f.write_str("-pre")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Version;

    #[test]
    fn reads_a_release() {
        assert_eq!(
            Version::from_version_output("codex-cli 0.167.1\n"),
            Some(Version::new(0, 167, 1))
        );
    }

    #[test]
    fn a_pre_release_comes_before_its_release_and_after_the_one_before() {
        let alpha = Version::from_version_output("codex-cli 0.169.0-alpha.9");
        assert!(alpha < Some(Version::new(0, 169, 0)));
        assert!(alpha > Some(Version::new(0, 168, 9)));
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(Version::new(0, 100, 0) > Version::new(0, 99, 9));
    }

    #[test]
    fn rejects_output_without_a_version() {
        for output in [
            "",
            "codex-cli",
            "codex-cli latest",
            "codex-cli 1.2",
            "codex 1.2.3.4",
        ] {
            assert_eq!(Version::from_version_output(output), None, "{output:?}");
        }
    }
}
