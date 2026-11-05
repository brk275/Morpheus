// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.

//! RTC transport types for MatrixRTC.
//!
//! This module defines the transport types that can appear in `m.rtc.member` events
//! as specified in MSC4143 and MSC4195.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single RTC transport specification from an m.rtc.member event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RtcTransport {
    /// LiveKit SFU transport as defined in MSC4195.
    LiveKit(LiveKitTransport),
    /// MSC4195 homeserver-mediated authorization for an SFU locator. This URL
    /// receives media connections only, never an account or OpenID token.
    LiveKitCsApi(LiveKitCsApiTransport),
    /// An unsupported or unknown transport type.
    /// Holds the raw transport data for forward compatibility.
    Unsupported(UnsupportedTransport),
}

/// LiveKit-specific transport configuration (MSC4195).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveKitTransport {
    /// URL of the service that issues JWT tokens for connecting to the LiveKit SFU.
    pub livekit_service_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveKitCsApiTransport {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

impl RtcTransport {
    pub fn is_livekit(&self) -> bool {
        matches!(self, Self::LiveKit(_) | Self::LiveKitCsApi(_))
    }

    pub fn livekit_service_url(&self) -> Option<&str> {
        match self {
            Self::LiveKit(service) => Some(&service.livekit_service_url),
            _ => None,
        }
    }
}

/// An unsupported transport type, storing raw data for forward compatibility.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedTransport {
    /// The transport type string, e.g., "webrtc", "p2p", etc.
    pub transport_type: String,
    /// The raw JSON fields for this transport (excluding "type").
    pub extra_fields: BTreeMap<String, serde_json::Value>,
}

/// Raw transport data as it appears in the JSON, before parsing into typed variants.
/// Used for deserialization from SDK events.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawRtcTransport {
    /// The transport type, e.g., "livekit".
    #[serde(rename = "type")]
    pub transport_type: String,
    /// Additional transport-specific fields.
    #[serde(flatten)]
    pub extra_fields: BTreeMap<String, serde_json::Value>,
}

/// The `content.transports` object of an `m.rtc.member` event (MSC4143).
///
/// Replaces the earlier flat `rtc_transports` array: publishing and subscribing
/// are now described separately, so peers can pick a transport that every member
/// can actually receive.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemberTransports {
    /// Transports this member publishes media on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published: Vec<RawRtcTransport>,
    /// Transport types this member is able to subscribe to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub can_subscribe: Vec<String>,
}

impl MemberTransports {
    /// True when neither list carries anything (used to skip serialization).
    pub fn is_empty(&self) -> bool {
        self.published.is_empty() && self.can_subscribe.is_empty()
    }

    /// Builds the object for a member publishing on `transport`, declaring that
    /// it can also subscribe to that transport type.
    pub fn publishing(transport: RawRtcTransport) -> Self {
        Self {
            can_subscribe: vec![transport.transport_type.clone()],
            published: vec![transport],
        }
    }
}

impl RawRtcTransport {
    /// Convert into a typed RtcTransport.
    /// Known transport types are parsed into their specific variants,
    /// while unknown types become UnsupportedTransport.
    pub fn into_typed(self) -> RtcTransport {
        match self.transport_type.as_str() {
            "livekit" => {
                let locator = self.extra_fields.get("url");
                let service = self.extra_fields.get("livekit_service_url");
                let server = self.extra_fields.get("server_name");
                // A present malformed field is not an absent field. In
                // particular, never reinterpret a malformed CSAPI descriptor
                // as an OpenID credential destination.
                if locator.is_some_and(|v| v.as_str().is_none_or(str::is_empty))
                    || service.is_some_and(|v| v.as_str().is_none_or(str::is_empty))
                    || server.is_some_and(|v| v.as_str().is_none_or(|s| s.is_empty() || s.len() > 1024
                        || s.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '#' | '@'))))
                {
                    return RtcTransport::Unsupported(UnsupportedTransport {
                        transport_type: self.transport_type,
                        extra_fields: self.extra_fields,
                    });
                }
                // Both standard fields select the legacy OpenID publisher
                // contract, matching current Element Call. A locator-only
                // descriptor selects homeserver mediation, never direct POST.
                if let Some(url) = service.and_then(|v| v.as_str()) {
                    RtcTransport::LiveKit(LiveKitTransport {
                        livekit_service_url: url.to_owned(),
                    })
                } else if let Some(url) = locator.and_then(|v| v.as_str()) {
                    RtcTransport::LiveKitCsApi(LiveKitCsApiTransport {
                        url: url.to_owned(),
                        server_name: server.and_then(|v| v.as_str()).map(str::to_owned),
                    })
                } else {
                    // LiveKit transport without required field -> unsupported
                    RtcTransport::Unsupported(UnsupportedTransport {
                        transport_type: self.transport_type,
                        extra_fields: self.extra_fields,
                    })
                }
            }
            _ => RtcTransport::Unsupported(UnsupportedTransport {
                transport_type: self.transport_type,
                extra_fields: self.extra_fields,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_locators_never_become_openid_services() {
        for value in [
            json!({"type": "livekit", "url": null, "livekit_service_url": "https://auth.example.org"}),
            json!({"type": "livekit", "url": "", "livekit_service_url": "https://auth.example.org"}),
            json!({"type": "livekit", "url": "wss://sfu.example.org", "livekit_service_url": null}),
        ] {
            let raw: RawRtcTransport = serde_json::from_value(value).unwrap();
            assert!(matches!(raw.into_typed(), RtcTransport::Unsupported(_)));
        }
        let raw: RawRtcTransport = serde_json::from_value(json!({
            "type": "livekit", "livekit_service_url": "https://auth.example.org"
        }))
        .unwrap();
        assert_eq!(
            raw.into_typed(),
            RtcTransport::LiveKit(LiveKitTransport {
                livekit_service_url: "https://auth.example.org".into(),
            })
        );
    }

    #[test]
    fn standard_locator_and_service_keep_distinct_contracts() {
        let raw: RawRtcTransport = serde_json::from_value(json!({
            "type": "livekit", "url": "wss://sfu.example.org"
        })).unwrap();
        assert_eq!(raw.into_typed(), RtcTransport::LiveKitCsApi(LiveKitCsApiTransport {
            url: "wss://sfu.example.org".into(), server_name: None }));
        let raw: RawRtcTransport = serde_json::from_value(json!({
            "type": "livekit", "url": "wss://sfu.example.org", "livekit_service_url": "https://auth.example.org"
        })).unwrap();
        assert!(matches!(raw.into_typed(), RtcTransport::LiveKit(_)));
        let raw: RawRtcTransport = serde_json::from_value(json!({
            "type": "livekit", "url": "wss://sfu.example.org", "server_name": "operator.example.org:8448"
        })).unwrap();
        let typed = raw.into_typed();
        assert_eq!(crate::own_membership::transport_to_json(&typed)["server_name"], "operator.example.org:8448");
        for server in [json!(null), json!(false), json!("https://operator.example.org")] {
            let raw: RawRtcTransport = serde_json::from_value(json!({"type":"livekit", "url":"wss://sfu.example.org", "server_name":server})).unwrap();
            assert!(matches!(raw.into_typed(), RtcTransport::Unsupported(_)));
        }
    }
}
