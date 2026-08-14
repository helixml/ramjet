//! DeepSeek-V4-Flash as served by the node06 vLLM r34 image.

use std::collections::HashMap;

use serde_json::Value;

use super::{FormatterSource, Modality, ModelProfile};
use crate::tokenizer::LocalFailure;

pub static DEEPSEEK_V4_R34: DeepSeekV4R34 = DeepSeekV4R34;

/// The r34 DeepSeek-V4 template profile.
///
/// The renderer ships a native Rust formatter for this family, so no external
/// chat template is needed.
pub struct DeepSeekV4R34;

impl ModelProfile for DeepSeekV4R34 {
    fn label(&self) -> &'static str {
        "deepseek-v4-r34"
    }

    fn family(&self) -> &'static str {
        "deepseek-v4"
    }

    fn formatter_source(&self) -> FormatterSource {
        FormatterSource::Native {
            model_type: "deepseek_v4",
            display_name: "deepseek-v4-flash",
        }
    }

    fn modality(&self) -> Modality {
        Modality::Text
    }

    fn template_kwarg_keys(&self) -> &'static [&'static str] {
        &["enable_thinking", "thinking", "reasoning_effort"]
    }

    /// Translates the r34 template's effort classes into renderer semantics.
    ///
    /// `max` and `xhigh` deliberately fall back to the remote authority: this
    /// renderer release lacks vLLM's newer "Beyond maximum" preamble, so
    /// rendering them locally would produce the wrong token vector.
    fn apply_reasoning(&self, args: &mut HashMap<String, Value>) -> Result<(), LocalFailure> {
        let thinking = args
            .get("enable_thinking")
            .or_else(|| args.get("thinking"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !thinking {
            args.insert("thinking".to_owned(), Value::Bool(false));
            args.insert(
                "reasoning_effort".to_owned(),
                Value::String("none".to_owned()),
            );
            return Ok(());
        }
        let effort = args
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .unwrap_or("high");
        let mapped = match effort {
            "none" | "low" => "none",
            "minimal" | "medium" | "high" | "auto" => "max",
            _ => return Err(LocalFailure::Unsupported),
        };
        args.insert(
            "reasoning_effort".to_owned(),
            Value::String(mapped.to_owned()),
        );
        Ok(())
    }

    fn reasoning_class(&self, effort: &str) -> Option<&'static str> {
        match effort {
            "high" => Some("reasoning_high"),
            "none" => Some("reasoning_none"),
            "minimal" => Some("reasoning_minimal"),
            "low" => Some("reasoning_low"),
            "medium" => Some("reasoning_medium"),
            _ => None,
        }
    }

    fn thinking_can_be_disabled(&self) -> bool {
        true
    }
}

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
    fn effort_classes_match_the_observed_r34_template() {
        for (input, expected) in [
            ("low", "none"),
            ("none", "none"),
            ("minimal", "max"),
            ("medium", "max"),
            ("high", "max"),
            ("auto", "max"),
        ] {
            let mut mapped = args(&json!({ "reasoning_effort": input }));
            DEEPSEEK_V4_R34.apply_reasoning(&mut mapped).unwrap();
            assert_eq!(
                mapped.get("reasoning_effort").and_then(Value::as_str),
                Some(expected),
                "effort {input} mapped incorrectly"
            );
        }
    }

    #[test]
    fn beyond_maximum_efforts_defer_to_the_remote_authority() {
        for effort in ["max", "xhigh"] {
            let mut mapped = args(&json!({ "reasoning_effort": effort }));
            assert!(
                DEEPSEEK_V4_R34.apply_reasoning(&mut mapped).is_err(),
                "effort {effort} must not be rendered locally"
            );
        }
    }

    #[test]
    fn disabled_thinking_pins_effort_to_none() {
        let mut mapped = args(&json!({ "enable_thinking": false }));
        DEEPSEEK_V4_R34.apply_reasoning(&mut mapped).unwrap();
        assert_eq!(mapped.get("thinking"), Some(&Value::Bool(false)));
        assert_eq!(
            mapped.get("reasoning_effort").and_then(Value::as_str),
            Some("none")
        );
    }
}
