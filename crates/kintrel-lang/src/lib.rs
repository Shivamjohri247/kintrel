//! Atomizer: splits a source file into atoms and glue with tree-sitter (SPEC §4.2,
//! ARCHITECTURE §4.1). Pure: bytes in, segments out; no store, no hashing.
//!
//! Each language is a grammar plus `langs/<lang>/atoms.scm`, which uses these captures:
//! - `@atom.<kind>`: an item; `<kind>` (a lowercase word) becomes the key segment kind.
//! - `@name`: the item's name. With several `@name` captures, the name is the source span
//!   from the first to the last, whitespace collapsed (`Display for Cart`).
//! - `@body`: marks a container; atoms that are direct children of the body are members.
//! - `@attach`: comments and attributes that attach to the item right after them.
//!
//! An atom's signature (SPEC §8.3) needs no capture: it runs from the item's start (attached
//! comments excluded) to its block body, found through the grammar's `body` field.
//!
//! Only direct children of the file root are top-level atoms, and only direct children of a
//! top-level container's body are members (one level deep). Everything else is glue, which
//! belongs to the file-level atom (or, inside a container, to the container's own atom).

mod key;

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::OnceLock;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator};

pub use key::{AtomKey, ParseKeyError, Segment};

/// A file as an ordered list of items. Concatenating every item's bytes gives the file back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segmented {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Glue(Range<u32>),
    Atom(Atom),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atom {
    pub segment: Segment,
    /// Whole item, including attached comments and, for a container, its members.
    pub range: Range<u32>,
    /// The signature: the item without attached comments, up to its block body (a container's
    /// header). Items without a block body (structs, constants, imports) are all signature.
    pub sig: Range<u32>,
    pub body: Option<Container>,
}

/// `header`, then `items`, then `footer` exactly cover the container atom's range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub header: Range<u32>,
    pub items: Vec<Item>,
    pub footer: Range<u32>,
}

/// One atom of a file and the byte ranges it owns (SPEC §4.2 `AtomEntry`, before hashing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomSpan {
    /// Empty for the file-level atom.
    pub symbol: Vec<Segment>,
    /// Concatenated, these are the atom's bytes. A container owns its header, the glue
    /// between its members and its footer; each member owns its own range.
    pub ranges: Vec<Range<u32>>,
    /// `None` for the file-level atom.
    pub sig: Option<Range<u32>>,
}

impl Segmented {
    /// Every atom of the file, file-level atom first, then in source order.
    pub fn atoms(&self) -> Vec<AtomSpan> {
        let mut out = vec![AtomSpan { symbol: vec![], ranges: glue_of(&self.items), sig: None }];
        for item in &self.items {
            let Item::Atom(a) = item else { continue };
            match &a.body {
                None => out.push(AtomSpan {
                    symbol: vec![a.segment.clone()],
                    ranges: vec![a.range.clone()],
                    sig: Some(a.sig.clone()),
                }),
                Some(c) => {
                    let mut ranges = vec![c.header.clone()];
                    ranges.extend(glue_of(&c.items));
                    ranges.push(c.footer.clone());
                    ranges.retain(|r| !r.is_empty());
                    out.push(AtomSpan { symbol: vec![a.segment.clone()], ranges, sig: Some(a.sig.clone()) });
                    for m in &c.items {
                        if let Item::Atom(m) = m {
                            out.push(AtomSpan {
                                symbol: vec![a.segment.clone(), m.segment.clone()],
                                ranges: vec![m.range.clone()],
                                sig: Some(m.sig.clone()),
                            });
                        }
                    }
                }
            }
        }
        out
    }

    /// The leaf ranges in file order. They tile `0..len` exactly; tests check this.
    pub fn leaf_ranges(&self) -> Vec<Range<u32>> {
        fn walk(items: &[Item], out: &mut Vec<Range<u32>>) {
            for item in items {
                match item {
                    Item::Glue(r) => out.push(r.clone()),
                    Item::Atom(Atom { range, body: None, .. }) => out.push(range.clone()),
                    Item::Atom(Atom { body: Some(c), .. }) => {
                        out.push(c.header.clone());
                        walk(&c.items, out);
                        out.push(c.footer.clone());
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.items, &mut out);
        out.retain(|r| !r.is_empty());
        out
    }
}

fn glue_of(items: &[Item]) -> Vec<Range<u32>> {
    items.iter().filter_map(|i| if let Item::Glue(r) = i { Some(r.clone()) } else { None }).collect()
}

pub struct Lang {
    pub name: &'static str,
    language: Language,
    query: Query,
}

struct LangDef {
    name: &'static str,
    exts: &'static [&'static str],
    language: fn() -> Language,
    atoms: &'static str,
}

const LANGS: &[LangDef] = &[
    LangDef {
        name: "rust",
        exts: &["rs"],
        language: || tree_sitter_rust::LANGUAGE.into(),
        atoms: include_str!("../langs/rust/atoms.scm"),
    },
    LangDef {
        name: "python",
        exts: &["py", "pyi"],
        language: || tree_sitter_python::LANGUAGE.into(),
        atoms: include_str!("../langs/python/atoms.scm"),
    },
    LangDef {
        name: "typescript",
        exts: &["ts", "mts", "cts"],
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        atoms: include_str!("../langs/typescript/atoms.scm"),
    },
    LangDef {
        name: "tsx",
        exts: &["tsx"],
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        atoms: include_str!("../langs/typescript/atoms.scm"),
    },
    LangDef {
        name: "javascript",
        exts: &["js", "mjs", "cjs", "jsx"],
        language: || tree_sitter_javascript::LANGUAGE.into(),
        atoms: include_str!("../langs/javascript/atoms.scm"),
    },
    LangDef {
        name: "go",
        exts: &["go"],
        language: || tree_sitter_go::LANGUAGE.into(),
        atoms: include_str!("../langs/go/atoms.scm"),
    },
    LangDef {
        name: "java",
        exts: &["java"],
        language: || tree_sitter_java::LANGUAGE.into(),
        atoms: include_str!("../langs/java/atoms.scm"),
    },
];

fn compiled() -> &'static [Lang] {
    static COMPILED: OnceLock<Vec<Lang>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        LANGS
            .iter()
            .map(|d| {
                let language = (d.language)();
                let query = Query::new(&language, d.atoms)
                    .unwrap_or_else(|e| panic!("langs/{}/atoms.scm does not compile: {e}", d.name));
                Lang { name: d.name, language, query }
            })
            .collect()
    })
}

/// The language for a path, by file extension.
pub fn lang_for_path(path: &str) -> Option<&'static Lang> {
    let file = path.rsplit('/').next()?;
    let ext = file.rsplit_once('.')?.1;
    LANGS.iter().position(|d| d.exts.contains(&ext)).map(|i| &compiled()[i])
}

/// Segments a file. An unsupported language, a parse error or a grammar surprise gives one
/// glue item: the whole file is the file-level atom and merges at line level (SPEC §4.2 rule 5).
pub fn segment(path: &str, src: &[u8]) -> Segmented {
    segment_checked(path, src).unwrap_or_else(|| Segmented {
        items: if src.is_empty() { vec![] } else { vec![Item::Glue(0..src.len() as u32)] },
    })
}

/// Like [`segment`], but `None` when the language is unsupported or the file does not parse.
pub fn segment_checked(path: &str, src: &[u8]) -> Option<Segmented> {
    let lang = lang_for_path(path)?;
    u32::try_from(src.len()).ok()?;
    let mut parser = Parser::new();
    parser.set_language(&lang.language).ok()?;
    let tree = parser.parse(src, None)?;
    if tree.root_node().has_error() {
        return None;
    }
    lang.segment(src, tree.root_node())
}

struct Found<'t> {
    node: Node<'t>,
    kind: String,
    name: String,
    body: Option<Node<'t>>,
}

impl Lang {
    fn segment(&self, src: &[u8], root: Node) -> Option<Segmented> {
        let names = self.query.capture_names();
        let mut found: Vec<Found> = Vec::new();
        let mut seen = HashSet::new();
        let mut attach = HashSet::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&self.query, root, src);
        while let Some(m) = matches.next() {
            let (mut atom, mut kind, mut span, mut body) = (None, "", None::<Range<usize>>, None);
            for c in m.captures() {
                match names[c.index as usize] {
                    "name" => {
                        let r = c.node.byte_range();
                        span = Some(span.map_or(r.clone(), |s| s.start.min(r.start)..s.end.max(r.end)));
                    }
                    "body" => body = Some(c.node),
                    "attach" => {
                        attach.insert(c.node.id());
                    }
                    cap => {
                        if let Some(k) = cap.strip_prefix("atom.") {
                            atom = Some(c.node);
                            kind = k;
                        }
                    }
                }
            }
            // A node matched by several patterns keeps the first (patterns are tried in file order).
            if let Some(node) = atom.filter(|n| seen.insert(n.id())) {
                let name = span
                    .map(|s| String::from_utf8_lossy(&src[s]).split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "<anon>".into());
                found.push(Found { node, kind: kind.to_owned(), name, body });
            }
        }

        let mut children: HashMap<usize, Vec<&Found>> = HashMap::new();
        for f in &found {
            if let Some(p) = f.node.parent() {
                children.entry(p.id()).or_default().push(f);
            }
        }
        let leaf = |f: &Found| Atom {
            segment: Segment { kind: f.kind.clone(), name: f.name.clone(), ordinal: 0 },
            range: extend_start(f.node, &attach, src)..f.node.end_byte() as u32,
            sig: f.node.start_byte() as u32..sig_end(f.node),
            body: None,
        };
        let mut top = Vec::new();
        for f in children.get(&root.id()).into_iter().flatten() {
            let mut atom = leaf(f);
            if let Some(b) = f.body {
                let members = children.get(&b.id()).into_iter().flatten().map(|m| leaf(m)).collect();
                let (bs, be) = (b.start_byte() as u32, b.end_byte() as u32);
                atom.sig.end = bs;
                atom.body = Some(Container {
                    header: atom.range.start..bs,
                    items: tile(bs..be, members)?,
                    footer: be..atom.range.end,
                });
            }
            top.push(atom);
        }
        Some(Segmented { items: tile(0..src.len() as u32, top)? })
    }
}

/// Lays atoms over `region`, filling gaps with glue and numbering same-named siblings.
/// `None` if atoms overlap or leave the region; the caller then falls back to whole-file glue.
fn tile(region: Range<u32>, mut atoms: Vec<Atom>) -> Option<Vec<Item>> {
    atoms.sort_by_key(|a| a.range.start);
    let mut items = Vec::new();
    let mut at = region.start;
    let mut ordinals: HashMap<(String, String), u32> = HashMap::new();
    for mut a in atoms {
        if a.range.start < at || a.range.end > region.end || a.range.start >= a.range.end {
            return None;
        }
        if a.range.start > at {
            items.push(Item::Glue(at..a.range.start));
        }
        at = a.range.end;
        let n = ordinals.entry((a.segment.kind.clone(), a.segment.name.clone())).or_default();
        a.segment.ordinal = *n;
        *n += 1;
        items.push(Item::Atom(a));
    }
    if at < region.end {
        items.push(Item::Glue(at..region.end));
    }
    Some(items)
}

/// Where a leaf item's signature ends: the start of its block body, looked for a few levels
/// down because decorators, `export` and `const f = () => {}` wrap the function. Without one,
/// the whole item is signature.
fn sig_end(node: Node) -> u32 {
    fn body(n: Node, depth: u32) -> Option<u32> {
        if let Some(b) = n.child_by_field_name("body")
            && (b.kind().ends_with("block") || b.kind().ends_with("body"))
        {
            return Some(b.start_byte() as u32);
        }
        if depth == 0 {
            return None;
        }
        let mut cursor = n.walk();
        n.named_children(&mut cursor).find_map(|c| body(c, depth - 1))
    }
    body(node, 3).unwrap_or(node.end_byte() as u32)
}

/// Pulls in `@attach` siblings right before `node`: each must start its own line and be
/// separated from what follows by whitespace with no blank line.
fn extend_start(node: Node, attach: &HashSet<usize>, src: &[u8]) -> u32 {
    let mut start = node.start_byte();
    let mut cur = node;
    while let Some(prev) = cur.prev_sibling() {
        if !attach.contains(&prev.id()) {
            break;
        }
        let gap = &src[prev.end_byte()..start];
        // Some grammars end a line comment after its newline, so count that one too.
        let newlines = gap.iter().filter(|&&b| b == b'\n').count() + usize::from(src[prev.end_byte() - 1] == b'\n');
        let line_start = src[..prev.start_byte()].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let own_line = src[line_start..prev.start_byte()].iter().all(|&b| b == b' ' || b == b'\t');
        if newlines > 1 || !gap.iter().all(u8::is_ascii_whitespace) || !own_line {
            break;
        }
        start = prev.start_byte();
        cur = prev;
    }
    start as u32
}
