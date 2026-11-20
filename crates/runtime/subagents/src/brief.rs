use nanocode_board::brief::BriefPolicy;

use super::{BoardCoordination, Inner, ReadFuture, ready};

impl BoardCoordination {
    pub fn set_brief(&self, policy: Option<BriefPolicy>) {
        self.inner.send(move |inner| inner.brief = policy);
    }

    pub(super) fn briefed<'a>(
        &'a self,
        session: &'a str,
    ) -> ReadFuture<'a, Result<Option<String>, String>> {
        let session = session.to_owned();
        Box::pin(self.inner.call(move |inner| inner.brief_text(&session)))
    }
}

impl Inner {
    fn brief_text(&mut self, session: &str) -> Result<Option<String>, String> {
        let Some(policy) = self.brief else {
            return Ok(None);
        };
        let task = self
            .sessions
            .get(session)
            .map(|state| state.task(session))
            .ok_or_else(|| format!("subagent brief: session {session} has no board task"))?;
        let board = self
            .dispatcher_board()
            .ok_or_else(|| "subagent brief: the board is not open".to_owned())?;
        if let Some(Err(error)) = ready(board.refresh()) {
            return Err(format!("subagent brief: board refresh: {error}"));
        }
        board
            .fold()
            .brief(&task, &policy)
            .map(|brief| Some(brief.text))
            .map_err(|error| format!("subagent brief for {}: {error:?}", task.0))
    }
}
