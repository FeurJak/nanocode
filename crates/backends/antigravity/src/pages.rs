use nanocode_backend_antigravity_client::Content;
use nanocode_durability::{
    EncodedPayload, Error, PagedWriter, PayloadReader, Result, Sequence, SequenceLog,
    sequence_references,
};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub(crate) const PAGED: &str = "antigravity.v2";
const CONTENTS: &str = "contents";
const PAGES: &str = "contents_pages";
const FORMAT: &str = "format";

#[derive(Default)]
pub(crate) struct Pages {
    items: Vec<Content>,
    log: SequenceLog,
}

fn invalid(what: &str, error: impl std::fmt::Display) -> Error {
    Error::InvalidState(format!("antigravity {what}: {error}"))
}

impl Pages {
    pub(crate) fn encode(
        &mut self,
        mut head: Map<String, Value>,
        contents: &[Content],
    ) -> Result<EncodedPayload> {
        let mut unchanged = self
            .items
            .iter()
            .zip(contents)
            .take_while(|(cached, content)| cached == content)
            .count();
        assert!(
            self.items.len() == self.log.len(),
            "the antigravity page cache holds as many contents as its log"
        );
        if unchanged < self.log.floor() {
            *self = Self::default();
            unchanged = 0;
        }
        self.log.sync(
            unchanged,
            contents[unchanged..].iter().map(EncodedPayload::encode),
        )?;
        self.items.truncate(unchanged);
        self.items.extend_from_slice(&contents[unchanged..]);
        head.insert(FORMAT.to_owned(), Value::String(PAGED.to_owned()));
        head.insert(
            PAGES.to_owned(),
            serde_json::to_value(self.log.sequence()).map_err(|error| invalid("pages", error))?,
        );
        Ok(EncodedPayload::encode(&head)?.with_payloads(self.log.frontier().cloned().collect()))
    }

    pub(crate) fn committed(&mut self) {
        self.log.release();
    }
}

pub(crate) fn encode_whole(mut checkpoint: Map<String, Value>) -> Result<EncodedPayload> {
    let contents: Vec<Content> = match checkpoint.remove(CONTENTS) {
        Some(contents) => {
            serde_json::from_value(contents).map_err(|error| invalid("contents", error))?
        }
        None => Vec::new(),
    };
    let known = HashSet::new();
    let mut writer = PagedWriter::new(&known);
    let sequence = writer.sequence(&contents)?;
    checkpoint.insert(FORMAT.to_owned(), Value::String(PAGED.to_owned()));
    checkpoint.insert(
        PAGES.to_owned(),
        serde_json::to_value(sequence).map_err(|error| invalid("pages", error))?,
    );
    Ok(writer.finish(&checkpoint)?.payload)
}

pub(crate) fn paged(checkpoint: &Value) -> bool {
    checkpoint.get(FORMAT).and_then(Value::as_str) == Some(PAGED)
}

pub(crate) async fn hydrate(
    reader: PayloadReader<'_>,
    mut stored: Value,
    read: &mut HashSet<String>,
) -> Result<(Value, Option<Pages>)> {
    if !paged(&stored) {
        return Ok((stored, None));
    }
    let object = stored
        .as_object_mut()
        .ok_or_else(|| Error::InvalidState("an antigravity checkpoint is not an object".into()))?;
    let pages = object.remove(PAGES).ok_or_else(|| {
        Error::InvalidState("a paged antigravity checkpoint holds no contents pages".into())
    })?;
    let sequence: Sequence =
        serde_json::from_value(pages).map_err(|error| invalid("pages", error))?;
    let references = sequence_references(reader, sequence, read).await?;
    let loaded = match references.is_empty() {
        true => Vec::new(),
        false => reader.take_payloads(references).await?,
    };
    let mut log = SequenceLog::default();
    let mut items = Vec::with_capacity(loaded.len());
    for payload in loaded {
        items.push(payload.decode::<Content>()?);
        log.push(payload.reference())?;
    }
    log.release();
    object.insert(
        FORMAT.to_owned(),
        Value::String(crate::durability::FORMAT.to_owned()),
    );
    object.insert(
        CONTENTS.to_owned(),
        serde_json::to_value(&items).map_err(|error| invalid("contents", error))?,
    );
    Ok((stored, Some(Pages { items, log })))
}
