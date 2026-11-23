//! Per-message token usage extraction from raw connector payloads.

use serde_json::Value;

/// Quality indicator for extracted token data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TokenDataSource {
    /// Actual token counts from API response usage block.
    Api,
    /// Estimated from content character count (~4 chars per token).
    #[default]
    Estimated,
    /// Deliberately no usage and no estimate (e.g. a row whose original usage
    /// is already accounted for elsewhere). Consumers must exclude
    /// these from both API totals and estimated totals.
    Suppressed,
}

impl TokenDataSource {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Estimated => "estimated",
            Self::Suppressed => "suppressed",
        }
    }
}

/// Extracted token usage from a single message's raw data.
#[derive(Debug, Clone, Default)]
pub struct ExtractedTokenUsage {
    pub model_name: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub thinking_tokens: Option<i64>,
    pub service_tier: Option<String>,
    pub has_tool_calls: bool,
    pub tool_call_count: u32,
    pub data_source: TokenDataSource,
}

impl ExtractedTokenUsage {
    /// Compute total tokens from all components.
    #[must_use]
    pub fn total_tokens(&self) -> Option<i64> {
        let mut total: i64 = 0;
        let mut has_any = false;
        for v in [
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_creation_tokens,
        ]
        .into_iter()
        .flatten()
        {
            total = total.saturating_add(v);
            has_any = true;
        }
        if has_any { Some(total) } else { None }
    }

    /// Whether this extraction has any meaningful token data.
    #[must_use]
    pub const fn has_token_data(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_creation_tokens.is_some()
    }
}

/// Normalized model identification.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub family: String,
    pub tier: String,
    pub provider: String,
}

/// Normalize raw model strings into (family, tier, provider).
#[must_use]
pub fn normalize_model(raw: &str) -> ModelInfo {
    let lower = raw.to_lowercase();

    if lower.starts_with("claude") {
        let tier = if lower.contains("opus") {
            "opus"
        } else if lower.contains("sonnet") {
            "sonnet"
        } else if lower.contains("haiku") {
            "haiku"
        } else {
            "unknown"
        };
        return ModelInfo {
            family: "claude".into(),
            tier: tier.into(),
            provider: "anthropic".into(),
        };
    }

    if lower.starts_with("o1") || lower.starts_with("o3") || lower.starts_with("o4") {
        return ModelInfo {
            family: "gpt".into(),
            tier: lower.split('-').next().unwrap_or(&lower).into(),
            provider: "openai".into(),
        };
    }

    if lower.starts_with("gpt") {
        let tier = lower.strip_prefix("gpt-").unwrap_or(&lower).to_string();
        return ModelInfo {
            family: "gpt".into(),
            tier,
            provider: "openai".into(),
        };
    }

    if lower.starts_with("gemini") {
        let tier = if lower.contains("flash") {
            "flash"
        } else if lower.contains("pro") {
            "pro"
        } else if lower.contains("ultra") {
            "ultra"
        } else {
            "unknown"
        };
        return ModelInfo {
            family: "gemini".into(),
            tier: tier.into(),
            provider: "google".into(),
        };
    }

    if lower.starts_with("mistral")
        || lower.starts_with("codestral")
        || lower.starts_with("devstral")
        || lower.starts_with("pixtral")
        || lower.starts_with("ministral")
    {
        return ModelInfo {
            family: "mistral".into(),
            tier: raw.to_string(),
            provider: "mistral".into(),
        };
    }

    if lower.starts_with("grok") {
        return ModelInfo {
            family: "grok".into(),
            tier: raw.to_string(),
            provider: "xai".into(),
        };
    }

    if lower.starts_with("kimi") || lower.starts_with("moonshot") {
        return ModelInfo {
            family: "kimi".into(),
            tier: raw.to_string(),
            provider: "moonshot".into(),
        };
    }

    if lower.starts_with("deepseek") {
        return ModelInfo {
            family: "deepseek".into(),
            tier: raw.to_string(),
            provider: "deepseek".into(),
        };
    }

    ModelInfo {
        family: "unknown".into(),
        tier: raw.to_string(),
        provider: "unknown".into(),
    }
}

/// Extract billed Anthropic `server_tool_use.web_search_requests` count
/// from Claude Code / Qoder message `extra` payloads.
#[must_use]
pub fn extract_web_search_requests(extra: &Value) -> Option<u32> {
    extra
        .pointer("/cass/web_search_requests")
        .or_else(|| extra.pointer("/cass/token_usage/server_tool_use/web_search_requests"))
        .or_else(|| extra.pointer("/cass/token_usage/web_search_requests"))
        .or_else(|| extra.pointer("/message/usage/server_tool_use/web_search_requests"))
        .or_else(|| extra.pointer("/usage/server_tool_use/web_search_requests"))
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
}

/// Extract token usage from a Claude Code message's raw data.
#[must_use]
pub fn extract_claude_code_tokens(extra: &Value) -> ExtractedTokenUsage {
    let model_name = extra
        .pointer("/cass/model")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            extra
                .pointer("/message/model")
                .and_then(|v| v.as_str())
                .map(String::from)
        });

    let provider = model_name
        .as_deref()
        .map(|name| normalize_model(name).provider);

    let compact_usage = extra.pointer("/cass/token_usage");
    let usage_block = compact_usage.or_else(|| extra.pointer("/message/usage"));
    let input_tokens = usage_block
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_i64);
    let output_tokens = usage_block
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_i64);
    let cache_read_tokens = usage_block
        .and_then(|u| {
            u.get("cache_read_tokens")
                .or_else(|| u.get("cache_read_input_tokens"))
        })
        .and_then(Value::as_i64);
    let cache_creation_tokens = usage_block
        .and_then(|u| {
            u.get("cache_creation_tokens")
                .or_else(|| u.get("cache_creation_input_tokens"))
        })
        .and_then(Value::as_i64);
    let service_tier = usage_block
        .and_then(|u| u.get("service_tier"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let has_api_data = input_tokens.is_some()
        || output_tokens.is_some()
        || cache_read_tokens.is_some()
        || cache_creation_tokens.is_some()
        || compact_usage
            .and_then(|usage| usage.get("data_source"))
            .and_then(|value| value.as_str())
            == Some("api");

    let compact_tool_call_count = extra
        .pointer("/cass/tool_call_count")
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok());
    let (has_tool_calls, tool_call_count) = compact_tool_call_count.map_or_else(
        || {
            extra
                .pointer("/message/content")
                .and_then(|v| v.as_array())
                .map_or((false, 0), |content_arr| {
                    let count = content_arr
                        .iter()
                        .filter(|item| {
                            item.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        })
                        .count();
                    let count = u32::try_from(count).unwrap_or(u32::MAX);
                    (count > 0, count)
                })
        },
        |count| (count > 0, count),
    );

    ExtractedTokenUsage {
        model_name,
        provider,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        thinking_tokens: None,
        service_tier,
        has_tool_calls,
        tool_call_count,
        data_source: if has_api_data {
            TokenDataSource::Api
        } else {
            TokenDataSource::Estimated
        },
    }
}

#[derive(Default)]
struct CodexUsageFields {
    input_tokens: Option<i64>,
    uncached_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    thinking_tokens: Option<i64>,
    service_tier: Option<String>,
}

impl CodexUsageFields {
    const fn has_any_counts(&self) -> bool {
        self.input_tokens.is_some()
            || self.uncached_input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_creation_tokens.is_some()
            || self.thinking_tokens.is_some()
    }
}

fn read_nonneg_i64(obj: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| obj.get(*key))
        .and_then(Value::as_i64)
        .filter(|count| *count >= 0)
}

fn read_nonempty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn read_codex_usage_block(block: &Value) -> CodexUsageFields {
    CodexUsageFields {
        input_tokens: read_nonneg_i64(block, &["input_tokens"]),
        uncached_input_tokens: read_nonneg_i64(block, &["uncached_input_tokens"]),
        output_tokens: read_nonneg_i64(block, &["output_tokens", "tokens"]),
        cache_read_tokens: read_nonneg_i64(
            block,
            &[
                "cached_input_tokens",
                "cache_read_input_tokens",
                "cache_read_tokens",
            ],
        ),
        cache_creation_tokens: read_nonneg_i64(
            block,
            &[
                "cache_write_input_tokens",
                "cache_creation_input_tokens",
                "cache_creation_tokens",
            ],
        ),
        thinking_tokens: read_nonneg_i64(
            block,
            &[
                "reasoning_output_tokens",
                "reasoning_tokens",
                "thinking_tokens",
            ],
        ),
        service_tier: read_nonempty_str(block.get("service_tier")),
    }
}

fn codex_tool_call_count(extra: &Value) -> u32 {
    if let Some(count) = extra
        .pointer("/cass/tool_call_count")
        .and_then(Value::as_u64)
    {
        return u32::try_from(count).unwrap_or(u32::MAX);
    }
    let entry_type = extra.get("type").and_then(Value::as_str);
    let payload = extra.get("payload");
    let payload_type = payload.and_then(|p| p.get("type")).and_then(Value::as_str);
    if matches!(
        (entry_type, payload_type),
        (
            Some("response_item"),
            Some("function_call" | "custom_tool_call")
        ) | (Some("event_msg"), Some("tool_call"))
    ) || matches!(entry_type, Some("function_call" | "custom_tool_call"))
    {
        return 1;
    }
    payload
        .and_then(|p| p.get("content"))
        .or_else(|| extra.get("content"))
        .and_then(Value::as_array)
        .map_or(0, |blocks| {
            u32::try_from(
                blocks
                    .iter()
                    .filter(|block| {
                        matches!(
                            block.get("type").and_then(Value::as_str),
                            Some("tool_use" | "function_call" | "tool_call")
                        )
                    })
                    .count(),
            )
            .unwrap_or(u32::MAX)
        })
}

/// Extract token usage from a Codex message's raw data.
///
/// **CRITICAL INVARIANT**: `input_tokens` MUST remain *inclusive* of
/// `cache_read_tokens` and `cache_creation_tokens`. `cass`'s `compute_cost`
/// subtracts `cache_read_tokens + cache_creation_tokens` from `input_tokens`
/// to derive uncached input cost. Codex's native `input_tokens` already
/// includes cached tokens, so we preserve it verbatim when present; when
/// `uncached_input_tokens` is provided separately (or raw `input_tokens` is
/// smaller than `cache_read + cache_creation`), we reconstruct
/// `input_tokens = uncached + cache_read + cache_creation` so
/// `input_tokens >= cache_read + cache_creation` holds.
#[must_use]
pub fn extract_codex_tokens(extra: &Value) -> ExtractedTokenUsage {
    let mut fields = CodexUsageFields::default();
    let mut data_source = TokenDataSource::Estimated;

    if let Some(attached) = extra.pointer("/cass/token_usage") {
        fields = read_codex_usage_block(attached);
        if attached.get("data_source").and_then(Value::as_str) == Some("api")
            || fields.has_any_counts()
        {
            data_source = TokenDataSource::Api;
        }
    }

    if !fields.has_any_counts()
        && extra.get("type").and_then(Value::as_str) == Some("event_msg")
        && let Some(payload) = extra.get("payload")
        && payload.get("type").and_then(Value::as_str) == Some("token_count")
    {
        // Real rollouts nest per-turn usage at info.last_token_usage
        // (info.total_token_usage is cumulative — never attach it).
        let usage_block = payload.pointer("/info/last_token_usage").unwrap_or(payload);
        let fallback = read_codex_usage_block(usage_block);
        if fields.service_tier.is_none() {
            fields.service_tier = fallback.service_tier;
        }
        fields.input_tokens = fallback.input_tokens;
        fields.uncached_input_tokens = fallback.uncached_input_tokens;
        fields.output_tokens = fallback.output_tokens;
        fields.cache_read_tokens = fallback.cache_read_tokens;
        fields.cache_creation_tokens = fallback.cache_creation_tokens;
        fields.thinking_tokens = fallback.thinking_tokens;
        if fields.has_any_counts() {
            data_source = TokenDataSource::Api;
        }
    }

    let cache_sum = fields
        .cache_read_tokens
        .unwrap_or(0)
        .saturating_add(fields.cache_creation_tokens.unwrap_or(0));
    let mut input_tokens = fields.input_tokens;
    if let Some(uncached) = fields.uncached_input_tokens {
        let inclusive = uncached.saturating_add(cache_sum);
        input_tokens = Some(match input_tokens {
            Some(raw) if raw >= cache_sum => raw,
            _ => inclusive,
        });
    }

    let service_tier = fields
        .service_tier
        .or_else(|| read_nonempty_str(extra.pointer("/cass/service_tier")))
        .or_else(|| read_nonempty_str(extra.get("service_tier")))
        .or_else(|| read_nonempty_str(extra.pointer("/payload/service_tier")))
        .or_else(|| read_nonempty_str(extra.pointer("/response/service_tier")));

    let model_name = extra
        .get("model")
        .or_else(|| extra.pointer("/cass/model"))
        .or_else(|| extra.pointer("/response/model"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let provider = model_name
        .as_deref()
        .map(|name| normalize_model(name).provider);
    let tool_call_count = codex_tool_call_count(extra);

    ExtractedTokenUsage {
        model_name,
        provider,
        input_tokens,
        output_tokens: fields.output_tokens,
        cache_read_tokens: fields.cache_read_tokens,
        cache_creation_tokens: fields.cache_creation_tokens,
        thinking_tokens: fields.thinking_tokens,
        service_tier,
        has_tool_calls: tool_call_count > 0,
        tool_call_count,
        data_source,
    }
}

/// Normalize Gemini CLI's per-message `TokensSummary` into disjoint totals.
/// The prompt includes cached input; generated thoughts are additional output.
/// Tool-use prompt tokens are additional input. These semantics are documented
/// at <https://cloud.google.com/vertex-ai/generative-ai/docs/reference/rest/v1/GenerateContentResponse#UsageMetadata>.
fn extract_gemini_tokens(extra: &Value, role: &str) -> ExtractedTokenUsage {
    let model_name = extra
        .get("model")
        .or_else(|| extra.pointer("/cass/model"))
        .or_else(|| extra.pointer("/message/model"))
        .or_else(|| extra.pointer("/modelConfig/modelName"))
        .or_else(|| extra.get("modelType"))
        .or_else(|| extra.get("modelID"))
        .and_then(Value::as_str)
        .map(String::from);
    let provider = model_name
        .as_deref()
        .map(|name| normalize_model(name).provider);
    let tool_call_count = extra
        .pointer("/cass/tool_call_count")
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or_else(|| {
            extra
                .get("toolCalls")
                .and_then(Value::as_array)
                .map_or(0, |calls| {
                    u32::try_from(
                        calls
                            .iter()
                            .filter(|call| {
                                call.get("name")
                                    .and_then(Value::as_str)
                                    .is_some_and(|name| !name.trim().is_empty())
                            })
                            .count(),
                    )
                    .unwrap_or(u32::MAX)
                })
        });
    let mut extracted = ExtractedTokenUsage {
        model_name,
        provider,
        has_tool_calls: tool_call_count > 0,
        tool_call_count,
        ..Default::default()
    };
    if role != "assistant" {
        return extracted;
    }

    // Compaction preserves this same native shape, with numeric scalars only.
    let usage = extra.get("tokens");
    let read = |key| {
        usage
            .and_then(|usage| usage.get(key))
            .and_then(Value::as_i64)
            .filter(|count| *count >= 0)
    };
    let prompt = read("input");
    // An impossible cache subset must not inflate the known prompt or produce
    // negative input. Keep the prompt intact and decline that invalid split.
    let cached = read("cached").filter(|cached| prompt.is_none_or(|prompt| *cached <= prompt));
    let uncached = match (prompt, cached) {
        (Some(prompt), Some(cached)) => prompt.checked_sub(cached),
        (prompt, _) => prompt,
    };
    // Missing components remain missing unless another recorded component is
    // available. Never wrap/saturate an overflowing category into an API count.
    let add = |left: Option<i64>, right: Option<i64>| match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right),
        (left, right) => left.or(right),
    };
    extracted.input_tokens = add(uncached, read("tool"));
    extracted.output_tokens = add(read("output"), read("thoughts"));
    extracted.cache_read_tokens = cached;
    // Informational subset of output_tokens; total_tokens() does not add it.
    extracted.thinking_tokens = read("thoughts");
    if extracted.has_token_data() {
        extracted.data_source = TokenDataSource::Api;
    }
    // `total` alone cannot identify input/output categories. Preserve it in
    // metadata, but do not use it to invent a missing split or residual count.
    extracted
}

fn qwen_tool_call_count(extra: &Value) -> u32 {
    if let Some(count) = extra
        .pointer("/cass/tool_call_count")
        .and_then(Value::as_u64)
    {
        return u32::try_from(count).unwrap_or(u32::MAX);
    }
    let count = extra
        .pointer("/message/parts")
        .and_then(Value::as_array)
        .map_or(0, |parts| {
            parts
                .iter()
                .filter(|part| {
                    part.pointer("/functionCall/name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.trim().is_empty())
                })
                .count()
        });
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Qwen uses Google's usage shape for several providers, including `OpenAI`.
/// Those conversions preserve reasoning inside candidates and can estimate
/// `thoughtsTokenCount`, so thoughts cannot be added unconditionally.
fn extract_qwen_tokens(extra: &Value, role: &str) -> ExtractedTokenUsage {
    let model_name = extra
        .get("model")
        .or_else(|| extra.pointer("/cass/model"))
        .or_else(|| extra.pointer("/message/model"))
        .or_else(|| extra.pointer("/modelConfig/modelName"))
        .or_else(|| extra.get("modelType"))
        .or_else(|| extra.get("modelID"))
        .and_then(Value::as_str)
        .map(String::from);
    let provider = model_name
        .as_deref()
        .map(|name| normalize_model(name).provider);
    let tool_call_count = qwen_tool_call_count(extra);
    let mut extracted = ExtractedTokenUsage {
        model_name,
        provider,
        has_tool_calls: tool_call_count > 0,
        tool_call_count,
        ..Default::default()
    };
    if role != "assistant" {
        return extracted;
    }

    let native = extra
        .get("usageMetadata")
        .or_else(|| extra.pointer("/cass/usage"));
    let usage = native.or_else(|| extra.get("tokens"));
    let value = |native_key, legacy_key| {
        usage.and_then(|usage| {
            usage.get(if native.is_some() {
                native_key
            } else {
                legacy_key
            })
        })
    };
    let read = |native_key, legacy_key| {
        value(native_key, legacy_key)
            .and_then(Value::as_i64)
            .filter(|count| *count >= 0)
    };
    let prompt = read("promptTokenCount", "input");
    let candidates = read("candidatesTokenCount", "output");
    let has_primary_counts = prompt.is_some() || candidates.is_some();
    // OpenAI's total-only conversion materializes auxiliary zeroes despite
    // omitting input/output. Those zeroes do not establish a usable breakdown.
    let cached = read("cachedContentTokenCount", "cached")
        .filter(|cached| has_primary_counts || *cached > 0)
        .filter(|cached| prompt.is_none_or(|prompt| *cached <= prompt));
    let tool =
        read("toolUsePromptTokenCount", "tool").filter(|tool| has_primary_counts || *tool > 0);
    let uncached = match (prompt, cached) {
        (Some(prompt), Some(cached)) => prompt.checked_sub(cached),
        (prompt, _) => prompt,
    };
    extracted.input_tokens = match (uncached, tool) {
        (Some(input), Some(tool)) => input.checked_add(tool),
        (input, tool) => input.or(tool),
    };
    extracted.cache_read_tokens = cached;
    extracted.output_tokens = candidates;

    // Only a complete, consistent recorded total can establish that thoughts
    // are separate. Never infer a residual or guess overlap from model names.
    let tool_for_total = if value("toolUsePromptTokenCount", "tool").is_none() {
        Some(0)
    } else {
        tool
    };
    let base_total = prompt
        .zip(candidates)
        .and_then(|(prompt, candidates)| prompt.checked_add(candidates))
        .zip(tool_for_total)
        .and_then(|(base, tool)| base.checked_add(tool));
    if let (Some(base), Some(thoughts), Some(total)) = (
        base_total,
        read("thoughtsTokenCount", "thoughts"),
        read("totalTokenCount", "total"),
    ) && base != total
        && base.checked_add(thoughts) == Some(total)
    {
        extracted.output_tokens = candidates.and_then(|output| output.checked_add(thoughts));
    }
    // The serialized record lacks provenance for locally estimated thoughts.
    // Leave thinking_tokens unset even when total reconciliation is possible.
    if extracted.has_token_data() {
        extracted.data_source = TokenDataSource::Api;
    }
    extracted
}

/// Estimate tokens from content length for agents that do not provide token data.
#[must_use]
pub fn estimate_tokens_from_content(content: &str, role: &str) -> ExtractedTokenUsage {
    let char_count = i64::try_from(content.len()).unwrap_or(i64::MAX);
    let estimated = char_count / 4;

    let mut usage = ExtractedTokenUsage {
        data_source: TokenDataSource::Estimated,
        ..Default::default()
    };

    if role == "user" {
        usage.input_tokens = Some(estimated);
    } else {
        usage.output_tokens = Some(estimated);
    }

    usage
}

fn extract_opencode_tokens(extra: &Value, role: &str) -> ExtractedTokenUsage {
    let model_name = extra
        .get("modelID")
        .or_else(|| extra.get("model"))
        .or_else(|| extra.pointer("/model/id"))
        .or_else(|| extra.pointer("/model/modelID"))
        .or_else(|| extra.pointer("/cass/model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let provider = extra
        .get("providerID")
        .or_else(|| extra.get("provider"))
        .or_else(|| extra.pointer("/model/providerID"))
        .or_else(|| extra.pointer("/model/provider"))
        .or_else(|| extra.pointer("/cass/provider"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            model_name
                .as_deref()
                .map(|name| normalize_model(name).provider)
        });
    let tool_call_count = u32::try_from(
        extra
            .pointer("/cass/tool_call_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
    .unwrap_or(u32::MAX);
    let mut extracted = ExtractedTokenUsage {
        model_name,
        provider,
        has_tool_calls: tool_call_count > 0,
        tool_call_count,
        ..Default::default()
    };
    if role != "assistant" {
        return extracted;
    }

    if let Some(tokens) = extra
        .get("tokens")
        .or_else(|| extra.get("usage"))
        .filter(|v| v.is_object())
    {
        let read_nonneg = |ptr: &str| {
            tokens
                .pointer(ptr)
                .and_then(Value::as_i64)
                .filter(|v| *v >= 0)
        };
        let input = read_nonneg("/input").or_else(|| read_nonneg("/input_tokens"));
        let output = read_nonneg("/output").or_else(|| read_nonneg("/output_tokens"));
        let reasoning = read_nonneg("/reasoning").or_else(|| read_nonneg("/reasoning_tokens"));
        let cache_read = read_nonneg("/cache/read")
            .or_else(|| read_nonneg("/cacheRead"))
            .or_else(|| read_nonneg("/cache_read_tokens"));
        let cache_write = read_nonneg("/cache/write")
            .or_else(|| read_nonneg("/cacheWrite"))
            .or_else(|| read_nonneg("/cache_creation_tokens"));

        // OpenCode zero-initializes `tokens` on message creation and populates
        // real counts when the step completes; require at least one positive
        // count so unpopulated zero-initialized structs still fall back to
        // content estimation.
        let has_positive_api_count = [input, output, reasoning, cache_read, cache_write]
            .into_iter()
            .flatten()
            .any(|v| v > 0);
        if has_positive_api_count {
            extracted.input_tokens = input;
            extracted.output_tokens = output;
            extracted.thinking_tokens = reasoning;
            extracted.cache_read_tokens = cache_read;
            extracted.cache_creation_tokens = cache_write;
            extracted.data_source = TokenDataSource::Api;
        }
    }
    extracted
}

fn extract_generic_tokens(extra: &Value, role: &str) -> ExtractedTokenUsage {
    let model_name = extra
        .get("model")
        .or_else(|| extra.pointer("/cass/model"))
        .or_else(|| extra.pointer("/message/model"))
        .or_else(|| extra.pointer("/modelConfig/modelName"))
        .or_else(|| extra.pointer("/modelInfo/modelName"))
        .or_else(|| extra.get("modelType"))
        .or_else(|| extra.get("modelID"))
        .or_else(|| extra.get("model_name"))
        .or_else(|| extra.pointer("/_meta/modelId"))
        .or_else(|| extra.pointer("/metadata/model_slug"))
        .or_else(|| extra.pointer("/data/model"))
        .or_else(|| extra.pointer("/payload/model"))
        .or_else(|| extra.pointer("/session_state/model_info/model_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from);
    let provider = extra
        .get("provider")
        .or_else(|| extra.get("provider_name"))
        .or_else(|| extra.pointer("/message/provider"))
        .or_else(|| extra.pointer("/modelInfo/provider"))
        .or_else(|| extra.pointer("/cass/provider"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .or_else(|| {
            model_name
                .as_deref()
                .map(|name| normalize_model(name).provider)
        });
    let tool_call_count = extra
        .pointer("/cass/tool_call_count")
        .and_then(Value::as_u64)
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or_else(|| {
            if extra.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call")
                || (role == "tool" && extra.get("agy_type").is_some())
            {
                1
            } else if extra.get("role").and_then(Value::as_str) == Some("assistant") {
                extra
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map_or(0, |calls| {
                        u32::try_from(
                            calls
                                .iter()
                                .filter(|call| {
                                    call.as_object().is_some_and(|obj| {
                                        matches!(obj.get("type"), None | Some(Value::Null))
                                            || obj.get("type").and_then(Value::as_str)
                                                == Some("function")
                                    }) && call
                                        .pointer("/function/name")
                                        .and_then(Value::as_str)
                                        .is_some_and(|name| !name.trim().is_empty())
                                })
                                .count(),
                        )
                        .unwrap_or(u32::MAX)
                    })
            } else {
                0
            }
        });
    ExtractedTokenUsage {
        model_name,
        provider,
        has_tool_calls: tool_call_count > 0,
        tool_call_count,
        ..Default::default()
    }
}

/// Extract API token usage from a Devin message's raw `extra` payload.
///
/// Reads `extra.metrics` (or `extra.metadata.metrics`, `extra.cass.token_usage`,
/// `extra.token_usage`) for `input_tokens`, `output_tokens`, `cache_read_tokens`,
/// and `cache_creation_tokens`, ignoring JSON `null` values, and prefers
/// `extra.generation_model` over `extra.model`. Falls back to
/// `extract_generic_tokens` when no API metrics are present.
#[must_use]
pub fn extract_devin_tokens(extra: &Value) -> ExtractedTokenUsage {
    let role = extra.get("role").and_then(Value::as_str).unwrap_or("");
    extract_devin_tokens_with_role(extra, role)
}

fn extract_devin_tokens_with_role(extra: &Value, role: &str) -> ExtractedTokenUsage {
    let mut extracted = extract_generic_tokens(extra, role);

    let explicit_provider = extra
        .get("provider")
        .or_else(|| extra.get("provider_name"))
        .or_else(|| extra.pointer("/message/provider"))
        .or_else(|| extra.pointer("/modelInfo/provider"))
        .or_else(|| extra.pointer("/cass/provider"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());

    let generation_model = read_nonempty_str(
        extra
            .get("generation_model")
            .or_else(|| extra.pointer("/metadata/generation_model")),
    );

    if let Some(gen_model) = generation_model {
        if !explicit_provider {
            extracted.provider = Some(normalize_model(&gen_model).provider);
        }
        extracted.model_name = Some(gen_model);
    }

    for metrics in [
        extra.get("metrics"),
        extra.pointer("/metadata/metrics"),
        extra.pointer("/cass/token_usage"),
        extra.get("token_usage"),
    ]
    .into_iter()
    .flatten()
    .filter(|v| v.is_object())
    {
        let input_tokens = read_nonneg_i64(metrics, &["input_tokens"]);
        let output_tokens = read_nonneg_i64(metrics, &["output_tokens"]);
        let cache_read_tokens =
            read_nonneg_i64(metrics, &["cache_read_tokens", "cache_read_input_tokens"]);
        let cache_creation_tokens = read_nonneg_i64(
            metrics,
            &["cache_creation_tokens", "cache_creation_input_tokens"],
        );

        if input_tokens.is_some()
            || output_tokens.is_some()
            || cache_read_tokens.is_some()
            || cache_creation_tokens.is_some()
        {
            extracted.input_tokens = input_tokens;
            extracted.output_tokens = output_tokens;
            extracted.cache_read_tokens = cache_read_tokens;
            extracted.cache_creation_tokens = cache_creation_tokens;
            extracted.data_source = TokenDataSource::Api;
            break;
        }
    }

    extracted
}

/// Extract token usage from a message, dispatching by agent type.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn extract_tokens_for_agent(
    agent_slug: &str,
    extra: &Value,
    content: &str,
    role: &str,
) -> ExtractedTokenUsage {
    let resolved_slug = crate::canonical_connector_slug(agent_slug).unwrap_or(agent_slug);
    let extracted = match resolved_slug {
        "claude_code" | "claude" | "qoder" => extract_claude_code_tokens(extra),
        "codex" => extract_codex_tokens(extra),
        "gemini" => extract_gemini_tokens(extra, role),
        "qwen" => extract_qwen_tokens(extra, role),
        "opencode" => extract_opencode_tokens(extra, role),
        "devin" => extract_devin_tokens_with_role(extra, role),
        "cursor" | "antigravity" | "chatgpt" => extract_generic_tokens(extra, role),
        _ => ExtractedTokenUsage::default(),
    };

    if matches!(extracted.data_source, TokenDataSource::Suppressed) {
        // Explicit suppressed state: no API data AND no content-based
        // estimation fallback.
        return extracted;
    }

    if !extracted.has_token_data() && !content.is_empty() {
        let mut estimated = estimate_tokens_from_content(content, role);
        estimated.model_name = extracted.model_name;
        estimated.provider = extracted.provider;
        estimated.has_tool_calls = extracted.has_tool_calls;
        estimated.tool_call_count = extracted.tool_call_count;
        estimated.service_tier = extracted.service_tier;
        return estimated;
    }

    extracted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_model_claude_opus() {
        let info = normalize_model("claude-opus-4-6");
        assert_eq!(info.family, "claude");
        assert_eq!(info.tier, "opus");
        assert_eq!(info.provider, "anthropic");
    }

    #[test]
    fn normalize_model_claude_sonnet() {
        let info = normalize_model("claude-sonnet-4-5-20250929");
        assert_eq!(info.family, "claude");
        assert_eq!(info.tier, "sonnet");
        assert_eq!(info.provider, "anthropic");
    }

    #[test]
    fn normalize_model_gpt4o() {
        let info = normalize_model("gpt-4o");
        assert_eq!(info.family, "gpt");
        assert_eq!(info.tier, "4o");
        assert_eq!(info.provider, "openai");
    }

    #[test]
    fn normalize_model_o3() {
        let info = normalize_model("o3");
        assert_eq!(info.family, "gpt");
        assert_eq!(info.tier, "o3");
        assert_eq!(info.provider, "openai");
    }

    #[test]
    fn normalize_model_gemini_flash() {
        let info = normalize_model("gemini-2.0-flash");
        assert_eq!(info.family, "gemini");
        assert_eq!(info.tier, "flash");
        assert_eq!(info.provider, "google");
    }

    #[test]
    fn normalize_model_unknown() {
        let info = normalize_model("llama-3-70b");
        assert_eq!(info.family, "unknown");
        assert_eq!(info.provider, "unknown");
    }

    #[test]
    fn extract_claude_code_tokens_full() {
        let raw: Value = serde_json::json!({
            "message": {
                "model": "claude-opus-4-6",
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 500,
                    "cache_read_input_tokens": 20000,
                    "cache_creation_input_tokens": 5000,
                    "service_tier": "standard"
                },
                "content": [
                    {"type": "text", "text": "Hello"},
                    {"type": "tool_use", "name": "Read", "input": {"file_path": "/foo"}}
                ]
            }
        });

        let usage = extract_claude_code_tokens(&raw);
        assert_eq!(usage.model_name.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(usage.provider.as_deref(), Some("anthropic"));
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(500));
        assert_eq!(usage.cache_read_tokens, Some(20000));
        assert_eq!(usage.cache_creation_tokens, Some(5000));
        assert_eq!(usage.service_tier.as_deref(), Some("standard"));
        assert_eq!(usage.data_source, TokenDataSource::Api);
        assert!(usage.has_tool_calls);
        assert_eq!(usage.tool_call_count, 1);
        assert_eq!(usage.total_tokens(), Some(25600));
    }

    #[test]
    fn extract_claude_code_tokens_no_usage() {
        let raw: Value = serde_json::json!({
            "type": "user",
            "content": "Hello"
        });
        let usage = extract_claude_code_tokens(&raw);
        assert!(!usage.has_token_data());
        assert_eq!(usage.data_source, TokenDataSource::Estimated);
    }

    #[test]
    fn extract_codex_tokens_from_attached_token_usage() {
        let raw: Value = serde_json::json!({
            "type": "response_item",
            "payload": {
                "role": "assistant",
                "content": "answer"
            },
            "cass": {
                "token_usage": {
                    "input_tokens": 111,
                    "output_tokens": 222,
                    "data_source": "api"
                }
            }
        });

        let usage = extract_codex_tokens(&raw);
        assert_eq!(usage.input_tokens, Some(111));
        assert_eq!(usage.output_tokens, Some(222));
        assert_eq!(usage.data_source, TokenDataSource::Api);
    }

    #[test]
    fn extract_codex_tokens_from_legacy_event_msg_payload() {
        let raw: Value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "input_tokens": 10,
                "output_tokens": 20
            }
        });

        let usage = extract_codex_tokens(&raw);
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.data_source, TokenDataSource::Api);
    }

    #[test]
    fn extract_claude_code_tokens_from_compact_cass_payload() {
        let raw: Value = serde_json::json!({
            "cass": {
                "model": "claude-opus-4-6",
                "tool_call_count": 2,
                "token_usage": {
                    "input_tokens": 100,
                    "output_tokens": 500,
                    "cache_read_tokens": 20000,
                    "cache_creation_tokens": 5000,
                    "service_tier": "standard",
                    "data_source": "api"
                }
            }
        });

        let usage = extract_claude_code_tokens(&raw);
        assert_eq!(usage.model_name.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(usage.provider.as_deref(), Some("anthropic"));
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(500));
        assert_eq!(usage.cache_read_tokens, Some(20000));
        assert_eq!(usage.cache_creation_tokens, Some(5000));
        assert_eq!(usage.service_tier.as_deref(), Some("standard"));
        assert_eq!(usage.data_source, TokenDataSource::Api);
        assert!(usage.has_tool_calls);
        assert_eq!(usage.tool_call_count, 2);
    }

    #[test]
    fn extract_codex_tokens_model_from_compact_cass_payload() {
        let raw: Value = serde_json::json!({
            "cass": {
                "model": "gpt-5-codex",
                "token_usage": {
                    "input_tokens": 11,
                    "output_tokens": 22,
                    "data_source": "api"
                }
            }
        });

        let usage = extract_codex_tokens(&raw);
        assert_eq!(usage.model_name.as_deref(), Some("gpt-5-codex"));
        assert_eq!(usage.provider.as_deref(), Some("openai"));
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(22));
        assert_eq!(usage.data_source, TokenDataSource::Api);
    }

    #[test]
    fn extract_codex_tokens_legacy_tokens_fallback() {
        let raw: Value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "tokens": 77
            }
        });

        let usage = extract_codex_tokens(&raw);
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, Some(77));
        assert_eq!(usage.data_source, TokenDataSource::Api);
    }

    #[test]
    fn estimate_tokens_user_message() {
        let usage = estimate_tokens_from_content("Hello, this is a test message!", "user");
        assert!(usage.input_tokens.unwrap() > 0);
        assert!(usage.output_tokens.is_none());
        assert_eq!(usage.data_source, TokenDataSource::Estimated);
    }

    #[test]
    fn estimate_tokens_assistant_message() {
        let usage =
            estimate_tokens_from_content("Here is my response to your question.", "assistant");
        assert!(usage.input_tokens.is_none());
        assert!(usage.output_tokens.unwrap() > 0);
        assert_eq!(usage.data_source, TokenDataSource::Estimated);
    }

    #[test]
    fn extract_tokens_for_agent_claude_with_data() {
        let raw: Value = serde_json::json!({
            "message": {
                "model": "claude-sonnet-4-5-20250929",
                "usage": {
                    "input_tokens": 50,
                    "output_tokens": 200
                },
                "content": [{"type": "text", "text": "Response"}]
            }
        });
        let usage = extract_tokens_for_agent("claude_code", &raw, "Response", "assistant");
        assert_eq!(usage.data_source, TokenDataSource::Api);
        assert_eq!(usage.input_tokens, Some(50));
        assert_eq!(usage.output_tokens, Some(200));
    }

    #[test]
    fn extract_tokens_for_agent_unknown_falls_back() {
        let raw = Value::Null;
        let usage =
            extract_tokens_for_agent("unknown_agent", &raw, "Some content here", "assistant");
        assert_eq!(usage.data_source, TokenDataSource::Estimated);
        assert!(usage.output_tokens.unwrap() > 0);
    }

    #[test]
    fn extract_tokens_for_agent_opencode_extracts_api_tokens_and_zero_fallback() {
        let raw_api = serde_json::json!({
            "message_id": "msg_1",
            "session_id": "ses_1",
            "modelID": "claude-sonnet-4-5",
            "providerID": "anthropic",
            "tokens": {
                "input": 420,
                "output": 135,
                "reasoning": 25,
                "cache": {
                    "read": 90,
                    "write": 15
                }
            },
            "cass": {
                "tool_call_count": 2
            }
        });
        let usage = extract_tokens_for_agent("opencode", &raw_api, "Done!", "assistant");
        assert_eq!(usage.data_source, TokenDataSource::Api);
        assert_eq!(usage.model_name.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(usage.provider.as_deref(), Some("anthropic"));
        assert_eq!(usage.input_tokens, Some(420));
        assert_eq!(usage.output_tokens, Some(135));
        assert_eq!(usage.thinking_tokens, Some(25));
        assert_eq!(usage.cache_read_tokens, Some(90));
        assert_eq!(usage.cache_creation_tokens, Some(15));
        assert!(usage.has_tool_calls);
        assert_eq!(usage.tool_call_count, 2);

        let raw_zero = serde_json::json!({
            "message_id": "msg_2",
            "session_id": "ses_1",
            "modelID": "gpt-5",
            "providerID": "openai",
            "tokens": {
                "input": 0,
                "output": 0,
                "reasoning": 0,
                "cache": {
                    "read": 0,
                    "write": 0
                }
            }
        });
        let zero_usage = extract_tokens_for_agent(
            "opencode",
            &raw_zero,
            "Estimated fallback text",
            "assistant",
        );
        assert_eq!(zero_usage.data_source, TokenDataSource::Estimated);
        assert_eq!(zero_usage.model_name.as_deref(), Some("gpt-5"));
        assert_eq!(zero_usage.provider.as_deref(), Some("openai"));
        assert!(zero_usage.output_tokens.unwrap() > 0);
    }

    #[test]
    fn extract_tokens_for_agent_attributes_models_and_tool_calls_across_connectors() {
        for (slug, extra, expected_model, expected_provider, expected_tools) in [
            (
                "chatgpt",
                serde_json::json!({"metadata": {"model_slug": "gpt-4o"}, "cass": {"tool_call_count": 1}}),
                "gpt-4o",
                "openai",
                1_u32,
            ),
            (
                "antigravity",
                serde_json::json!({"model": "gemini-2.5-pro", "cass": {"tool_call_count": 1}}),
                "gemini-2.5-pro",
                "google",
                1_u32,
            ),
            (
                "cursor",
                serde_json::json!({"model": "claude-sonnet-4-5", "cass": {"tool_call_count": 2}}),
                "claude-sonnet-4-5",
                "anthropic",
                2_u32,
            ),
        ] {
            let usage = extract_tokens_for_agent(slug, &extra, "Assistant reply", "assistant");
            assert_eq!(usage.model_name.as_deref(), Some(expected_model), "{slug}");
            assert_eq!(usage.provider.as_deref(), Some(expected_provider), "{slug}");
            assert!(usage.has_tool_calls, "{slug}");
            assert_eq!(usage.tool_call_count, expected_tools, "{slug}");
        }
    }

    #[test]
    fn extract_tokens_for_agent_counts_tool_calls_for_codex_and_gemini() {
        let codex_raw = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "shell",
                "arguments": "{\"cmd\":[\"ls\"]}"
            },
            "cass": {
                "token_usage": {
                    "input_tokens": 100,
                    "output_tokens": 25,
                    "data_source": "api"
                }
            }
        });
        let codex_usage =
            extract_tokens_for_agent("codex", &codex_raw, "[Tool: shell]", "assistant");
        assert_eq!(codex_usage.data_source, TokenDataSource::Api);
        assert_eq!(codex_usage.input_tokens, Some(100));
        assert_eq!(codex_usage.output_tokens, Some(25));
        assert!(codex_usage.has_tool_calls);
        assert_eq!(codex_usage.tool_call_count, 1);

        let gemini_raw = serde_json::json!({
            "model": "gemini-2.5-pro",
            "tokens": {
                "input": 200,
                "output": 80
            },
            "toolCalls": [
                {"name": "read_file", "args": {"path": "src/lib.rs"}},
                {"name": "run_shell_command", "args": {"command": "cargo test"}}
            ]
        });
        let gemini_usage =
            extract_tokens_for_agent("gemini", &gemini_raw, "Running checks", "assistant");
        assert_eq!(gemini_usage.data_source, TokenDataSource::Api);
        assert_eq!(gemini_usage.model_name.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(gemini_usage.provider.as_deref(), Some("google"));
        assert!(gemini_usage.has_tool_calls);
        assert_eq!(gemini_usage.tool_call_count, 2);
    }

    #[test]
    fn extract_devin_tokens_reads_api_metrics_nulls_zero_and_nonzero_cache() {
        let with_cache = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "generation_model": "claude-opus-4-6",
            "metrics": {
                "input_tokens": 1200,
                "output_tokens": 350,
                "cache_read_tokens": 800,
                "cache_creation_tokens": 200
            },
            "cass": {
                "tool_call_count": 2
            }
        });
        let usage = extract_tokens_for_agent("devin", &with_cache, "Answer", "assistant");
        assert_eq!(usage.data_source, TokenDataSource::Api);
        assert_eq!(usage.model_name.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(usage.provider.as_deref(), Some("anthropic"));
        assert_eq!(usage.input_tokens, Some(1200));
        assert_eq!(usage.output_tokens, Some(350));
        assert_eq!(usage.cache_read_tokens, Some(800));
        assert_eq!(usage.cache_creation_tokens, Some(200));
        assert!(usage.has_tool_calls);
        assert_eq!(usage.tool_call_count, 2);

        let with_nulls_and_zeros = serde_json::json!({
            "model": "gpt-4o",
            "metrics": {
                "input_tokens": 500,
                "output_tokens": 125,
                "cache_read_tokens": 0,
                "cache_creation_tokens": null
            }
        });
        let zero_cache =
            extract_tokens_for_agent("devin", &with_nulls_and_zeros, "Answer", "assistant");
        assert_eq!(zero_cache.data_source, TokenDataSource::Api);
        assert_eq!(zero_cache.model_name.as_deref(), Some("gpt-4o"));
        assert_eq!(zero_cache.provider.as_deref(), Some("openai"));
        assert_eq!(zero_cache.input_tokens, Some(500));
        assert_eq!(zero_cache.output_tokens, Some(125));
        assert_eq!(zero_cache.cache_read_tokens, Some(0));
        assert_eq!(zero_cache.cache_creation_tokens, None);

        let via_cass_token_usage = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "cass": {
                "token_usage": {
                    "input_tokens": 42,
                    "output_tokens": 18,
                    "cache_read_tokens": null,
                    "cache_creation_tokens": 0
                }
            }
        });
        let direct = extract_devin_tokens(&via_cass_token_usage);
        assert_eq!(direct.data_source, TokenDataSource::Api);
        assert_eq!(direct.input_tokens, Some(42));
        assert_eq!(direct.output_tokens, Some(18));
        assert_eq!(direct.cache_read_tokens, None);
        assert_eq!(direct.cache_creation_tokens, Some(0));
    }

    #[test]
    fn extract_devin_tokens_falls_back_to_generic_estimated_when_metrics_absent_or_null() {
        let all_null_metrics = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "metrics": {
                "input_tokens": null,
                "output_tokens": null,
                "cache_read_tokens": null,
                "cache_creation_tokens": null
            },
            "cass": {
                "tool_call_count": 1
            }
        });
        let fallback = extract_tokens_for_agent(
            "devin",
            &all_null_metrics,
            "Estimated assistant response text.",
            "assistant",
        );
        assert_eq!(fallback.data_source, TokenDataSource::Estimated);
        assert_eq!(fallback.model_name.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(fallback.provider.as_deref(), Some("anthropic"));
        assert!(fallback.has_tool_calls);
        assert_eq!(fallback.tool_call_count, 1);
        assert!(fallback.output_tokens.unwrap() > 0);
        assert_eq!(fallback.input_tokens, None);
    }

    #[test]
    fn extract_codex_tokens_populates_cache_reasoning_tier_and_keeps_input_inclusive() {
        // Native Codex `last_token_usage` where `input_tokens` already includes cached tokens.
        let native_event = serde_json::json!({
            "type": "event_msg",
            "model": "gpt-5-codex",
            "service_tier": "priority",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 1000,
                        "cached_input_tokens": 600,
                        "cache_write_input_tokens": 150,
                        "output_tokens": 300,
                        "reasoning_output_tokens": 120
                    }
                }
            }
        });
        let usage = extract_codex_tokens(&native_event);
        assert_eq!(usage.data_source, TokenDataSource::Api);
        assert_eq!(usage.input_tokens, Some(1000));
        assert_eq!(usage.output_tokens, Some(300));
        assert_eq!(usage.cache_read_tokens, Some(600));
        assert_eq!(usage.cache_creation_tokens, Some(150));
        assert_eq!(usage.thinking_tokens, Some(120));
        assert_eq!(usage.service_tier.as_deref(), Some("priority"));
        assert!(
            usage.input_tokens.unwrap()
                >= usage.cache_read_tokens.unwrap() + usage.cache_creation_tokens.unwrap()
        );

        // Attached `/cass/token_usage` with Anthropic-style cache keys and tier override.
        let attached_usage = serde_json::json!({
            "service_tier": "default",
            "cass": {
                "model": "gpt-5-codex",
                "service_tier": "flex",
                "token_usage": {
                    "input_tokens": 2000,
                    "output_tokens": 400,
                    "cache_read_input_tokens": 1200,
                    "cache_creation_input_tokens": 300,
                    "reasoning_output_tokens": 80,
                    "service_tier": "scale"
                }
            }
        });
        let attached = extract_codex_tokens(&attached_usage);
        assert_eq!(attached.input_tokens, Some(2000));
        assert_eq!(attached.cache_read_tokens, Some(1200));
        assert_eq!(attached.cache_creation_tokens, Some(300));
        assert_eq!(attached.thinking_tokens, Some(80));
        assert_eq!(attached.service_tier.as_deref(), Some("scale"));

        // Separate `uncached_input_tokens` reconstructs inclusive `input_tokens >= cache_read + cache_creation`.
        let uncached_only = serde_json::json!({
            "cass": {
                "service_tier": "standard",
                "token_usage": {
                    "uncached_input_tokens": 250,
                    "output_tokens": 100,
                    "cached_input_tokens": 600,
                    "cache_write_input_tokens": 150,
                    "reasoning_output_tokens": 40
                }
            }
        });
        let reconstructed = extract_codex_tokens(&uncached_only);
        assert_eq!(reconstructed.input_tokens, Some(1000));
        assert_eq!(reconstructed.cache_read_tokens, Some(600));
        assert_eq!(reconstructed.cache_creation_tokens, Some(150));
        assert_eq!(reconstructed.thinking_tokens, Some(40));
        assert_eq!(reconstructed.service_tier.as_deref(), Some("standard"));
        assert!(
            reconstructed.input_tokens.unwrap()
                >= reconstructed.cache_read_tokens.unwrap()
                    + reconstructed.cache_creation_tokens.unwrap()
        );

        // Pre-stripped `input_tokens` paired with `uncached_input_tokens` also enforces inclusive invariant.
        let pre_stripped = serde_json::json!({
            "cass": {
                "token_usage": {
                    "input_tokens": 250,
                    "uncached_input_tokens": 250,
                    "cached_input_tokens": 600,
                    "cache_write_input_tokens": 150
                }
            }
        });
        let repaired = extract_codex_tokens(&pre_stripped);
        assert_eq!(repaired.input_tokens, Some(1000));
    }

    #[test]
    fn extract_web_search_requests_reads_claude_and_cass_paths() {
        let native = serde_json::json!({
            "message": {
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "server_tool_use": {
                        "web_search_requests": 3
                    }
                }
            }
        });
        assert_eq!(extract_web_search_requests(&native), Some(3));

        let cass_override = serde_json::json!({
            "cass": {
                "web_search_requests": 5
            },
            "message": {
                "usage": {
                    "server_tool_use": {
                        "web_search_requests": 2
                    }
                }
            }
        });
        assert_eq!(extract_web_search_requests(&cass_override), Some(5));

        let zero_count = serde_json::json!({
            "message": {
                "usage": {
                    "server_tool_use": {
                        "web_search_requests": 0
                    }
                }
            }
        });
        assert_eq!(extract_web_search_requests(&zero_count), Some(0));

        let invalid_or_missing = serde_json::json!({
            "message": {
                "usage": {
                    "server_tool_use": {
                        "web_search_requests": null
                    }
                }
            }
        });
        assert_eq!(extract_web_search_requests(&invalid_or_missing), None);
        assert_eq!(
            extract_web_search_requests(&serde_json::json!({"cass": {"web_search_requests": -1}})),
            None
        );
    }
}
