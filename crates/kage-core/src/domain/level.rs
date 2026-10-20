//! The camouflage level of a request (spec §25, ADR 0012, decision D50).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::Render;

/// How much of a person a request asks Kage to be, chosen by the caller on
/// every request (spec §25) and floored by the site's policy (`min_level`).
/// Written as the integer 0 to 3; ordered, so that a floor compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// 0, plain: an HTTP client and nothing else. No persona, no browser,
    /// no behaviour; a client that says who it is.
    Plain,
    /// 1, http-emulated: a persona's verified http client and its pacing.
    Emulated,
    /// 2, browser-fast: a persona's browser, a still visit: typed, no
    /// input, a short dwell; the persona's pacing, visits and nights.
    Fast,
    /// 3, human: everything of 1.0, the behaviour engine and the reader.
    Human,
}

/// A number that is no level.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a level: a level is 0, 1, 2 or 3")]
pub struct InvalidLevel(pub String);

impl Level {
    /// Every level, lowest first.
    pub const ALL: [Self; 4] = [Self::Plain, Self::Emulated, Self::Fast, Self::Human];

    /// The level as the integer a request writes.
    pub const fn number(self) -> u8 {
        match self {
            Self::Plain => 0,
            Self::Emulated => 1,
            Self::Fast => 2,
            Self::Human => 3,
        }
    }

    /// The level a request's integer names, if any.
    pub const fn from_number(number: u64) -> Option<Self> {
        match number {
            0 => Some(Self::Plain),
            1 => Some(Self::Emulated),
            2 => Some(Self::Fast),
            3 => Some(Self::Human),
            _ => None,
        }
    }

    /// Its name in the spec's table.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Emulated => "http_emulated",
            Self::Fast => "browser_fast",
            Self::Human => "human",
        }
    }

    /// The number as text: the value of the metrics' `level` label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plain => "0",
            Self::Emulated => "1",
            Self::Fast => "2",
            Self::Human => "3",
        }
    }

    /// The path the level takes: the http path at 0 and 1, a browser at 2
    /// and 3. Nothing escalates from one to the other (spec §25: the level
    /// is the request's ceiling).
    pub const fn render(self) -> Render {
        match self {
            Self::Plain | Self::Emulated => Render::Http,
            Self::Fast | Self::Human => Render::Browser,
        }
    }

    /// Whether a persona makes the request: at every level but 0.
    pub const fn has_persona(self) -> bool {
        !matches!(self, Self::Plain)
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Level {
    type Err = InvalidLevel;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.trim()
            .parse::<u64>()
            .ok()
            .and_then(Self::from_number)
            .ok_or_else(|| InvalidLevel(text.to_owned()))
    }
}

impl Serialize for Level {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.number())
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = u64::deserialize(deserializer)?;
        Self::from_number(number).ok_or_else(|| {
            serde::de::Error::custom(format!("{number} is not a level: a level is 0, 1, 2 or 3"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{InvalidLevel, Level};
    use crate::domain::Render;

    #[test]
    fn the_levels_are_the_numbers_of_the_spec_in_order() {
        assert_eq!(
            Level::ALL.map(Level::number),
            [0, 1, 2, 3],
            "the numbers a request writes"
        );
        assert_eq!(
            Level::ALL.map(Level::name),
            ["plain", "http_emulated", "browser_fast", "human"]
        );
        assert_eq!(Level::ALL.map(Level::label), ["0", "1", "2", "3"]);
        // A floor compares: lower is less of a person.
        assert!(Level::Plain < Level::Emulated);
        assert!(Level::Emulated < Level::Fast);
        assert!(Level::Fast < Level::Human);
        for level in Level::ALL {
            assert_eq!(Level::from_number(u64::from(level.number())), Some(level));
            assert_eq!(level.to_string(), level.label());
            assert_eq!(level.label().parse::<Level>(), Ok(level));
        }
    }

    #[test]
    fn a_level_is_one_of_four_integers_and_nothing_else() {
        for number in 0..4 {
            let level: Level = serde_json::from_str(&number.to_string()).unwrap();
            assert_eq!(u64::from(level.number()), number);
            assert_eq!(serde_json::to_string(&level).unwrap(), number.to_string());
        }
        for refused in ["4", "-1", "1.5", "\"2\"", "null", "true", "255"] {
            assert!(
                serde_json::from_str::<Level>(refused).is_err(),
                "{refused} was taken for a level"
            );
        }
        assert_eq!(Level::from_number(4), None);
        assert_eq!(Level::from_number(u64::MAX), None);
        assert_eq!(
            "seven".parse::<Level>(),
            Err(InvalidLevel("seven".to_owned()))
        );
        assert_eq!(" 2 ".parse::<Level>(), Ok(Level::Fast));
        assert!("".parse::<Level>().is_err());
    }

    #[test]
    fn the_level_decides_the_path_and_the_persona() {
        assert_eq!(
            Level::ALL.map(Level::render),
            [Render::Http, Render::Http, Render::Browser, Render::Browser]
        );
        assert_eq!(
            Level::ALL.map(Level::has_persona),
            [false, true, true, true]
        );
    }
}
