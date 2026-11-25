use std::borrow::Cow;

use nanocode_tool_host::code_mode::{CellJournalUpdate, JournalEntry};
use nanocode_turnlog::Fold;
use serde_json::{Value, value::RawValue};

use super::{invalid, raw, stored_key};
use crate::durability::{EncodedPayload, StoreError, StoreRecord};

const ENTRIES: &str = "entries";
const KIND: &str = "kind";
const RESULT: &str = "result";
const ENCODED: &str = "encoded";
const SHARED: &str = "stored";
const CONTENT: &str = "b3:";

pub(super) struct Shared {
    pub(super) update: Box<RawValue>,
    pub(super) values: Vec<(String, String)>,
}

fn content(record: &StoreRecord) -> bool {
    record.key.starts_with(CONTENT) && record.value.starts_with('=')
}

fn spliced(key: &str, value: &str) -> bool {
    key.starts_with(CONTENT) && EncodedPayload::is_spliced(value)
}

pub(super) fn candidates(state: &str, records: &[StoreRecord]) -> Vec<String> {
    records
        .iter()
        .filter(|record| content(record))
        .flat_map(|record| EncodedPayload::shareable(&record.value))
        .map(|key| stored_key(state, &key))
        .collect()
}

pub(super) fn staged<'a>(fold: &Fold, state: &str, record: &'a StoreRecord) -> Cow<'a, str> {
    if !content(record) {
        return Cow::Borrowed(&record.value);
    }
    let held = |key: &str| fold.contains(&stored_key(state, key));
    EncodedPayload::splice(&record.value, &held).map_or(Cow::Borrowed(&record.value), Cow::Owned)
}

pub(super) fn spliced_keys<'a>(
    state: &str,
    records: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<Vec<String>, StoreError> {
    let mut keys = Vec::new();
    for (key, value) in records {
        if spliced(key, value) {
            for key in EncodedPayload::spliced_keys(value).map_err(invalid)? {
                keys.push(stored_key(state, key));
            }
        }
    }
    keys.sort_unstable();
    keys.dedup();
    Ok(keys)
}

pub(super) fn whole(
    fold: &Fold,
    state: &str,
    key: &str,
    value: String,
) -> Result<String, StoreError> {
    if !spliced(key, &value) {
        return Ok(value);
    }
    EncodedPayload::unspliced(&value, &|key| fold.stored(&stored_key(state, key))).map_err(invalid)
}

pub(super) fn shared_results(update: &CellJournalUpdate) -> Result<Option<Shared>, StoreError> {
    let sharing = update.entries.iter().any(|entry| {
        matches!(entry, JournalEntry::Result { encoded, .. } if EncodedPayload::shared_value(encoded).is_some())
    });
    if !sharing {
        return Ok(None);
    }
    let mut value = serde_json::to_value(update)
        .map_err(|error| invalid(format!("cell journal update: {error}")))?;
    let mut values = Vec::new();
    let entries = value
        .get_mut(ENTRIES)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid("a cell journal update encodes without entries"))?;
    for entry in entries.iter_mut().filter_map(Value::as_object_mut) {
        if entry.get(KIND).and_then(Value::as_str) != Some(RESULT) {
            continue;
        }
        let Some((key, record)) = entry
            .get(ENCODED)
            .and_then(Value::as_str)
            .and_then(EncodedPayload::shared_value)
        else {
            continue;
        };
        entry.remove(ENCODED);
        entry.insert(SHARED.to_owned(), Value::String(key.clone()));
        values.push((key, record));
    }
    assert!(
        !values.is_empty(),
        "a sharing cell journal update shares a result"
    );
    Ok(Some(Shared {
        update: raw(&value)?,
        values,
    }))
}

fn shared_entries(update: &RawValue) -> Result<Option<Value>, StoreError> {
    if !update.get().contains(SHARED) {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(update.get())
        .map_err(|error| invalid(format!("cell journal update: {error}")))?;
    let shares = value
        .get(ENTRIES)
        .and_then(Value::as_array)
        .is_some_and(|entries| entries.iter().any(|entry| entry.get(SHARED).is_some()));
    Ok(shares.then_some(value))
}

pub(super) fn result_keys(
    session: &str,
    updates: &[Box<RawValue>],
) -> Result<Vec<String>, StoreError> {
    let mut keys = Vec::new();
    for update in updates {
        let Some(value) = shared_entries(update)? else {
            continue;
        };
        for entry in value[ENTRIES].as_array().into_iter().flatten() {
            if let Some(key) = entry.get(SHARED) {
                let key = key.as_str().ok_or_else(|| {
                    invalid("a cell journal result names a non-string stored value")
                })?;
                keys.push(stored_key(session, key));
            }
        }
    }
    Ok(keys)
}

pub(super) fn with_results<'a>(
    fold: &Fold,
    session: &str,
    update: &'a RawValue,
) -> Result<Cow<'a, str>, StoreError> {
    let Some(mut value) = shared_entries(update)? else {
        return Ok(Cow::Borrowed(update.get()));
    };
    for entry in value[ENTRIES]
        .as_array_mut()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
    {
        let Some(key) = entry.remove(SHARED) else {
            continue;
        };
        let key = key
            .as_str()
            .ok_or_else(|| invalid("a cell journal result names a non-string stored value"))?;
        let stored = fold
            .stored(&stored_key(session, key))
            .ok_or_else(|| invalid(format!("cell journal result {key} is missing")))?;
        let encoded = stored
            .strip_prefix('=')
            .filter(|encoded| {
                EncodedPayload::shared_value(encoded).is_some_and(|(held, _)| held == key)
            })
            .ok_or_else(|| {
                invalid(format!(
                    "cell journal result {key} does not hold the content its key names"
                ))
            })?;
        entry.insert(ENCODED.to_owned(), Value::String(encoded.to_owned()));
    }
    serde_json::to_string(&value)
        .map(Cow::Owned)
        .map_err(|error| invalid(format!("cell journal update: {error}")))
}
