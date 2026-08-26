//! The Qwen3.8 family.
//!
//! The registered members share a chat template and reasoning vocabulary and
//! differ only in modality and whether thinking may be turned off, so they are
//! one struct with separate, stable profile labels.
//!
//! Qwen ships no native Rust formatter in this renderer release, so these
//! profiles render through the model's own `tokenizer_config.json`.

use std::collections::HashMap;

use serde_json::Value;

use super::{FormatterSource, Modality, ModelProfile};
use crate::tokenizer::LocalFailure;

/// `Qwen/Qwen3.8-27B` — dense, vision-language, thinking optional.
pub static QWEN38_27B: Qwen38 = Qwen38 {
    label: "qwen3.8-27b",
    modality: Modality::Multimodal,
    thinking_optional: true,
};

/// `Qwen/Qwen3.8-Flash-Next` — sparse `MoE`, vision-language, thinking optional.
///
/// The pinned Flash-Next tokenizer and chat-template artifacts qualified for
/// this repository are byte-identical to the Qwen3.8-27B artifacts, so the two
/// profiles deliberately share rendering semantics. This profile does not
/// attest model weights or serving-runtime behavior; those remain separate
/// manifest and live-engine checks.
pub static QWEN38_FLASH_NEXT: Qwen38 = Qwen38 {
    label: "qwen3.8-flash-next",
    modality: Modality::Multimodal,
    thinking_optional: true,
};

/// `Qwen/Qwen3.8-2.4T-A95B` — sparse `MoE`, text only, thinking mandatory.
pub static QWEN38_2_4T_A95B: Qwen38 = Qwen38 {
    label: "qwen3.8-2.4t-a95b",
    modality: Modality::Text,
    thinking_optional: false,
};

pub struct Qwen38 {
    label: &'static str,
    modality: Modality,
    /// The 27B and Flash-Next accept `enable_thinking: false`; the 2.4T reasons
    /// unconditionally and silently ignores the request to stop.
    thinking_optional: bool,
}

impl ModelProfile for Qwen38 {
    fn label(&self) -> &'static str {
        self.label
    }

    fn family(&self) -> &'static str {
        "qwen3.8"
    }

    fn formatter_source(&self) -> FormatterSource {
        FormatterSource::HfChatTemplate
    }

    fn modality(&self) -> Modality {
        self.modality
    }

    /// `preserve_thinking` carries reasoning across turns and is on by default,
    /// so agent traffic sets it explicitly. Omitting it here would push nearly
    /// all real traffic onto the remote authority.
    fn template_kwarg_keys(&self) -> &'static [&'static str] {
        &[
            "enable_thinking",
            "thinking",
            "reasoning_effort",
            "preserve_thinking",
        ]
    }

    /// Qwen3.8's template consumes `enable_thinking` and `reasoning_effort`
    /// directly, so this normalizes rather than translates.
    ///
    /// An effort outside Qwen's own vocabulary is *not* remapped onto the
    /// nearest neighbour. Rewriting a caller's `high` into `xhigh` would change
    /// the token budget the caller asked for, so those requests defer to the
    /// engine instead.
    fn apply_reasoning(&self, args: &mut HashMap<String, Value>) -> Result<(), LocalFailure> {
        let thinking = args
            .get("enable_thinking")
            .or_else(|| args.get("thinking"))
            .and_then(Value::as_bool);
        if thinking == Some(false) {
            if !self.thinking_optional {
                return Err(LocalFailure::Unsupported);
            }
            args.remove("thinking");
            args.insert("enable_thinking".to_owned(), Value::Bool(false));
            args.remove("reasoning_effort");
            return Ok(());
        }
        args.remove("thinking");
        args.insert("enable_thinking".to_owned(), Value::Bool(true));
        let effort = args
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_EFFORT);
        if !NATIVE_EFFORTS.contains(&effort) {
            return Err(LocalFailure::Unsupported);
        }
        args.insert(
            "reasoning_effort".to_owned(),
            Value::String(effort.to_owned()),
        );
        Ok(())
    }

    fn reasoning_class(&self, effort: &str) -> Option<&'static str> {
        match effort {
            "low" => Some("reasoning_low"),
            "medium" => Some("reasoning_medium"),
            "xhigh" => Some("reasoning_xhigh"),
            _ => None,
        }
    }

    fn thinking_can_be_disabled(&self) -> bool {
        self.thinking_optional
    }
}

/// The effort levels Qwen3.8 documents. `xhigh` is the model's default.
const NATIVE_EFFORTS: &[&str] = &["low", "medium", "xhigh"];
const DEFAULT_EFFORT: &str = "xhigh";

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn args(value: &Value) -> HashMap<String, Value> {
        value
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    #[test]
    fn native_efforts_pass_through_unchanged() {
        for effort in ["low", "medium", "xhigh"] {
            let mut mapped = args(&json!({ "reasoning_effort": effort }));
            QWEN38_27B.apply_reasoning(&mut mapped).unwrap();
            assert_eq!(
                mapped.get("reasoning_effort").and_then(Value::as_str),
                Some(effort)
            );
        }
    }

    #[test]
    fn the_default_effort_is_applied_when_the_caller_omits_one() {
        let mut mapped = args(&json!({}));
        QWEN38_27B.apply_reasoning(&mut mapped).unwrap();
        assert_eq!(
            mapped.get("reasoning_effort").and_then(Value::as_str),
            Some("xhigh"),
            "xhigh is Qwen3.8's documented default"
        );
        assert_eq!(mapped.get("enable_thinking"), Some(&Value::Bool(true)));
    }

    #[test]
    fn foreign_efforts_defer_rather_than_being_remapped() {
        // `high` is a valid OpenAI value with no Qwen equivalent. Silently
        // promoting it to `xhigh` would change the caller's token budget.
        for effort in ["high", "minimal", "none", "max", "auto"] {
            let mut mapped = args(&json!({ "reasoning_effort": effort }));
            assert!(
                QWEN38_27B.apply_reasoning(&mut mapped).is_err(),
                "effort {effort} must defer to the engine, not be remapped"
            );
        }
    }

    #[test]
    fn the_multimodal_members_can_disable_thinking_and_the_2_4t_cannot() {
        for profile in [&QWEN38_27B, &QWEN38_FLASH_NEXT] {
            let mut mapped = args(&json!({ "enable_thinking": false }));
            profile.apply_reasoning(&mut mapped).unwrap();
            assert_eq!(mapped.get("enable_thinking"), Some(&Value::Bool(false)));
            assert!(
                !mapped.contains_key("reasoning_effort"),
                "a non-thinking request carries no effort for {}",
                profile.label()
            );
        }

        let mut mapped = args(&json!({ "enable_thinking": false }));
        assert!(
            QWEN38_2_4T_A95B.apply_reasoning(&mut mapped).is_err(),
            "the 2.4T model reasons unconditionally"
        );
    }

    #[test]
    fn the_thinking_alias_is_normalized_onto_enable_thinking() {
        let mut mapped = args(&json!({ "thinking": true }));
        QWEN38_27B.apply_reasoning(&mut mapped).unwrap();
        assert_eq!(mapped.get("enable_thinking"), Some(&Value::Bool(true)));
        assert!(
            !mapped.contains_key("thinking"),
            "only vLLM's canonical kwarg reaches the template"
        );
    }

    #[test]
    fn preserve_thinking_is_understood_by_every_member() {
        for profile in [&QWEN38_27B, &QWEN38_FLASH_NEXT, &QWEN38_2_4T_A95B] {
            assert!(
                profile.template_kwarg_keys().contains(&"preserve_thinking"),
                "{} would push default agent traffic to the remote authority",
                profile.label()
            );
        }
    }

    #[test]
    fn only_the_vision_language_members_accept_image_parts() {
        assert!(QWEN38_27B.modality().accepts_non_text_parts());
        assert!(QWEN38_FLASH_NEXT.modality().accepts_non_text_parts());
        assert!(!QWEN38_2_4T_A95B.modality().accepts_non_text_parts());
    }

    #[test]
    fn flash_next_reuses_the_27b_rendering_contract() {
        assert_eq!(
            QWEN38_FLASH_NEXT.formatter_source(),
            QWEN38_27B.formatter_source()
        );
        assert_eq!(
            QWEN38_FLASH_NEXT.template_kwarg_keys(),
            QWEN38_27B.template_kwarg_keys()
        );
        for input in [
            json!({}),
            json!({"reasoning_effort": "low"}),
            json!({"reasoning_effort": "medium"}),
            json!({"reasoning_effort": "xhigh"}),
            json!({"enable_thinking": false}),
        ] {
            let mut flash = args(&input);
            let mut dense = args(&input);
            QWEN38_FLASH_NEXT.apply_reasoning(&mut flash).unwrap();
            QWEN38_27B.apply_reasoning(&mut dense).unwrap();
            assert_eq!(flash, dense, "rendering controls diverged for {input}");
        }
    }
}
