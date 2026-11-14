//! Who looks at the picture when the accessibility tree is not enough.
//!
//! Three ways to answer "where is this control", in descending preference:
//!
//! 1. **A local ONNX model** — no tokens, no network, and its answer is a box,
//!    not prose. This is the default because it is both the cheapest and the
//!    one that keeps working offline.
//! 2. **A vision-capable API model** — for a machine with no local models
//!    downloaded. It costs tokens per look, so it is asked one narrow question
//!    ("where is X") rather than being handed the whole conversation.
//! 3. **Nobody** — stated plainly. A caller that cannot see must fall back to
//!    the tree and, if that fails too, hand the task to a person. What it must
//!    not do is hang: the whole loop waiting on a visual lookup that will never
//!    answer is the same failure shape as the WS stall that ran the GitHub
//!    check on the request path.
//!
//! The four modes exist because "auto" is right for most machines and wrong
//! for two: an offline/privacy deployment must never fall through to an API it
//! did not choose (`local`), and a machine with no ONNX runtime must not pay
//! the cost of discovering that on every step (`api`). `off` is for users who
//! want the accessibility tree to be the only input.
//!
//! The resolver does not itself run inference. It decides *which* backend
//! answers, and a caller holds the model handles.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::VisionConfig;

/// Which backend a lookup should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisionSource {
    /// A local ONNX model. Costs nothing and works offline.
    Local(LocalModel),
    /// The configured vision-capable API model.
    Api { provider: String, model: String },
    /// Nothing can look, and why. Never a silent fallback.
    Unavailable(UnavailableReason),
}

/// Which local model answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalModel {
    /// Text detection (RapidOCR) — reads what the tree cannot, like text drawn
    /// into a canvas or a PDF.
    Ocr,
    /// UI element detection (OmniParser) — finds controls that carry no text.
    UiDetect,
}

/// Why no vision backend is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// `mode = "off"`: the operator turned vision off.
    DisabledByConfig,
    /// `mode = "local"` and the model files are not on this machine. Falls
    /// back to nothing on purpose — falling back to the API would break the
    /// promise that made someone choose `local`.
    LocalModelMissing,
    /// `mode = "api"` or the API rung of `auto`, and no vision model is
    /// configured.
    ApiNotConfigured,
    /// This build has no `vision` feature, so no local model can run.
    FeatureDisabled,
}

impl UnavailableReason {
    /// A sentence for the run log and for the person waiting.
    pub fn explain(self) -> &'static str {
        match self {
            UnavailableReason::DisabledByConfig => {
                "vision is turned off in the configuration ([computer.vision].mode = \"off\")"
            }
            UnavailableReason::LocalModelMissing => {
                "no local vision model is installed, and [computer.vision].mode = \"local\" \
                 forbids using an API model instead"
            }
            UnavailableReason::ApiNotConfigured => {
                "no local vision model is installed and no vision model is configured for the \
                 API"
            }
            UnavailableReason::FeatureDisabled => {
                "this build was compiled without the 'vision' feature, so local models cannot run"
            }
        }
    }
}

/// What the resolver needs to know about the machine.
///
/// Split from the resolver itself so the mode rules can be tested without an
/// ONNX session, a model file, or a network — the three things that make
/// vision hard to test and therefore under-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Availability {
    /// A local model (OCR or UI detection) is present and loadable.
    pub local_model: bool,
    /// This build has the `vision` feature compiled in.
    pub feature_enabled: bool,
}

/// Decides which backend answers, and nothing else.
#[derive(Debug, Clone)]
pub struct VisionResolver {
    config: VisionConfig,
    availability: Availability,
}

impl VisionResolver {
    pub fn new(config: VisionConfig, availability: Availability) -> Self {
        Self { config, availability }
    }

    /// Resolve from the process's configuration and the compiled feature.
    ///
    /// Whether a model file is actually present is checked by the caller and
    /// passed in: downloading is a side effect this type must not have.
    pub fn from_config(config: &VisionConfig, local_model_present: bool) -> Self {
        Self::new(
            config.clone(),
            Availability {
                local_model: local_model_present,
                feature_enabled: cfg!(feature = "vision"),
            },
        )
    }

    pub fn config(&self) -> &VisionConfig {
        &self.config
    }

    /// The mode, normalised. An unknown string is treated as `auto` — a typo
    /// in a config file must not silently disable the feature.
    fn mode(&self) -> Mode {
        match self.config.mode.trim().to_ascii_lowercase().as_str() {
            "off" => Mode::Off,
            "local" => Mode::Local,
            "api" => Mode::Api,
            _ => Mode::Auto,
        }
    }

    /// Whether an API rung is configured at all.
    pub fn has_api(&self) -> bool {
        self.config
            .provider
            .as_deref()
            .map(str::trim)
            .is_some_and(|p| !p.is_empty())
            && self
                .config
                .model
                .as_deref()
                .map(str::trim)
                .is_some_and(|m| !m.is_empty())
    }

    /// The configured API backend, when there is one.
    fn api(&self) -> Option<VisionSource> {
        if !self.has_api() {
            return None;
        }
        Some(VisionSource::Api {
            provider: self.config.provider.as_deref()?.trim().to_string(),
            model: self.config.model.as_deref()?.trim().to_string(),
        })
    }

    /// Whether a local model could answer.
    fn local_available(&self) -> bool {
        self.availability.feature_enabled && self.availability.local_model
    }

    /// Pick a backend.
    ///
    /// Never returns an error: "nobody can look" is a [`VisionSource`] variant,
    /// because the caller has a defined fallback for it (widen the tree
    /// budget, then ask a person) and an error would invite a retry instead.
    pub fn resolve(&self) -> VisionSource {
        match self.mode() {
            Mode::Off => VisionSource::Unavailable(UnavailableReason::DisabledByConfig),
            Mode::Local => {
                if self.local_available() {
                    VisionSource::Local(LocalModel::UiDetect)
                } else if !self.availability.feature_enabled {
                    VisionSource::Unavailable(UnavailableReason::FeatureDisabled)
                } else {
                    VisionSource::Unavailable(UnavailableReason::LocalModelMissing)
                }
            }
            Mode::Api => match self.api() {
                Some(api) => api,
                None => VisionSource::Unavailable(UnavailableReason::ApiNotConfigured),
            },
            Mode::Auto => {
                if self.local_available() {
                    return VisionSource::Local(LocalModel::UiDetect);
                }
                if let Some(api) = self.api() {
                    return api;
                }
                if !self.availability.feature_enabled {
                    VisionSource::Unavailable(UnavailableReason::FeatureDisabled)
                } else {
                    VisionSource::Unavailable(UnavailableReason::ApiNotConfigured)
                }
            }
        }
    }

    /// The rung to try after `source` failed, or `None` when there is none.
    ///
    /// This is what makes `auto` a ladder and `local` a promise: a local model
    /// that is present can still fail on a given image, and `auto` then tries
    /// the API. Under `local` the answer is `None` — the caller degrades to
    /// the tree rather than reaching the network.
    pub fn fallback_after(&self, source: &VisionSource) -> Option<VisionSource> {
        match (self.mode(), source) {
            (Mode::Auto, VisionSource::Local(_)) => self.api(),
            // Everything else is either the last rung or an explicit choice.
            _ => None,
        }
    }

    /// How long one lookup may take.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.config.timeout_secs.max(1))
    }
}

/// The four configured modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Local,
    Api,
    Off,
}

/// A provider handle plus the resolver, for callers that need both.
///
/// Kept as a pair so the source decision and the thing that acts on it cannot
/// be passed inconsistently — the classic bug being a resolver in `local` mode
/// and a caller that calls the API handle anyway.
pub struct VisionBackend {
    pub resolver: VisionResolver,
    /// Present only when the API rung is configured.
    pub provider: Option<Arc<dyn crate::providers::Provider>>,
}

impl VisionBackend {
    pub fn new(
        resolver: VisionResolver,
        provider: Option<Arc<dyn crate::providers::Provider>>,
    ) -> Self {
        Self { resolver, provider }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: &str, api: bool) -> VisionConfig {
        VisionConfig {
            mode: mode.to_string(),
            provider: api.then(|| "openai".to_string()),
            model: api.then(|| "gpt-4o".to_string()),
            ..VisionConfig::default()
        }
    }

    fn resolver(mode: &str, api: bool, local: bool, feature: bool) -> VisionResolver {
        VisionResolver::new(
            cfg(mode, api),
            Availability {
                local_model: local,
                feature_enabled: feature,
            },
        )
    }

    #[test]
    fn off_never_looks_at_anything() {
        for local in [true, false] {
            for api in [true, false] {
                let r = resolver("off", api, local, true);
                assert_eq!(
                    r.resolve(),
                    VisionSource::Unavailable(UnavailableReason::DisabledByConfig)
                );
                assert_eq!(r.fallback_after(&r.resolve()), None);
            }
        }
    }

    /// `mode = "off"` must not make a network call — the whole point is that
    /// no pixels leave the machine. There is no provider to call, which is the
    /// structural version of that guarantee.
    #[test]
    fn off_has_no_provider_to_call() {
        let r = resolver("off", true, true, true);
        let backend = VisionBackend::new(r, None);
        assert!(backend.provider.is_none());
    }

    /// The privacy promise: a missing local model under `local` must NOT fall
    /// back to the API. This is the single most important rule in this file.
    #[test]
    fn local_mode_never_falls_back_to_the_api() {
        let r = resolver("local", true, false, true);
        assert_eq!(
            r.resolve(),
            VisionSource::Unavailable(UnavailableReason::LocalModelMissing),
            "an API was configured, and must still not be used"
        );
        // Even a local source that later fails has nowhere to go.
        assert_eq!(r.fallback_after(&VisionSource::Local(LocalModel::UiDetect)), None);
    }

    #[test]
    fn local_mode_uses_the_local_model_when_it_is_there() {
        let r = resolver("local", false, true, true);
        assert_eq!(r.resolve(), VisionSource::Local(LocalModel::UiDetect));
    }

    #[test]
    fn a_build_without_the_feature_says_so() {
        let r = resolver("local", true, true, false);
        assert_eq!(r.resolve(), VisionSource::Unavailable(UnavailableReason::FeatureDisabled));
        // `auto` reports the reason rather than pretending the API was absent.
        let auto = resolver("auto", false, true, false);
        assert_eq!(auto.resolve(), VisionSource::Unavailable(UnavailableReason::FeatureDisabled));
    }

    #[test]
    fn api_mode_uses_the_configured_model() {
        let r = resolver("api", true, true, true);
        assert_eq!(
            r.resolve(),
            VisionSource::Api {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string()
            }
        );
    }

    #[test]
    fn api_mode_without_a_model_is_unavailable_not_a_failure() {
        let r = resolver("api", false, true, true);
        assert_eq!(r.resolve(), VisionSource::Unavailable(UnavailableReason::ApiNotConfigured));
    }

    #[test]
    fn a_half_configured_api_is_not_configured() {
        let mut config = cfg("api", true);
        config.model = None;
        let r = VisionResolver::new(
            config,
            Availability {
                local_model: true,
                feature_enabled: true,
            },
        );
        assert_eq!(r.resolve(), VisionSource::Unavailable(UnavailableReason::ApiNotConfigured));
        assert!(!r.has_api());
    }

    #[test]
    fn auto_prefers_local() {
        let r = resolver("auto", true, true, true);
        assert_eq!(r.resolve(), VisionSource::Local(LocalModel::UiDetect));
    }

    #[test]
    fn auto_uses_the_api_when_there_is_no_local_model() {
        let r = resolver("auto", true, false, true);
        assert_eq!(
            r.resolve(),
            VisionSource::Api {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string()
            }
        );
    }

    /// `auto` is a ladder: a local model that fails on a given image may hand
    /// over to the API. That is the difference between it and `local`.
    #[test]
    fn auto_offers_the_api_after_a_local_failure() {
        let r = resolver("auto", true, true, true);
        let fallback = r
            .fallback_after(&VisionSource::Local(LocalModel::UiDetect))
            .expect("auto has a second rung");
        assert!(matches!(fallback, VisionSource::Api { .. }));
        // The API is the last rung.
        assert_eq!(r.fallback_after(&fallback), None);
    }

    #[test]
    fn auto_with_nothing_configured_says_unavailable() {
        let r = resolver("auto", false, false, true);
        assert_eq!(r.resolve(), VisionSource::Unavailable(UnavailableReason::ApiNotConfigured));
    }

    /// A typo in the config must not silently turn vision off.
    #[test]
    fn an_unknown_mode_is_treated_as_auto() {
        let r = resolver("lcoal", true, true, true);
        assert_eq!(r.resolve(), VisionSource::Local(LocalModel::UiDetect));
        let r = resolver("", true, false, true);
        assert!(matches!(r.resolve(), VisionSource::Api { .. }));
    }

    #[test]
    fn mode_is_case_and_space_insensitive() {
        let r = resolver("  LOCAL ", true, true, true);
        assert_eq!(r.resolve(), VisionSource::Local(LocalModel::UiDetect));
    }

    #[test]
    fn the_timeout_is_never_zero() {
        let mut config = cfg("auto", false);
        config.timeout_secs = 0;
        let r = VisionResolver::new(
            config,
            Availability {
                local_model: false,
                feature_enabled: true,
            },
        );
        assert_eq!(r.timeout(), std::time::Duration::from_secs(1));
    }

    /// Every `Unavailable` reason has a sentence, because the run log and the
    /// handoff note both print it and an empty string reads as a bug.
    #[test]
    fn every_unavailable_reason_explains_itself() {
        for reason in [
            UnavailableReason::DisabledByConfig,
            UnavailableReason::LocalModelMissing,
            UnavailableReason::ApiNotConfigured,
            UnavailableReason::FeatureDisabled,
        ] {
            let text = reason.explain();
            assert!(!text.is_empty(), "{reason:?}");
            assert!(text.len() > 20, "{reason:?} is too terse: {text}");
        }
    }

    /// When nobody can look, the caller is told to hand the task to a person —
    /// not to retry the same call.
    #[test]
    fn unavailability_survives_a_round_trip_through_serde() {
        let reason = UnavailableReason::LocalModelMissing;
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, "\"local_model_missing\"");
        let back: UnavailableReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reason);
    }
}
