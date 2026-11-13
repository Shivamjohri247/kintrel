//! `sync`: Git commits into the board, as notes from `git` sessions.

use std::collections::HashMap;

use kintrel_core::Hash;
use kintrel_core::note::NoteKind;
use kintrel_core::proto::{self, MAX_OPS, RpcError, code};
use kintrel_core::tree::Kind;

use super::{Board, ViewFile};

impl Board {
    /// Brings a Git branch's new commits into the board (SPEC §11): each one becomes notes from
    /// its own `git` session, made against the commit's base, so the combiner merges it into head
    /// and conflicts with agents' work are recorded like any other. The sync mark moves past
    /// each commit once its notes are folded.
    pub(super) async fn sync(&self, p: proto::Sync) -> Result<proto::Synced, RpcError> {
        let invalid = |e: anyhow::Error| RpcError::new(code::INVALID_PARAMS, format!("{e:#}"));
        let internal = |e: &dyn std::fmt::Display| RpcError::new(code::INTERNAL, e.to_string());
        let repo = self.repo.clone();
        let (branch, commits) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let branch = match p.branch {
                Some(b) => b,
                None => crate::git::head_branch(&repo)?,
            };
            let commits = crate::git::pending(&repo, &branch, p.since.as_deref())?;
            Ok((branch, commits))
        })
        .await
        .map_err(|e| internal(&e))?
        .map_err(invalid)?;
        let mut synced = vec![];
        for c in commits {
            let started = self.open_session(crate::git::SYNC_AGENT.into()).await?;
            let mut notes = vec![];
            let mut result = Ok(());
            for chunk in c.files.chunks(MAX_OPS) {
                let mut from_git = HashMap::new();
                let mut files = vec![];
                for f in chunk {
                    let base = f.base.as_ref().map(|(kind, bytes)| ViewFile { kind: *kind, id: Hash::of(bytes), bytes: bytes.clone() });
                    from_git.insert(f.path.clone(), (base, f.new.as_ref().map_or(Kind::File, |(k, _)| *k)));
                    files.push((f.path.clone(), f.new.as_ref().map(|(_, b)| b.clone()), None));
                }
                match self.post_files(&started.token, files, NoteKind::Code, from_git).await {
                    Ok(posted) => notes.push(posted.note_id),
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            let lost = match result {
                Ok(()) => self.refresh(proto::Refresh { token: started.token.clone() }).await.map(|r| r.lost),
                Err(e) => Err(e),
            };
            self.end(proto::SessionRef { token: started.token }).await?;
            let lost = lost.map_err(|e| RpcError::new(e.code, format!("commit {}: {}", c.id, e.message)))?;
            let (repo, branch, id) = (self.repo.clone(), branch.clone(), c.id);
            tokio::task::spawn_blocking(move || crate::git::set_sync_mark(&repo, &branch, id))
                .await
                .map_err(|e| internal(&e))?
                .map_err(|e| internal(&format!("{e:#}")))?;
            synced.push(proto::SyncedCommit { commit: c.id.to_string(), summary: c.summary, notes, lost });
        }
        Ok(proto::Synced { branch, commits: synced })
    }
}
