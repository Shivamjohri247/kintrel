//! Three-way merge of file versions at atom granularity (SPEC §7.2–7.3, ARCHITECTURE §4.1).
//!
//! A parsed file is a *level*: leading glue, then atoms in order, each followed by its gap
//! (the glue up to the next atom). A container atom holds a header, an inner level and a footer.
//! Merging a level:
//! - **Atoms** are matched by key. An atom changed on one side takes that side; changed on both,
//!   it is merged as text (tier 1, diff3). A container changed on both merges recursively.
//!   Delete against modify is a conflict.
//! - **Order** follows the side that reordered (or ours). Atoms only the other side added go
//!   after their anchor, past atoms the first side added there, so concurrent inserts at one spot
//!   land in fold order.
//! - **Gaps and leading glue** merge like atoms; a gap travels with the atom before it, so a
//!   deleted atom takes its gap along.
//!
//! A file that is unsupported or does not parse on any side merges as text. A structured
//! merge whose result no longer parses is a conflict: merging never breaks parsing.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use kintrel_lang::{AtomKey, Item, Segment, Segmented, segment_checked};

/// Merges `ours` and `theirs` (each `None` = absent) against `base`. `Ok(None)` deletes the file;
/// `Err` lists the conflicting atoms.
pub fn merge3(
    path: &str,
    base: Option<&[u8]>,
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, Vec<AtomKey>> {
    merge3_tier(path, base, ours, theirs).map(|(m, _)| m)
}

/// Which tier of the cascade (SPEC §7.3) produced a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// One side unchanged, or both the same.
    Trivial,
    /// Tier 1 on a file without symbols (unsupported language or a parse error).
    Lines,
    /// The structured, per-symbol merge.
    Symbols,
    /// Tier 1 on the whole file after the structured merge conflicted (shifted glue).
    WholeFile,
    /// Tier 2 (Mergiraf).
    Mergiraf,
}

/// [`merge3`], also saying which tier produced the result (for B6).
pub fn merge3_tier(
    path: &str,
    base: Option<&[u8]>,
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
) -> Result<(Option<Vec<u8>>, Tier), Vec<AtomKey>> {
    let r = merge3_tier_uncounted(path, base, ours, theirs);
    match &r {
        Ok((_, tier)) => crate::metrics::MERGES[*tier as usize].add(1),
        Err(_) => crate::metrics::MERGE_CONFLICTS.add(1),
    }
    r
}

fn merge3_tier_uncounted(
    path: &str,
    base: Option<&[u8]>,
    ours: Option<&[u8]>,
    theirs: Option<&[u8]>,
) -> Result<(Option<Vec<u8>>, Tier), Vec<AtomKey>> {
    if ours == theirs || base == theirs {
        return Ok((ours.map(<[u8]>::to_vec), Tier::Trivial));
    }
    if base == ours {
        return Ok((theirs.map(<[u8]>::to_vec), Tier::Trivial));
    }
    let key = |symbol: Vec<Segment>| AtomKey { path: path.to_owned(), symbol };
    let (Some(o), Some(t)) = (ours, theirs) else {
        // Delete against modify: the atoms the surviving side changed.
        let changed = crate::note::changed_atoms(path, base, ours.or(theirs));
        return Err(if changed.is_empty() { vec![key(vec![])] } else { changed });
    };
    let b = base.unwrap_or_default();
    // Tier 2 where Tier 1 fails. Its result must parse (Tier 4) and keep both sides' symbol
    // edits: Mergiraf, like a line merge, resolves delete-against-modify by dropping the edit.
    let tier2 = || {
        crate::tier2::merge(path, b, o, t)
            .filter(|m| {
                (kintrel_lang::lang_for_path(path).is_none() || segment_checked(path, m).is_some())
                    && keeps_changes(path, b, o, m)
                    && keeps_changes(path, b, t, m)
            })
            .map(|m| (Some(m), Tier::Mergiraf))
    };
    let levels = (|| Some((level(path, b)?, level(path, o)?, level(path, t)?)))();
    let Some((lb, lo, lt)) = levels else {
        return text3(b, o, t).map(|m| (Some(m), Tier::Lines)).or_else(tier2).ok_or_else(|| vec![key(vec![])]);
    };
    let mut cx = Cx { conflicts: vec![], touched: vec![] };
    let merged = merge_level(&lb, &lo, &lt, &[], &mut cx);
    if !cx.conflicts.is_empty() {
        // Gap ownership can shift (a new atom before a long run of glue takes that glue as its
        // gap), so a structured conflict may be a clean line merge. Symbol merge must succeed
        // wherever plain line merge does (B3), so try the whole file before Tier 2.
        // It must keep each side's symbol edits intact, though: a line merge happily drops a
        // function one side deleted while the other edited it (found by the property test).
        let whole = text3(b, o, t)
            .filter(|m| segment_checked(path, m).is_some() && keeps_changes(path, b, o, m) && keeps_changes(path, b, t, m));
        return whole.map(|m| (Some(m), Tier::WholeFile)).or_else(tier2).ok_or_else(|| cx.conflicts.into_iter().map(key).collect());
    }
    let mut out = Vec::new();
    merged.render(&mut out);
    if segment_checked(path, &out).is_none() {
        if let Some(m) = tier2() {
            return Ok(m);
        }
        let mut bad: Vec<_> = cx.touched.into_iter().map(key).collect();
        if bad.is_empty() {
            bad.push(key(vec![]));
        }
        return Err(bad);
    }
    Ok((Some(out), Tier::Symbols))
}

/// A Tier 3 proposal may decide only what both sides changed: every leaf symbol changed (or
/// added, or deleted) by exactly one side must come out as that side left it, and the result
/// must parse. A model that "tidies" untouched code or drops one side's edit is refused.
pub fn proposal_keeps_sides(path: &str, base: &[u8], ours: &[u8], theirs: &[u8], merged: &[u8]) -> bool {
    // Fail closed: without symbol support (or a version that does not segment), the whole file
    // is one atom that both sides changed, and the check below would pass anything.
    // ponytail: no Tier 3 for unsupported file types; a line-level one-sided-hunk check would add them.
    if kintrel_lang::lang_for_path(path).is_none() || [base, ours, theirs, merged].iter().any(|v| segment_checked(path, v).is_none()) {
        return false;
    }
    let index = |bytes: &[u8]| -> BTreeMap<AtomKey, crate::Hash> { crate::note::atom_hashes(path, bytes).into_iter().collect() };
    let (b, o, t, m) = (index(base), index(ours), index(theirs), index(merged));
    // Every atom, glue and container headers included: a one-sided change to any of them is kept.
    let keys: BTreeSet<&AtomKey> = b.keys().chain(o.keys()).chain(t.keys()).collect();
    keys.into_iter().all(|k| {
        let (bk, ok, tk) = (b.get(k), o.get(k), t.get(k));
        match (ok != bk, tk != bk) {
            (true, false) => m.get(k) == ok,  // only ours changed it
            (false, true) => m.get(k) == tk,  // only theirs
            _ => true,                        // both (the model's call) or neither
        }
    })
}

/// Every named leaf symbol that `side` changed (or added) is in `merged` exactly as `side` has
/// it, and every one it deleted is gone. Glue and container headers may combine freely.
fn keeps_changes(path: &str, base: &[u8], side: &[u8], merged: &[u8]) -> bool {
    let index = |bytes: &[u8]| -> BTreeMap<AtomKey, crate::Hash> { crate::note::atom_hashes(path, bytes).into_iter().collect() };
    let (b, s, m) = (index(base), index(side), index(merged));
    let leaf = |k: &AtomKey, map: &BTreeMap<AtomKey, crate::Hash>| {
        !k.symbol.is_empty() && !map.keys().any(|o| o.symbol.len() > k.symbol.len() && o.symbol.starts_with(&k.symbol))
    };
    let changed_kept = s.iter().filter(|(k, h)| b.get(*k) != Some(*h) && leaf(k, &s)).all(|(k, h)| m.get(k) == Some(h));
    let deleted_gone = b.keys().filter(|k| !s.contains_key(*k) && leaf(k, &b)).all(|k| !m.contains_key(k));
    changed_kept && deleted_gone
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Level {
    lead: Vec<u8>,
    items: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    seg: Segment,
    node: Node,
    gap: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Leaf(Vec<u8>),
    Container { header: Vec<u8>, inner: Level, footer: Vec<u8> },
}

fn level(path: &str, bytes: &[u8]) -> Option<Level> {
    if bytes.is_empty() {
        return Some(Level::default());
    }
    let seg: Segmented = segment_checked(path, bytes)?;
    Some(Level::from_items(&seg.items, bytes))
}

fn slice(bytes: &[u8], r: &std::ops::Range<u32>) -> Vec<u8> {
    bytes[r.start as usize..r.end as usize].to_vec()
}

impl Level {
    fn from_items(items: &[Item], bytes: &[u8]) -> Level {
        let mut level = Level::default();
        for item in items {
            match item {
                Item::Glue(r) => match level.items.last_mut() {
                    Some(e) => e.gap.extend_from_slice(&slice(bytes, r)),
                    None => level.lead.extend_from_slice(&slice(bytes, r)),
                },
                Item::Atom(a) => {
                    let node = match &a.body {
                        None => Node::Leaf(slice(bytes, &a.range)),
                        Some(c) => Node::Container {
                            header: slice(bytes, &c.header),
                            inner: Level::from_items(&c.items, bytes),
                            footer: slice(bytes, &c.footer),
                        },
                    };
                    level.items.push(Entry { seg: a.segment.clone(), node, gap: vec![] });
                }
            }
        }
        level
    }

    fn render(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.lead);
        for e in &self.items {
            e.node.render(out);
            out.extend_from_slice(&e.gap);
        }
    }

    fn get(&self, seg: &Segment) -> Option<&Entry> {
        self.items.iter().find(|e| &e.seg == seg)
    }

    fn keys(&self) -> Vec<&Segment> {
        self.items.iter().map(|e| &e.seg).collect()
    }
}

impl Node {
    fn render(&self, out: &mut Vec<u8>) {
        match self {
            Node::Leaf(b) => out.extend_from_slice(b),
            Node::Container { header, inner, footer } => {
                out.extend_from_slice(header);
                inner.render(out);
                out.extend_from_slice(footer);
            }
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.render(&mut out);
        out
    }
}

struct Cx {
    conflicts: Vec<Vec<Segment>>,
    /// Atoms merged from two real edits (tier 1); blamed if the result does not parse.
    touched: Vec<Vec<Segment>>,
}

/// Tier 1: line-based diff3. `None` on a conflict.
fn text3(b: &[u8], o: &[u8], t: &[u8]) -> Option<Vec<u8>> {
    if o == t || b == t {
        return Some(o.to_vec());
    }
    if b == o {
        return Some(t.to_vec());
    }
    // Like Git, edits that touch (no unchanged line between them) conflict: diffy would merge
    // them, which B6 caught mixing a variable rename on one side with new uses of the old name
    // on the next lines on the other.
    if touching(&hunks(b, o), &hunks(b, t)) {
        return None;
    }
    diffy::merge_bytes(b, o, t).ok()
}

/// A side's changes to base: (first base line, line count, the lines put there). An insertion
/// is a point at the line it goes before.
fn hunks(base: &[u8], side: &[u8]) -> Vec<(usize, usize, Vec<u8>)> {
    let mut opts = diffy::DiffOptions::new();
    opts.set_context_len(0);
    opts.create_patch_bytes(base, side)
        .hunks()
        .iter()
        .map(|h| {
            let r = h.old_range();
            let new: Vec<u8> =
                h.lines().iter().filter_map(|l| if let diffy::Line::Insert(t) = l { Some(&t[..]) } else { None }).flatten().copied().collect();
            (if r.is_empty() { r.start() + 1 } else { r.start() }, r.len(), new)
        })
        .collect()
}

/// Two changes overlap or touch, and are not the same change (identical edits on both sides
/// agree, as in Git).
fn touching(a: &[(usize, usize, Vec<u8>)], b: &[(usize, usize, Vec<u8>)]) -> bool {
    a.iter().any(|x| b.iter().any(|y| x.0 <= y.0 + y.1 && y.0 <= x.0 + x.1 && x != y))
}

/// Picks one side of a three-way choice where `None` means absent. `both` handles real
/// edits on both sides and returns `None` for a conflict.
fn pick3<T: PartialEq + Clone>(
    b: Option<&T>,
    o: Option<&T>,
    t: Option<&T>,
    both: impl FnOnce(Option<&T>, &T, &T) -> Option<T>,
) -> Result<Option<T>, ()> {
    if o == t || b == t {
        return Ok(o.cloned());
    }
    if b == o {
        return Ok(t.cloned());
    }
    match (o, t) {
        (Some(o), Some(t)) => both(b, o, t).map(Some).ok_or(()),
        _ => Err(()),
    }
}

fn merge_level(b: &Level, o: &Level, t: &Level, owner: &[Segment], cx: &mut Cx) -> Level {
    let path_of = |seg: &Segment| {
        let mut p = owner.to_vec();
        p.push(seg.clone());
        p
    };

    // Atoms.
    let mut resolved: BTreeMap<Segment, Option<Node>> = BTreeMap::new();
    // Ordered, so conflicts come out the same way every time (determinism, SPEC §7.1).
    let all: BTreeSet<&Segment> = o.keys().into_iter().chain(t.keys()).collect();
    for seg in all {
        let (bn, on, tn) = (b.get(seg).map(|e| &e.node), o.get(seg).map(|e| &e.node), t.get(seg).map(|e| &e.node));
        let symbol = path_of(seg);
        let r = pick3(bn, on, tn, |bn, on, tn| merge_node(bn, on, tn, &symbol, cx));
        match r {
            Ok(n) => {
                resolved.insert(seg.clone(), n);
            }
            Err(()) => cx.conflicts.push(symbol),
        }
    }

    // Order.
    let in_b: HashSet<&Segment> = b.keys().into_iter().collect();
    let common = |side: &Level, other: &Level| -> Vec<Segment> {
        let keep: HashSet<&Segment> = other.keys().into_iter().collect();
        side.keys().into_iter().filter(|k| keep.contains(k) && in_b.contains(k)).cloned().collect()
    };
    let o_moved = common(o, b) != common(b, o);
    let t_moved = common(t, b) != common(b, t);
    if o_moved && t_moved && common(o, t) != common(t, o) {
        cx.conflicts.push(owner.to_vec());
    }
    let (primary, secondary) = if t_moved && !o_moved { (t, o) } else { (o, t) };
    let alive = |k: &Segment| matches!(resolved.get(k), Some(Some(_)));
    let mut order: Vec<Segment> = primary.keys().into_iter().filter(|k| alive(k)).cloned().collect();
    let from_primary: HashSet<&Segment> = primary.keys().into_iter().filter(|k| !in_b.contains(k)).collect();
    let mut placed: HashSet<Segment> = order.iter().cloned().collect();
    for (i, k) in secondary.items.iter().map(|e| &e.seg).enumerate() {
        if placed.contains(k) || !alive(k) {
            continue;
        }
        let anchor = secondary.items[..i].iter().rev().map(|e| &e.seg).find(|a| placed.contains(*a));
        let mut pos = anchor.and_then(|a| order.iter().position(|x| x == a)).map_or(0, |p| p + 1);
        while pos < order.len() && from_primary.contains(&order[pos]) {
            pos += 1;
        }
        order.insert(pos, k.clone());
        placed.insert(k.clone());
    }

    // Gaps and leading glue belong to the level's owner.
    let mut owner_conflict = false;
    let lead = text3(&b.lead, &o.lead, &t.lead).unwrap_or_else(|| {
        owner_conflict = true;
        o.lead.clone()
    });
    let mut items = Vec::with_capacity(order.len());
    for seg in order {
        let g = |l: &Level| l.get(&seg).map(|e| e.gap.clone());
        let (gb, go, gt) = (g(b), g(o), g(t));
        let gap = match pick3(gb.as_ref(), go.as_ref(), gt.as_ref(), |gb, go, gt| text3(gb.map_or(&[][..], |v| v), go, gt)) {
            Ok(gap) => gap.unwrap_or_default(),
            Err(()) => {
                owner_conflict = true;
                go.unwrap_or_default()
            }
        };
        let node = resolved.remove(&seg).flatten().expect("ordered atoms are alive");
        items.push(Entry { seg, node, gap });
    }
    // A deleted atom takes its gap (the glue after it) along. If a side that kept the atom
    // edited that glue, the edit has nowhere to go: a conflict, never a silent loss (B6 found
    // `if TYPE_CHECKING:  # pragma: no cover` dropped this way).
    let placed: HashSet<&Segment> = items.iter().map(|e| &e.seg).collect();
    for seg in b.keys().into_iter().chain(o.keys()).chain(t.keys()).filter(|s| !placed.contains(s)) {
        let gb = b.get(seg).map(|e| &e.gap);
        if [o.get(seg).map(|e| &e.gap), t.get(seg).map(|e| &e.gap)].into_iter().any(|side| side.is_some() && side != gb) {
            owner_conflict = true;
        }
    }
    if owner_conflict {
        cx.conflicts.push(owner.to_vec());
    }
    Level { lead, items }
}

fn merge_node(b: Option<&Node>, o: &Node, t: &Node, symbol: &[Segment], cx: &mut Cx) -> Option<Node> {
    if let (Node::Container { header: oh, inner: oi, footer: of }, Node::Container { header: th, inner: ti, footer: tf }) = (o, t) {
        let empty = Level::default();
        let (bh, bi, bf) = match b {
            Some(Node::Container { header, inner, footer }) => (&header[..], inner, &footer[..]),
            _ => (&[][..], &empty, &[][..]),
        };
        // Member conflicts are recorded inside; a header or footer conflict is the container's.
        let inner = merge_level(bi, oi, ti, symbol, cx);
        return Some(Node::Container { header: text3(bh, oh, th)?, inner, footer: text3(bf, of, tf)? });
    }
    cx.touched.push(symbol.to_vec());
    let base = b.map(Node::bytes).unwrap_or_default();
    text3(&base, &o.bytes(), &t.bytes()).map(Node::Leaf)
}

#[cfg(test)]
fn keys_of(r: &Result<Option<Vec<u8>>, Vec<AtomKey>>) -> Vec<String> {
    match r {
        Err(ks) => ks.iter().map(ToString::to_string).collect(),
        Ok(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    /// A proposal may only decide the symbols both sides changed (Tier 3).
    #[test]
    fn proposals_keep_one_sided_edits() {
        let base = "def a():\n    return 1\n\n\ndef b():\n    return 2\n";
        let ours = base.replace("return 1", "return 10").replace("return 2", "return 20");
        let theirs = base.replace("return 2", "return 30");
        let ok = |m: &str| proposal_keeps_sides("t.py", base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), m.as_bytes());
        assert!(ok(&base.replace("return 1", "return 10").replace("return 2", "return 50")), "b() is both sides': any choice");
        assert!(!ok(&base.replace("return 2", "return 50")), "a() lost ours' one-sided edit");
        assert!(!ok("def a(:\n"), "must parse");
        // Glue is checked too: ours' one-sided import must survive.
        let ours = format!("import os\n{}", base.replace("return 2", "return 20"));
        let ok = |m: &str| proposal_keeps_sides("t.py", base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), m.as_bytes());
        assert!(ok(&format!("import os\n{}", base.replace("return 2", "return 50"))));
        assert!(!ok(&base.replace("return 2", "return 50")), "dropped ours' import");
        // No symbol support: the whole file is one atom both sides changed, so nothing is checkable.
        assert!(!proposal_keeps_sides("t.txt", b"a\n", b"b\n", b"c\n", b"anything\n"));
    }

    /// Edits on adjacent lines conflict, as in Git: one line apart they merge (B6).
    #[test]
    fn adjacent_line_edits_conflict_like_git() {
        let base = "a\nb\nc\nd\n";
        let m = |o: &str, t: &str| text3(base.as_bytes(), o.as_bytes(), t.as_bytes());
        assert_eq!(m("A\nb\nc\nd\n", "a\nB\nc\nd\n"), None, "touching");
        assert_eq!(m("A\nb\nc\nd\n", "a\nb\nC\nd\n").as_deref(), Some(&b"A\nb\nC\nd\n"[..]), "one line apart");
        assert_eq!(m("x\na\nb\nc\nd\n", "A\nb\nc\nd\n"), None, "an insertion right before an edited line");
        let m5 = |o: &str, t: &str| text3(b"a\nb\nc\nd\ne\n", o.as_bytes(), t.as_bytes());
        assert_eq!(m5("A\nb\nC\nd\ne\n", "A\nb\nc\nd\nE\n").as_deref(), Some(&b"A\nb\nC\nd\nE\n"[..]), "the same edit on both sides agrees");
        assert_eq!(m("A\nb\nc\nd\n", "A\nB\nc\nd\n"), None, "overlapping but different (Git conflicts too)");
    }

    /// One side deletes an atom while the other edits the glue after it (the deleted atom's
    /// gap): the edit must survive (B6, flask `ctx.py`).
    #[test]
    fn glue_edit_after_a_deleted_atom_survives() {
        let base = "import a\nimport b\n\nif X:\n    pass\n";
        let ours = base.replace("if X:", "if X:  # note");
        let theirs = base.replace("import b\n", "");
        let merged = merge3("t.py", Some(base.as_bytes()), Some(ours.as_bytes()), Some(theirs.as_bytes())).unwrap().unwrap();
        assert_eq!(String::from_utf8(merged).unwrap(), "import a\n\nif X:  # note\n    pass\n");
    }

    /// A new atom before a long run of glue takes that glue as its gap; the per-gap merge then
    /// sees a conflict that a line merge does not (found by B3 on express's mocha tests).
    #[test]
    fn inserted_atom_before_glue_still_merges_like_lines() {
        let base = "var a = 1\n\ndescribe('x', function () {\n  it('one', f)\n  it('mid', h)\n  it('two', g)\n})\n";
        let ours = base.replace("var a = 1\n", "var a = 1\nvar b = 2\n").replace("'one', f", "'one', f2");
        let theirs = base.replace("'two', g", "'two', g2");
        let merged = merge3("t.js", Some(base.as_bytes()), Some(ours.as_bytes()), Some(theirs.as_bytes())).unwrap().unwrap();
        assert_eq!(String::from_utf8(merged).unwrap(), ours.replace("'two', g", "'two', g2"));
    }

    use super::*;

    const BASE: &str = "import os\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return 2\n\n\nclass C:\n    def m(self):\n        return 3\n\n    def n(self):\n        return 4\n";

    fn m(o: &str, t: &str) -> Result<Option<Vec<u8>>, Vec<AtomKey>> {
        merge3("x.py", Some(BASE.as_bytes()), Some(o.as_bytes()), Some(t.as_bytes()))
    }

    fn text(r: Result<Option<Vec<u8>>, Vec<AtomKey>>) -> String {
        String::from_utf8(r.unwrap().unwrap()).unwrap()
    }

    #[test]
    fn disjoint_atom_edits_merge() {
        let o = BASE.replace("return 1", "return 10");
        let t = BASE.replace("return 4", "return 40");
        assert_eq!(text(m(&o, &t)), BASE.replace("return 1", "return 10").replace("return 4", "return 40"));
    }

    #[test]
    fn same_atom_different_lines_merge_by_diff3() {
        let base = "def f():\n    x = 1\n    y = 2\n    z = 3\n";
        let o = base.replace("x = 1", "x = 10");
        let t = base.replace("z = 3", "z = 30");
        let r = merge3("f.py", Some(base.as_bytes()), Some(o.as_bytes()), Some(t.as_bytes()));
        assert_eq!(text(r), "def f():\n    x = 10\n    y = 2\n    z = 30\n");
    }

    #[test]
    fn same_line_is_a_conflict_on_that_atom_only() {
        let o = BASE.replace("return 2", "return 20");
        let t = BASE.replace("return 2", "return 21");
        let r = m(&o, &t);
        assert_eq!(keys_of(&r), ["x.py::def b"]);
    }

    #[test]
    fn concurrent_inserts_at_one_anchor_keep_fold_order() {
        let o = BASE.replace("\n\n\ndef b", "\n\n\ndef x():\n    pass\n\n\ndef b");
        let t = BASE.replace("\n\n\ndef b", "\n\n\ndef y():\n    pass\n\n\ndef b");
        let got = text(m(&o, &t));
        let (x, y, b) = (got.find("def x").unwrap(), got.find("def y").unwrap(), got.find("def b").unwrap());
        assert!(x < y && y < b, "{got}");
        assert!(segment_checked("x.py", got.as_bytes()).is_some());
    }

    #[test]
    fn members_merge_inside_containers() {
        let o = BASE.replace("return 3", "return 30");
        let t = BASE.replace("    def n(self):", "    def k(self):\n        return 5\n\n    def n(self):");
        let got = text(m(&o, &t));
        assert!(got.contains("return 30") && got.contains("def k(self)"), "{got}");
    }

    #[test]
    fn delete_and_edit_elsewhere_merge_but_delete_against_edit_conflicts() {
        let del = BASE.replace("def a():\n    return 1\n\n\n", "");
        let edit_b = BASE.replace("return 2", "return 20");
        assert_eq!(text(m(&del, &edit_b)), BASE.replace("def a():\n    return 1\n\n\n", "").replace("return 2", "return 20"));
        let edit_a = BASE.replace("return 1", "return 10");
        assert_eq!(keys_of(&m(&del, &edit_a)), ["x.py::def a"]);
    }

    #[test]
    fn file_level_cases() {
        // Created on both sides with different atoms: merged against an empty base.
        let r = merge3("n.py", None, Some(b"def a():\n    pass\n"), Some(b"def b():\n    pass\n"));
        let got = text(r);
        assert!(got.contains("def a") && got.contains("def b"));
        // Deleted on one side, edited on the other.
        let r = merge3("x.py", Some(BASE.as_bytes()), None, Some(BASE.replace("return 1", "return 9").as_bytes()));
        assert_eq!(keys_of(&r), ["x.py::def a"]);
        // Unsupported language: plain diff3.
        let r = merge3("n.txt", Some(b"a\nb\nc\n"), Some(b"A\nb\nc\n"), Some(b"a\nb\nC\n"));
        assert_eq!(text(r), "A\nb\nC\n");
    }

    #[test]
    fn merge_is_symmetric_on_clean_results_for_disjoint_edits() {
        let o = BASE.replace("return 1", "return 10");
        let t = BASE.replace("return 3", "return 30");
        assert_eq!(text(m(&o, &t)), text(m(&t, &o)));
    }
}
