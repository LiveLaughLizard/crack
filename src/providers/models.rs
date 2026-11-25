//! What crack knows about a model: its context window, output limit and prices.
//!
//! Built-in values cover the Claude family (from Anthropic's published table) and
//! sensible guesses by model family elsewhere. Servers that report a context length
//! in their model list override the guess, and `[models."provider/model"]` in the
//! config overrides everything.

use serde::Deserialize;

/// Prices are US dollars per million tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ModelCaps {
    pub context: Option<u64>,
    pub max_output: Option<u32>,
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
    /// Reading cached input; defaults to a tenth of the input price.
    pub cache_read_price: Option<f64>,
    /// Writing the cache; defaults to 1.25× the input price.
    pub cache_write_price: Option<f64>,
}

impl ModelCaps {
    /// Fills unset fields from `base`.
    pub fn or(self, base: ModelCaps) -> ModelCaps {
        ModelCaps {
            context: self.context.or(base.context),
            max_output: self.max_output.or(base.max_output),
            input_price: self.input_price.or(base.input_price),
            output_price: self.output_price.or(base.output_price),
            cache_read_price: self.cache_read_price.or(base.cache_read_price),
            cache_write_price: self.cache_write_price.or(base.cache_write_price),
        }
    }

    /// The cost of one request's usage, when prices are known.
    pub fn cost(&self, usage: &super::Usage) -> Option<f64> {
        let input = self.input_price?;
        let output = self.output_price?;
        let cache_read = self.cache_read_price.unwrap_or(input * 0.1);
        let cache_write = self.cache_write_price.unwrap_or(input * 1.25);
        Some(
            (usage.input_tokens as f64 * input
                + usage.output_tokens as f64 * output
                + usage.cache_read_tokens as f64 * cache_read
                + usage.cache_write_tokens as f64 * cache_write)
                / 1_000_000.0,
        )
    }
}

fn caps(context: u64, max_output: u32, input: f64, output: f64) -> ModelCaps {
    ModelCaps {
        context: Some(context),
        max_output: Some(max_output),
        input_price: Some(input),
        output_price: Some(output),
        ..Default::default()
    }
}

fn window(context: u64) -> ModelCaps {
    ModelCaps {
        context: Some(context),
        ..Default::default()
    }
}

/// Built-in knowledge for a model id (without the provider prefix; Bedrock's
/// `anthropic.` prefix is ignored).
pub fn builtin(model: &str) -> ModelCaps {
    let m = model.to_ascii_lowercase();
    let m = m.trim_start_matches("anthropic.");
    // Claude, per Anthropic's model table (Oct 2026). Opus 5.5 caches reads at $0.20.
    let claude: &[(&str, ModelCaps)] = &[
        ("claude-fable-5-1", caps(1_000_000, 128_000, 10.0, 50.0)),
        ("claude-mythos-5-1", caps(1_000_000, 128_000, 10.0, 50.0)),
        ("claude-fable-5", caps(1_000_000, 128_000, 10.0, 50.0)),
        (
            "claude-opus-5-5",
            ModelCaps {
                cache_read_price: Some(0.20),
                ..caps(1_000_000, 128_000, 4.0, 20.0)
            },
        ),
        ("claude-opus-5", caps(1_000_000, 128_000, 5.0, 25.0)),
        ("claude-opus-4-8", caps(1_000_000, 128_000, 5.0, 25.0)),
        ("claude-opus-4-7", caps(1_000_000, 128_000, 5.0, 25.0)),
        ("claude-opus-4-6", caps(1_000_000, 128_000, 5.0, 25.0)),
        (
            "claude-sonnet-5-5",
            ModelCaps {
                cache_read_price: Some(0.20),
                ..caps(1_000_000, 128_000, 2.0, 10.0)
            },
        ),
        ("claude-sonnet-5", caps(1_000_000, 128_000, 2.0, 10.0)),
        ("claude-sonnet-4-6", caps(1_000_000, 128_000, 3.0, 15.0)),
        ("claude-haiku-5-5", caps(1_000_000, 128_000, 0.10, 0.50)),
        ("claude-haiku-4-5", caps(200_000, 64_000, 1.0, 5.0)),
    ];
    // Longest prefix first, so `claude-opus-5-5` isn't taken for `claude-opus-5`.
    if let Some((_, caps)) = claude
        .iter()
        .filter(|(id, _)| m.starts_with(id))
        .max_by_key(|(id, _)| id.len())
    {
        return *caps;
    }
    // Other families: context windows only (prices change too often to guess).
    let guesses: &[(&str, u64)] = &[
        ("claude", 200_000),
        ("gemini", 1_000_000),
        ("gpt-5", 400_000),
        ("gpt-4.1", 1_000_000),
        ("o3", 200_000),
        ("o4", 200_000),
        ("gpt-4o", 128_000),
        ("gpt-oss", 128_000),
        ("kimi-k3", 1_000_000),
        ("kimi", 256_000),
        ("qwen", 128_000),
        ("glm", 200_000),
        ("minimax", 1_000_000),
        ("deepseek", 128_000),
        ("grok", 256_000),
        ("mistral", 128_000),
        ("codestral", 256_000),
        ("llama", 128_000),
    ];
    guesses
        .iter()
        .find(|(family, _)| m.contains(family))
        .map(|(_, context)| window(*context))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Usage;

    #[test]
    fn lookup_and_cost() {
        assert_eq!(builtin("claude-opus-5-5").input_price, Some(4.0));
        assert_eq!(builtin("claude-opus-5").input_price, Some(5.0));
        assert_eq!(builtin("anthropic.claude-haiku-4-5").context, Some(200_000));
        assert_eq!(builtin("gemini-3-pro").context, Some(1_000_000));
        assert_eq!(builtin("some-local-model").context, None);
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_tokens: 1_000_000,
            cache_write_tokens: 0,
        };
        // 1M in at $4 + 0.1M out at $20 + 1M cached at $0.20.
        let cost = builtin("claude-opus-5-5").cost(&usage).unwrap();
        assert!((cost - 6.2).abs() < 1e-9, "{cost}");
        assert_eq!(builtin("some-local-model").cost(&usage), None);
    }
}
