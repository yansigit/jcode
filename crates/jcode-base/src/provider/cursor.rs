//! Cursor pure model-catalog data (compatibility shim).
//!
//! The Cursor provider *runtime* (`CursorCliProvider`) now lives in the
//! downstream `jcode-provider-cursor-runtime` crate so provider edits do not
//! rebuild the base -> app-core -> tui spine. The binary's composition root
//! registers it via [`crate::provider::external`]. Base keeps only the pure
//! model-catalog data that its routing logic (`provider::models`) needs.

// Cursor deprecated `composer-1.5` ("no longer available; use Composer 2.5").
// Default to a model Cursor currently serves; the live catalog overrides this
// whenever it is reachable.
pub const DEFAULT_MODEL: &str = "composer-2.5";

pub const AVAILABLE_MODELS: &[&str] = &[
    "composer-2.5",
    "composer-2-fast",
    "composer-2",
    "gpt-5.4-high",
    "gpt-5.4-medium",
    "gpt-5.4-low",
    "gpt-5",
    "sonnet-4.6",
    "sonnet-4.6-thinking",
    "opus-4.6",
    "gemini-3.1-pro",
];

/// Cursor publishes fast mode and reasoning effort as suffixes on the wire
/// model id rather than as separate request fields. Keep the exact id for the
/// provider, while exposing the two dimensions separately to model pickers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelVariant<'a> {
    pub base_model: &'a str,
    pub effort: Option<&'a str>,
    pub fast: bool,
}

impl ModelVariant<'_> {
    pub fn is_variant(&self) -> bool {
        self.effort.is_some() || self.fast
    }

    pub fn label(&self) -> Option<String> {
        match (self.effort, self.fast) {
            (Some(effort), true) => Some(format!("{effort} · fast")),
            (Some(effort), false) => Some(effort.to_string()),
            (None, true) => Some("fast".to_string()),
            (None, false) => None,
        }
    }
}

pub fn model_variant(model: &str) -> ModelVariant<'_> {
    let model = model.trim();
    let (without_fast, fast) = model
        .strip_suffix("-fast")
        .map_or((model, false), |base| (base, true));
    let effort = [
        "extra-high",
        "extra-low",
        "minimal",
        "medium",
        "xhigh",
        "high",
        "low",
        "none",
        "max",
    ]
    .into_iter()
    .find(|effort| without_fast.ends_with(&format!("-{effort}")));
    let base_model = effort
        .and_then(|effort| without_fast.strip_suffix(&format!("-{effort}")))
        .unwrap_or(without_fast);

    ModelVariant {
        base_model,
        effort,
        fast,
    }
}

pub fn is_known_model(model: &str) -> bool {
    let trimmed = model.trim();
    AVAILABLE_MODELS.contains(&trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_model_variants_split_effort_and_fast_dimensions() {
        assert_eq!(
            model_variant("cursor-grok-4.6-high-fast"),
            ModelVariant {
                base_model: "cursor-grok-4.6",
                effort: Some("high"),
                fast: true,
            }
        );
        assert_eq!(
            model_variant("muse-spark-1.3-minimal"),
            ModelVariant {
                base_model: "muse-spark-1.3",
                effort: Some("minimal"),
                fast: false,
            }
        );
        assert_eq!(
            model_variant("composer-2.5-fast"),
            ModelVariant {
                base_model: "composer-2.5",
                effort: None,
                fast: true,
            }
        );
        assert_eq!(
            model_variant("gpt-5.5-extra-high-fast"),
            ModelVariant {
                base_model: "gpt-5.5",
                effort: Some("extra-high"),
                fast: true,
            }
        );
        assert_eq!(
            model_variant("gemini-3.8-flash"),
            ModelVariant {
                base_model: "gemini-3.8-flash",
                effort: None,
                fast: false,
            }
        );
    }
}
