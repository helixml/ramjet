use std::{collections::HashMap, env, time::Instant};

use anyhow::{Context, Result};
use dynamo_protocols::types::CreateChatCompletionRequest;
use dynamo_renderer::dynamo_tokenizers::{FastTokenizer, HuggingFaceTokenizer, traits::Encoder};
use dynamo_renderer::{OAIChatLikeRequest, PromptFormatter, TextInput, deepseek_formatter_for};
use serde_json::{Value, json};

struct Request {
    inner: CreateChatCompletionRequest,
    args: HashMap<String, Value>,
}

impl OAIChatLikeRequest for Request {
    fn model(&self) -> String {
        OAIChatLikeRequest::model(&self.inner)
    }

    fn messages(&self) -> minijinja::Value {
        OAIChatLikeRequest::messages(&self.inner)
    }

    fn typed_messages(&self) -> Option<&[dynamo_protocols::types::ChatCompletionRequestMessage]> {
        Some(self.inner.messages.as_slice())
    }

    fn tools(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::tools(&self.inner)
    }

    fn tool_choice(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::tool_choice(&self.inner)
    }

    fn response_format(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::response_format(&self.inner)
    }

    fn reasoning_effort(&self) -> Option<minijinja::Value> {
        OAIChatLikeRequest::reasoning_effort(&self.inner)
    }

    fn should_add_generation_prompt(&self) -> bool {
        true
    }

    fn chat_template_args(&self) -> Option<&HashMap<String, Value>> {
        Some(&self.args)
    }

    fn extract_text(&self) -> Option<TextInput> {
        Some(TextInput::Single(String::new()))
    }
}

fn main() -> Result<()> {
    let tokenizer_path = env::args()
        .nth(1)
        .context("usage: local_tokenizer_probe TOKENIZER_JSON")?;
    let load_started = Instant::now();
    let hf = HuggingFaceTokenizer::from_file(&tokenizer_path)?;
    let hf_load_ms = load_started.elapsed().as_secs_f64() * 1_000.0;
    let fast_load_started = Instant::now();
    let fast = FastTokenizer::from_file(&tokenizer_path);
    let fast_load_ms = fast_load_started.elapsed().as_secs_f64() * 1_000.0;
    let PromptFormatter::OAI(formatter) =
        deepseek_formatter_for(&Some("deepseek_v4".to_owned()), "deepseek-v4-flash")
            .context("DeepSeek V4 formatter unavailable")?;

    let mut cases = vec![
        (
            "plain",
            json!({
                "model": "deepseek-v4-flash",
                "messages": [{"role": "user", "content": "Explain prefix caching briefly."}]
            }),
        ),
        (
            "system_multiturn",
            json!({
                "model": "deepseek-v4-flash",
                "messages": [
                    {"role": "system", "content": "Answer as a concise systems engineer."},
                    {"role": "user", "content": "Name one cache invariant."},
                    {"role": "assistant", "content": "A cached block must match its token prefix."},
                    {"role": "user", "content": "Now name one recovery invariant."}
                ]
            }),
        ),
        (
            "tools_declared",
            json!({
                "model": "deepseek-v4-flash",
                "messages": [{"role": "user", "content": "Read the deployment status."}],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "deployment_status",
                        "description": "Return deployment health.",
                        "parameters": {
                            "type": "object",
                            "properties": {"node": {"type": "string"}},
                            "required": ["node"]
                        }
                    }
                }]
            }),
        ),
    ];
    for (name, bytes) in [
        ("long_32k", 32 << 10),
        ("long_160k", 160 << 10),
        ("long_640k", 640 << 10),
    ] {
        let unit = "cache routing invariant ";
        let content = unit.repeat(bytes / unit.len() + 1);
        cases.push((
            name,
            json!({
                "model": "deepseek-v4-flash",
                "messages": [{"role": "user", "content": &content[..bytes]}]
            }),
        ));
    }

    for (name, request) in cases {
        probe_case(name, request, formatter.as_ref(), &hf, fast.as_ref().ok())?;
    }
    println!(
        "{}",
        json!({
            "backend": "load",
            "fast_available": fast.is_ok(),
            "fast_load_ms": fast_load_ms,
            "hf_load_ms": hf_load_ms,
        })
    );
    if let Err(error) = fast {
        eprintln!("fastokens unavailable: {error}");
    }
    Ok(())
}

fn probe_case(
    name: &str,
    request: Value,
    formatter: &dyn dynamo_renderer::OAIPromptFormatter,
    hf: &HuggingFaceTokenizer,
    fast: Option<&FastTokenizer>,
) -> Result<()> {
    let request = Request {
        inner: serde_json::from_value(request)?,
        args: HashMap::from([
            ("thinking".to_owned(), Value::Bool(true)),
            (
                "reasoning_effort".to_owned(),
                Value::String("max".to_owned()),
            ),
        ]),
    };
    let render_started = Instant::now();
    let prompt = formatter.render(&request)?;
    let render_us = render_started.elapsed().as_secs_f64() * 1_000_000.0;
    let hf_encoding = hf.encode(&prompt)?;
    let hf_us = median_encode_us(7, || hf.encode(&prompt))?;
    let (fast_matches, fast_us) = if let Some(fast) = fast {
        let encoding = fast.encode(&prompt)?;
        let matches = encoding.token_ids() == hf_encoding.token_ids();
        let elapsed = median_encode_us(7, || fast.encode(&prompt))?;
        (Some(matches), Some(elapsed))
    } else {
        (None, None)
    };
    println!(
        "{}",
        json!({
            "case": name,
            "fast_matches_hf": fast_matches,
            "fast_us": fast_us,
            "hf_us": hf_us,
            "render_us": render_us,
            "tokens": hf_encoding.token_ids().len(),
        })
    );
    Ok(())
}

fn median_encode_us<T, F>(iterations: usize, mut encode: F) -> Result<f64>
where
    F: FnMut() -> Result<T>,
{
    for _ in 0..3 {
        std::hint::black_box(encode()?);
    }
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let result = encode()?;
        samples.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        std::hint::black_box(result);
    }
    samples.sort_by(f64::total_cmp);
    Ok(samples[samples.len() / 2])
}
