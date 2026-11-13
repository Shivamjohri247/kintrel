//! Tier 3: Claude proposes a merge for a conflict, posted as a `Resolution` note.

use kintrel_core::Hash;
use kintrel_core::note::{NoteKind, Op};
use kintrel_core::proto::{self, RpcError, code};

use super::{Board, TIER3_AGENT};

impl Board {
    /// Tier 3 (SPEC §7.3): asks Claude to merge a conflicting note into current head, and posts
    /// the validated result as a `Resolution` note from the `kintrel-tier3` session, where the
    /// gate judges it like any note. Only when the trusted (promoted) kintrel.toml enables it.
    pub(super) async fn tier3(&self, losing_session: u64, conflict: &kintrel_core::combine::Conflict, token: &mut Option<String>) -> Result<(), RpcError> {
        let internal = |m: String| RpcError::new(code::INTERNAL, m);
        let promoted = self.heads.borrow().promoted;
        let settings = self
            .with_reader(move |st| {
                let Some(id) = kintrel_ci::config_of(st, promoted).map_err(|e| kintrel_core::Error::Invalid(e.to_string()))? else { return Ok(None) };
                let text = st.get_blob(id)?.unwrap_or_default();
                Ok(kintrel_ci::Config::parse(&String::from_utf8_lossy(&text)).ok().map(|c| c.merge))
            })
            .await?;
        let Some(settings) = settings.filter(|m| m.tier3) else { return Ok(()) };
        if settings.tier3_command.is_none() && !kintrel_core::tier3::available() {
            return Err(internal("no backend: set ANTHROPIC_API_KEY or install the claude CLI".into()));
        }
        let model = settings.tier3_model.unwrap_or_else(|| kintrel_core::tier3::DEFAULT_MODEL.into());
        let command = settings.tier3_command;
        if token.is_none() {
            let started = self.open_session(TIER3_AGENT.into()).await?;
            *token = Some(started.token);
        }
        let token = token.clone().expect("set above");
        // Merge into the current head.
        let _: proto::Refreshed = self.refresh(proto::Refresh { token: token.clone() }).await?;
        let (path, losing) = (conflict.atom.path.clone(), conflict.losing.to_string());
        let ours = self.read_file(proto::ReadFile { token: token.clone(), path: path.clone(), at: None }).await?;
        let p = path.clone();
        let sides = self
            .with_reader(move |st| {
                let Some((_, note)) = st.find_note(&losing)? else { return Ok(None) };
                let blob = |id: Option<Hash>| -> kintrel_core::Result<Vec<u8>> {
                    Ok(id.map(|id| st.get_blob(id)).transpose()?.flatten().unwrap_or_default())
                };
                Ok(match note.ops.iter().find(|op| op.path() == p) {
                    Some(Op::Write { base, content, .. }) => Some((blob(*base)?, blob(Some(*content))?)),
                    _ => None, // a deletion against an edit: nothing to merge textually
                })
            })
            .await?;
        let Some((base, theirs)) = sides else { return Ok(()) };
        let ours_bytes = ours.content.clone().into_bytes();
        let p = path.clone();
        let merged = tokio::task::spawn_blocking(move || {
            let merged = kintrel_core::tier3::propose_with(&p, &base, &ours_bytes, &theirs, &model, command.as_deref())?;
            if !kintrel_core::merge::proposal_keeps_sides(&p, &base, &ours_bytes, &theirs, &merged) {
                return Err("the proposal changed one-sided edits or does not parse".to_string());
            }
            String::from_utf8(merged).map_err(|_| "the proposal is not UTF-8".to_string())
        })
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(internal)?;
        let posted = self
            .post_as(
                proto::Post {
                    token,
                    files: vec![proto::FileWrite { path, content: Some(merged), base: Some(ours.hash), binary: None }],
                    summary: Some(format!("Tier 3 merge for conflict {}", conflict.id().short())),
                },
                NoteKind::Resolution { conflict: conflict.id() },
            )
            .await?;
        self.coord.lock().unwrap().merge_proposed(losing_session, &conflict.atom, posted.note_id);
        Ok(())
    }
}
