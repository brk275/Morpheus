//! Wayland capture uses the source selected by the system ScreenCast portal.
//! No Xwayland coordinates, screenshot fallback, or remembered grants are used.
//! All PipeWire objects live on the capture worker; its loop has finite waits.

use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    io::Cursor,
    rc::Rc,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use pipewire::{
    context::ContextRc,
    loop_::Timeout,
    main_loop::MainLoopRc,
    properties,
    spa::{
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, serialize::PodSerializer},
        utils::{Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamListener, StreamRc, StreamState},
};
use tokio::runtime::{Builder, Runtime};
use zbus::{
    Connection, Proxy,
    proxy::SignalStream,
    zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value},
};

use crate::MediaError;

const SERVICE: &str = "org.freedesktop.portal.Desktop";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
const SELECT_TIMEOUT: Duration = Duration::from_secs(180);
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_millis(200);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
type Dict = HashMap<String, OwnedValue>;

fn error(message: &str) -> MediaError {
    MediaError::Device(message.into())
}
fn portal_error(_: impl std::fmt::Debug) -> MediaError {
    error("system screen-sharing portal is unavailable or returned an invalid response")
}

#[derive(Clone, Copy)]
enum RequestPhase {
    CreateSession,
    SelectSources,
    Start,
}

impl RequestPhase {
    fn method(self) -> &'static str {
        match self {
            Self::CreateSession => "CreateSession",
            Self::SelectSources => "SelectSources",
            Self::Start => "Start",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::CreateSession => "creating the screen-sharing session",
            Self::SelectSources => "selecting the screen-sharing source",
            Self::Start => "starting the screen-sharing stream",
        }
    }

    fn with_error(self, failure: MediaError) -> MediaError {
        // Request errors below are static, locally generated messages. Never
        // include D-Bus error bodies, session paths, source names or node IDs.
        let reason = match failure {
            MediaError::Device(message) => message,
            _ => "system screen-sharing request failed".into(),
        };
        error(&format!("{}: {reason}", self.label()))
    }
}

fn response_values(code: u32, values: Dict) -> Result<Dict, MediaError> {
    match code {
        0 => Ok(values),
        1 => Err(error("screen sharing cancelled in the system dialog")),
        2 => Err(error(
            "system screen-sharing portal could not complete the request",
        )),
        _ => Err(error(
            "system screen-sharing portal returned an unsupported response",
        )),
    }
}

fn runtime() -> Result<Runtime, MediaError> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(portal_error)
}

pub(crate) fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value == "wayland")
}

pub(crate) fn source_type(key: &str) -> Result<u32, MediaError> {
    match key {
        "portal:monitor" => Ok(1),
        "portal:window" => Ok(2),
        _ => Err(error("select a screen-sharing source again")),
    }
}

pub(crate) fn sources(kind: u32) -> Result<Vec<(String, String)>, MediaError> {
    let available = runtime()?.block_on(wait(
        async {
            let connection = Connection::session().await.map_err(portal_error)?;
            let proxy = Proxy::new(&connection, SERVICE, DESKTOP, SCREENCAST)
                .await
                .map_err(portal_error)?;
            proxy
                .get_property::<u32>("AvailableSourceTypes")
                .await
                .map_err(portal_error)
        },
        &|| false,
        RPC_TIMEOUT,
    ))?;
    if available & kind == 0 {
        return Ok(Vec::new());
    }
    match kind {
        1 => Ok(vec![(
            "portal:monitor".into(),
            "Choose a display in the system dialog…".into(),
        )]),
        2 => Ok(vec![(
            "portal:window".into(),
            "Choose a window in the system dialog…".into(),
        )]),
        _ => Err(error("unsupported screen-sharing source type")),
    }
}

async fn wait<T>(
    future: impl Future<Output = Result<T, MediaError>>,
    cancelled: &dyn Fn() -> bool,
    budget: Duration,
) -> Result<T, MediaError> {
    tokio::pin!(future);
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);
    loop {
        if cancelled() {
            return Err(error("screen sharing cancelled"));
        }
        tokio::select! {
            biased;
            _ = &mut deadline => return Err(error("screen-sharing request timed out")),
            result = &mut future => return result,
            _ = tokio::time::sleep(POLL_INTERVAL) => {},
        }
    }
}

fn token() -> String {
    format!("morpheus_{}", uuid::Uuid::new_v4().simple())
}
fn path(connection: &Connection, kind: &str, token: &str) -> Result<OwnedObjectPath, MediaError> {
    let sender = connection
        .unique_name()
        .ok_or_else(|| portal_error("missing bus name"))?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    OwnedObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/{kind}/{sender}/{token}"
    ))
    .map_err(portal_error)
}

/// Subscribe before calling; responses may arrive before the method reply.
/// Close uses the predicted request even when cancellation races the reply.
async fn request(
    proxy: &Proxy<'_>,
    phase: RequestPhase,
    session: &OwnedObjectPath,
    mut options: HashMap<&str, Value<'_>>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Dict, MediaError> {
    let request_token = token();
    let request_path = path(proxy.connection(), "request", &request_token)
        .map_err(|failure| phase.with_error(failure))?;
    let request = wait(
        async {
            Proxy::new(
                proxy.connection(),
                SERVICE,
                request_path.as_str(),
                "org.freedesktop.portal.Request",
            )
            .await
            .map_err(portal_error)
        },
        cancelled,
        RPC_TIMEOUT,
    )
    .await
    .map_err(|failure| phase.with_error(failure))?;
    options.insert("handle_token", Value::from(request_token));
    let result = wait(
        async {
            let mut responses = request
                .receive_signal("Response")
                .await
                .map_err(portal_error)?;
            let returned: OwnedObjectPath = match phase {
                RequestPhase::CreateSession => proxy.call(phase.method(), &options).await,
                RequestPhase::SelectSources => {
                    proxy.call(phase.method(), &(session, options)).await
                }
                RequestPhase::Start => proxy.call(phase.method(), &(session, "", options)).await,
            }
            .map_err(portal_error)?;
            if returned != request_path {
                return Err(portal_error("request mismatch"));
            }
            let response = responses
                .next()
                .await
                .ok_or_else(|| portal_error("response ended"))?;
            let (code, values): (u32, Dict) =
                response.body().deserialize().map_err(portal_error)?;
            response_values(code, values)
        },
        cancelled,
        SELECT_TIMEOUT,
    )
    .await;
    if result.is_err() {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, request.call::<_, _, ()>("Close", &())).await;
    }
    result.map_err(|failure| phase.with_error(failure))
}

struct PortalSession {
    runtime: Runtime,
    connection: Connection,
    session: OwnedObjectPath,
    closed: RefCell<SignalStream<'static>>,
}

impl PortalSession {
    fn open(
        kind: u32,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(Self, u32, Option<u64>, OwnedFd), MediaError> {
        let runtime = runtime()?;
        let (connection, session, session_token, closed) = runtime.block_on(wait(
            async {
                let connection = Connection::session().await.map_err(portal_error)?;
                let session_token = token();
                let session = path(&connection, "session", &session_token)?;
                let proxy = Proxy::new(
                    &connection,
                    SERVICE,
                    session.as_str(),
                    "org.freedesktop.portal.Session",
                )
                .await
                .map_err(portal_error)?;
                let closed = proxy.receive_signal("Closed").await.map_err(portal_error)?;
                Ok((connection, session, session_token, closed))
            },
            cancelled,
            RPC_TIMEOUT,
        ))?;
        // Own cleanup before CreateSession: even a cancelled request can race
        // session creation. Closing the private connection also revokes it.
        let owner = Self {
            runtime,
            connection,
            session,
            closed: RefCell::new(closed),
        };
        let (node, serial, fd) = owner.runtime.block_on(async {
            let proxy = wait(
                async {
                    Proxy::new(&owner.connection, SERVICE, DESKTOP, SCREENCAST)
                        .await
                        .map_err(portal_error)
                },
                cancelled,
                RPC_TIMEOUT,
            )
            .await?;
            let available = wait(
                async {
                    proxy
                        .get_property::<u32>("AvailableSourceTypes")
                        .await
                        .map_err(portal_error)
                },
                cancelled,
                RPC_TIMEOUT,
            )
            .await?;
            if available & kind == 0 {
                return Err(error("the system portal cannot share that source type"));
            }
            let mut options = HashMap::new();
            options.insert("session_handle_token", Value::from(session_token));
            let mut created = request(
                &proxy,
                RequestPhase::CreateSession,
                &owner.session,
                options,
                cancelled,
            )
            .await?;
            let returned = created
                .remove("session_handle")
                .ok_or_else(|| portal_error("no session"))?;
            // The portal specification returns a string variant, not an 'o'.
            let returned = String::try_from(returned).map_err(portal_error)?;
            if returned != owner.session.as_str() {
                return Err(portal_error("session mismatch"));
            }
            let mut options = HashMap::new();
            options.insert("types", Value::from(kind));
            options.insert("multiple", Value::from(false));
            options.insert("persist_mode", Value::from(0u32));
            request(
                &proxy,
                RequestPhase::SelectSources,
                &owner.session,
                options,
                cancelled,
            )
            .await?;
            let mut started = request(
                &proxy,
                RequestPhase::Start,
                &owner.session,
                HashMap::new(),
                cancelled,
            )
            .await?;
            let streams = started
                .remove("streams")
                .ok_or_else(|| portal_error("no streams"))?;
            let streams: Vec<(u32, Dict)> = Vec::try_from(streams).map_err(portal_error)?;
            let (node, serial) = selected_stream(streams, kind)?;
            let options: HashMap<&str, Value<'_>> = HashMap::new();
            let fd: OwnedFd = wait(
                async {
                    proxy
                        .call("OpenPipeWireRemote", &(&owner.session, options))
                        .await
                        .map_err(portal_error)
                },
                cancelled,
                RPC_TIMEOUT,
            )
            .await?;
            Ok::<_, MediaError>((node, serial, fd))
        })?;
        Ok((owner, node, serial, fd))
    }

    fn ended(&self) -> bool {
        self.runtime.block_on(async {
            let mut closed = self.closed.borrow_mut();
            // Dependencies can enable zbus's Tokio transport globally. Yield
            // to this worker's reactor so incoming Closed signals are driven
            // even while the PipeWire loop owns most of the worker's time.
            tokio::select! {
                biased;
                _ = self.connection.closed() => true,
                _ = closed.next() => true,
                _ = tokio::time::sleep(Duration::from_millis(1)) => false,
            }
        })
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        self.runtime.block_on(async {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
                let proxy = Proxy::new(
                    &self.connection,
                    SERVICE,
                    self.session.as_str(),
                    "org.freedesktop.portal.Session",
                )
                .await?;
                proxy.call::<_, _, ()>("Close", &()).await
            })
            .await;
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, self.connection.clone().close()).await;
        });
    }
}

fn selected_stream(
    mut streams: Vec<(u32, Dict)>,
    kind: u32,
) -> Result<(u32, Option<u64>), MediaError> {
    if streams.len() != 1 {
        return Err(error("system portal must select exactly one source"));
    }
    let (node, mut properties) = streams.remove(0);
    let actual = properties
        .remove("source_type")
        .and_then(|value| u32::try_from(value).ok());
    if actual != Some(kind) {
        return Err(error("system portal selected a different source type"));
    }
    let serial = properties
        .remove("pipewire-serial")
        .map(u64::try_from)
        .transpose()
        .map_err(portal_error)?;
    Ok((node, serial))
}

#[derive(Default)]
struct FrameState {
    latest: Option<xcap::image::RgbaImage>,
    failed: bool,
}
struct StreamData {
    format: VideoInfoRaw,
}

pub(crate) struct PortalCapture {
    // Drop the listener before the stream. StreamRc retains its core/context.
    _listener: StreamListener<StreamData>,
    stream: StreamRc,
    main_loop: MainLoopRc,
    session: PortalSession,
    state: Rc<RefCell<FrameState>>,
}

impl PortalCapture {
    pub(crate) fn open(key: &str, cancelled: &dyn Fn() -> bool) -> Result<Self, MediaError> {
        let (session, node, serial, fd) = PortalSession::open(source_type(key)?, cancelled)?;
        if cancelled() {
            return Err(error("screen sharing cancelled"));
        }
        pipewire::init();
        let main_loop = MainLoopRc::new(None).map_err(portal_error)?;
        let context = ContextRc::new(&main_loop, None).map_err(portal_error)?;
        let core = context
            .connect_fd_rc(fd.into(), None)
            .map_err(portal_error)?;
        let mut properties = properties::properties! {
            "media.type" => "Video", "media.category" => "Capture", "media.role" => "Screen",
            "node.dont-reconnect" => "true",
        };
        if let Some(serial) = serial {
            properties.insert("target.object", serial.to_string());
        }
        let stream =
            StreamRc::new(core, "Morpheus screen sharing", properties).map_err(portal_error)?;
        let state = Rc::new(RefCell::new(FrameState::default()));
        let format_state = Rc::clone(&state);
        let status_state = Rc::clone(&state);
        let frame_state = Rc::clone(&state);
        let listener = stream
            .add_local_listener_with_user_data(StreamData {
                format: VideoInfoRaw::default(),
            })
            .state_changed(move |_, _, old, new| {
                if matches!(new, StreamState::Error(_) | StreamState::Unconnected)
                    || (old == StreamState::Streaming && new == StreamState::Paused)
                {
                    status_state.borrow_mut().failed = true;
                }
            })
            .param_changed(move |_, data, id, param| {
                if id == ParamType::Format.as_raw() {
                    if param.is_none_or(|param| data.format.parse(param).is_err()) {
                        format_state.borrow_mut().failed = true;
                    }
                }
            })
            .process(move |stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                let Some(data_buffer) = datas.first_mut() else {
                    frame_state.borrow_mut().failed = true;
                    return;
                };
                let chunk = data_buffer.chunk();
                let (offset, length, stride) = (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride(),
                );
                if length == 0 {
                    return;
                }
                let size = data.format.size();
                let result = data_buffer
                    .data()
                    .ok_or(MediaError::FrameFormatMismatch)
                    .and_then(|bytes| {
                        unpack_frame(
                            bytes,
                            offset,
                            length,
                            stride,
                            size.width,
                            size.height,
                            data.format.format(),
                        )
                    });
                let mut state = frame_state.borrow_mut();
                match result {
                    Ok(image) => state.latest = Some(image),
                    Err(_) => state.failed = true,
                }
            })
            .register()
            .map_err(portal_error)?;
        let object = pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::EnumFormat,
            pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pod::property!(
                FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::BGRx,
                VideoFormat::BGRA,
                VideoFormat::RGBA,
                VideoFormat::RGBx,
                VideoFormat::RGB
            ),
            pod::property!(
                FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                Rectangle {
                    width: 1280,
                    height: 720
                },
                Rectangle {
                    width: 1,
                    height: 1
                },
                Rectangle {
                    width: 7680,
                    height: 4320
                }
            ),
            pod::property!(
                FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                Fraction { num: 15, denom: 1 },
                Fraction { num: 0, denom: 1 },
                Fraction { num: 60, denom: 1 }
            ),
        );
        let serialized =
            PodSerializer::serialize(Cursor::new(Vec::new()), &pod::Value::Object(object))
                .map_err(portal_error)?
                .0
                .into_inner();
        let mut parameters =
            [Pod::from_bytes(&serialized).ok_or_else(|| portal_error("invalid format"))?];
        stream
            .connect(
                Direction::Input,
                if serial.is_some() { None } else { Some(node) },
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::DONT_RECONNECT,
                &mut parameters,
            )
            .map_err(portal_error)?;
        Ok(Self {
            _listener: listener,
            stream,
            main_loop,
            session,
            state,
        })
    }

    pub(crate) fn next_image(
        &self,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<xcap::image::RgbaImage, MediaError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if cancelled() {
                return Err(error("screen sharing cancelled"));
            }
            // Drain stream changes before returning even a repeated frame.
            // Some compositors deliver frames only when the content changes.
            self.main_loop
                .loop_()
                .iterate(Timeout::Finite(POLL_INTERVAL));
            if self.session.ended() || self.state.borrow().failed {
                return Err(error("selected screen-sharing source ended"));
            }
            if let Some(image) = &self.state.borrow().latest {
                return Ok(image.clone());
            }
            if Instant::now() >= deadline {
                return Err(error("screen-sharing stream stopped producing frames"));
            }
        }
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        let _ = self.stream.disconnect();
    }
}

/// A chunk can start inside the mapped allocation and contain padded rows.
/// Reject negative stride, wraparound, unmapped/DMA-only data and oversized
/// dimensions instead of treating arbitrary buffer bytes as a screenshot.
fn unpack_frame(
    bytes: &[u8],
    offset: usize,
    length: usize,
    stride: i32,
    width: u32,
    height: u32,
    format: VideoFormat,
) -> Result<xcap::image::RgbaImage, MediaError> {
    let channels = match format {
        VideoFormat::RGB => 3,
        VideoFormat::RGBA | VideoFormat::RGBx | VideoFormat::BGRA | VideoFormat::BGRx => 4,
        _ => return Err(MediaError::InvalidFormat),
    };
    if width == 0 || height == 0 || width > 7680 || height > 4320 || stride <= 0 {
        return Err(MediaError::InvalidFormat);
    }
    let row = (width as usize)
        .checked_mul(channels)
        .ok_or(MediaError::InvalidFormat)?;
    let stride = stride as usize;
    if stride < row {
        return Err(MediaError::FrameFormatMismatch);
    }
    let needed = stride
        .checked_mul(height as usize - 1)
        .and_then(|value| value.checked_add(row))
        .ok_or(MediaError::FrameFormatMismatch)?;
    let end = offset
        .checked_add(length)
        .ok_or(MediaError::FrameFormatMismatch)?;
    if needed > length || end > bytes.len() {
        return Err(MediaError::FrameFormatMismatch);
    }
    let mut rgba = vec![0; width as usize * height as usize * 4];
    for y in 0..height as usize {
        let input = &bytes[offset + y * stride..offset + y * stride + row];
        for (source, target) in input
            .chunks_exact(channels)
            .zip(rgba[y * width as usize * 4..(y + 1) * width as usize * 4].chunks_exact_mut(4))
        {
            if matches!(format, VideoFormat::BGRA | VideoFormat::BGRx) {
                target[..3].copy_from_slice(&[source[2], source[1], source[0]]);
            } else {
                target[..3].copy_from_slice(&source[..3]);
            }
            target[3] = 255;
        }
    }
    xcap::image::RgbaImage::from_raw(width, height, rgba).ok_or(MediaError::FrameFormatMismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_failures_identify_phase_without_response_payloads() {
        for phase in [
            RequestPhase::CreateSession,
            RequestPhase::SelectSources,
            RequestPhase::Start,
        ] {
            let failure = response_values(
                2,
                HashMap::from([("private-source".into(), OwnedValue::from(987654u32))]),
            )
            .unwrap_err();
            let message = phase.with_error(failure).to_string();
            assert!(message.contains(phase.label()));
            assert!(message.contains("could not complete the request"));
            assert!(!message.contains("private-source"));
            assert!(!message.contains("987654"));
            assert!(!message.contains("denied"));
        }
        assert!(response_values(0, HashMap::new()).is_ok());
        assert!(
            response_values(1, HashMap::new())
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(
            response_values(u32::MAX, HashMap::new())
                .unwrap_err()
                .to_string()
                .contains("unsupported response")
        );
    }

    #[test]
    fn rejects_ambiguous_or_wrong_portal_source() {
        let stream = |kind| {
            (
                42,
                HashMap::from([("source_type".into(), OwnedValue::from(kind))]),
            )
        };
        assert!(selected_stream(vec![], 1).is_err());
        assert!(selected_stream(vec![stream(1u32), stream(1u32)], 1).is_err());
        assert!(selected_stream(vec![stream(2u32)], 1).is_err());
        assert!(selected_stream(vec![(42, HashMap::new())], 1).is_err());
        assert_eq!(selected_stream(vec![stream(1u32)], 1).unwrap(), (42, None));
        assert!(source_type("display:42").is_err());
    }

    #[test]
    fn copies_offset_and_padded_rows_without_padding_or_alpha() {
        let data = [9, 9, 30, 20, 10, 0, 8, 8, 60, 50, 40, 0];
        let image = unpack_frame(&data, 2, 10, 6, 1, 2, VideoFormat::BGRx).unwrap();
        assert_eq!(image.as_raw(), &[10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn rejects_truncated_and_overflowing_chunks() {
        for (offset, length, stride, width, height) in [
            (0, 3, 4, 1, 1),
            (usize::MAX, 4, 4, 1, 1),
            (0, 8, -4, 1, 2),
            (0, 8, 2, 1, 2),
            (0, 8, 4, 99999, 1),
        ] {
            assert!(
                unpack_frame(
                    &[0; 8],
                    offset,
                    length,
                    stride,
                    width,
                    height,
                    VideoFormat::RGBA
                )
                .is_err()
            );
        }
    }

    #[test]
    fn cancelled_request_never_polls_backend() {
        runtime().unwrap().block_on(async {
            let result = wait(
                async {
                    panic!("cancelled request polled backend");
                    #[allow(unreachable_code)]
                    Ok(())
                },
                &|| true,
                RPC_TIMEOUT,
            )
            .await;
            assert!(result.is_err());
        });
    }
}
