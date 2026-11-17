//! Continue IDE/CLI snapshots at `<CONTINUE_GLOBAL_DIR>/sessions/<id>.json`.
//!
//! Native contracts: continuedev/continue@5522c6f44ca0ac3528b37244818fbfa39b5af470,
//! `core/util/{history,paths,messageConversion}.ts`, `core/index.d.ts`, and
//! `extensions/cli/src/{session,services/ChatHistoryService}.ts`.
//! Each file is the current ordered history, not a stream of additive updates.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use super::json_stream::{JsonContainerKind, SeqStreamSeed, peek_json_container};

use super::token_extraction::{ExtractedTokenUsage, TokenDataSource, normalize_model};
use super::utils::{
    dedupe_path_key, excluded_scan_paths_from_env, path_is_excluded, unwrap_skill_invocations,
};
use super::{
    Connector, DiscoveredSourceFile, DiscoveredSourceRole, ScanContext, ScanRoot, SourceCompletion,
    SourceScanHooks, file_modified_since, franken_detection_for_connector,
};
use crate::types::{
    DetectionResult, NormalizedConversation, NormalizedInvocation, NormalizedMessage,
};

const MAX_ARGUMENT_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub struct ContinueConnector;

struct Source {
    root: ScanRoot,
    file: DiscoveredSourceFile,
}

impl ContinueConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn global_dir() -> Option<PathBuf> {
        crate::continue_env_global_dir().or_else(|| dirs::home_dir().map(|p| p.join(".continue")))
    }

    fn native_data_dir(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == ".continue")
            || (path.file_name().is_some_and(|name| name == "sessions")
                && path.parent().is_some_and(|parent| {
                    parent.file_name().is_some_and(|name| name == ".continue")
                }))
            || Self::global_dir()
                .is_some_and(|global| path == global || path == global.join("sessions"))
    }

    fn expand_root(root: &ScanRoot) -> Vec<ScanRoot> {
        if let Some(mirrored) = root.mirrored_store(&[".continue", "sessions"]) {
            return vec![root.with_path(mirrored)];
        }
        if root.path.is_file() {
            return vec![root.clone()];
        }
        let mut expanded = Vec::new();
        // Home/profile and relocated global-home scopes do not recursively
        // roam unrelated stores. A supplied session directory may be renamed.
        for candidate in [
            root.path.join(".continue/sessions"),
            root.path.join("sessions"),
        ] {
            if candidate.is_dir() {
                expanded.push(root.with_path(candidate));
            }
        }
        if expanded.is_empty() && root.path.is_dir() {
            expanded.push(root.clone());
        }
        expanded
    }

    fn sources(ctx: &ScanContext) -> Result<Vec<Source>> {
        let excluded_paths = excluded_scan_paths_from_env()?;
        let roots = if ctx.use_default_detection() {
            let root = if Self::native_data_dir(&ctx.data_dir) {
                Some(ctx.data_dir.clone())
            } else {
                Self::global_dir().map(|root| root.join("sessions"))
            };
            root.map(ScanRoot::local).into_iter().collect()
        } else {
            ctx.scan_roots.clone()
        };
        let mut roots: Vec<_> = roots.iter().flat_map(Self::expand_root).collect();
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        let mut seen = HashSet::new();
        let mut sources = Vec::new();
        for root in roots {
            let mut files = if root.path.is_file() {
                vec![root.path.clone()]
            } else {
                fs::read_dir(&root.path)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file())
                    .collect()
            };
            files.sort();
            for file in files {
                if file.extension().is_none_or(|extension| extension != "json")
                    || file.file_name().is_some_and(|name| name == "sessions.json")
                    || path_is_excluded(&file, &excluded_paths)
                    || !file_modified_since(&file, ctx.since_ts)
                {
                    continue;
                }
                // Ancestor directory aliases need canonicalization too;
                // dedupe_path_key alone only resolves a symlink at the leaf.
                let file = file
                    .canonicalize()
                    .unwrap_or_else(|_| dedupe_path_key(&file));
                if !seen.insert(file.clone()) {
                    continue;
                }
                sources.push(Source {
                    file: DiscoveredSourceFile::new(
                        "continue",
                        &root,
                        file,
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata(),
                    root: root.clone(),
                });
            }
        }
        Ok(sources)
    }

    fn scan_sources(
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        emit: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        for source in Self::sources(ctx)? {
            if !hooks.should_scan(&source.file) {
                continue;
            }
            if let Some(tick) = &ctx.progress_tick {
                tick();
            }
            let parsed = fs::File::open(&source.file.source_path)
                .context("open Continue session")
                .and_then(|file| {
                    parse_session_reader(&source.file.source_path, BufReader::new(file))
                });
            let mut conversation = match parsed {
                Ok(conversation) => conversation,
                Err(error) => {
                    tracing::debug!(path = %source.file.source_path.display(), %error,
                        "continue: skipped unreadable or unsupported session");
                    continue;
                }
            };
            let emitted = usize::from(conversation.is_some());
            if let Some(conversation) = &mut conversation {
                conversation.metadata["origin"] = serde_json::to_value(&source.root.origin)?;
                source.root.apply_workspace_rewrite(conversation);
            }
            if let Some(conversation) = conversation {
                emit(conversation)?;
            }
            // A callback may run while Continue rewrites its snapshot. Never
            // authorize a resume skip using a fingerprint that changed.
            if !source.file.fs_metadata_changed() {
                hooks.complete(&SourceCompletion {
                    source: source.file,
                    required_sidecars: Vec::new(),
                    conversations_emitted: emitted,
                })?;
            }
        }
        Ok(())
    }
}

impl Connector for ContinueConnector {
    fn detect(&self) -> DetectionResult {
        franken_detection_for_connector("continue").unwrap_or_else(DetectionResult::not_found)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn supports_source_boundaries(&self) -> bool {
        true
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        Ok(Self::sources(ctx)?
            .into_iter()
            .map(|source| source.file)
            .collect())
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        Self::scan_sources(ctx, &mut SourceScanHooks::default(), on_conversation)
    }

    fn scan_with_source_boundaries(
        &self,
        ctx: &ScanContext,
        hooks: &mut SourceScanHooks<'_>,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        Self::scan_sources(ctx, hooks, on_conversation)
    }
}

// Typed deserialization skips duplicated editor state, retrieved context,
// prompt/completion logs, tool definitions, parsedArgs and binary attachments
// directly in the reader. None of those become a second retained transcript.
#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "camelCase")]
enum SavedSessionField {
    SessionId,
    Title,
    WorkspaceDirectory,
    History,
    Mode,
    ChatModelTitle,
    Usage,
    Version,
    #[serde(other)]
    Ignore,
}

struct ReadUsageSeed;

impl<'de> DeserializeSeed<'de> for ReadUsageSeed {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        read_usage(deserializer)
    }
}

struct StreamedHistoryEntry {
    primary: Option<NormalizedMessage>,
    primary_requires_tool_results: bool,
    tool_results: Vec<(Option<String>, Option<String>, String)>,
    history_index: usize,
}

struct StreamedSession {
    session_id: String,
    title: String,
    workspace_directory: String,
    entries: Vec<StreamedHistoryEntry>,
    native_results: HashSet<String>,
    mode: Option<String>,
    chat_model_title: Option<String>,
    usage: Value,
    version: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryItem {
    message: Message,
    #[serde(default)]
    tool_call_states: Vec<ToolState>,
    #[serde(default)]
    prompt_logs: Vec<PromptModel>,
    reasoning: Option<Reasoning>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    role: String,
    id: Option<String>,
    #[serde(default)]
    content: TextContent,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    tool_call_id: Option<String>,
    #[serde(default, deserialize_with = "read_usage")]
    usage: Value,
    #[serde(default)]
    metadata: MessageMetadata,
}

#[derive(Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    responses_output_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    responses_output_item_ids: Vec<String>,
}

#[derive(Deserialize)]
struct Reasoning {
    text: String,
}

#[derive(Deserialize, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptModel {
    model_title: String,
    model_provider: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolState {
    tool_call_id: Option<String>,
    tool_call: ToolCall,
    status: Option<String>,
    #[serde(default)]
    output: Vec<ToolOutput>,
}

#[derive(Deserialize)]
struct ToolOutput {
    content: String,
}

#[derive(Deserialize)]
struct ToolCall {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    function: ToolFunction,
}

#[derive(Default, Deserialize)]
struct ToolFunction {
    #[serde(default)]
    name: String,
    #[serde(default, deserialize_with = "read_arguments")]
    arguments: Option<Value>,
}

fn read_arguments<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw
        .filter(|text| text.len() <= MAX_ARGUMENT_BYTES)
        .map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

#[derive(Default)]
struct TextContent(String);

impl<'de> Deserialize<'de> for TextContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TextVisitor;
        impl<'de> Visitor<'de> for TextVisitor {
            type Value = TextContent;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("Continue text or an array of message parts")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(TextContent(value.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(TextContent(value))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(TextContent::default())
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                #[derive(Deserialize)]
                struct Part {
                    #[serde(rename = "type")]
                    kind: String,
                    text: Option<String>,
                }
                let mut text = String::new();
                while let Some(part) = sequence.next_element::<Part>()? {
                    if part.kind == "text" {
                        if let Some(part) = part.text {
                            append_text(&mut text, &part);
                        }
                    } else if part.kind == "imageUrl" {
                        append_text(&mut text, "[Image]");
                    }
                }
                Ok(TextContent(text))
            }
        }
        deserializer.deserialize_any(TextVisitor)
    }
}

fn append_text(target: &mut String, text: &str) {
    if !text.is_empty() {
        if !target.is_empty() {
            target.push('\n');
        }
        target.push_str(text);
    }
}

fn read_usage<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    let raw = Value::deserialize(deserializer)?;
    let mut compact = Map::new();
    for key in [
        "promptTokens",
        "completionTokens",
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "totalCost",
        "cost_cents",
    ] {
        if let Some(value) = raw.get(key) {
            // Preserve invalid presence without retaining arbitrary payloads.
            compact.insert(
                key.into(),
                if value.is_number() {
                    value.clone()
                } else {
                    Value::Null
                },
            );
        }
    }
    if let Some(model) = raw
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 4096)
    {
        compact.insert("model".into(), Value::String(model.to_owned()));
    }
    for (key, fields) in [
        (
            "promptTokensDetails",
            &["cachedTokens", "cacheWriteTokens", "audioTokens"][..],
        ),
        (
            "prompt_tokens_details",
            &[
                "cached_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "audio_tokens",
            ][..],
        ),
        (
            "completionTokensDetails",
            &[
                "reasoningTokens",
                "acceptedPredictionTokens",
                "rejectedPredictionTokens",
                "audioTokens",
            ][..],
        ),
        (
            "completion_tokens_details",
            &[
                "reasoning_tokens",
                "accepted_prediction_tokens",
                "rejected_prediction_tokens",
                "audio_tokens",
            ][..],
        ),
    ] {
        if let Some(details) = raw.get(key) {
            let mut selected = Map::new();
            for field in fields {
                if let Some(value) = details.get(field) {
                    selected.insert(
                        (*field).into(),
                        if value.is_number() {
                            value.clone()
                        } else {
                            Value::Null
                        },
                    );
                }
            }
            compact.insert(
                key.into(),
                if details.is_object() {
                    Value::Object(selected)
                } else {
                    Value::Null
                },
            );
        }
    }
    Ok(if compact.is_empty() {
        Value::Null
    } else {
        Value::Object(compact)
    })
}

fn push_message(
    messages: &mut Vec<NormalizedMessage>,
    role: &str,
    content: String,
    extra: Value,
    invocations: Vec<NormalizedInvocation>,
) {
    messages.push(NormalizedMessage {
        idx: i64::try_from(messages.len()).unwrap_or(i64::MAX),
        role: role.into(),
        author: None,
        // The native snapshot does not persist per-message timestamps. The
        // shared sessions.json index is not a message-time source or a log.
        created_at: None,
        content,
        extra,
        snippets: Vec::new(),
        invocations,
    });
}

fn add_invocation(
    call: ToolCall,
    seen: &mut HashSet<String>,
    content: &mut String,
    invocations: &mut Vec<NormalizedInvocation>,
) {
    if call.kind.as_deref().is_some_and(|kind| kind != "function")
        || call.function.name.trim().is_empty()
    {
        return;
    }
    let id = call.id.filter(|id| !id.is_empty());
    if id.as_ref().is_some_and(|id| !seen.insert(id.clone())) {
        return;
    }
    append_text(content, &format!("[Tool: {}]", call.function.name));
    invocations.push(NormalizedInvocation {
        kind: "tool".into(),
        name: call.function.name,
        raw_name: None,
        call_id: id,
        arguments: call.function.arguments,
    });
}

#[allow(clippy::too_many_lines)]
fn convert_history_item(
    history_index: usize,
    item: HistoryItem,
    native_results: &mut HashSet<String>,
) -> Result<Option<StreamedHistoryEntry>> {
    let message = item.message;
    // IDE stores explicit tool rows as well as toolCallStates.output. CLI
    // stores only the latter. Prefer each actual tool row when both exist.
    if message.role == "tool"
        && let Some(id) = message.tool_call_id.as_ref().filter(|id| !id.is_empty())
    {
        native_results.insert(id.clone());
    }
    let role = match message.role.as_str() {
        "assistant" | "thinking" => "assistant",
        "user" => "user",
        "system" => "system",
        "tool" => "tool",
        _ => return Ok(None),
    };
    let mut content = String::new();
    if let Some(reasoning) = item.reasoning.filter(|_| role == "assistant")
        && !reasoning.text.trim().is_empty()
    {
        append_text(&mut content, &format!("[Thinking] {}", reasoning.text));
    }
    if message.role == "thinking" && !message.content.0.trim().is_empty() {
        append_text(&mut content, &format!("[Thinking] {}", message.content.0));
    } else {
        append_text(&mut content, &message.content.0);
    }
    let mut invocations = Vec::new();
    let mut seen_calls = HashSet::new();
    let has_native_calls = !message.tool_calls.is_empty();
    if role == "assistant" {
        for call in message.tool_calls {
            add_invocation(call, &mut seen_calls, &mut content, &mut invocations);
        }
    }
    let mut tool_results = Vec::new();
    let mut statuses = Vec::new();
    if role == "assistant" {
        for mut state in item.tool_call_states {
            let id = state
                .tool_call_id
                .take()
                .filter(|id| !id.is_empty())
                .or_else(|| state.tool_call.id.clone().filter(|id| !id.is_empty()));
            state.tool_call.id.clone_from(&id);
            if id.is_some() || !has_native_calls {
                add_invocation(
                    state.tool_call,
                    &mut seen_calls,
                    &mut content,
                    &mut invocations,
                );
            }
            if let Some(status) = &state.status {
                statuses.push(json!({"id": id, "status": status}));
            }
            let mut output = String::new();
            for part in state.output {
                append_text(&mut output, &part.content);
            }
            if !output.trim().is_empty() {
                tool_results.push((id, state.status, output));
            }
        }
    }
    unwrap_skill_invocations(&mut invocations);
    let mut models = Vec::new();
    for model in item.prompt_logs {
        if !models.contains(&model) {
            models.push(model);
        }
    }
    let mut cass = json!({"source_format": "continue_session", "history_index": history_index});
    if !invocations.is_empty() {
        cass["tool_call_count"] = json!(invocations.len());
    }
    if !statuses.is_empty() {
        cass["tool_states"] = json!(statuses);
    }
    if !models.is_empty() {
        cass["prompt_models"] = json!(models);
    }
    let mut saved_message = json!({"role": message.role});
    if let Some(id) = message.id {
        saved_message["id"] = json!(id);
    }
    if let Some(id) = message.tool_call_id {
        saved_message["toolCallId"] = json!(id);
    }
    if !message.usage.is_null() {
        saved_message["usage"] = message.usage;
    }
    if message.metadata.responses_output_item_id.is_some()
        || !message.metadata.responses_output_item_ids.is_empty()
    {
        saved_message["metadata"] = serde_json::to_value(message.metadata)?;
    }
    let has_primary_body = !content.trim().is_empty() || saved_message.get("usage").is_some();
    if !has_primary_body && tool_results.is_empty() {
        return Ok(None);
    }
    let primary = NormalizedMessage {
        idx: 0,
        role: role.into(),
        author: None,
        // The native snapshot does not persist per-message timestamps. The
        // shared sessions.json index is not a message-time source or a log.
        created_at: None,
        content,
        extra: json!({"message": saved_message, "cass": cass}),
        snippets: Vec::new(),
        invocations,
    };
    Ok(Some(StreamedHistoryEntry {
        primary: Some(primary),
        primary_requires_tool_results: !has_primary_body,
        tool_results,
        history_index,
    }))
}

struct SavedSessionSeed<'a> {
    callback_error: &'a mut Option<anyhow::Error>,
}

impl<'de> DeserializeSeed<'de> for SavedSessionSeed<'_> {
    type Value = StreamedSession;

    #[allow(clippy::too_many_lines)]
    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SessionVisitor<'a> {
            callback_error: &'a mut Option<anyhow::Error>,
        }

        impl<'de> Visitor<'de> for SessionVisitor<'_> {
            type Value = StreamedSession;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("struct SavedSession")
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut session_id: Option<String> = None;
                let mut title: Option<String> = None;
                let mut workspace_directory: Option<String> = None;
                let mut saw_history = false;
                let mut mode: Option<Option<String>> = None;
                let mut chat_model_title: Option<Option<String>> = None;
                let mut usage: Option<Value> = None;
                let mut version: Option<Option<Value>> = None;
                let mut entries = Vec::new();
                let mut native_results = HashSet::new();

                while let Some(key) = map.next_key::<SavedSessionField>()? {
                    match key {
                        SavedSessionField::SessionId => {
                            if session_id.is_some() {
                                return Err(serde::de::Error::duplicate_field("sessionId"));
                            }
                            session_id = Some(map.next_value()?);
                        }
                        SavedSessionField::Title => {
                            if title.is_some() {
                                return Err(serde::de::Error::duplicate_field("title"));
                            }
                            title = Some(map.next_value()?);
                        }
                        SavedSessionField::WorkspaceDirectory => {
                            if workspace_directory.is_some() {
                                return Err(serde::de::Error::duplicate_field(
                                    "workspaceDirectory",
                                ));
                            }
                            workspace_directory = Some(map.next_value()?);
                        }
                        SavedSessionField::History => {
                            if saw_history {
                                return Err(serde::de::Error::duplicate_field("history"));
                            }
                            saw_history = true;
                            let mut history_index = 0usize;
                            let mut on_item = |item: HistoryItem| -> Result<()> {
                                let idx = history_index;
                                history_index += 1;
                                if let Some(entry) =
                                    convert_history_item(idx, item, &mut native_results)?
                                {
                                    entries.push(entry);
                                }
                                Ok(())
                            };
                            map.next_value_seed(SeqStreamSeed::<HistoryItem, _>::new(
                                &mut on_item,
                                self.callback_error,
                            ))?;
                        }
                        SavedSessionField::Mode => {
                            if mode.is_some() {
                                return Err(serde::de::Error::duplicate_field("mode"));
                            }
                            mode = Some(map.next_value()?);
                        }
                        SavedSessionField::ChatModelTitle => {
                            if chat_model_title.is_some() {
                                return Err(serde::de::Error::duplicate_field("chatModelTitle"));
                            }
                            chat_model_title = Some(map.next_value()?);
                        }
                        SavedSessionField::Usage => {
                            if usage.is_some() {
                                return Err(serde::de::Error::duplicate_field("usage"));
                            }
                            usage = Some(map.next_value_seed(ReadUsageSeed)?);
                        }
                        SavedSessionField::Version => {
                            if version.is_some() {
                                return Err(serde::de::Error::duplicate_field("version"));
                            }
                            version = Some(map.next_value()?);
                        }
                        SavedSessionField::Ignore => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }

                let session_id =
                    session_id.ok_or_else(|| serde::de::Error::missing_field("sessionId"))?;
                let title = title.ok_or_else(|| serde::de::Error::missing_field("title"))?;
                let workspace_directory = workspace_directory
                    .ok_or_else(|| serde::de::Error::missing_field("workspaceDirectory"))?;
                if !saw_history {
                    return Err(serde::de::Error::missing_field("history"));
                }

                Ok(StreamedSession {
                    session_id,
                    title,
                    workspace_directory,
                    entries,
                    native_results,
                    mode: mode.flatten(),
                    chat_model_title: chat_model_title.flatten(),
                    usage: usage.unwrap_or(Value::Null),
                    version: version.flatten(),
                })
            }
        }

        deserializer.deserialize_map(SessionVisitor {
            callback_error: self.callback_error,
        })
    }
}

fn parse_session_reader(path: &Path, reader: impl Read) -> Result<Option<NormalizedConversation>> {
    let (kind, stream_reader) =
        peek_json_container(BufReader::new(reader)).context("read Continue snapshot")?;
    if kind != JsonContainerKind::Object {
        bail!("read Continue snapshot: expected JSON object");
    }
    let mut deserializer = serde_json::Deserializer::from_reader(stream_reader);
    let mut callback_error = None;
    let session_result = SavedSessionSeed {
        callback_error: &mut callback_error,
    }
    .deserialize(&mut deserializer);
    if let Some(err) = callback_error {
        return Err(err);
    }
    let session = session_result.context("read Continue snapshot")?;
    deserializer.end().context("read Continue snapshot")?;

    if session.version.is_some() {
        bail!("unsupported versioned Continue snapshot");
    }
    if session.session_id.trim().is_empty()
        || path.file_stem().and_then(|name| name.to_str()) != Some(session.session_id.as_str())
    {
        bail!("Continue session identity does not match its file");
    }

    let mut messages = Vec::new();
    for entry in session.entries {
        let surviving_results: Vec<_> = entry
            .tool_results
            .into_iter()
            .filter(|(id, _, _)| {
                !id.as_ref()
                    .is_some_and(|id| session.native_results.contains(id))
            })
            .collect();
        if let Some(mut primary) = entry.primary
            && (!entry.primary_requires_tool_results || !surviving_results.is_empty())
        {
            primary.idx = i64::try_from(messages.len()).unwrap_or(i64::MAX);
            messages.push(primary);
        }
        for (id, status, output) in surviving_results {
            push_message(
                &mut messages,
                "tool",
                output,
                json!({
                    "message": {"role": "tool", "toolCallId": id},
                    "cass": {
                        "source_format": "continue_session",
                        "history_index": entry.history_index,
                        "from_tool_state": true,
                        "tool_status": status
                    }
                }),
                Vec::new(),
            );
        }
    }
    if messages.is_empty() {
        return Ok(None);
    }
    let mut metadata = json!({"source_format": "continue_session"});
    if let Some(mode) = session.mode {
        metadata["mode"] = json!(mode);
    }
    if let Some(model) = session.chat_model_title {
        metadata["chat_model_title"] = json!(model);
    }
    // Session totals include calls whose history may have been compacted or
    // pruned. Keep them separate; never copy them onto countable messages.
    if !session.usage.is_null() {
        metadata["session_usage"] = session.usage;
    }
    Ok(Some(NormalizedConversation {
        agent_slug: "continue".into(),
        external_id: Some(session.session_id),
        title: (!session.title.trim().is_empty()).then_some(session.title),
        workspace: (!session.workspace_directory.trim().is_empty())
            .then(|| PathBuf::from(session.workspace_directory)),
        source_path: path.to_path_buf(),
        started_at: None,
        ended_at: None,
        metadata,
        messages,
    }))
}

/// Read per-reply Continue usage in either the IDE camel-case form or CLI's
/// saved OpenAI-style form. Cache-bearing input is deliberately left in
/// metadata: the legacy Anthropic adapter writes uncached `promptTokens`,
/// while other adapters use cache-inclusive prompt totals. The output
/// counter is comparable across both forms, and uncached input is unambiguous.
pub(crate) fn extract_tokens(extra: &Value, role: &str) -> ExtractedTokenUsage {
    let tool_call_count = if role == "assistant" {
        u32::try_from(
            extra
                .pointer("/cass/tool_call_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
        .unwrap_or(u32::MAX)
    } else {
        0
    };
    let mut extracted = ExtractedTokenUsage {
        tool_call_count,
        has_tool_calls: tool_call_count > 0,
        ..Default::default()
    };
    if role != "assistant"
        || extra.pointer("/message/role").and_then(Value::as_str) != Some("assistant")
    {
        return extracted;
    }
    let Some(usage) = extra.pointer("/message/usage") else {
        return extracted;
    };
    let count = |key: &str| usage.get(key).and_then(Value::as_i64).filter(|n| *n >= 0);
    // An invalid present native key must not fall through to a different
    // schema's alias. No per-session usage fallback is permitted.
    let snake = usage.get("prompt_tokens").is_some() || usage.get("completion_tokens").is_some();
    let (input, output) = if snake {
        ("prompt_tokens", "completion_tokens")
    } else {
        ("promptTokens", "completionTokens")
    };
    let cache_unambiguous = ["promptTokensDetails", "prompt_tokens_details"]
        .iter()
        .all(|key| {
            usage.get(key).is_none_or(|details| {
                details.as_object().is_some_and(|details| {
                    details
                        .iter()
                        .filter(|(key, _)| !key.to_ascii_lowercase().contains("audio"))
                        .all(|(_, value)| value.as_i64() == Some(0))
                })
            })
        });
    extracted.input_tokens = cache_unambiguous.then(|| count(input)).flatten();
    extracted.output_tokens = count(output);
    extracted.model_name = usage
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    extracted.provider = extracted
        .model_name
        .as_deref()
        .map(|name| normalize_model(name).provider);
    if extracted.input_tokens.is_some() || extracted.output_tokens.is_some() {
        extracted.data_source = TokenDataSource::Api;
    }
    extracted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn continue_read_error_after_valid_json_is_not_success() {
        struct FailingReader {
            bytes: io::Cursor<Vec<u8>>,
        }
        impl Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let count = self.bytes.read(buf)?;
                if count == 0 {
                    Err(io::Error::other("fixture read failure"))
                } else {
                    Ok(count)
                }
            }
        }
        let bytes = br#"{"sessionId":"id","title":"Title","workspaceDirectory":"/work","history":[{"message":{"role":"user","content":"hello"}}]}"#.to_vec();
        let result = parse_session_reader(
            Path::new("id.json"),
            FailingReader {
                bytes: io::Cursor::new(bytes),
            },
        );
        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("fixture read failure"));
    }

    #[test]
    fn json_stream_handles_bom_empty_multi_and_truncated_history() {
        // 1. Empty history returns Ok(None).
        let empty = parse_session_reader(
            Path::new("empty.json"),
            io::Cursor::new(
                br#"{"sessionId":"empty","title":"Empty","workspaceDirectory":"/work","history":[]}"#,
            ),
        )
        .unwrap();
        assert!(empty.is_none());

        // 2. Leading UTF-8 BOM + whitespace + forward-referenced native tool row
        //    suppressing synthetic tool_call_states output (and omitting an
        //    otherwise-empty assistant item whose only output was suppressed).
        let mut bom_multi = Vec::from(b"\xEF\xBB\xBF \r\n\t".as_slice());
        bom_multi.extend_from_slice(
            br#"{
                "sessionId": "bom-id",
                "title": "BOM Continue",
                "workspaceDirectory": "/work/proj",
                "history": [
                    {"message": {"role": "user", "content": "Run tool"}},
                    {
                        "message": {"role": "assistant", "content": ""},
                        "toolCallStates": [{
                            "toolCallId": "call-1",
                            "toolCall": {"id": "call-1", "type": "function", "function": {"name": "read", "arguments": "{}"}},
                            "status": "done",
                            "output": [{"content": "stale"}]
                        }]
                    },
                    {"message": {"role": "tool", "toolCallId": "call-1", "content": "native tool output"}}
                ]
            }"#,
        );
        let conv = parse_session_reader(Path::new("bom-id.json"), io::Cursor::new(&bom_multi))
            .unwrap()
            .unwrap();
        assert_eq!(conv.external_id.as_deref(), Some("bom-id"));
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages[0].idx, 0);
        assert_eq!(conv.messages[0].content, "Run tool");
        assert_eq!(conv.messages[1].idx, 1);
        assert_eq!(conv.messages[1].role, "assistant");
        assert_eq!(conv.messages[1].content, "[Tool: read]");
        assert_eq!(conv.messages[2].idx, 2);
        assert_eq!(conv.messages[2].role, "tool");
        assert_eq!(conv.messages[2].content, "native tool output");

        // 3. Top-level array dispatch returns Err cleanly.
        assert!(
            parse_session_reader(
                Path::new("arr.json"),
                io::Cursor::new(br#"[{"message":{"role":"user","content":"x"}}]"#),
            )
            .is_err()
        );

        // 4. Truncated mid-array history returns Err (never silently truncating).
        assert!(
            parse_session_reader(
                Path::new("trunc.json"),
                io::Cursor::new(
                    br#"{"sessionId":"trunc","title":"T","workspaceDirectory":"/w","history":[{"message":{"role":"user","content":"ok"}},{"message":{"role":"user""#,
                ),
            )
            .is_err()
        );
    }
}
