use nanocode_turnlog::Record;

use super::{ThreadState, ThreadStore, facts::refused, sim, store::records_of};
use crate::{
    durability::{StoreError, StoreRecord},
    one_log::{Exported, Imported, shared, stored_key},
};

type Unsaved = Vec<(String, String)>;

fn unsaved(state: &ThreadState, state_id: &str, records: &[StoreRecord]) -> Unsaved {
    records
        .iter()
        .map(|record| {
            let value = shared::staged(state, state_id, record).into_owned();
            (stored_key(state_id, &record.key), value)
        })
        .filter(|(key, value)| state.stored(key) != Some(value.as_str()))
        .collect()
}

fn decided(
    state: &ThreadState,
    state_id: &str,
    revision: u64,
    payload: &str,
    unsaved: &Unsaved,
) -> Result<(Imported, u64), StoreError> {
    match state.checkpoint(state_id) {
        None => Ok((Imported::Copied, revision)),
        Some(held) if held.revision != revision => Err(StoreError::Conflict {
            expected: revision,
            actual: held.revision,
        }),
        Some(held) if held.payload != payload => Err(StoreError::NotCommitted(format!(
            "the thread holds state {state_id} at revision {revision} with a different payload"
        ))),
        Some(_) if !unsaved.is_empty() => Err(StoreError::NotCommitted(format!(
            "the thread holds state {state_id} at revision {revision} with a different record {}",
            unsaved[0].0
        ))),
        Some(_) if sim::mutated(sim::Mutation::ReimportAdvances) => {
            Ok((Imported::Copied, revision + 1))
        }
        Some(_) => Ok((Imported::Held, revision)),
    }
}

impl ThreadStore {
    pub async fn import_state(
        &self,
        state_id: &str,
        revision: u64,
        payload: &str,
        records: &[StoreRecord],
    ) -> Result<Imported, StoreError> {
        if revision == 0 {
            return Err(StoreError::NotCommitted(format!(
                "state {state_id} is imported at revision 0, which holds no checkpoint"
            )));
        }
        let store = self.clone();
        let state_id = state_id.to_owned();
        let payload = payload.to_owned();
        let records = records.to_vec();
        self.log()
            .job(move |staged| {
                if store.owned(&state_id) {
                    return Ok(Err(StoreError::Fenced));
                }
                let unsaved = unsaved(staged.state(), &state_id, &records);
                let (imported, written) =
                    match decided(staged.state(), &state_id, revision, &payload, &unsaved) {
                        Ok(decided) => decided,
                        Err(error) => return Ok(Err(error)),
                    };
                if imported == Imported::Held {
                    return Ok(Ok(imported));
                }
                for (key, value) in unsaved {
                    staged.push(Record::Stored { key, value })?;
                }
                staged.push(Record::Checkpoint {
                    payload: payload.clone(),
                    revision: written,
                    state: state_id.clone(),
                })?;
                Ok(Ok(imported))
            })
            .await
            .map_err(refused)?
    }

    pub async fn export_state(&self, state_id: &str) -> Result<Option<Exported>, StoreError> {
        self.log().flush(None).await.map_err(refused)?;
        let state = self.log().state().ok_or_else(|| {
            refused(nanocode_scope::ScopeError::Routed {
                holder: self.log().holder().unwrap_or_default().to_owned(),
            })
        })?;
        let Some(checkpoint) = state.checkpoint(state_id) else {
            return Ok(None);
        };
        let records = records_of(&state, state_id)
            .map(|(key, value)| {
                Ok(StoreRecord {
                    key: key.to_owned(),
                    value: shared::whole(&*state, state_id, key, value.to_owned())?.into(),
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Some(Exported {
            revision: checkpoint.revision,
            payload: checkpoint.payload.clone(),
            records,
        }))
    }
}
