//! Atom keys and their text form (SPEC §4.2): `<path>::<kind> <name>[#<ordinal>]`, segments
//! joined by `::`, `#0` omitted. `%`, `::` and `#` inside a path or name are percent-escaped,
//! so splitting on `::` and `#` is always unambiguous.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Segment {
    /// Short lowercase word from the language's `atoms.scm` (`fn`, `impl`, `class`, …).
    pub kind: String,
    /// Source name with whitespace collapsed; `<anon>` if the item has none.
    pub name: String,
    /// 0-based index among siblings with the same kind and name.
    pub ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AtomKey {
    pub path: String,
    /// Empty for the file-level atom.
    pub symbol: Vec<Segment>,
}

fn escape(s: &str) -> String {
    s.replace('%', "%25").replace("::", "%3A%3A").replace('#', "%23")
}

fn unescape(s: &str) -> String {
    s.replace("%3A", ":").replace("%23", "#").replace("%25", "%")
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.kind, escape(&self.name))?;
        if self.ordinal > 0 {
            write!(f, "#{}", self.ordinal)?;
        }
        Ok(())
    }
}

impl fmt::Display for AtomKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&escape(&self.path))?;
        self.symbol.iter().try_for_each(|s| write!(f, "::{s}"))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseKeyError(pub String);

impl fmt::Display for ParseKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid atom key: {}", self.0)
    }
}

impl std::error::Error for ParseKeyError {}

impl FromStr for Segment {
    type Err = ParseKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseKeyError(s.to_owned());
        let (kind, rest) = s.split_once(' ').ok_or_else(err)?;
        if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_lowercase()) {
            return Err(err());
        }
        let (name, ordinal) = match rest.split_once('#') {
            Some((n, o)) => match o.parse::<u32>() {
                Ok(ord) if ord > 0 && !o.starts_with('+') => (n, ord),
                _ => return Err(err()),
            },
            None => (rest, 0),
        };
        if name.is_empty() {
            return Err(err());
        }
        Ok(Segment { kind: kind.to_owned(), name: unescape(name), ordinal })
    }
}

impl FromStr for AtomKey {
    type Err = ParseKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split("::");
        let path = parts.next().filter(|p| !p.is_empty()).ok_or_else(|| ParseKeyError(s.to_owned()))?;
        Ok(AtomKey { path: unescape(path), symbol: parts.map(str::parse).collect::<Result<_, _>>()? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(kind: &str, name: &str, ordinal: u32) -> Segment {
        Segment { kind: kind.into(), name: name.into(), ordinal }
    }

    #[test]
    fn text_form_round_trips() {
        let keys = [
            AtomKey { path: "src/cart.rs".into(), symbol: vec![seg("impl", "Cart", 0), seg("fn", "price", 0)] },
            AtomKey { path: "src/cart.rs".into(), symbol: vec![] },
            AtomKey { path: "a::b/100%.rs".into(), symbol: vec![seg("impl", "Display for crate::Cart<T>", 2)] },
            AtomKey { path: "x.py".into(), symbol: vec![seg("def", "f#3x", 0), seg("def", "g#1", 4)] },
        ];
        assert_eq!(keys[0].to_string(), "src/cart.rs::impl Cart::fn price");
        assert_eq!(keys[2].to_string(), "a%3A%3Ab/100%25.rs::impl Display for crate%3A%3ACart<T>#2");
        assert_eq!(keys[3].to_string(), "x.py::def f%233x::def g%231#4");
        for k in keys {
            assert_eq!(k.to_string().parse::<AtomKey>().unwrap(), k, "{k}");
        }
        for bad in ["", "::fn x", "a.rs::fn", "a.rs::Fn x", "a.rs::fn x#0", "a.rs::fn ", "a.rs::fn x#1#2", "a.rs::fn x#y"] {
            assert!(bad.parse::<AtomKey>().is_err(), "{bad:?}");
        }
    }
}
