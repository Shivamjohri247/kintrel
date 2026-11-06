//! Notes (SPEC §4.3): immutable, content-addressed edits.
//!
//! Ops are file-level: a note stores whole new file contents against the base it saw. Atoms are
//! derived: the write footprint is the set of atoms whose content differs between base and new.
//! Content blobs are referenced by id, so unchanged files and repeated contents cost nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use kintrel_lang::AtomKey;

use crate::tree::Kind;
use crate::{Hash, encode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub author: String,
    pub session: u64,
    pub rank: u64,
    pub seq: u64,
    pub base_root: Hash,
    /// Causal dependencies: the author's previous note, plus unpromoted notes in `base_root`.
    pub parents: Vec<Hash>,
    pub kind: NoteKind,
    /// Applied atomically: all or nothing.
    pub ops: Vec<Op>,
    /// Read footprint: atoms the author read since its previous note, with the hashes it saw.
    pub reads: Vec<AtomRead>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteKind {
    Code,
    /// A code note that resolves a conflict (SPEC §4.6): once it applies, the conflict is
    /// resolved. Tier 3 proposals are posted this way. (Intents and claims are not notes; see
    /// `notices.rs`. A revert is an ordinary code note.)
    Resolution { conflict: Hash },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Creates (`base: None`) or replaces a file. `kind` is never `Dir`.
    Write { path: String, base: Option<Hash>, content: Hash, kind: Kind },
    Delete { path: String, base: Hash },
}

impl Op {
    pub fn path(&self) -> &str {
        match self {
            Op::Write { path, .. } | Op::Delete { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomRead {
    pub key: AtomKey,
    pub hash: Hash,
}

impl Note {
    /// `NoteId`: the hash of the canonical encoding.
    pub fn id(&self) -> Hash {
        encode::hash(self)
    }
}

/// Every atom of a file with its identity hash. The file-level atom and container atoms skip
/// whitespace-only glue, so inserting or removing an item (with its blank lines) changes only
/// that item, not its parent.
pub fn atom_hashes(path: &str, bytes: &[u8]) -> Vec<(AtomKey, Hash)> {
    let seg = kintrel_lang::segment(path, bytes);
    seg.atoms()
        .into_iter()
        .map(|a| {
            let h = identity(&seg, &a, bytes);
            (AtomKey { path: path.to_owned(), symbol: a.symbol }, h)
        })
        .collect()
}

fn identity(seg: &kintrel_lang::Segmented, a: &kintrel_lang::AtomSpan, bytes: &[u8]) -> Hash {
    let own = a.symbol.is_empty() || is_container(seg, &a.symbol);
    let mut h = blake3::Hasher::new();
    for r in &a.ranges {
        let piece = &bytes[r.start as usize..r.end as usize];
        if !(own && piece.iter().all(u8::is_ascii_whitespace)) {
            h.update(&(piece.len() as u64).to_le_bytes());
            h.update(piece);
        }
    }
    Hash(*h.finalize().as_bytes())
}

fn is_container(seg: &kintrel_lang::Segmented, symbol: &[kintrel_lang::Segment]) -> bool {
    symbol.len() == 1
        && seg.items.iter().any(|i| matches!(i, kintrel_lang::Item::Atom(a) if a.segment == symbol[0] && a.body.is_some()))
}

/// One atom's signature and text, as notices show them (SPEC §8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomDetail {
    pub hash: Hash,
    /// Whitespace collapsed; `None` for the file-level atom.
    pub sig: Option<String>,
    /// The atom's own bytes (a container's without its members), lossily decoded.
    pub text: String,
}

/// Every atom of a file with its signature and text.
pub fn atom_details(path: &str, bytes: &[u8]) -> BTreeMap<AtomKey, AtomDetail> {
    let seg = kintrel_lang::segment(path, bytes);
    seg.atoms()
        .into_iter()
        .map(|a| {
            let hash = identity(&seg, &a, bytes);
            let slice = |r: &std::ops::Range<u32>| String::from_utf8_lossy(&bytes[r.start as usize..r.end as usize]).into_owned();
            let key = AtomKey { path: path.to_owned(), symbol: a.symbol };
            let detail = AtomDetail {
                hash,
                sig: a.sig.map(|r| slice(&r).split_whitespace().collect::<Vec<_>>().join(" ")),
                text: a.ranges.iter().map(slice).collect(),
            };
            (key, detail)
        })
        .collect()
}

/// The identifiers an atom's text mentions: every `[A-Za-z_][A-Za-z0-9_]*` word. A lexical
/// over-approximation of what it references (SPEC §8.1 rule 3, risk R4).
pub fn idents(text: &str) -> std::collections::BTreeSet<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| w.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_'))
        .collect()
}

/// The identifier other code would use for an atom: the last word of its name, generics
/// dropped (`fn price` → `price`, `(c *Cart) Total` → `Total`). `None` for the file-level atom.
pub fn ref_name(key: &AtomKey) -> Option<&str> {
    let name = &key.symbol.last()?.name;
    name.split('<').next()?.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).rfind(|w| !w.is_empty())
}

/// The write footprint of changing a file from `base` to `new` (`None` = absent).
pub fn changed_atoms(path: &str, base: Option<&[u8]>, new: Option<&[u8]>) -> Vec<AtomKey> {
    let index = |b: Option<&[u8]>| -> BTreeMap<AtomKey, Hash> {
        b.map(|b| atom_hashes(path, b).into_iter().collect()).unwrap_or_default()
    };
    let (old, new) = (index(base), index(new));
    let mut out: Vec<AtomKey> = old.iter().filter(|(k, h)| new.get(*k) != Some(h)).map(|(k, _)| k.clone()).collect();
    out.extend(new.keys().filter(|k| !old.contains_key(*k)).cloned());
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(v: Vec<AtomKey>) -> Vec<String> {
        v.into_iter().map(|k| k.to_string()).collect()
    }

    const BASE: &str = "use std::fmt;\n\nimpl Cart {\n    fn price(&self) -> u64 { 1 }\n\n    fn tax(&self) -> u64 { 2 }\n}\n";

    #[test]
    fn footprint_is_the_changed_atoms_only() {
        let edit = BASE.replace("{ 1 }", "{ 10 }");
        assert_eq!(keys(changed_atoms("a.rs", Some(BASE.as_bytes()), Some(edit.as_bytes()))), ["a.rs::impl Cart::fn price"]);

        // Adding a method (and its blank line) touches only the new method.
        let add = BASE.replace("    fn tax", "    fn fee(&self) -> u64 { 3 }\n\n    fn tax");
        assert_eq!(keys(changed_atoms("a.rs", Some(BASE.as_bytes()), Some(add.as_bytes()))), ["a.rs::impl Cart::fn fee"]);

        // A comment in the glue is real content of the file-level atom.
        let glue = BASE.replace("\n\nimpl", "\n// note\n\nimpl");
        assert_eq!(keys(changed_atoms("a.rs", Some(BASE.as_bytes()), Some(glue.as_bytes()))), ["a.rs"]);

        // Creating or deleting a file writes all of its atoms.
        assert_eq!(changed_atoms("a.rs", None, Some(BASE.as_bytes())).len(), 5);
        assert_eq!(changed_atoms("a.rs", Some(BASE.as_bytes()), None).len(), 5);
        assert!(changed_atoms("a.rs", Some(BASE.as_bytes()), Some(BASE.as_bytes())).is_empty());
    }

    #[test]
    fn signatures_names_and_identifiers() {
        let d = atom_details("a.rs", BASE.as_bytes());
        let price: AtomKey = "a.rs::impl Cart::fn price".parse().unwrap();
        assert_eq!(d[&price].sig.as_deref(), Some("fn price(&self) -> u64"));
        assert_eq!(d[&price].text, "fn price(&self) -> u64 { 1 }");
        assert_eq!(d[&"a.rs".parse::<AtomKey>().unwrap()].sig, None);
        assert_eq!(ref_name(&price), Some("price"));
        assert_eq!(ref_name(&"m.go::method (c *Cart) Total".parse().unwrap()), Some("Total"));
        assert_eq!(ref_name(&"a.rs::impl Wrapper<T>".parse().unwrap()), Some("Wrapper"));
        assert_eq!(idents("x.price(2) + tax_1 - 3d").into_iter().collect::<Vec<_>>(), ["price", "tax_1", "x"]);
    }

    #[test]
    fn note_id_is_content_addressed() {
        let note = Note {
            author: "a".into(),
            session: 1,
            rank: 1,
            seq: 1,
            base_root: Hash::default(),
            parents: vec![],
            kind: NoteKind::Code,
            ops: vec![Op::Write { path: "a.rs".into(), base: None, content: Hash::of(b"x"), kind: Kind::File }],
            reads: vec![],
        };
        let decoded: Note = encode::decode(&encode::encode(&note)).unwrap();
        assert_eq!(decoded.id(), note.id());
        assert_ne!(Note { seq: 2, ..note.clone() }.id(), note.id());
    }
}
