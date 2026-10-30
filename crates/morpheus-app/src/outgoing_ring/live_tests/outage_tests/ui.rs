//! Opt-in Linux live received-media tail of the one generated-window loop.
//! Uses the owned outage fixture, real RTC publications and production presenter.
//! This observes actual Slint Image items, not a GPU/installed-device capture.

use super::*;
use slint::{ComponentHandle, Model};
use tokio::runtime::Handle;

const CAMERA: u8 = 55;
const SCREEN: u8 = 185;

pub(crate) async fn run_if_configured(runtime: Handle) {
    if std::env::var_os("MORPHEUS_RTC_OUTAGE_TARGET").is_none() {
        return;
    }
    let case = OutageCase::read().expect("Owned received-video UI fixture rejected");
    let ui = crate::AppWindow::new().expect("Received-video window must initialize");
    ui.window().set_size(slint::PhysicalSize::new(1280, 900));
    ui.set_live_session(true);
    ui.set_preview_session(false);
    ui.set_page(2);
    ui.show().expect("Received-video window must show");
    // Only the two production presenter tasks use this owned runtime. Do not
    // enter it: fixture/RTC operations remain on the existing outer runtime.
    // Construct it before admitting any session/native playback owner.
    let presenters = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("Received-video presenter runtime must initialize");
    let opened = std::panic::catch_unwind(AssertUnwindSafe(|| OutageSession::open(&case)));
    let mut session = match opened {
        Ok(Ok(session)) => session,
        outcome => {
            if let Err(payload) = outcome {
                std::mem::forget(payload);
            }
            let profile = case.profile.keep();
            let _retained = Box::leak(Box::new((presenters, ui, profile)));
            panic!("Owned received-video session rejected");
        }
    };
    let controller = Arc::downgrade(&session.calls);
    let managers: Vec<_> = session
        .peers
        .iter()
        .map(|peer| Arc::downgrade(&peer.manager))
        .collect();
    let mut screen = None;
    let observed = AssertUnwindSafe(time::timeout(CASE_WORK, async {
        crate::call_stage::bind(&ui, presenters.handle(), session.calls.clone());
        exercise(&ui, &case, &mut session, &mut screen).await
    }))
    .catch_unwind()
    .await;
    let passed = matches!(&observed, Ok(Ok(Ok(()))));
    if let Err(payload) = observed {
        std::mem::forget(payload);
    }
    // A failed/cancelled unpublish must retain its owner and mark the peer
    // unproved, just like the existing fixture's admitted native operations.
    let owner = OutageSession::owner(&case).expect("Validated fixture owner");
    let remote = (0..session.peers.len())
        .find(|index| *index != owner)
        .expect("Validated fixture peer");
    let cleaned = AssertUnwindSafe(async {
        let screen_clean = if let Some(publication) = screen.as_ref() {
            let prior_unproved = session.peers[remote].operation_unproved;
            session.peers[remote].operation_unproved = true;
            let clean = matches!(
                time::timeout(Duration::from_secs(8), publication.unpublish()).await,
                Ok(Ok(()))
            );
            if clean {
                screen = None;
                session.peers[remote].operation_unproved = prior_unproved;
            }
            clean && !prior_unproved
        } else {
            !session.peers[remote].operation_unproved
        };
        session.cleanup(&case).await && screen_clean
    })
    .catch_unwind()
    .await;
    let clean = match cleaned {
        Ok(clean) => clean,
        Err(payload) => {
            std::mem::forget(payload);
            false
        }
    };
    // Hide is not callback destruction. Catch UI teardown too: a failure
    // cannot unwind through the actual runtime/session/profile owners.
    let dropped = std::panic::catch_unwind(AssertUnwindSafe(move || {
        crate::call_stage::clear_private_presentation(&ui);
        let cleared = empty(&ui);
        let hidden = ui.hide().is_ok();
        drop(ui);
        (cleared, hidden)
    }));
    let (cleared, hidden, ui_dropped) = match dropped {
        Ok((cleared, hidden)) => (cleared, hidden, true),
        Err(payload) => {
            std::mem::forget(payload);
            (false, false, false)
        }
    };
    // Runtime::drop waits for its worker/task destruction; do not substitute
    // shutdown_background/shutdown_timeout, which can detach remaining work.
    // Await through &mut so a deadline never drops the actual shutdown owner.
    // Admission itself can panic after Tokio queues the blocking task. Keep
    // Runtime ownership outside that closure, and do not allow it to take the
    // Runtime until spawn_blocking returned the actual join owner to us.
    let presenters = Arc::new(StdMutex::new(Some(presenters)));
    let handoff = presenters.clone();
    let (permit, start) = std::sync::mpsc::sync_channel::<()>(1);
    let admission = std::panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.spawn_blocking(move || {
            if start.recv().is_err() {
                return false;
            }
            let owned = handoff
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let Some(owned) = owned else {
                return false;
            };
            // No handoff mutex is held while the actual Runtime is dropped.
            drop(owned);
            true
        })
    }));
    let mut shutdown = match admission {
        Ok(task) => Some(task),
        Err(payload) => {
            std::mem::forget(payload);
            None
        }
    };
    // One send into capacity one cannot block. Failed admission closes the
    // permit without sending: even a queued body cannot begin Runtime Drop.
    let started = shutdown.is_some() && permit.send(()).is_ok();
    drop(permit);
    let retired = match shutdown.as_mut() {
        Some(task) if started => match time::timeout(Duration::from_secs(5), task).await {
            Ok(Ok(true)) => true,
            Ok(Err(error)) => {
                std::mem::forget(error);
                false
            }
            Ok(Ok(false)) | Err(_) => false,
        },
        _ => false,
    };
    // A completed runtime cannot post more work, but its earlier UI closures
    // may still retain calls. Flush those before checking actual strong owners.
    let drained = if retired && ui_dropped {
        let (sent, received) = tokio::sync::oneshot::channel();
        let posted = slint::invoke_from_event_loop(move || {
            let _ = sent.send(());
        });
        posted.is_ok()
            && matches!(
                time::timeout(Duration::from_secs(5), received).await,
                Ok(Ok(()))
            )
    } else {
        false
    };
    let clean = if passed && clean && cleared && hidden && retired && drained {
        let dropped = std::panic::catch_unwind(AssertUnwindSafe(move || {
            drop(screen);
            drop(session);
        }));
        let session_dropped = match dropped {
            Ok(()) => true,
            Err(payload) => {
                std::mem::forget(payload);
                false
            }
        };
        // AccountManager::stop does not drop its lifetime profile lock. The
        // actual controller AND every manager must have no strong owner left.
        let controller_owner = controller.upgrade();
        let manager_owners: Vec<_> = managers
            .iter()
            .filter_map(|owner| owner.upgrade())
            .collect();
        let released = session_dropped && controller_owner.is_none() && manager_owners.is_empty();
        eprintln!("MORPHEUS_RTC_UI_CHECK presenter_owners_released={released}");
        if released {
            case.profile.close().is_ok()
        } else {
            let profile = case.profile.keep();
            let _retained = Box::leak(Box::new((controller_owner, manager_owners, profile)));
            false
        }
    } else {
        // Preserve both the actual JoinHandle (including a still-running
        // runtime Drop) and original native/account owners. A failed work,
        // teardown or barrier is terminal uncertainty, never profile erasure.
        // The existing owned outer namespace must destroy this failed child.
        let profile = case.profile.keep();
        let _retained = Box::leak(Box::new((shutdown, presenters, screen, session, profile)));
        false
    };
    eprintln!("MORPHEUS_RTC_UI_CHECK cleanup={clean}");
    let passed = passed && clean;
    eprintln!("MORPHEUS_RTC_UI_RESULT passed={passed}");
    assert!(passed, "Owned received-video UI acceptance failed");
}

async fn exercise(
    ui: &crate::AppWindow,
    case: &OutageCase,
    session: &mut OutageSession,
    screen: &mut Option<Publication>,
) -> Result<(), ()> {
    for (peer, account) in session.peers.iter_mut().zip(&case.fixture.accounts) {
        peer.login(account, &case.room).await?;
    }
    for first in 0..session.peers.len() {
        for second in 0..session.peers.len() {
            if first != second {
                trust_fixture_peer(
                    session.peers[first].client.as_ref().ok_or(())?,
                    session.peers[second].client.as_ref().ok_or(())?,
                )
                .await?;
            }
        }
    }
    let owner = OutageSession::owner(case)?;
    let remote = (0..session.peers.len())
        .find(|index| *index != owner)
        .ok_or(())?;
    let client = session.peers[owner].client.as_ref().ok_or(())?;
    let room = client.get_room(&case.room).ok_or(())?;
    let creator = &case
        .fixture
        .accounts
        .iter()
        .find(|account| account.label == "a_primary")
        .ok_or(())?
        .user_id;
    verify_creation(&room, &case.creation, creator).await?;
    room.set_is_direct(case.dm).await.map_err(|_| ())?;
    let account = session.peers[owner].account.as_ref().ok_or(())?.clone();
    let selection = session.peers[owner]
        .manager
        .account_selection(&account.id)
        .ok_or(())?;
    ui.set_active_account_local_id(account.id.as_str().into());
    ui.set_active_room_id(case.room.as_str().into());
    let store = morpheus_config::ConfigStore::at(case.profile.path().join("ui-calls.toml"));
    store.load_or_create().map_err(|_| ())?;
    let preferences = Arc::new(VoiceSpaces::new(store));
    session
        .calls
        .open_slot(
            selection.clone(),
            case.room.as_str(),
            CallJoinMode::Standard,
        )
        .await
        .map_err(|_| ())?;
    session
        .calls
        .start_audio(
            selection.clone(),
            case.room.as_str(),
            CallJoinMode::Standard,
            AudioDeviceSelection {
                microphone: Some(alsa_playback::INPUT_ID.to_owned()),
                speaker: Some(alsa_playback::OUTPUT_ID.to_owned()),
            },
            CallStartIntent::Notify,
            preferences.clone(),
        )
        .await
        .map_err(|_| ())?;
    let token = session.calls.video_token();
    require(token != 0 && selection.is_current())?;
    for index in 0..session.peers.len() {
        if index != owner {
            session.peers[index].join(&case.room).await?;
        }
    }
    session.peers[remote].operation_unproved = true;
    *screen = Some(
        time::timeout(
            PHASE_LIMIT,
            session.peers[remote]
                .rtc
                .as_mut()
                .ok_or(())?
                .publish_screen(case.room.as_str(), 320, 180, false),
        )
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?,
    );
    session.peers[remote].operation_unproved = false;
    // These are layout context, not fabricated call/media state. Joining above
    // returned the real current call before this stage is made visible.
    ui.set_call_connected(true);
    ui.set_native_call_enabled(true);
    let mut sequence = 0u32;
    observe_media(
        ui,
        case,
        session,
        screen.as_ref().ok_or(())?,
        false,
        &mut sequence,
    )
    .await?;
    eprintln!("MORPHEUS_RTC_UI_CHECK decoded_camera_and_screen_in_slint=true");
    session.calls.end_call().await.map_err(|_| ())?;
    require(session.calls.video_token() != token)?;
    wait_empty(ui).await?;
    eprintln!("MORPHEUS_RTC_UI_CHECK leave_clears_received_images=true");
    session
        .calls
        .open_slot(
            selection.clone(),
            case.room.as_str(),
            CallJoinMode::Standard,
        )
        .await
        .map_err(|_| ())?;
    session
        .calls
        .start_audio(
            selection.clone(),
            case.room.as_str(),
            CallJoinMode::Standard,
            AudioDeviceSelection {
                microphone: Some(alsa_playback::INPUT_ID.to_owned()),
                speaker: Some(alsa_playback::OUTPUT_ID.to_owned()),
            },
            CallStartIntent::Notify,
            preferences,
        )
        .await
        .map_err(|_| ())?;
    let fresh = session.calls.video_token();
    require(fresh != 0 && fresh != token && selection.is_current())?;
    // The real new decoder/presenter generation must show reversed patterns.
    // Retained old-camera or old-screen pictures cannot satisfy this phase.
    observe_media(
        ui,
        case,
        session,
        screen.as_ref().ok_or(())?,
        true,
        &mut sequence,
    )
    .await?;
    eprintln!("MORPHEUS_RTC_UI_CHECK new_epoch_replaces_both_images=true");
    // Production account clearing follows revocation of call authority. Drive
    // real leave first; no synthetic token or stale-frame injection is used.
    session.calls.end_call().await.map_err(|_| ())?;
    require(session.calls.video_token() != fresh)?;
    wait_empty(ui).await?;
    let logout = session.peers[owner]
        .manager
        .logout_account(&account.id)
        .await
        .map_err(|_| ())?;
    require(logout.remote_logout_succeeded && !selection.is_current())?;
    crate::call_stage::clear_private_presentation(ui);
    require(empty(ui))?;
    // Flush real already-posted presenter work after the synchronous detach
    // fence. No old watch snapshot may repopulate this account's image slots.
    for _ in 0..3 {
        let (sent, received) = tokio::sync::oneshot::channel();
        ui.as_weak()
            .upgrade_in_event_loop(move |_| {
                let _ = sent.send(());
            })
            .map_err(|_| ())?;
        received.await.map_err(|_| ())?;
        time::sleep(Duration::from_millis(40)).await;
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        require(empty(ui))?;
    }
    eprintln!("MORPHEUS_RTC_UI_CHECK retired_account_stays_empty=true");
    Ok(())
}

async fn observe_media(
    ui: &crate::AppWindow,
    case: &OutageCase,
    session: &OutageSession,
    screen: &Publication,
    swapped: bool,
    sequence: &mut u32,
) -> Result<(), ()> {
    let owner = OutageSession::owner(case)?;
    let remote = (0..session.peers.len())
        .find(|index| *index != owner)
        .ok_or(())?;
    let token = session.calls.video_token();
    let account = session.peers[owner].account.as_ref().ok_or(())?;
    let selection = session.peers[owner]
        .manager
        .account_selection(&account.id)
        .ok_or(())?;
    require(token != 0 && selection.is_current())?;
    let mut updates = session.calls.subscribe_video();
    time::timeout(PHASE_LIMIT, async {
        loop {
            require(selection.is_current() && session.calls.video_token() == token)?;
            for (index, peer) in session.peers.iter().enumerate() {
                if index != owner {
                    peer.publication
                        .as_ref()
                        .ok_or(())?
                        .push_video(frame(*sequence, if swapped { SCREEN } else { CAMERA }))
                        .map_err(|_| ())?;
                }
            }
            screen
                .push_video(frame(*sequence, if swapped { CAMERA } else { SCREEN }))
                .map_err(|_| ())?;
            *sequence = sequence.checked_add(1).ok_or(())?;
            let snapshot = updates.borrow_and_update().clone();
            if let VideoUpdate::Stage { token: seen, stage } = snapshot {
                if seen == token
                    && received_pair(ui, &stage, &session.peers[remote], token, session, swapped)?
                {
                    return Ok::<_, ()>(());
                }
            }
            time::sleep(Duration::from_millis(67)).await;
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        }
    })
    .await
    .map_err(|_| ())??;
    Ok(())
}

async fn wait_empty(ui: &crate::AppWindow) -> Result<(), ()> {
    time::timeout(Duration::from_secs(10), async {
        loop {
            if empty(ui) {
                return;
            }
            time::sleep(Duration::from_millis(20)).await;
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        }
    })
    .await
    .map_err(|_| ())
}

fn received_pair(
    ui: &crate::AppWindow,
    stage: &StageSnapshot,
    peer: &Peer,
    token: u64,
    session: &OutageSession,
    swapped: bool,
) -> Result<bool, ()> {
    let info = peer.info.as_ref().ok_or(())?;
    let account = peer.account.as_ref().ok_or(())?;
    require(session.calls.video_token() == token)?;
    let mut both = [false; 2];
    for tile in stage
        .layout
        .featured
        .iter()
        .chain(stage.layout.tiles.iter())
    {
        if tile.is_local
            || tile.key.member_id != info.membership_id
            || tile.user_id != account.user_id
            || tile.state != TileState::Live
        {
            continue;
        }
        let level = match (tile.key.medium, swapped) {
            (TileMedium::Camera, false) | (TileMedium::Screen, true) => CAMERA,
            (TileMedium::Screen, false) | (TileMedium::Camera, true) => SCREEN,
        };
        let index = usize::from(tile.key.medium == TileMedium::Screen);
        let Some(slot) = tile.frame_slot else {
            continue;
        };
        let Some(decoded) = stage.frames.get(slot) else {
            return Err(());
        };
        if decoded.epoch == 0 || decoded.sequence == 0 {
            continue;
        }
        let Some(pixels) = decoded.pixels.as_ref() else {
            continue;
        };
        if !pixels_match(pixels, level) {
            continue;
        }
        let rows = ui.get_native_call_tiles();
        let mut projected = rows.iter().chain(
            ui.get_native_call_featured_visible()
                .then(|| ui.get_native_call_featured()),
        );
        let Some(shown) = projected.find(|shown| {
            i32::try_from(tile.id).ok() == Some(shown.id)
                && shown.live
                && !shown.is_local
                && shown.screen == (index == 1)
                && shown.name.as_str() == account.user_id
                && usize::try_from(shown.frame_slot).ok() == Some(slot)
        }) else {
            continue;
        };
        let Some(image) = ui.get_native_call_frames().row_data(slot) else {
            continue;
        };
        if !image
            .to_rgb8()
            .is_some_and(|pixels| pixels_match(&pixels, level))
        {
            continue;
        }
        // Walk the actual generated tree: a positive model alone would not
        // prove the call stage bound that slot to its visible Image element.
        both[index] = visible_image(ui, &shown, level);
    }
    Ok(both.into_iter().all(|matched| matched))
}

fn pixels_match(pixels: &slint::SharedPixelBuffer<slint::Rgb8Pixel>, level: u8) -> bool {
    let width = pixels.width();
    let height = pixels.height();
    if !(160..=320).contains(&width) || !(90..=180).contains(&height) {
        return false;
    }
    if width * 9 != height * 16 {
        return false;
    }
    let expected = (i32::from(level) - 16) * 255 / 219;
    [(width / 4, height / 4), (width * 3 / 4, height * 3 / 4)]
        .into_iter()
        .all(|(x, y)| {
            let pixel = pixels.as_slice()[(y * width + x) as usize];
            [pixel.r, pixel.g, pixel.b]
                .into_iter()
                .all(|value| (i32::from(value) - expected).abs() <= 18)
        })
}

fn visible_image(ui: &crate::AppWindow, tile: &crate::CallTileItem, level: u8) -> bool {
    use slint::private_unstable_api::re_exports::{
        AccessibleStringProperty, ImageItem, ItemRc, WindowInner,
    };
    use std::ops::ControlFlow;
    let label = format!(
        "{}{}, video",
        if tile.screen {
            "Screen share from "
        } else {
            ""
        },
        tile.name
    );
    let root = ItemRc::new_root(WindowInner::from_pub(ui.window()).component());
    let mut matched = false;
    let mut matching_tiles = 0usize;
    root.visit_descendants(|item| {
        if item
            .accessible_string_property(AccessibleStringProperty::Label)
            .is_some_and(|value| value.starts_with(&label))
        {
            matching_tiles += 1;
            if !item.is_visible() || !opaque_ancestors(&root, item) {
                return ControlFlow::<()>::Continue(());
            }
            item.visit_descendants(|child| {
                let bounds = child.geometry();
                let center = child.map_to_window(bounds.center());
                let window = ui.window().size();
                let scale = ui.window().scale_factor();
                if child.is_visible()
                    && opaque_ancestors(&root, child)
                    && bounds.size.width > 0.0
                    && bounds.size.height > 0.0
                    && center.x >= 0.0
                    && center.y >= 0.0
                    && center.x < window.width as f32 / scale
                    && center.y < window.height as f32 / scale
                {
                    if let Some(image) = child.downcast::<ImageItem>() {
                        matched |= image
                            .as_pin_ref()
                            .source()
                            .to_rgb8()
                            .is_some_and(|pixels| pixels_match(&pixels, level));
                    }
                }
                ControlFlow::<()>::Continue(())
            });
        }
        ControlFlow::<()>::Continue(())
    });
    matching_tiles == 1 && matched
}

fn opaque_ancestors(
    root: &slint::private_unstable_api::re_exports::ItemRc,
    target: &slint::private_unstable_api::re_exports::ItemRc,
) -> bool {
    use slint::private_unstable_api::re_exports::Opacity;
    use std::ops::ControlFlow;
    // is_visible covers ancestor clips and transforms, but not Opacity. Use
    // the same already-exported getter as the existing generated-UI tests.
    // A subtree membership check avoids adding a dependency for a private
    // parent-traversal enum that Slint does not export through this API.
    root.visit_descendants(|item| {
        if let Some(opacity) = item.downcast::<Opacity>() {
            let value = opacity.as_pin_ref().opacity();
            if !value.is_finite() || value <= 0.0 {
                let contains = item
                    .visit_descendants(|child| {
                        if child == target {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    })
                    .is_some();
                if contains {
                    return ControlFlow::Break(());
                }
            }
        }
        ControlFlow::Continue(())
    })
    .is_none()
}

fn empty(ui: &crate::AppWindow) -> bool {
    ui.get_native_call_tiles().row_count() == 0
        && !ui.get_native_call_featured_visible()
        && ui
            .get_native_call_frames()
            .iter()
            .all(|image| image.size().width == 0)
        && ui.get_native_call_participants().row_count() == 0
}
