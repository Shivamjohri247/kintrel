//! Golden atomization tests. Each `tests/golden/<file>` has a `<file>.atoms` snapshot.
//! Regenerate snapshots with `UPDATE_GOLDEN=1 cargo test -p kintrel-lang`.

use std::fs;
use std::path::Path;

use kintrel_lang::{AtomKey, Segmented, segment};

fn render(path: &str, src: &[u8], seg: &Segmented) -> String {
    let mut out = String::new();
    for a in seg.atoms() {
        let key = AtomKey { path: path.into(), symbol: a.symbol };
        let ranges: Vec<String> = a.ranges.iter().map(|r| format!("{}..{}", r.start, r.end)).collect();
        let sig = a.sig.map(|r| String::from_utf8_lossy(&src[r.start as usize..r.end as usize]).split_whitespace().collect::<Vec<_>>().join(" "));
        let first = sig.map(|s| s.chars().take(60).collect()).or_else(|| a.ranges.iter().find_map(|r| {
            let text = String::from_utf8_lossy(&src[r.start as usize..r.end as usize]).into_owned();
            text.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.chars().take(50).collect::<String>())
        }));
        out += &format!("{key}\t{}\t{}\n", ranges.join(","), first.unwrap_or_default());
    }
    out
}

/// Every check that must hold for any file: exact tiling and unique keys.
pub fn check_invariants(path: &str, src: &[u8], seg: &Segmented) {
    let mut at = 0;
    for r in seg.leaf_ranges() {
        assert_eq!(r.start, at, "{path}: gap or overlap at byte {at}");
        at = r.end;
    }
    assert_eq!(at as usize, src.len(), "{path}: ranges do not cover the file");
    let atoms = seg.atoms();
    let mut keys: Vec<_> = atoms.iter().map(|a| &a.symbol).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), atoms.len(), "{path}: duplicate atom keys");
    // Owned ranges of all atoms also tile the file.
    let mut owned: Vec<_> = atoms.iter().flat_map(|a| a.ranges.iter().cloned()).filter(|r| !r.is_empty()).collect();
    owned.sort_by_key(|r| r.start);
    assert_eq!(owned, seg.leaf_ranges(), "{path}: atom ranges do not tile the file");
}

#[test]
fn golden() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut checked = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if name.ends_with(".atoms") {
            continue;
        }
        let src = fs::read(&path).unwrap();
        let seg = segment(&name, &src);
        check_invariants(&name, &src, &seg);
        let got = render(&name, &src, &seg);
        let snap = dir.join(format!("{name}.atoms"));
        if update {
            fs::write(&snap, &got).unwrap();
        } else {
            let want = fs::read_to_string(&snap).unwrap_or_default();
            assert_eq!(got, want, "{name}: atoms changed; rerun with UPDATE_GOLDEN=1 and review the diff");
        }
        checked += 1;
    }
    assert!(checked >= 2);
}

#[test]
fn fallbacks() {
    let broken = b"fn main( {";
    assert_eq!(segment("a.rs", broken).atoms().len(), 1, "parse error: whole file is one atom");
    assert_eq!(segment("notes.txt", b"hello").atoms().len(), 1);
    assert!(segment("empty.rs", b"").items.is_empty());
}

/// Runs over every supported source file under the directories in `KINTREL_CORPUS` (colon-separated).
#[test]
#[ignore]
fn corpus() {
    let roots = std::env::var("KINTREL_CORPUS").expect("set KINTREL_CORPUS");
    let (mut files, mut fallback, mut atoms, mut unparsed) = (0, 0, 0, 0);
    let mut stack: Vec<_> = roots.split(':').map(std::path::PathBuf::from).collect();
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else { continue };
        if meta.is_dir() {
            stack.extend(fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
            continue;
        }
        let name = p.to_string_lossy().into_owned();
        if kintrel_lang::lang_for_path(&name).is_none() || meta.len() > 1 << 20 {
            continue;
        }
        let src = fs::read(&p).unwrap();
        let seg = segment(&name, &src);
        check_invariants(&name, &src, &seg);
        files += 1;
        let n = seg.atoms().len();
        atoms += n;
        if n == 1 && !src.is_empty() {
            fallback += 1;
            if kintrel_lang::segment_checked(&name, &src).is_none() {
                unparsed += 1;
            }
        }
    }
    println!("corpus: {files} files, {atoms} atoms, {fallback} single-atom files ({unparsed} do not parse, the rest have no items)");
}

#[test]
fn every_query_compiles() {
    for path in ["a.rs", "a.py", "a.ts", "a.tsx", "a.js", "a.go", "a.java"] {
        assert!(kintrel_lang::lang_for_path(path).is_some(), "{path}");
    }
}
