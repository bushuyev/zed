use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::{Range, SymbolKind, SymbolTag, TypeHierarchyPrepareParams, Uri, request};

// The pinned lsp-types represents `tags` as a single tag. Servers send an array.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub name: String,
    pub kind: SymbolKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<SymbolTag>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub uri: Uri,
    pub range: Range,
    pub selection_range: Range,
    // Treat an explicit null as part of the opaque payload.
    #[serde(
        default,
        deserialize_with = "deserialize_data",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Box<RawValue>>,
}

fn deserialize_data<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, Serialize)]
pub struct FollowupParams {
    pub item: Item,
}

pub enum Prepare {}

impl request::Request for Prepare {
    type Params = TypeHierarchyPrepareParams;
    type Result = Option<Vec<Item>>;
    const METHOD: &'static str = <request::TypeHierarchyPrepare as request::Request>::METHOD;
}

pub enum Supertypes {}

impl request::Request for Supertypes {
    type Params = FollowupParams;
    type Result = Option<Vec<Item>>;
    const METHOD: &'static str = <request::TypeHierarchySupertypes as request::Request>::METHOD;
}

pub enum Subtypes {}

impl request::Request for Subtypes {
    type Params = FollowupParams;
    type Result = Option<Vec<Item>>;
    const METHOD: &'static str = <request::TypeHierarchySubtypes as request::Request>::METHOD;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_hierarchy_data_round_trips() -> serde_json::Result<()> {
        for data in ["null", r#"{"unknown":[18446744073709551616, null]}"#] {
            let response = format!(
                r#"{{
                    "name":"Derived", "kind":5, "tags":[1], "uri":"file:///test.cpp",
                    "range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":15}}}},
                    "selectionRange":{{"start":{{"line":0,"character":6}},"end":{{"line":0,"character":13}}}},
                    "data":{data}
                }}"#
            );
            let item = serde_json::from_str::<Item>(&response)?;
            let serialized = serde_json::to_string(&FollowupParams { item })?;
            let params = serde_json::from_str::<FollowupParams>(&serialized)?;
            assert_eq!(params.item.data.as_deref().map(RawValue::get), Some(data));
            assert_eq!(params.item.tags, Some(vec![SymbolTag::DEPRECATED]));
        }
        Ok(())
    }
}
