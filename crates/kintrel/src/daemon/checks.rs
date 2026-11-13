//! `checks.run`: checks on demand, against a session's view, a root or head.

use kintrel_core::proto::{self, RpcError, code};

use super::{Board, open_reader};

impl Board {
    /// Runs checks on a session's view (its base plus its own writes), a root, or head.
    pub(super) async fn checks(&self, p: proto::ChecksRun) -> Result<proto::ChecksRan, RpcError> {
        let tier = p.tier.map(|t| t.parse::<kintrel_ci::Tier>()).transpose().map_err(|e| RpcError::new(code::INVALID_PARAMS, e))?;
        let snap = match (&p.token, p.at) {
            (Some(token), _) => {
                let s = self.session(token)?;
                let s = s.lock().await;
                self.coord.lock().unwrap().touch(s.id);
                kintrel_ci::Snapshot { root: s.base_root, overlay: s.overlay.clone().into_iter().collect(), ..Default::default() }
            }
            (None, Some(root)) => kintrel_ci::Snapshot { root, ..Default::default() },
            (None, None) => kintrel_ci::Snapshot { root: self.heads.borrow().head, ..Default::default() },
        };
        let (ci, repo, root) = (self.ci.clone(), self.repo.clone(), snap.root);
        let promoted = self.heads.borrow().promoted;
        let outcomes = tokio::task::spawn_blocking(move || -> Result<Vec<kintrel_ci::Outcome>, String> {
            let st = open_reader(&repo).map_err(|e| e.to_string())?;
            // The promoted config, never the snapshot's own (see ci_worker).
            let config = kintrel_ci::config_of(&st, promoted).map_err(|e| e.to_string())?;
            ci.run(&st, &repo, config, &snap, tier).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?
        .map_err(|e| RpcError::new(code::INVALID_PARAMS, e))?;
        let outcomes = outcomes
            .into_iter()
            .map(|o| proto::CheckOutcome {
                check: o.check,
                tier: o.tier.name().into(),
                target: o.target,
                ok: o.ok,
                cached: o.cached,
                timed_out: o.timed_out,
                ms: o.ms,
                output: o.output,
                key: o.key,
            })
            .collect();
        Ok(proto::ChecksRan { root, outcomes })
    }
}
