//! Merkle directory trees (SPEC §4.1). History-independent: equal contents give an equal root.

use serde::{Deserialize, Serialize};

use crate::{Error, Hash, Result, encode};

/// The executable bit is the only mode Kintrel (and Git) keeps, so it is part of the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Exec,
    /// The blob holds the link target.
    Symlink,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub id: Hash,
}

/// Entries sorted by name, names unique and valid. Only [`Tree::new`] builds one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Tree {
    entries: Vec<Entry>,
}

impl Tree {
    pub fn new(mut entries: Vec<Entry>) -> Result<Self> {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in &entries {
            if e.name.is_empty() || e.name == "." || e.name == ".." || e.name.contains(['/', '\0']) {
                return Err(Error::Invalid(format!("invalid file name {:?}", e.name)));
            }
        }
        if let Some(w) = entries.windows(2).find(|w| w[0].name == w[1].name) {
            return Err(Error::Invalid(format!("duplicate name {:?}", w[0].name)));
        }
        Ok(Tree { entries })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.binary_search_by(|e| e.name.as_str().cmp(name)).ok().map(|i| &self.entries[i])
    }

    pub fn id(&self) -> Hash {
        encode::hash(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, n: u8) -> Entry {
        Entry { name: name.into(), kind: Kind::File, id: Hash([n; 32]) }
    }

    #[test]
    fn order_independent_and_validated() {
        let a = Tree::new(vec![e("b", 1), e("a", 2)]).unwrap();
        let b = Tree::new(vec![e("a", 2), e("b", 1)]).unwrap();
        assert_eq!(a.id(), b.id());
        assert_eq!(a.get("b").unwrap().id, Hash([1; 32]));
        assert!(a.get("c").is_none());
        assert_ne!(a.id(), Tree::new(vec![e("a", 2), e("b", 3)]).unwrap().id());
        for bad in ["", ".", "..", "a/b"] {
            assert!(Tree::new(vec![e(bad, 0)]).is_err());
        }
        assert!(Tree::new(vec![e("x", 0), e("x", 1)]).is_err());
    }

    #[test]
    fn encoding_is_frozen() {
        // Changing this hash changes every root ever written. Bump encode::FORMAT instead.
        let t = Tree::new(vec![e("a", 1), Entry { name: "d".into(), kind: Kind::Dir, id: Hash([2; 32]) }]).unwrap();
        assert_eq!(t.id().to_string(), GOLDEN);
        assert_eq!(encode::decode::<Tree>(&encode::encode(&t)).unwrap(), t);
    }

    const GOLDEN: &str = "c2da621cab6426645f5b6d13e2320f75a885c583529f4d473ac9df576f334ca6";
}
