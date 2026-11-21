//! Experimental interactive commands following the gopls wire protocol.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::{Command, ServerCapabilities, Uri, WorkspaceEdit, request::Request};

pub const CHANGE_SIGNATURE: &str = "refactor.changeSignature";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveCommand {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub form_fields: Vec<Field>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub form_answers: Vec<Answer>,
}

impl From<&Command> for InteractiveCommand {
    fn from(command: &Command) -> Self {
        Self {
            command: command.command.clone(),
            arguments: command.arguments.clone().unwrap_or_default(),
            form_fields: Vec::new(),
            form_answers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Field {
    pub id: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: FieldKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Value,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FieldKind {
    String,
    Bool,
    Number,
    Enum {
        entries: Vec<EnumEntry>,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EnumEntry {
    pub value: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Answer {
    pub id: String,
    pub value: Value,
}

pub enum ResolveCommand {}

impl Request for ResolveCommand {
    type Params = InteractiveCommand;
    type Result = InteractiveCommand;
    const METHOD: &'static str = "command/resolve";
}

pub enum ExecuteCommand {}

impl Request for ExecuteCommand {
    type Params = InteractiveCommand;
    type Result = Option<Value>;
    const METHOD: &'static str = "workspace/executeCommand";
}

#[derive(Deserialize, Serialize)]
pub struct PreviewEdit {
    pub edit: WorkspaceEdit,
    pub originals: HashMap<Uri, String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

pub fn supports_resolution(capabilities: &ServerCapabilities) -> bool {
    advertises(capabilities, "kinds", "command")
}

pub fn supports_preview(capabilities: &ServerCapabilities, command: &str) -> bool {
    supports_resolution(capabilities) && advertises(capabilities, "previewCommands", command)
}

fn advertises(capabilities: &ServerCapabilities, property: &str, value: &str) -> bool {
    capabilities
        .experimental
        .as_ref()
        .and_then(|capabilities| capabilities.get("interactiveResolveProvider"))
        .and_then(|provider| provider.get(property))
        .and_then(Value::as_array)
        .is_some_and(|entries| entries.iter().any(|entry| entry.as_str() == Some(value)))
}
