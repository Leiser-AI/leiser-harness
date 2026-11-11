//! Versions of agent CLIs, for the minimum version check (ADR 0002).

use std::fmt;

/// A `major.minor.patch` version, ordered by its numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `major.minor.patch`. A pre-release or build suffix after the
    /// patch number (`2.1.0-beta`, `2.1.0+abc`) is ignored.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.splitn(3, '.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let rest = parts.next()?;
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .map_or(rest, |end| &rest[..end]);
        let patch = digits.parse().ok()?;
        Some(Self::new(major, minor, patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::Version;

    #[test]
    fn parses_plain_and_suffixed_versions() {
        assert_eq!(Version::parse("2.1.321"), Some(Version::new(2, 1, 321)));
        assert_eq!(Version::parse("2.1.0-beta.1"), Some(Version::new(2, 1, 0)));
        assert_eq!(Version::parse("2.1"), None);
        assert_eq!(Version::parse("v2.1.0"), None);
    }

    #[test]
    fn orders_by_number_not_by_text() {
        assert!(Version::new(2, 1, 100) > Version::new(2, 1, 99));
        assert!(Version::new(2, 10, 0) > Version::new(2, 9, 999));
    }
}
