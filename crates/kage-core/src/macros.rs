//! Shared macro for enums whose values are stable lowercase names in the API,
//! the configuration files and the store.

/// A name that is not a value of the enum it was parsed as.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} {value:?}")]
pub struct UnknownVariant {
    /// The enum's type name.
    pub kind: &'static str,
    /// The rejected text.
    pub value: String,
}

/// Declares a fieldless enum with a stable name per variant, and derives
/// `as_str`, `ALL`, `Display`, `FromStr` and serde from that one table, so
/// the four can never disagree.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            ::serde::Serialize, ::serde::Deserialize,
        )]
        $vis enum $name {
            $( $(#[$vmeta])* #[serde(rename = $text)] $variant ),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// The stable name used in the API, configuration and store.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text ),+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::macros::UnknownVariant;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .iter()
                    .copied()
                    .find(|value| value.as_str() == text)
                    .ok_or_else(|| $crate::macros::UnknownVariant {
                        kind: stringify!($name),
                        value: text.to_owned(),
                    })
            }
        }
    };
}

pub(crate) use string_enum;

#[cfg(test)]
pub(crate) mod test_support {
    use std::fmt::{Debug, Display};
    use std::str::FromStr;

    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// Asserts that `as_str`, `Display`, `FromStr` and serde agree for every
    /// value of a `string_enum!` type, and that the names are the expected ones.
    pub(crate) fn assert_string_enum<T>(all: &[T], as_str: fn(T) -> &'static str, expected: &[&str])
    where
        T: Copy + Debug + PartialEq + Display + FromStr + Serialize + DeserializeOwned,
        <T as FromStr>::Err: Debug,
    {
        let names: Vec<&str> = all.iter().map(|value| as_str(*value)).collect();
        assert_eq!(names, expected);
        for value in all {
            let name = as_str(*value);
            assert_eq!(value.to_string(), name);
            assert_eq!(name.parse::<T>().unwrap(), *value);
            assert_eq!(serde_json::to_string(value).unwrap(), format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<T>(&format!("\"{name}\"")).unwrap(),
                *value
            );
        }
        assert!("definitely-not-a-value".parse::<T>().is_err());
    }
}

#[cfg(test)]
// The macro emits `pub` items; on this private test enum they are unreachable.
#[allow(unreachable_pub)]
mod tests {
    use super::UnknownVariant;

    string_enum! {
        /// Test enum.
        enum Colour {
            /// Red.
            Red => "red",
            /// Dark blue.
            DarkBlue => "dark_blue",
        }
    }

    #[test]
    fn table_drives_every_conversion() {
        super::test_support::assert_string_enum(Colour::ALL, Colour::as_str, &["red", "dark_blue"]);
    }

    #[test]
    fn unknown_names_report_the_type() {
        let error = "green".parse::<Colour>().unwrap_err();

        assert_eq!(
            error,
            UnknownVariant {
                kind: "Colour",
                value: "green".to_owned()
            }
        );
        assert_eq!(error.to_string(), "unknown Colour \"green\"");
    }
}
