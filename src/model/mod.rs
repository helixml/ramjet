//! Per-model behaviour behind one trait.
//!
//! Everything that differs between served models lives here: how the prompt
//! formatter is built, which `chat_template_kwargs` the model understands, how
//! `OpenAI` reasoning controls translate into template arguments, and which
//! request classes may be locally tokenized under attestation.
//!
//! Adding a model is a new file in this directory plus one line in [`PROFILES`].
//! Nothing outside this module may match on a model identity.
//!
//! A profile describes only *request rendering*. It is deliberately not a
//! deployment descriptor: image digests, engine flags, and KV-event topics stay
//! in the manifest and the environment, because those vary per deployment of
//! the same model.

mod deepseek_v4;
mod qwen3;

use std::{collections::HashMap, sync::Arc};

use anyhow::Context as _;
use dynamo_renderer::{
    ChatTemplate, ContextMixins, OAIPromptFormatter, PromptContextMixin, PromptFormatter,
    deepseek_formatter_for,
};
use serde_json::{Map, Value};

use crate::tokenizer::LocalFailure;

/// Which content parts a model accepts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Modality {
    /// Text only. A non-text content part is a client error against this model.
    Text,
    /// Text plus non-text parts such as images or video.
    Multimodal,
}

impl Modality {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Multimodal => "multimodal",
        }
    }

    #[must_use]
    pub const fn accepts_non_text_parts(self) -> bool {
        matches!(self, Self::Multimodal)
    }
}

/// How a profile's prompt formatter is constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatterSource {
    /// A formatter compiled into `dynamo-renderer`, selected by model type and
    /// display name.
    Native {
        model_type: &'static str,
        display_name: &'static str,
    },
    /// A `HuggingFace` `tokenizer_config.json` supplying the Jinja chat template.
    /// The file is an attested input: its digest is pinned exactly as the
    /// tokenizer's is, because a template edit silently changes token IDs.
    HfChatTemplate,
}

/// The model-specific half of local request rendering.
pub trait ModelProfile: Send + Sync + 'static {
    /// Stable identifier. Must equal the compatibility manifest's
    /// `renderer.profile` and the value accepted by `RJ_TOKENIZER_PROFILE`.
    fn label(&self) -> &'static str;

    /// Human-facing model family, for logs and metrics only.
    fn family(&self) -> &'static str;

    fn formatter_source(&self) -> FormatterSource;

    fn modality(&self) -> Modality;

    /// `chat_template_kwargs` keys this profile understands. A request carrying
    /// any other key is not locally renderable and falls back to the remote
    /// authority rather than being rendered against a template that ignores it.
    fn template_kwarg_keys(&self) -> &'static [&'static str];

    /// Translates `OpenAI` reasoning controls into this model's template
    /// arguments, in place.
    ///
    /// # Errors
    ///
    /// Returns [`LocalFailure::Unsupported`] for a control this profile cannot
    /// faithfully reproduce, which routes the request to the remote authority.
    fn apply_reasoning(&self, args: &mut HashMap<String, Value>) -> Result<(), LocalFailure>;

    /// Names the attested request class for a reasoning-effort value, or
    /// `None` when this model does not accept that effort.
    fn reasoning_class(&self, effort: &str) -> Option<&'static str>;

    /// Whether thinking can be turned off. Some models reason unconditionally.
    fn thinking_can_be_disabled(&self) -> bool;
}

/// Every profile the binary can serve.
///
/// Adding a model means appending one entry here.
pub static PROFILES: &[&dyn ModelProfile] = &[
    &deepseek_v4::DEEPSEEK_V4_R34,
    &qwen3::QWEN38_27B,
    &qwen3::QWEN38_2_4T_A95B,
];

/// Resolves a profile label to its implementation.
#[must_use]
pub fn profile_for(label: &str) -> Option<&'static dyn ModelProfile> {
    PROFILES
        .iter()
        .copied()
        .find(|profile| profile.label() == label)
}

/// Every known profile label, for error messages and config validation.
#[must_use]
pub fn profile_labels() -> Vec<&'static str> {
    PROFILES.iter().map(|profile| profile.label()).collect()
}

/// Builds the prompt formatter for a profile.
///
/// `chat_template` carries the verified `tokenizer_config.json` bytes and is
/// required exactly when the profile's source is
/// [`FormatterSource::HfChatTemplate`].
///
/// # Errors
///
/// Returns an error when the renderer has no formatter for a native profile, or
/// when a template-driven profile is missing or cannot parse its template.
pub fn build_formatter(
    profile: &dyn ModelProfile,
    chat_template: Option<&str>,
) -> anyhow::Result<Arc<dyn OAIPromptFormatter>> {
    let formatter = match profile.formatter_source() {
        FormatterSource::Native {
            model_type,
            display_name,
        } => deepseek_formatter_for(&Some(model_type.to_owned()), display_name).with_context(
            || {
                format!(
                    "renderer has no native formatter for profile {}",
                    profile.label()
                )
            },
        )?,
        FormatterSource::HfChatTemplate => {
            let raw = chat_template.with_context(|| {
                format!("profile {} requires RJ_CHAT_TEMPLATE_PATH", profile.label())
            })?;
            let config: ChatTemplate = serde_json::from_str(raw).with_context(|| {
                format!(
                    "parse tokenizer_config.json for profile {}",
                    profile.label()
                )
            })?;
            anyhow::ensure!(
                config.chat_template.is_some(),
                "tokenizer_config.json for profile {} declares no chat_template",
                profile.label()
            );
            PromptFormatter::from_parts(
                config,
                ContextMixins::new(&[PromptContextMixin::OaiChat]),
                true,
            )
            .with_context(|| format!("build chat-template formatter for {}", profile.label()))?
        }
    };
    let PromptFormatter::OAI(formatter) = formatter;
    Ok(formatter)
}

/// Content shapes a message can carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentShape {
    /// A plain string, or absent.
    Scalar,
    /// An array whose every part is text-typed.
    TextParts,
    /// An array carrying at least one non-text part.
    NonTextParts,
}

/// Classifies a single message's `content`.
///
/// Shared by the request sanitizer, which must not flatten non-text parts, and
/// by the attested tokenizer, which must not attempt to count image tokens.
#[must_use]
pub fn content_shape(message: &Map<String, Value>) -> ContentShape {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return ContentShape::Scalar;
    };
    if parts.iter().all(is_text_part) {
        ContentShape::TextParts
    } else {
        ContentShape::NonTextParts
    }
}

/// Whether a content part carries only text.
///
/// The test is deliberately conservative in one direction: a part is text only
/// when we can prove it is, because anything misjudged as text gets flattened
/// into a string and loses whatever else it carried.
///
/// - a bare string is text;
/// - an object declaring `type: "text"` is text, even with an absent or empty
///   `text` field — it carries no other payload to lose;
/// - an object with no `type` but a string `text` is text, the shape the Go
///   frontend emitted;
/// - everything else — `image_url`, `video_url`, `input_audio`, and any part
///   type added later — is not, and must reach the engine untouched.
#[must_use]
pub fn is_text_part(part: &Value) -> bool {
    if part.is_string() {
        return true;
    }
    let Some(part) = part.as_object() else {
        return false;
    };
    match part.get("type").and_then(Value::as_str) {
        Some("text") => true,
        Some(_) => false,
        None => part.get("text").is_some_and(Value::is_string),
    }
}

/// Whether any message in the request carries a non-text content part.
#[must_use]
pub fn has_non_text_parts(object: &Map<String, Value>) -> bool {
    object
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message
                    .as_object()
                    .is_some_and(|message| content_shape(message) == ContentShape::NonTextParts)
            })
        })
}

/// Classifies a chat request for attestation against `profile`.
///
/// The generic admission rules live here; only reasoning-effort naming and the
/// `chat_template_kwargs` allowlist are delegated to the profile.
///
/// # Errors
///
/// Returns [`LocalFailure::Unsupported`] when the request uses a feature this
/// path does not reproduce exactly, which defers to the remote authority.
pub fn request_class(
    profile: &dyn ModelProfile,
    object: &Map<String, Value>,
) -> Result<&'static str, LocalFailure> {
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or(LocalFailure::Unsupported)?;
    // A vision request's token count depends on the engine's image
    // preprocessing, which this process deliberately does not reimplement.
    if has_non_text_parts(object) {
        return Err(LocalFailure::Unsupported);
    }
    if messages.is_empty()
        || object
            .get("documents")
            .is_some_and(|value| !value.is_null())
        || has_tool_history(object)
        || !object
            .get("add_generation_prompt")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        || [
            "response_format",
            "tool_choice",
            "function_call",
            "parallel_tool_calls",
            "chat_template",
            "add_special_tokens",
            "truncate_prompt_tokens",
            "think",
            "thinking",
        ]
        .iter()
        .any(|key| object.get(*key).is_some_and(|value| !value.is_null()))
    {
        return Err(LocalFailure::Unsupported);
    }
    let args = object
        .get("chat_template_kwargs")
        .and_then(Value::as_object);
    let allowed = profile.template_kwarg_keys();
    if args.is_some_and(|args| args.keys().any(|key| !allowed.contains(&key.as_str()))) {
        return Err(LocalFailure::Unsupported);
    }
    let top_effort = object.get("reasoning_effort").and_then(Value::as_str);
    let arg_effort = args
        .and_then(|args| args.get("reasoning_effort"))
        .and_then(Value::as_str);
    if top_effort.is_some() && arg_effort.is_some() && top_effort != arg_effort {
        return Err(LocalFailure::Unsupported);
    }
    let effort = top_effort.or(arg_effort);
    let thinking = args
        .and_then(|args| args.get("enable_thinking").or_else(|| args.get("thinking")))
        .and_then(Value::as_bool);
    if thinking == Some(false) && !profile.thinking_can_be_disabled() {
        return Err(LocalFailure::Unsupported);
    }
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if tools {
        let one_user_message =
            messages.len() == 1 && messages[0].get("role").and_then(Value::as_str) == Some("user");
        if !one_user_message || effort.is_some() || thinking.is_some() {
            return Err(LocalFailure::Unsupported);
        }
        return Ok("tools_declared");
    }
    if let Some(effort) = effort {
        return profile
            .reasoning_class(effort)
            .ok_or(LocalFailure::Unsupported);
    }
    if thinking == Some(false) {
        return Ok("thinking_disabled");
    }
    if messages.len() > 1
        || messages.iter().any(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
    {
        return Ok("system_multiturn");
    }
    Ok("plain")
}

/// Whether the request replays prior tool calls or results.
#[must_use]
pub fn has_tool_history(object: &Map<String, Value>) -> bool {
    object
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("tool")
                    || message
                        .get("tool_calls")
                        .is_some_and(|value| !value.is_null())
                    || message
                        .get("function_call")
                        .is_some_and(|value| !value.is_null())
            })
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: &Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn every_registered_profile_has_a_unique_label() {
        let mut labels = profile_labels();
        let total = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), total, "profile labels must be unique");
        assert!(total >= 2, "registry should carry more than one model");
    }

    #[test]
    fn profile_lookup_round_trips_every_label() {
        for label in profile_labels() {
            let profile = profile_for(label).expect("registered label resolves");
            assert_eq!(profile.label(), label);
        }
        assert!(profile_for("no-such-model").is_none());
    }

    #[test]
    fn reasoning_effort_keyword_is_accepted_by_every_profile() {
        // Each profile must admit the effort it declares as its own default,
        // otherwise ordinary traffic can never be attested.
        for profile in PROFILES {
            let mut args = HashMap::new();
            assert!(
                profile.apply_reasoning(&mut args).is_ok(),
                "{} rejects a request carrying no explicit reasoning control",
                profile.label()
            );
        }
    }

    #[test]
    fn text_parts_are_recognized_and_non_text_parts_are_not() {
        assert!(is_text_part(&json!("bare string")));
        assert!(is_text_part(&json!({"type": "text", "text": "hello"})));
        assert!(is_text_part(&json!({"text": "no declared type"})));
        assert!(!is_text_part(&json!({
            "type": "image_url",
            "image_url": {"url": "https://example.invalid/a.png"}
        })));
        assert!(!is_text_part(&json!({
            "type": "video_url",
            "video_url": {"url": "https://example.invalid/a.mp4"}
        })));
        // A text-typed part with no payload is still text: flattening it to the
        // empty string loses nothing. The Go frontend emitted this shape.
        assert!(is_text_part(&json!({"type": "text"})));
        // An untyped part with no text field could carry anything, so it is not
        // assumed to be text.
        assert!(!is_text_part(
            &json!({"image_url": {"url": "https://x.invalid/y.png"}})
        ));
    }

    #[test]
    fn content_shape_separates_scalar_text_and_non_text() {
        assert_eq!(
            content_shape(&object(&json!({"content": "plain"}))),
            ContentShape::Scalar
        );
        assert_eq!(
            content_shape(&object(
                &json!({"content": [{"type": "text", "text": "a"}]})
            )),
            ContentShape::TextParts
        );
        assert_eq!(
            content_shape(&object(&json!({
                "content": [
                    {"type": "text", "text": "a"},
                    {"type": "image_url", "image_url": {"url": "https://x.invalid/y.png"}}
                ]
            }))),
            ContentShape::NonTextParts
        );
    }

    #[test]
    fn a_vision_request_is_never_locally_tokenized() {
        // Counting image tokens is the engine's job. Every profile, including
        // the multimodal ones, must defer rather than guess.
        let request = object(&json!({
            "model": "any",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "https://x.invalid/y.png"}}
            ]}]
        }));
        for profile in PROFILES {
            assert!(
                request_class(*profile, &request).is_err(),
                "{} attempted to locally tokenize a vision request",
                profile.label()
            );
        }
    }

    #[test]
    fn unknown_template_kwargs_defer_to_the_remote_authority() {
        for profile in PROFILES {
            let request = object(&json!({
                "messages": [{"role": "user", "content": "hi"}],
                "chat_template_kwargs": {"some_future_kwarg": true}
            }));
            assert!(
                request_class(*profile, &request).is_err(),
                "{} rendered a template kwarg it does not understand",
                profile.label()
            );
        }
    }
}
