// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.

//! The pure half of the MSC4195 LiveKit control plane.
//!
//! Everything a MatrixRTC client must *compute* to reach a LiveKit SFU, with
//! none of the IO: the hash derivations ([`identity`]), the authorisation
//! service's request/response shapes ([`token`]), and the per-generation
//! dialect choices ([`TokenEndpoint`], [`identity_mapper`]). The native
//! transport (`matrix-rtc-livekit`, which owns libwebrtc and reqwest) builds on
//! this; so does the web binding, where the browser does the fetching and
//! livekit-js does the media — which is why this module must stay free of
//! `livekit`, HTTP clients, and async runtimes, and compile for
//! `wasm32-unknown-unknown`.

pub mod identity;
pub mod token;

pub use token::{MemberClaims, SfuToken, TokenServiceError};

use matrix_rtc_core::compat::{MembershipFormat, room_state};

/// The participant-identity derivation a MatrixRTC generation's authorisation
/// service uses.
///
/// One of the two things `matrix_rtc_core::compat` deliberately cannot own — the
/// other being [`TokenEndpoint`] — because the modern derivation hashes per
/// MSC4195, which is a LiveKit document rather than a Matrix wire format. The
/// `compat` module decides *which generation*; this decides *what that means
/// for an identity*.
///
/// Call it once per call and share the returned `Arc`: it has four uses (the
/// core's encryption manager, the media transport, our own identity, and the
/// key ring — see `matrix_rtc_call_sdk::attach_media`), and they must not
/// skew. That matters more than it looks, because a divergence is not an error
/// but a silence: peers appear in the roster with no media, their keys land
/// under an identity the SFU never assigned, and nothing anywhere logs a
/// problem.
///
/// This is deliberately a plain Rust closure even on the web:
/// `RtcIdentityMapper` is `Send + Sync`, which a JS-backed function can never
/// be, and the derivation is a hash with no reason to cross the boundary.
pub fn identity_mapper(compat: MembershipFormat) -> matrix_rtc_core::RtcIdentityMapper {
    use std::sync::Arc;

    match compat {
        MembershipFormat::Current | MembershipFormat::Sticky2025 => {
            Arc::new(|user, device, member, _memberships| {
                Some(identity::pseudonymous_identity(user, device, member))
            })
        }
        // That generation's authorisation service issues the unhashed
        // `{user}:{device}` string, and has no session component at all.
        MembershipFormat::RoomState => Arc::new(
            |user_id: &str, device_id: &str, _member_id: &str, _memberships| {
                Some(room_state::participant_identity(user_id, device_id))
            },
        ),
    }
}

/// A complete OpenID authorisation and participant-identity contract.
/// This is independent of the Matrix membership carrier. CSAPI `transport.url`
/// is not an OpenID service and is deliberately not represented here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveKitAuthDialect {
    /// Advertised `livekit_service_url`: `/sfu/get`, authenticated user/device.
    /// This service addresses only the standard upper-case room call.
    LegacyService,
    /// Explicit deprecated direct `/get_token` draft: room+slot and triple hash.
    /// Retained for existing custom-slot and draft peers; never auto-tried.
    DirectDraft,
}

impl LiveKitAuthDialect {
    /// Resolve the advertised legacy room flow without collapsing custom slots.
    /// The custom-slot default preserves the existing direct draft API route.
    pub fn for_service_slot(slot_id: &str) -> Self {
        if slot_id == "m.call#ROOM" {
            Self::LegacyService
        } else {
            Self::DirectDraft
        }
    }

    pub fn validate_slot(self, slot_id: &str) -> Result<(), &'static str> {
        if slot_id.is_empty() || (self == Self::LegacyService && slot_id != "m.call#ROOM") {
            return Err("the selected LiveKit auth dialect cannot address this slot");
        }
        Ok(())
    }

    pub fn token_endpoint(self) -> TokenEndpoint {
        match self {
            Self::LegacyService => TokenEndpoint::LegacyElementCall,
            Self::DirectDraft => TokenEndpoint::Msc4195,
        }
    }

    pub fn identity(self, user: &str, device: &str, member: &str) -> Option<String> {
        if user.is_empty() || device.is_empty() || member.is_empty() {
            return None;
        }
        Some(match self {
            Self::LegacyService => room_state::participant_identity(user, device),
            Self::DirectDraft => identity::pseudonymous_identity(user, device, member),
        })
    }
}

/// Which authorisation-service dialect to speak.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TokenEndpoint {
    /// MSC4195 `POST /get_token`.
    #[default]
    Msc4195,
    /// Pre-MSC4195 `POST /sfu/get`, for Element Call builds older than MSC4354.
    /// Temporary; see `matrix_rtc_core::compat`.
    LegacyElementCall,
}

impl TokenEndpoint {
    /// The endpoint a MatrixRTC generation's authorisation service speaks.
    /// `/sfu/get` is also where the pre-sticky generation's unhashed identity
    /// comes from — the endpoint mints the identity — so this and
    /// [`identity_mapper`] are one decision, taken from the same format.
    pub fn for_format(compat: MembershipFormat) -> Self {
        match compat {
            MembershipFormat::RoomState => Self::LegacyElementCall,
            MembershipFormat::Current | MembershipFormat::Sticky2025 => Self::Msc4195,
        }
    }
}

/// Configuration identifying the MatrixRTC slot to connect to.
#[derive(Clone, Debug)]
pub struct LiveKitTransportConfig {
    /// `livekit_service_url` advertised by the transport (the authorisation
    /// service base URL, e.g. `https://matrix-rtc.example.com/livekit/jwt`).
    pub livekit_service_url: String,
    /// Matrix room ID hosting the `m.rtc.member` event.
    pub room_id: String,
    /// MatrixRTC slot ID.
    pub slot_id: String,
    /// `member` claims identifying this membership to the authorisation service.
    pub member: MemberClaims,
    /// Which authorisation-service dialect to speak. Defaults to MSC4195.
    pub token_endpoint: TokenEndpoint,
}

#[cfg(test)]
mod auth_dialect_tests {
    use super::*;

    #[test]
    fn legacy_auth_and_identity_are_one_contract() {
        let dialect = LiveKitAuthDialect::for_service_slot("m.call#ROOM");
        assert_eq!(dialect, LiveKitAuthDialect::LegacyService);
        assert_eq!(dialect.token_endpoint(), TokenEndpoint::LegacyElementCall);
        assert_eq!(
            dialect.identity("@alice:example.org", "DEVICE", "member"),
            Some("@alice:example.org:DEVICE".into())
        );
        assert!(dialect.validate_slot("m.call#ROOM").is_ok());
        assert!(dialect.validate_slot("m.call#custom").is_err());
        assert_eq!(dialect.identity("@alice:example.org", "", "member"), None);
    }

    #[test]
    fn custom_slot_retains_explicit_direct_draft_contract() {
        let dialect = LiveKitAuthDialect::for_service_slot("m.call#custom");
        assert_eq!(dialect, LiveKitAuthDialect::DirectDraft);
        assert_eq!(dialect.token_endpoint(), TokenEndpoint::Msc4195);
        assert!(dialect.validate_slot("m.call#custom").is_ok());
        assert_eq!(
            dialect.identity("@alice:example.org", "DEVICE", "member"),
            Some(identity::pseudonymous_identity(
                "@alice:example.org",
                "DEVICE",
                "member"
            ))
        );
        assert_ne!(
            dialect.identity("@alice:example.org", "DEVICE", "member"),
            dialect.identity("@alice:example.org", "DEVICE", "another-member")
        );
        assert!(dialect.validate_slot("").is_err());
    }
}
