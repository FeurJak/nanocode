use super::*;
use serde_json::json;

enum Noticed {
    Appended(Option<Snapshot>),
    Retry,
}

pub(super) fn append_developer_notice<'a>(
    state: &'a State,
    text: String,
    prefix: Option<&'a str>,
) -> AgentFuture<'a, Result<Option<Snapshot>>> {
    let prefix = prefix.map(str::to_owned);
    Box::pin(async move {
        loop {
            let notified = state.persistence_ready.notified();
            let mut notified = std::pin::pin!(notified);
            notified.as_mut().enable();
            let shared = state.shared();
            let (text, prefix) = (text.clone(), prefix.clone());
            let noticed = state
                .session
                .admit(move || Box::pin(async move { append_admitted(shared, text, prefix).await }))
                .await??;
            match noticed {
                Noticed::Appended(snapshot) => return Ok(snapshot),
                Noticed::Retry => notified.await,
            }
        }
    })
}

async fn append_admitted(
    state: Arc<State>,
    text: String,
    prefix: Option<String>,
) -> Result<Noticed> {
    loop {
        let seen = state.session.revision();
        if state.life.stopped() {
            return Err(NanocodeError::AgentStopped);
        }
        let mut snapshot = state.latest_boundary().await?;
        if prefix
            .as_deref()
            .and_then(|prefix| latest_developer_notice(&snapshot, prefix))
            .is_some_and(|latest| latest.ends_with(&text))
        {
            return Ok(Noticed::Appended(None));
        }
        snapshot
            .developer_notices
            .push((nanocode_platform::uuid_v7().to_string(), text.clone()));
        let owner = Arc::clone(&state);
        if let Some(noticed) = state
            .session
            .persist_at(seen, move || Box::pin(persist_notice(owner, snapshot)))
            .await?
        {
            return noticed;
        }
    }
}

async fn persist_notice(state: Arc<State>, snapshot: Snapshot) -> Result<Noticed> {
    let notice = snapshot
        .developer_notices
        .last()
        .cloned()
        .ok_or_else(|| unsupported("Claude developer notice is missing"))?;
    if let Some(policy) = &state.policy {
        let operation = state
            .steering
            .call(|turns| {
                turns
                    .iter()
                    .filter_map(|(key, turn)| turn.operation.as_ref().map(|id| (key.0, id.clone())))
                    .min_by_key(|(key, _)| *key)
                    .map(|(_, id)| id)
            })
            .await?;
        if let Some(operation) = operation {
            let step = format!("developer-notice-{}", notice.0);
            policy
                .begin_step(
                    operation.clone(),
                    step.clone(),
                    "developer_notice".into(),
                    json!(notice),
                )
                .await?;
            policy.complete_step(operation, step, Value::Null).await?;
        } else if let Err(error) = policy
            .checkpoint(serde_json::to_value(&snapshot).map_err(provider_error)?)
            .await
        {
            if error.execution_policy_disposition()
                == Some(nanocode_agent_core::ExecutionPolicyDisposition::Retry)
            {
                return Ok(Noticed::Retry);
            }
            return Err(error);
        }
    }
    state.developer_notices.update(|queued| {
        if !queued.iter().any(|(id, _)| id == &notice.0) {
            queued.push(notice.clone());
        }
    });
    Ok(Noticed::Appended(Some(snapshot)))
}
