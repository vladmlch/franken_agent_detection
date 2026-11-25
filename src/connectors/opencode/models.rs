//! Data models and serde structures for `OpenCode` SQLite and JSON storage.

#![allow(
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::if_not_else,
    clippy::manual_let_else,
    clippy::manual_string_new,
    clippy::map_unwrap_or,
    clippy::missing_const_for_fn,
    clippy::must_use_candidate,
    clippy::nonminimal_bool,
    clippy::option_if_let_else,
    clippy::single_option_map,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_wraps,
    clippy::unreadable_literal
)]

use serde::Deserialize;

use crate::connectors::sqlite_sync::SqliteValue;
use crate::types::NormalizedMessage;

pub(super) struct SqliteMessageRow {
    pub(super) session_id: String,
    pub(super) id: String,
    pub(super) data_json: String,
    pub(super) time_created_raw: Option<SqliteValue>,
}

pub(super) struct PendingSqliteMessage {
    pub(super) created_at: Option<i64>,
    pub(super) message_id: String,
    pub(super) message: NormalizedMessage,
}

/// One OpenCode 2.x `session_message` row (cass GH #504).
pub(super) struct SqliteV2MessageRow {
    pub(super) session_id: String,
    pub(super) id: String,
    pub(super) message_type: String,
    pub(super) seq: Option<i64>,
    pub(super) data_json: String,
    pub(super) time_created_raw: Option<SqliteValue>,
}

/// Session row from SQLite.
/// Timestamps are read as raw SQLite values because Drizzle ORM
/// may store them as TEXT (ISO 8601) or INTEGER (epoch seconds/ms).
pub(super) struct SqliteSession {
    pub(super) id: String,
    pub(super) title: Option<String>,
    pub(super) directory: Option<String>,
    pub(super) project_id: Option<String>,
    pub(super) time_created_raw: Option<SqliteValue>,
    pub(super) time_updated_raw: Option<SqliteValue>,
}

/// Deserialized message.data JSON from SQLite.
#[derive(Debug, Deserialize)]
pub(super) struct SqliteMessageData {
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) time: Option<MessageTime>,
    #[serde(rename = "modelID", default)]
    pub(super) model_id: Option<String>,
    #[serde(rename = "providerID", default)]
    pub(super) provider_id: Option<String>,
    #[serde(default)]
    pub(super) model: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) tokens: Option<serde_json::Value>,
}

/// Deserialized part.data JSON from SQLite.
#[derive(Debug, Default, Deserialize)]
pub(super) struct SqlitePartData {
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default, alias = "order", alias = "sequence")]
    pub(super) index: Option<i64>,
    #[serde(rename = "type", default)]
    pub(super) part_type: Option<String>,
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default, alias = "name", alias = "toolName")]
    pub(super) tool: Option<String>,
    #[serde(default, rename = "callID", alias = "callId", alias = "call_id")]
    pub(super) call_id: Option<String>,
    #[serde(default)]
    pub(super) state: Option<ToolState>,
}

// ============================================================================
// JSON Structures for OpenCode Storage (pre-v1.2 flat files)
// ============================================================================

/// Session info from session/{projectID}/{sessionID}.json
#[derive(Debug, Deserialize)]
pub(super) struct SessionInfo {
    pub(super) id: String,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) directory: Option<String>,
    #[serde(rename = "projectID", default)]
    pub(super) project_id: Option<String>,
    #[serde(default)]
    pub(super) time: Option<SessionTime>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SessionTime {
    #[serde(default)]
    pub(super) created: Option<i64>,
    #[serde(default)]
    pub(super) updated: Option<i64>,
}

/// Message info from message/{sessionID}/{messageID}.json
#[derive(Debug, Deserialize)]
pub(super) struct MessageInfo {
    pub(super) id: String,
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) time: Option<MessageTime>,
    #[serde(rename = "modelID", default)]
    pub(super) model_id: Option<String>,
    #[serde(rename = "providerID", default)]
    pub(super) provider_id: Option<String>,
    #[serde(default)]
    pub(super) model: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) tokens: Option<serde_json::Value>,
    #[serde(rename = "sessionID", default)]
    pub(super) session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MessageTime {
    #[serde(default)]
    pub(super) created: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) completed: Option<i64>,
}

/// Part info from part/{messageID}/{partID}.json
#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct PartInfo {
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) id: Option<String>,
    #[serde(default, alias = "order", alias = "sequence")]
    pub(super) index: Option<i64>,
    #[serde(rename = "messageID", default)]
    #[allow(dead_code)]
    pub(super) message_id: Option<String>,
    #[serde(rename = "type", default)]
    pub(super) part_type: Option<String>,
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default, alias = "name", alias = "toolName")]
    pub(super) tool: Option<String>,
    #[serde(default, rename = "callID", alias = "callId", alias = "call_id")]
    pub(super) call_id: Option<String>,
    // Tool state for tool parts
    #[serde(default)]
    pub(super) state: Option<ToolState>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct ToolState {
    #[serde(default, alias = "arguments", alias = "args")]
    pub(super) input: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) output: Option<String>,
    // A failed tool run's error record in `state.error` — source text the
    // v1 render dropped (F14).
    #[serde(default)]
    pub(super) error: Option<ToolStateError>,
}

/// A failed tool run's error record. OpenCode writes either a plain string
/// or an object carrying the failure text under `message` (the measured
/// real shape is `{message, type}`); any other shape is still kept so the
/// part never loses its identity over the error's shape (F14).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum ToolStateError {
    Text(String),
    Object {
        #[serde(default)]
        message: Option<String>,
    },
    // Any other shape: kept unread (the raw record is archived verbatim in
    // the message extras anyway) so a part never loses its identity over
    // the error's shape.
    Other(#[allow(dead_code)] serde_json::Value),
}

impl ToolStateError {
    /// The renderable failure text: the string form, or the object's
    /// `message`.
    pub(super) fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Object { message } => message.as_deref(),
            Self::Other(_) => None,
        }
    }
}

/// A decoded part together with its raw source JSON (F14). The raw record is
/// archived in the message extras so the host can verify the connector
/// delivered everything — identity, input, state — instead of inferring it
/// from the rendered text.
#[derive(Debug, Clone)]
pub(super) struct LoadedPart {
    pub(super) info: PartInfo,
    pub(super) raw: serde_json::Value,
}
