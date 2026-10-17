//! Media generation and transcription, as a provider capability.
//!
//! `image_generate`, `tts` and `stt` each grew their own path: a hardcoded
//! `https://api.openai.com/...` endpoint and their own read of `OPENAI_API_KEY`.
//! That means media work ignores the configured provider, its credential and its
//! fallback chain — a second, parallel way of reaching a vendor that happens to
//! be OpenAI.
//!
//! This module is the vocabulary for the fix rather than the fix: a kind, a
//! request, and the three shapes a provider can answer in. The reason it is
//! separate from chat is that media is not a completion — the inputs are images
//! and audio, and the output is bytes that do not go through a message stream.
//!
//! **What is deliberately not decided here.** A provider that answers with
//! bytes (speech) cannot use the shared gateway client's `post_json` /
//! `post_json_text`, which deserialize a response body; whether media gets a
//! binary-response path on that client or talks to its vendor directly is a real
//! choice — the first keeps auth, retry and rate limiting, the second does not —
//! and it belongs to the first implementation, not to this vocabulary.

use serde::{Deserialize, Serialize};

use crate::error::SyscityError;

/// What a media request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// Text to an image.
    ///
    /// The default only exists because [`MediaRequest`] derives `Default`; the
    /// kind is always set explicitly in practice, and an image is the least
    /// surprising placeholder.
    #[default]
    Image,
    /// Text to spoken audio.
    Speech,
    /// Audio to text.
    Transcription,
    /// Text (or an image) to video.
    Video,
}

impl MediaKind {
    /// How to name this kind to a person, for an error that has to say what the
    /// provider cannot do.
    pub fn describe(self) -> &'static str {
        match self {
            MediaKind::Image => "image generation",
            MediaKind::Speech => "speech generation",
            MediaKind::Transcription => "audio transcription",
            MediaKind::Video => "video generation",
        }
    }
}

/// One media request.
///
/// Fields are optional where a kind does not use them: `input` is the audio to
/// transcribe, `voice` and `format` shape speech, and `model` overrides the
/// provider's default. Keeping one shape for all four kinds is what lets the
/// trait carry one method instead of four, and the doc says which field belongs
/// to which kind rather than leaving a caller to guess.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaRequest {
    /// What is being asked for.
    pub kind: MediaKind,
    /// A model override, when the provider offers more than one for this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The prompt: the text to speak, the description of the image.
    pub prompt: String,
    /// The audio to transcribe (`Transcription` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<u8>>,
    /// The voice to speak in (`Speech` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// The container to return (`Speech` only) — `mp3` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The requested image size (`Image` only).
    ///
    /// A vendor hint rather than part of the abstraction: almost every image API
    /// has some notion of it, and the spelling is theirs. A provider that does
    /// not understand it ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// The requested image style (`Image` only) — DALL·E's `vivid`/`natural`.
    ///
    /// A vendor hint for the same reason as [`size`](Self::size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// The spoken language, when the caller knows it (`Transcription` only).
    ///
    /// A hint that improves accuracy rather than a constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Sampling temperature (`Transcription` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Speaking rate (`Speech` only), `1.0` being normal.
    ///
    /// A vendor hint; the range a vendor accepts is its own. This is why
    /// `MediaRequest` is `PartialEq` but not `Eq` — a float has no total
    /// equality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

impl MediaRequest {
    /// A request of one kind, with only the field that kind needs.
    pub fn new(kind: MediaKind, prompt: impl Into<String>) -> Self {
        Self {
            kind,
            prompt: prompt.into(),
            ..Default::default()
        }
    }
}

/// What a provider answered with.
///
/// Three shapes, because the vendors genuinely differ: an image comes back as a
/// URL *or* inline bytes, speech only ever as bytes, and a transcription only as
/// text. Modelling them as one `Vec<u8>` would force a caller to guess which of
/// the three it got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaOutput {
    /// The media itself, with the type a caller should treat it as.
    Bytes {
        /// The bytes.
        data: Vec<u8>,
        /// The MIME type, when the provider said.
        mime: Option<String>,
    },
    /// A URL the provider returned instead of the media.
    Url(String),
    /// Text, which is what a transcription produces.
    Text(String),
}

/// A completed media request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaResponse {
    /// The answer.
    pub output: MediaOutput,
    /// The model that produced it, when the provider reports one.
    pub model: Option<String>,
}

impl MediaResponse {
    /// A response that is bytes.
    pub fn bytes(data: Vec<u8>, mime: Option<String>) -> Self {
        Self {
            output: MediaOutput::Bytes { data, mime },
            model: None,
        }
    }

    /// A response that is a URL.
    pub fn url(url: impl Into<String>) -> Self {
        Self {
            output: MediaOutput::Url(url.into()),
            model: None,
        }
    }

    /// A response that is text.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            output: MediaOutput::Text(text.into()),
            model: None,
        }
    }
}

/// The answer for a provider that does not do media at all.
///
/// A free function rather than a `Provider` default so the message has one
/// wording, and so a provider that leaves the method alone still says something
/// a caller can act on instead of failing obscurely.
pub fn unsupported(provider: &str, kind: MediaKind) -> SyscityError {
    SyscityError::Unsupported(format!("{provider} does not support {}", kind.describe()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_names_itself_for_an_error() {
        assert_eq!(MediaKind::Image.describe(), "image generation");
        assert_eq!(MediaKind::Speech.describe(), "speech generation");
        assert_eq!(MediaKind::Transcription.describe(), "audio transcription");
        assert_eq!(MediaKind::Video.describe(), "video generation");
    }

    #[test]
    fn a_refusal_names_the_provider_and_what_it_cannot_do() {
        let error = unsupported("anthropic", MediaKind::Speech);
        let rendered = error.to_string();
        assert!(rendered.contains("anthropic"), "{rendered}");
        assert!(rendered.contains("speech generation"), "{rendered}");
    }

    /// A request of one kind carries only what that kind needs, so a caller
    /// cannot accidentally send a voice to an image model.
    #[test]
    fn a_request_is_shaped_by_its_kind() {
        let request = MediaRequest::new(MediaKind::Image, "a red fox");
        assert_eq!(request.kind, MediaKind::Image);
        assert_eq!(request.prompt, "a red fox");
        assert!(request.voice.is_none());
        assert!(request.input.is_none());

        // Round-trips through JSON, since a media request may cross the wire.
        let json = serde_json::to_string(&request).expect("serialize");
        let back: MediaRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, request);
        assert!(!json.contains("voice"), "an unset field is not sent: {json}");
    }

    /// The default is what most providers get, so it has to be a usable answer
    /// rather than an empty one — and it must not need every implementation to
    /// opt out.
    #[tokio::test]
    async fn a_provider_that_leaves_the_method_alone_says_so() {
        use crate::providers::Provider;

        let provider = crate::providers::mock::MockProvider::default();
        let error = provider
            .generate_media(MediaRequest::new(MediaKind::Image, "a fox"))
            .await
            .expect_err("the mock provider does not generate media");

        let rendered = error.to_string();
        assert!(rendered.contains(&provider.name().to_string()), "{rendered}");
        assert!(rendered.contains("image generation"), "{rendered}");
    }

    #[test]
    fn a_response_can_be_any_of_the_three_shapes() {
        assert!(matches!(
            MediaResponse::bytes(vec![1, 2, 3], Some("audio/mpeg".to_string())).output,
            MediaOutput::Bytes { .. }
        ));
        assert!(matches!(
            MediaResponse::url("https://example/image.png").output,
            MediaOutput::Url(_)
        ));
        assert!(matches!(MediaResponse::text("hello").output, MediaOutput::Text(_)));
    }
}
