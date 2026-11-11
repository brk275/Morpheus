// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.

//! Native LiveKit media on a joined call: the MSC4195 key provider, the
//! [`LiveKitMediaTransport`] over it, and [`attach_media`](crate::attach_media)'s
//! ordered wiring. Shared by the `LiveKitCall` facade and the FFI media session.

use std::sync::Arc;

use super::auth::AuthGuard;
use crate::attach::{attach_media_guarded, token_slot_id};
use crate::{AttachError, AttachOptions, MediaAttachment, StabilityConfig};
use matrix_rtc_call::RtcCall;
use matrix_rtc_core::compat::MembershipFormat;
use matrix_rtc_core::{MatrixBackend, RtcIdentityMapper};
use matrix_rtc_transport::{OwnMemberClaims, livekit::LiveKitAuthDialect};

use matrix_rtc_livekit::{
    LiveKitMediaTransport, LiveKitTransportConnection, MediaKeyBridge, msc4195_key_provider,
    msc4195_media_key_bridge,
};

/// What [`attach_livekit`] needs besides the call.
#[derive(Clone, Debug)]
pub struct LiveKitAttachOptions {
    /// Membership carrier and token slot spelling, not auth identity.
    pub format: MembershipFormat,
    /// One explicit endpoint/identity contract, shared by every media/key path.
    pub auth_dialect: LiveKitAuthDialect,
    /// HTTP client for the token exchange; `None` builds a default one.
    pub http: Option<reqwest::Client>,
    /// `false` never subscribes to peers' media; see
    /// [`LiveKitMediaTransport::with_auto_subscribe`].
    pub auto_subscribe: bool,
    pub stability: StabilityConfig,
}

/// A call with native LiveKit media attached.
pub struct LiveKitAttachment {
    pub media: MediaAttachment<LiveKitTransportConnection>,
    /// The key handler installed on the call, over the connections' shared key
    /// provider.
    pub key_bridge: Arc<MediaKeyBridge>,
}

/// Attach native LiveKit media to `call`, joined on `backend` in
/// `options.format`.
///
/// One key provider serves every SFU connection, own and peers': keys are
/// indexed by participant identity, which is globally unique per membership.
pub async fn attach_livekit<B: MatrixBackend + 'static>(
    call: &RtcCall<B>,
    backend: Arc<dyn MatrixBackend>,
    options: LiveKitAttachOptions,
) -> Result<LiveKitAttachment, AttachError> {
    options
        .auth_dialect
        .validate_slot(&token_slot_id(options.format, call.slot_id()))
        .map_err(|_| AttachError::IdentityRejected)?;
    let (own_user_id, own_device_id) = (backend.own_user_id(), backend.own_device_id());
    let guard = AuthGuard::new(
        options.auth_dialect,
        OwnMemberClaims {
            member_id: call.member_id().to_owned(),
            user_id: own_user_id.clone(),
            device_id: own_device_id.clone(),
        },
        call.subscribe_memberships().await,
    );
    if !guard.seed_initial() {
        return Err(AttachError::IdentityRejected);
    }
    // The same checked mapper and irreversible call latch serve the core,
    // all SFU connections, live keys, replays and detached delayed activation.
    let for_mapper = Arc::clone(&guard);
    let mapper: RtcIdentityMapper = Arc::new(move |user, device, member, snapshot| {
        for_mapper.map(user, device, member, snapshot)
    });
    let provider = msc4195_key_provider();
    let for_keys = Arc::clone(&guard);
    let for_memberships = Arc::clone(&guard);
    let key_bridge = Arc::new(
        msc4195_media_key_bridge(provider.clone())
            .with_call_admission(Arc::clone(&guard.active))
            .with_member_admission(Arc::new(move |identity, member| {
                for_keys.member_lease(identity, member)
            }))
            .with_membership_admission(Arc::new(move |snapshot| {
                for_memberships.on_core_snapshot(snapshot);
            })),
    );
    let transport = Arc::new(
        LiveKitMediaTransport::new(options.http.unwrap_or_default(), backend, provider)
            .with_auto_subscribe(options.auto_subscribe)
            .with_auth_dialect(options.auth_dialect)
            .with_identity_mapper(mapper.clone())
            .with_call_admission(Arc::clone(&guard.active)),
    );
    let media = attach_media_guarded(
        call,
        transport,
        key_bridge.clone(),
        AttachOptions {
            own_user_id,
            own_device_id,
            identity_mapper: mapper,
            format: options.format,
            stability: options.stability,
        },
        Some(guard.admission()),
    )
    .await?;
    Ok(LiveKitAttachment { media, key_bridge })
}
