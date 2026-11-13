//! `revert`: undoing one note as a per-file three-way merge.

use kintrel_core::Hash;
use kintrel_core::merge::merge3;
use kintrel_core::proto::{self, RpcError, code};

use super::{Board, view_file};

impl Board {
    /// Reverts a note as a three-way merge per file: base = the note's result, ours = the view,
    /// theirs = the note's base. Only the note's own atoms go back; later work stays.
    pub(super) async fn revert(&self, p: proto::Revert) -> Result<proto::Posted, RpcError> {
        let (root, overlay) = {
            let s = self.session(&p.token)?;
            let s = s.lock().await;
            (s.base_root, s.overlay.clone())
        };
        let prefix = p.note.clone();
        let files = self
            .with_reader(move |st| {
                let Some((_, note)) = st.find_note(&prefix)? else {
                    return Ok(Err(RpcError::new(code::NOT_FOUND, format!("no single note matches {prefix:?}"))));
                };
                let blob = |id: Option<Hash>| id.map(|id| st.blob(id)).transpose();
                let mut out = vec![];
                let mut stuck = vec![];
                for op in &note.ops {
                    let (before, after) = op.sides();
                    let (before, after) = (blob(before)?, blob(after)?);
                    let view = view_file(st, root, overlay.get(op.path()).cloned(), op.path())?.map(|f| f.bytes);
                    match merge3(op.path(), after.as_deref(), view.as_deref(), before.as_deref()) {
                        Ok(r) if r == view => {}
                        Ok(r) => out.push((op.path().to_owned(), r)),
                        Err(atoms) => stuck.extend(atoms.into_iter().map(|a| a.to_string())),
                    }
                }
                if stuck.is_empty() {
                    return Ok(Ok(out));
                }
                Ok(Err(RpcError {
                    data: Some(serde_json::json!({ "atoms": stuck })),
                    ..RpcError::new(code::CONFLICT, format!("cannot revert cleanly: {} changed since", stuck.join(", ")))
                }))
            })
            .await??;
        if files.is_empty() {
            return Err(RpcError::new(code::INVALID_PARAMS, "nothing to revert: your view already lacks that note's changes"));
        }
        let mut writes = vec![];
        for (path, content) in files {
            let content = content
                .map(String::from_utf8)
                .transpose()
                .map_err(|_| RpcError::new(code::INVALID_PARAMS, format!("{path:?} is not UTF-8 text")))?;
            writes.push(proto::FileWrite { path, content, base: None, binary: None });
        }
        self.post(proto::Post { token: p.token, files: writes, summary: Some(format!("revert {}", p.note)) }).await
    }
}
