use serde_json::{Value, json};

/// Morpheus's optional room-state hint for drop-in voice presentation.
/// Standard Matrix room access and MatrixRTC slot events remain authoritative.
pub const VOICE_SPACE_EVENT_TYPE: &str = "io.github.brk275.morpheus.voice";
pub const VOICE_SPACE_STATE_KEY: &str = "";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceSpaceMarker {
    pub enabled: bool,
}

impl VoiceSpaceMarker {
    pub const ENABLED: Self = Self { enabled: true };

    /// Parse v1 content, allowing extra fields for future extensions.
    pub fn parse_content(content: &Value) -> Result<Self, VoiceSpaceMarkerError> {
        let object = content
            .as_object()
            .ok_or(VoiceSpaceMarkerError::InvalidContent)?;
        let version = object
            .get("v")
            .and_then(Value::as_u64)
            .ok_or(VoiceSpaceMarkerError::InvalidContent)?;
        if version != 1 {
            return Err(VoiceSpaceMarkerError::UnsupportedVersion(version));
        }
        let enabled = object
            .get("enabled")
            .and_then(Value::as_bool)
            .ok_or(VoiceSpaceMarkerError::InvalidContent)?;
        Ok(Self { enabled })
    }

    /// Serialize only the interoperable v1 fields.
    pub fn content(self) -> Value {
        json!({"v": 1, "enabled": self.enabled})
    }
}

/// Return `None` for unrelated state events, and a typed result for this
/// marker. A malformed marker is reported instead of silently enabled.
pub fn parse_voice_space_state_event(
    event_type: &str,
    state_key: &str,
    content: &Value,
) -> Result<Option<VoiceSpaceMarker>, VoiceSpaceMarkerError> {
    if event_type != VOICE_SPACE_EVENT_TYPE || state_key != VOICE_SPACE_STATE_KEY {
        return Ok(None);
    }
    VoiceSpaceMarker::parse_content(content).map(Some)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum VoiceSpaceMarkerError {
    #[error("voice-space marker must contain integer v and boolean enabled")]
    InvalidContent,
    #[error("unsupported voice-space marker version {0}")]
    UnsupportedVersion(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_marker_round_trips_exact_v1_content() {
        let content = VoiceSpaceMarker::ENABLED.content();
        assert_eq!(content, json!({"v": 1, "enabled": true}));
        assert_eq!(
            parse_voice_space_state_event(VOICE_SPACE_EVENT_TYPE, "", &content),
            Ok(Some(VoiceSpaceMarker::ENABLED))
        );
    }

    #[test]
    fn disabled_marker_and_extensions_parse_without_enabling_voice() {
        let content = json!({"v": 1, "enabled": false, "future": "extension"});
        assert_eq!(
            VoiceSpaceMarker::parse_content(&content),
            Ok(VoiceSpaceMarker { enabled: false })
        );
        assert_eq!(
            VoiceSpaceMarker { enabled: false }.content(),
            json!({"v": 1, "enabled": false})
        );
    }

    #[test]
    fn wrong_event_and_state_key_are_not_markers() {
        let content = VoiceSpaceMarker::ENABLED.content();
        assert_eq!(
            parse_voice_space_state_event("m.rtc.slot", "", &content),
            Ok(None)
        );
        assert_eq!(
            parse_voice_space_state_event(VOICE_SPACE_EVENT_TYPE, "other", &content),
            Ok(None)
        );
    }

    #[test]
    fn malformed_or_newer_content_is_reported() {
        assert_eq!(
            VoiceSpaceMarker::parse_content(&json!({"v": 2, "enabled": true})),
            Err(VoiceSpaceMarkerError::UnsupportedVersion(2))
        );
        assert_eq!(
            VoiceSpaceMarker::parse_content(&json!({"v": 1, "enabled": "true"})),
            Err(VoiceSpaceMarkerError::InvalidContent)
        );
    }
}
