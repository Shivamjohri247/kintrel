//! Packages and their dependency graph (SPEC §10.3).
//!
//! A package is a directory holding a manifest; a file belongs to the deepest one above it.
//! Dependencies come from the import atoms the atomizer already extracts (`use`, `import`,
//! `export`): relative imports resolve by path, others by their first segment against package
//! names (`kintrel_core::x` → the package named `kintrel-core`). Over-approximate by design.
// ponytail: no manifest dependency parsing (Cargo path deps, workspaces); a dependency used only
// through macros or build scripts is missed, which the backstop run catches (SPEC §9.1).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use kintrel_core::Hash;
use kintrel_core::tree::Kind;

pub const MARKERS: &[&str] =
    &["Cargo.toml", "package.json", "go.mod", "pyproject.toml", "setup.py", "pom.xml", "build.gradle", "build.gradle.kts"];

/// What the graph needs from one file, cached by blob hash.
#[derive(Debug, Clone, Default)]
pub struct FileFacts {
    /// Names of import atoms, as written (`std::fmt`, `../lib`, `os`).
    pub imports: Vec<String>,
    /// (atom key, signature) of every atom with a signature.
    pub sigs: Vec<(String, String)>,
}

impl FileFacts {
    pub fn of(path: &str, bytes: &[u8]) -> FileFacts {
        let mut f = FileFacts::default();
        for (key, d) in kintrel_core::note::atom_details(path, bytes) {
            if let Some(seg) = key.symbol.last()
                && key.symbol.len() == 1
                && matches!(seg.kind.as_str(), "use" | "import" | "export")
            {
                f.imports.push(seg.name.trim_matches(|c| c == '"' || c == '\'').to_owned());
            }
            // Rust (and C++-style) code names other crates through full paths without a `use`
            // (`other_crate::f()`): every identifier written as a path root counts as an import.
            for (i, _) in d.text.match_indices("::") {
                let head = &d.text[..i];
                let start = head.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |j| j + 1);
                let word = &head[start..];
                if word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && !f.imports.iter().any(|x| x == word) {
                    f.imports.push(word.to_owned());
                }
            }
            if let Some(sig) = d.sig {
                f.sigs.push((key.to_string(), sig));
            }
        }
        f
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Directory, "" for the repository root.
    pub dir: String,
    /// Manifest name if it has one, else the directory's base name.
    pub name: String,
    /// Indexes of packages this one imports from.
    pub deps: BTreeSet<usize>,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub packages: Vec<Package>,
}

impl Graph {
    /// Detects packages in `files` and their dependencies. `facts` gives a file's facts;
    /// `text` gives a manifest's contents.
    pub fn build(
        files: &BTreeMap<String, (Kind, Hash)>,
        mut facts: impl FnMut(&str, Hash) -> FileFacts,
        mut text: impl FnMut(Hash) -> String,
    ) -> Graph {
        let mut packages = vec![];
        for (path, (_, id)) in files {
            let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
            if MARKERS.contains(&file) && packages.iter().all(|p: &Package| p.dir != dir) {
                let base = dir.rsplit('/').next().unwrap_or(dir);
                let manifest = text(*id);
                // Only a Cargo.toml with `[workspace]` and no `[package]` is a virtual workspace
                // root, not a package. Anything unclear stays a package: dropping one would
                // silently skip its checks.
                if file == "Cargo.toml" && is_virtual_workspace(&manifest) {
                    continue;
                }
                let name = manifest_name(file, &manifest).unwrap_or_else(|| base.to_owned());
                packages.push(Package { dir: dir.to_owned(), name, deps: BTreeSet::new() });
            }
        }
        let mut g = Graph { packages };
        let by_name: HashMap<String, usize> = g.packages.iter().enumerate().map(|(i, p)| (normalize(&p.name), i)).collect();
        for (path, (kind, id)) in files {
            let Some(me) = g.owner(path) else { continue };
            if *kind == Kind::Symlink || kintrel_lang::lang_for_path(path).is_none() {
                continue;
            }
            for import in facts(path, *id).imports {
                let dep = if import.starts_with('.') {
                    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
                    resolve(dir, &import).and_then(|p| g.owner(&p))
                } else {
                    let first = import.split(['/', ':', '.']).find(|s| !s.is_empty()).unwrap_or("");
                    by_name.get(&normalize(first)).copied()
                };
                if let Some(dep) = dep.filter(|d| *d != me) {
                    g.packages[me].deps.insert(dep);
                }
            }
        }
        g
    }

    /// The package a path belongs to: the deepest package directory above it.
    pub fn owner(&self, path: &str) -> Option<usize> {
        self.packages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.dir.is_empty() || path == p.dir || path.starts_with(&format!("{}/", p.dir)))
            .max_by_key(|(_, p)| p.dir.len())
            .map(|(i, _)| i)
    }

    /// Every package `i` depends on, directly or not.
    pub fn closure(&self, i: usize) -> BTreeSet<usize> {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<usize> = self.packages[i].deps.iter().copied().collect();
        while let Some(d) = todo.pop() {
            if seen.insert(d) {
                todo.extend(self.packages[d].deps.iter().copied());
            }
        }
        seen.remove(&i);
        seen
    }

    /// Packages containing a changed path, plus every package that depends on one (SPEC §10.3).
    pub fn affected<'a>(&self, changed: impl IntoIterator<Item = &'a str>) -> BTreeSet<usize> {
        let direct: BTreeSet<usize> = changed.into_iter().filter_map(|p| self.owner(p)).collect();
        (0..self.packages.len()).filter(|i| direct.contains(i) || !self.closure(*i).is_disjoint(&direct)).collect()
    }
}

/// Poetry projects name themselves under `[tool.poetry]`.
fn poetry_name(text: &str) -> Option<String> {
    let t = text.parse::<toml::Table>().ok()?;
    Some(t.get("tool")?.get("poetry")?.get("name")?.as_str()?.to_owned())
}

fn normalize(name: &str) -> String {
    name.to_ascii_lowercase().replace('-', "_")
}

/// `../lib/x` from `src/app` → `src/lib/x`; `None` if it climbs out of the repository.
fn resolve(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn is_virtual_workspace(text: &str) -> bool {
    text.parse::<toml::Table>().is_ok_and(|t| t.contains_key("workspace") && !t.contains_key("package"))
}

fn manifest_name(file: &str, text: &str) -> Option<String> {
    let toml_name = |table: &str| {
        let t = text.parse::<toml::Table>().ok()?;
        Some(t.get(table)?.get("name")?.as_str()?.to_owned()).filter(|n| !n.is_empty())
    };
    match file {
        "Cargo.toml" => toml_name("package"),
        "pyproject.toml" => toml_name("project").or_else(|| poetry_name(text)),
        "package.json" => {
            let rest = text.split("\"name\"").nth(1)?.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
            Some(rest[..rest.find('"')?].to_owned()).filter(|n| !n.is_empty())
        }
        "go.mod" => text.lines().find_map(|l| l.trim().strip_prefix("module ").map(|m| m.trim().rsplit('/').next().unwrap_or(m).to_owned())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_imports_and_affected() {
        let blob = |s: &str| Hash::of(s.as_bytes());
        let src = [
            ("Cargo.toml", "[workspace]\nmembers = [\"*\"]\n"),
            ("odd/Cargo.toml", "this is not toml [\n"),
            ("core/Cargo.toml", "[package]\nname = \"my-core\"\n"),
            ("core/src/lib.rs", "pub fn f() {}\n"),
            ("app/Cargo.toml", "[package]\nname = \"app\"\n"),
            ("app/src/main.rs", "use my_core::f;\nfn main() { f() }\n"),
            ("tool/Cargo.toml", "[package]\nname = \"tool\"\n"),
            ("tool/src/main.rs", "fn main() {\n    my_core::f();\n}\n"),
            ("web/package.json", "{\n  \"name\": \"web\"\n}\n"),
            ("web/index.js", "import { g } from \"../shared/util.js\";\n"),
            ("shared/package.json", "{\n  \"name\": \"shared\"\n}\n"),
            ("shared/util.js", "export function g() {}\n"),
            ("README.md", "hi\n"),
        ];
        let files: BTreeMap<String, (Kind, Hash)> = src.iter().map(|(p, t)| (p.to_string(), (Kind::File, blob(t)))).collect();
        let by_hash: HashMap<Hash, &str> = src.iter().map(|(_, t)| (blob(t), *t)).collect();
        let g = Graph::build(&files, |p, h| FileFacts::of(p, by_hash[&h].as_bytes()), |h| by_hash[&h].to_owned());
        let idx = |name: &str| g.packages.iter().position(|p| p.name == name).unwrap();
        let (core, app, web, shared) = (idx("my-core"), idx("app"), idx("web"), idx("shared"));
        assert_eq!(g.packages[app].deps, BTreeSet::from([core]));
        assert_eq!(g.packages[idx("tool")].deps, BTreeSet::from([core]), "a full path with no `use` is a dependency");
        assert_eq!(g.packages[web].deps, BTreeSet::from([shared]));
        assert_eq!(g.owner("README.md"), None, "a virtual workspace root is no package");
        assert!(g.packages.iter().any(|p| p.dir == "odd"), "an unparsable manifest stays a package (fail closed)");
        assert_eq!(g.affected(["core/src/lib.rs"]), BTreeSet::from([core, app, idx("tool")]));
        assert_eq!(g.affected(["app/src/main.rs"]), BTreeSet::from([app]));
    }
}
