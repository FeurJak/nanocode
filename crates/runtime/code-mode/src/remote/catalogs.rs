use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use nanocode_sync::Atom;
use serde::Serialize;
use serde_json::{Value, value::RawValue};

use super::{RemoteCatalog, RemoteFrame, serialize_catalog_definitions};
use crate::{ToolDefinition, embedded::CodeModeHostError};

pub const CATALOG_HEADER: &str = "nanocode-catalog";

const DEFINITIONS: &str = "definitions";
const CATALOG_HASH: &str = "catalog_hash";
const TYPE: &str = "type";

struct Definitions<'a>(&'a [ToolDefinition]);

impl Serialize for Definitions<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_catalog_definitions(self.0, serializer)
    }
}

fn error(issue: impl ToString) -> CodeModeHostError {
    CodeModeHostError::new(issue.to_string())
}

fn digest(text: &str) -> String {
    nanocode_digest::blake3(text.as_bytes())
        .to_hex()
        .to_string()
}

pub fn catalog_digest(definitions: &[ToolDefinition]) -> Result<String, CodeModeHostError> {
    serde_json::to_string(&Definitions(definitions))
        .map(|text| digest(&text))
        .map_err(error)
}

pub fn bodiless_catalog(
    frame: &RemoteFrame,
    catalog_hash: &str,
    max_frame_bytes: usize,
) -> Result<String, CodeModeHostError> {
    let mut value = serde_json::to_value(frame).map_err(error)?;
    let fields = value
        .as_object_mut()
        .ok_or_else(|| error("a catalog frame is an object"))?;
    fields.remove(DEFINITIONS);
    fields.insert(CATALOG_HASH.into(), Value::String(catalog_hash.into()));
    let text = value.to_string();
    if text.len() > max_frame_bytes {
        return Err(super::oversized(text.len(), max_frame_bytes));
    }
    Ok(text)
}

pub struct CatalogLease {
    catalog_hash: String,
    definitions: Arc<Vec<ToolDefinition>>,
}

impl CatalogLease {
    #[must_use]
    pub fn catalog_hash(&self) -> &str {
        &self.catalog_hash
    }
}

#[derive(Default)]
pub struct RemoteCatalogs {
    held: Atom<HashMap<String, Weak<Vec<ToolDefinition>>>>,
}

impl RemoteCatalogs {
    #[must_use]
    pub fn lease(&self, catalog_hash: &str) -> Option<CatalogLease> {
        self.held(catalog_hash).map(|definitions| CatalogLease {
            catalog_hash: catalog_hash.to_owned(),
            definitions,
        })
    }

    fn held(&self, catalog_hash: &str) -> Option<Arc<Vec<ToolDefinition>>> {
        self.held
            .read(|held| held.get(catalog_hash).and_then(Weak::upgrade))
    }

    fn keep(&self, catalog_hash: &str, parsed: Vec<ToolDefinition>) -> Arc<Vec<ToolDefinition>> {
        let parsed = Arc::new(parsed);
        self.held.update(|held| {
            held.retain(|_, definitions| definitions.strong_count() > 0);
            if let Some(kept) = held.get(catalog_hash).and_then(Weak::upgrade) {
                return kept;
            }
            held.insert(catalog_hash.to_owned(), Arc::downgrade(&parsed));
            Arc::clone(&parsed)
        })
    }

    pub fn decode(
        &self,
        text: &str,
        max_frame_bytes: usize,
        offered: Option<CatalogLease>,
    ) -> Result<(RemoteCatalog, CatalogLease), CodeModeHostError> {
        if text.len() > max_frame_bytes {
            return Err(error("remote frame exceeds caller max_frame_bytes policy"));
        }
        let mut fields = serde_json::from_str::<HashMap<String, &RawValue>>(text).map_err(error)?;
        let claimed = fields.remove(CATALOG_HASH);
        let lease = match fields.remove(DEFINITIONS) {
            Some(raw) => {
                let catalog_hash = digest(raw.get());
                match offered
                    .filter(|offered| offered.catalog_hash == catalog_hash)
                    .or_else(|| self.lease(&catalog_hash))
                {
                    Some(lease) => lease,
                    None => CatalogLease {
                        definitions: self.keep(
                            &catalog_hash,
                            serde_json::from_str(raw.get()).map_err(error)?,
                        ),
                        catalog_hash,
                    },
                }
            }
            None => {
                let catalog_hash: String = claimed
                    .ok_or_else(|| error("missing field `definitions`"))
                    .and_then(|raw| serde_json::from_str(raw.get()).map_err(error))?;
                offered
                    .filter(|offered| offered.catalog_hash == catalog_hash)
                    .ok_or_else(|| {
                        error(format!(
                            "catalog {catalog_hash} came without definitions the brain did not offer to hold"
                        ))
                    })?
            }
        };
        let kind = fields
            .remove(TYPE)
            .ok_or_else(|| error("missing field `type`"))?;
        let mut rest = format!(r#"{{"{TYPE}":{},"{DEFINITIONS}":[]"#, kind.get());
        for (name, value) in fields {
            rest.push(',');
            rest.push_str(&serde_json::to_string(&name).map_err(error)?);
            rest.push(':');
            rest.push_str(value.get());
        }
        rest.push('}');
        let mut catalog =
            RemoteCatalog::try_from(serde_json::from_str::<RemoteFrame>(&rest).map_err(error)?)?;
        catalog.definitions = Vec::clone(&lease.definitions);
        Ok((catalog, lease))
    }
}
