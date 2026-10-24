use std::{
    ffi::OsString,
    fmt,
    hash::Hash,
    mem,
    os::windows::ffi::OsStringExt,
    ptr, slice,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    error::ResultExt,
    host::{com::ComString, ErrorCallbackArc},
    BufferSize, Data, DeviceDescription, DeviceDescriptionBuilder, DeviceDirection, DeviceId,
    DeviceType, Error, ErrorKind, FrameCount, InputCallbackInfo, InterfaceType, OutputCallbackInfo,
    SampleFormat, SampleRate, StreamConfig, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange, COMMON_SAMPLE_RATES,
};

use windows::{
    core::{Interface, GUID},
    Win32::{
        Devices::Properties,
        Foundation::PROPERTYKEY,
        Media::{Audio, Audio::IAudioRenderClient, KernelStreaming, Multimedia},
        System::{
            Com,
            Com::{StructuredStorage, STGM_READ},
            Variant::{VT_LPWSTR, VT_UI4},
        },
        UI::Shell::PropertiesSystem::IPropertyStore,
    },
};

use super::stream::{AudioClientFlow, DefaultDeviceMonitor, OwnedEvent, Stream, StreamInner};
pub use crate::iter::{SupportedInputConfigs, SupportedOutputConfigs};
use crate::traits::DeviceTrait;

// PKEY_AudioEndpoint properties not yet in windows-rs

/// PKEY_AudioEndpoint_FormFactor (PID 0) - VT_UI4 containing EndpointFormFactor enum
const PKEY_AUDIOENDPOINT_FORMFACTOR: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 0,
};

/// PKEY_AudioEndpoint_JackSubType (PID 8) - VT_LPWSTR containing KS node type GUID
const PKEY_AUDIOENDPOINT_JACKSUBTYPE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 8,
};

const DEFAULT_FLAGS: u32 = Audio::AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

/// Copied endpoint identity; no native interface or apartment is shared.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DeviceHandle {
    DefaultOutput,
    DefaultInput,
    Specific { id: String, flow: i32 },
}

#[derive(Clone)]
pub struct Device {
    device: DeviceHandle,
    pending: Arc<Mutex<Option<Prepared>>>,
}
impl Device {
    fn new(device: DeviceHandle) -> Self {
        Self {
            device,
            pending: Arc::new(Mutex::new(None)),
        }
    }
    fn prepare(&self) -> Result<(), Error> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| backend_error("Device request lock failed"))?;
        if pending.is_none() {
            *pending = Some(Prepared::new(self.device.clone())?);
        }
        Ok(())
    }
    fn query<T: Send + 'static>(
        &self,
        query: impl FnOnce(&NativeDevice) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        self.prepare()?;
        let pending = self
            .pending
            .lock()
            .map_err(|_| backend_error("Device request lock failed"))?;
        pending
            .as_ref()
            .ok_or_else(|| backend_error("Device owner was retired"))?
            .query(query)
    }
    fn take_prepared(&self) -> Result<Prepared, Error> {
        self.prepare()?;
        self.pending
            .lock()
            .map_err(|_| backend_error("Device request lock failed"))?
            .take()
            .ok_or_else(|| backend_error("Device owner was retired"))
    }
    pub fn description(&self) -> Result<DeviceDescription, Error> {
        let descriptor = self.device.clone();
        super::owner::query(move || NativeDevice::metadata_description(descriptor))
    }
    fn id(&self) -> Result<DeviceId, Error> {
        match &self.device {
            DeviceHandle::Specific { id, .. } => {
                Ok(DeviceId::new(crate::platform::HostId::Wasapi, id.clone()))
            }
            _ => {
                let descriptor = self.device.clone();
                super::owner::query(move || NativeDevice::metadata_id(descriptor))
            }
        }
    }
    pub(crate) fn data_flow(&self) -> Audio::EDataFlow {
        match &self.device {
            DeviceHandle::DefaultOutput => Audio::eRender,
            DeviceHandle::DefaultInput => Audio::eCapture,
            DeviceHandle::Specific { flow, .. } => Audio::EDataFlow(*flow),
        }
    }
    pub fn supported_input_configs(&self) -> Result<SupportedInputConfigs, Error> {
        self.query(NativeDevice::supported_input_configs)
    }
    pub fn supported_output_configs(&self) -> Result<SupportedOutputConfigs, Error> {
        self.query(NativeDevice::supported_output_configs)
    }
    pub fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.query(NativeDevice::default_input_config)
    }
    pub fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.query(NativeDevice::default_output_config)
    }

    /// Resolve only in the caller's ALREADY initialized COM apartment. No
    /// temporary COM lease, cached STA interface, or owner reference is exposed.
    #[deprecated(
        note = "Requires an already initialized caller COM apartment; no interface is transferred from the audio owner"
    )]
    pub fn immdevice(&self) -> Option<Audio::IMMDevice> {
        let mut kind = Com::APTTYPE::default();
        let mut qualifier = Com::APTTYPEQUALIFIER::default();
        unsafe {
            Com::CoGetApartmentType(&mut kind, &mut qualifier).ok()?;
        }
        resolve_device(&self.device).ok()
    }
}
fn backend_error(message: &'static str) -> Error {
    Error::with_message(ErrorKind::StreamInvalidated, message)
}

type NativeRequest = Box<dyn FnOnce(&NativeDevice, &super::owner::Control) -> bool + Send>;
pub(crate) struct Prepared {
    requests: std::sync::mpsc::SyncSender<NativeRequest>,
    control: super::owner::Control,
    retire_on_drop: bool,
}
impl Prepared {
    fn new(descriptor: DeviceHandle) -> Result<Self, Error> {
        let (requests, received) = std::sync::mpsc::sync_channel::<NativeRequest>(1);
        let control = super::owner::spawn(super::owner::Kind::Stream, move |control| {
            let apartment = super::apartment::Sta::new()?;
            let mut device =
                std::mem::ManuallyDrop::new(NativeDevice::new(descriptor, Some(control.clone())));
            while control.active() {
                match received.try_recv() {
                    Ok(request) => {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            request(&device, &control)
                        }));
                        match result {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(_) => control
                                .retain_failure(super::owner::WasapiCloseError::WorkerPanicked),
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        if super::apartment::wait(
                            &[control.event().handle()],
                            Duration::from_millis(20),
                        )
                        .is_err()
                        {
                            control.retain_failure(super::owner::WasapiCloseError::Apartment);
                        }
                    }
                }
            }
            // No callback/stream reference remains before the transfer origin
            // is released; this exact STA then exits before the actual TLS join.
            control.request_close();
            let device = unsafe { std::mem::ManuallyDrop::take(&mut device) };
            device.finish();
            drop(apartment);
            Ok(())
        })?;
        Ok(Self {
            requests,
            control,
            retire_on_drop: true,
        })
    }
    fn query<T: Send + 'static>(
        &self,
        query: impl FnOnce(&NativeDevice) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        self.submit(Box::new(move |device, _| {
            let result = query(device);
            let _ = send.send(result);
            true
        }))?;
        receive
            .recv()
            .map_err(|_| backend_error("Device owner did not return its query"))?
    }
    fn submit(&self, request: NativeRequest) -> Result<(), Error> {
        if !self.control.active() {
            return Err(backend_error("Device owner is retiring"));
        }
        self.requests
            .try_send(request)
            .map_err(|_| backend_error("Device owner request is busy"))?;
        self.control.event().signal().map_err(Error::from)
    }
    pub(crate) fn run(mut self, request: NativeRequest) -> Result<super::owner::Control, Error> {
        self.submit(request)?;
        self.retire_on_drop = false;
        Ok(self.control.clone())
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if self.retire_on_drop {
            self.control.request_close();
        }
    }
}

struct NativeBuild {
    client: Option<Audio::IAudioClient>,
    event: Option<OwnedEvent>,
}
impl Drop for NativeBuild {
    fn drop(&mut self) {
        drop(self.client.take());
        drop(self.event.take());
    }
}

pub(crate) struct NativeDevice {
    device: DeviceHandle,
    future_audio_client: std::cell::RefCell<Option<Audio::IAudioClient>>,
    capture_origin: std::cell::RefCell<Option<super::activation::CaptureLease>>,
    control: Option<super::owner::Control>,
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn description(&self) -> Result<DeviceDescription, Error> {
        Device::description(self)
    }

    fn id(&self) -> Result<DeviceId, Error> {
        Self::id(self)
    }

    fn supports_input(&self) -> bool {
        self.data_flow() == Audio::eCapture
    }

    fn supports_output(&self) -> bool {
        self.data_flow() == Audio::eRender
    }

    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, Error> {
        Self::supported_input_configs(self)
    }

    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        Self::supported_output_configs(self)
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        Self::default_input_config(self)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        Self::default_output_config(self)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        let prepared = self.take_prepared()?;
        let error_callback: ErrorCallbackArc = Arc::new(Mutex::new(error_callback));
        let stream = Stream::new_input(
            prepared,
            config,
            sample_format,
            timeout,
            data_callback,
            error_callback,
        )?;
        stream.signal_ready();
        Ok(stream)
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        // Keep `playback` monotonic: an underrun can saturate `buffered` to zero, pulling
        // `playback` backward.
        let data_callback = crate::host::monotonic_output_callback(data_callback);
        let prepared = self.take_prepared()?;
        let error_callback: ErrorCallbackArc = Arc::new(Mutex::new(error_callback));
        let stream = Stream::new_output(
            prepared,
            config,
            sample_format,
            timeout,
            data_callback,
            error_callback,
        )?;
        stream.signal_ready();
        Ok(stream)
    }
}

// Use RAII to make sure CoTaskMemFree is called when we are responsible for freeing.
struct WaveFormatExPtr(*mut Audio::WAVEFORMATEX);

impl Drop for WaveFormatExPtr {
    fn drop(&mut self) {
        unsafe {
            Com::CoTaskMemFree(Some(self.0 as *mut _));
        }
    }
}

// Given the audio client and format, returns whether the audio engine supports it natively in
// shared mode without format conversion.
pub unsafe fn is_format_supported(
    client: &Audio::IAudioClient,
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
) -> Result<bool, Error> {
    let mut closest_match: *mut Audio::WAVEFORMATEX = ptr::null_mut();
    let hr = client.IsFormatSupported(
        Audio::AUDCLNT_SHAREMODE_SHARED,
        waveformatex_ptr,
        Some(&mut closest_match),
    );
    if !closest_match.is_null() {
        let _free = WaveFormatExPtr(closest_match);
    }

    // S_OK (hr.0 == 0): format is natively supported, Initialize will accept it without conversion.
    // S_FALSE (hr.0 == 1): only usable when AUTOCONVERTPCM is set (output).
    Ok(hr.0 == 0)
}

// Get a cpal Format from a WAVEFORMATEX.
unsafe fn format_from_waveformatex_ptr(
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
    audio_client: &Audio::IAudioClient,
) -> Option<SupportedStreamConfig> {
    fn cmp_guid(a: &GUID, b: &GUID) -> bool {
        (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
    }
    let sample_format = match (
        (*waveformatex_ptr).wBitsPerSample,
        (*waveformatex_ptr).wFormatTag as u32,
    ) {
        (8, Audio::WAVE_FORMAT_PCM) => SampleFormat::U8,
        (16, Audio::WAVE_FORMAT_PCM) => SampleFormat::I16,
        (32, Multimedia::WAVE_FORMAT_IEEE_FLOAT) => SampleFormat::F32,
        (64, Multimedia::WAVE_FORMAT_IEEE_FLOAT) => SampleFormat::F64,
        (n_bits, KernelStreaming::WAVE_FORMAT_EXTENSIBLE) => {
            let waveformatextensible_ptr = waveformatex_ptr as *const Audio::WAVEFORMATEXTENSIBLE;
            let sub = unsafe { (*waveformatextensible_ptr).SubFormat };
            let valid_bits = unsafe { (*waveformatextensible_ptr).Samples.wValidBitsPerSample };

            if cmp_guid(&sub, &KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM) {
                match n_bits {
                    8 => SampleFormat::U8,
                    16 => SampleFormat::I16,
                    24 => SampleFormat::I24,
                    32 if valid_bits == 24 => SampleFormat::I24,
                    32 => SampleFormat::I32,
                    64 => SampleFormat::I64,
                    _ => return None,
                }
            } else if cmp_guid(&sub, &Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT) {
                match n_bits {
                    32 => SampleFormat::F32,
                    64 => SampleFormat::F64,
                    _ => return None,
                }
            } else {
                return None;
            }
        }
        // Unknown data format returned by GetMixFormat.
        _ => return None,
    };

    let sample_rate = (*waveformatex_ptr).nSamplesPerSec;

    // GetBufferSizeLimits is only used for Hardware-Offloaded Audio
    // Processing, which was added in Windows 8, which places hardware
    // limits on the size of the audio buffer. If the sound system
    // *isn't* using offloaded audio, we're using a software audio
    // processing stack and have pretty much free rein to set buffer
    // size.
    //
    // In software audio stacks GetBufferSizeLimits returns
    // AUDCLNT_E_OFFLOAD_MODE_ONLY.
    //
    // https://docs.microsoft.com/en-us/windows-hardware/drivers/audio/hardware-offloaded-audio-processing
    let (mut min_buffer_duration, mut max_buffer_duration) = (0, 0);
    let buffer_size_is_limited = audio_client
        .cast::<Audio::IAudioClient2>()
        .and_then(|audio_client| {
            audio_client.GetBufferSizeLimits(
                waveformatex_ptr,
                true,
                &mut min_buffer_duration,
                &mut max_buffer_duration,
            )
        })
        .is_ok();
    let buffer_size = if buffer_size_is_limited {
        SupportedBufferSize::Range {
            min: buffer_duration_to_frames(min_buffer_duration, sample_rate),
            max: buffer_duration_to_frames(max_buffer_duration, sample_rate),
        }
    } else {
        // Software audio stack: no hardware buffer constraint to report.
        SupportedBufferSize::Unknown
    };

    let format = SupportedStreamConfig {
        channels: (*waveformatex_ptr).nChannels as _,
        sample_rate,
        buffer_size,
        sample_format,
    };
    Some(format)
}

fn jacksubtype_to_interface_type(guid_str: &str) -> Option<InterfaceType> {
    let guid_upper = guid_str.to_uppercase();
    let typ = match guid_upper.as_str() {
        "{D9E55EA0-0C89-4692-84FF-EB3C4B0D172F}" => InterfaceType::Hdmi,
        "{E47E4031-3EA6-418D-8F9B-B73843CCB2AD}" => InterfaceType::DisplayPort,
        "{DFF21CE1-F70F-11D0-B917-00A0C9223196}" => InterfaceType::Spdif,
        _ => return None,
    };

    Some(typ)
}

/// Maps WASAPI FormFactor values to DeviceType and optionally InterfaceType.
fn form_factor_to_types(form_factor: u32) -> (DeviceType, Option<InterfaceType>) {
    match form_factor {
        0 => (DeviceType::Unknown, Some(InterfaceType::Network)), // RemoteNetworkDevice
        1 => (DeviceType::Speaker, None),                         // Speakers
        2 => (DeviceType::Unknown, Some(InterfaceType::Line)),    // LineLevel
        3 => (DeviceType::Headphones, None),                      // Headphones
        4 => (DeviceType::Microphone, None),                      // Microphone
        5 => (DeviceType::Headset, None),                         // Headset
        6 => (DeviceType::Handset, None),                         // Handset
        7 => (DeviceType::Unknown, None),                         // UnknownDigitalPassthrough
        8 => (DeviceType::Unknown, Some(InterfaceType::Spdif)),   // SPDIF
        9 => (DeviceType::Unknown, Some(InterfaceType::Hdmi)),    // DigitalAudioDisplayDevice
        _ => (DeviceType::Unknown, None), // UnknownFormFactor or future values
    }
}

/// Maps WASAPI EnumeratorName to InterfaceType.
fn enumerator_to_interface_type(enumerator: &str) -> Option<InterfaceType> {
    let typ = match enumerator.to_uppercase().as_str() {
        "HDAUDIO" => InterfaceType::BuiltIn,
        "USB" => InterfaceType::Usb,
        "BTHENUM" => InterfaceType::Bluetooth,
        "MMDEVAPI" | "SW" => InterfaceType::Virtual,
        _ => return None,
    };
    Some(typ)
}

/// Activates an `IAudioClient` via `ActivateAudioInterfaceAsync` synchronously.
///
/// Used for virtual default-device GUIDs (`DEVINTERFACE_AUDIO_RENDER`/`DEVINTERFACE_AUDIO_CAPTURE`)
/// so that the Windows audio engine automatically reroutes the stream when the system default
/// device changes.
impl NativeDevice {
    pub fn description(&self) -> Result<DeviceDescription, Error> {
        let device = self.immdevice().ok_or_else(|| {
            Error::with_message(ErrorKind::DeviceNotAvailable, "Default device not found")
        })?;
        unsafe {
            // Open the device's property store.
            let property_store = device
                .OpenPropertyStore(STGM_READ)
                .expect("could not open property store");

            // Query all available properties
            let friendly_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_FriendlyName as *const _ as *const _,
            );

            let device_desc = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_DeviceDesc as *const _ as *const _,
            );

            let interface_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_DeviceInterface_FriendlyName as *const _ as *const _,
            );

            let enumerator_name = get_property_string(
                &property_store,
                &Properties::DEVPKEY_Device_EnumeratorName as *const _ as *const _,
            );

            let form_factor = get_property_u32(
                &property_store,
                &PKEY_AUDIOENDPOINT_FORMFACTOR as *const _ as *const _,
            );

            let jack_subtype = get_property_string(
                &property_store,
                &PKEY_AUDIOENDPOINT_JACKSUBTYPE as *const _ as *const _,
            );

            // Prefer FriendlyName for name (e.g., "Speakers (XYZ Audio Adapter)"), fall back to DeviceDesc
            let name = friendly_name.or(device_desc).ok_or_else(|| {
                Error::with_message(
                    ErrorKind::DeviceNotAvailable,
                    "Failed to retrieve device name",
                )
            })?;

            // Get direction from data flow (eCapture = Input, eRender = Output)
            let direction = self.data_flow().into();

            // Determine device_type and initial interface_type from FormFactor
            let (device_type, mut interface_type) = form_factor
                .map(form_factor_to_types)
                .unwrap_or((DeviceType::Unknown, None));

            // Override interface_type from EnumeratorName if available
            if let Some(ref enumerator) = enumerator_name {
                if let Some(itype) = enumerator_to_interface_type(enumerator) {
                    interface_type = Some(itype);
                }
            }

            // JackSubType has highest priority for interface_type
            if let Some(ref jack_guid) = jack_subtype {
                if let Some(itype) = jacksubtype_to_interface_type(jack_guid) {
                    interface_type = Some(itype);
                }
            }

            let mut builder = DeviceDescriptionBuilder::new(name)
                .direction(direction)
                .device_type(device_type);

            if let Some(itype) = interface_type {
                builder = builder.interface_type(itype);
            }

            // Add interface name to driver field if available
            if let Some(iface_name) = interface_name {
                builder = builder.driver(iface_name);
            }

            Ok(builder.build())
        }
    }

    fn id(&self) -> Result<DeviceId, Error> {
        let device = self.immdevice().ok_or_else(|| {
            Error::with_message(ErrorKind::DeviceNotAvailable, "Default device not found")
        })?;
        unsafe {
            match device.GetId() {
                Ok(pwstr) => {
                    let _id = ComString(pwstr);
                    match pwstr.to_string() {
                        Ok(id_str) => Ok(DeviceId::new(crate::platform::HostId::Wasapi, id_str)),
                        Err(e) => Err(Error::with_message(
                            ErrorKind::BackendError,
                            format!("Failed to convert device ID to string: {e}"),
                        )),
                    }
                }
                Err(e) => Err(Error::from(e)),
            }
        }
    }

    fn new(device: DeviceHandle, control: Option<super::owner::Control>) -> Self {
        Self {
            device,
            control,
            future_audio_client: std::cell::RefCell::new(None),
            capture_origin: std::cell::RefCell::new(None),
        }
    }
    fn metadata_description(device: DeviceHandle) -> Result<DeviceDescription, Error> {
        Self::new(device, None).description()
    }
    fn metadata_id(device: DeviceHandle) -> Result<DeviceId, Error> {
        Self::new(device, None).id()
    }
    fn immdevice(&self) -> Option<Audio::IMMDevice> {
        resolve_device(&self.device).ok()
    }
    fn finish(self) {
        drop(self.future_audio_client.into_inner());
        if let Some(origin) = self.capture_origin.into_inner() {
            origin.close();
        }
    }

    /// Creates a `DefaultDeviceMonitor` for default-device streams, or `None` for specific devices.
    pub(crate) fn default_device_monitor(&self) -> Result<Option<DefaultDeviceMonitor>, Error> {
        let flow = match &self.device {
            DeviceHandle::DefaultOutput => Audio::eRender,
            DeviceHandle::DefaultInput => Audio::eCapture,
            DeviceHandle::Specific { .. } => return Ok(None),
        };
        let enumerator = get_enumerator()?;
        DefaultDeviceMonitor::new(
            enumerator,
            flow,
            self.control
                .as_ref()
                .ok_or_else(|| backend_error("Missing stream owner"))?,
        )
        .map(Some)
    }

    fn ensure_future_audio_client(
        &self,
        activation_timeout: Option<Duration>,
    ) -> Result<std::cell::RefMut<'_, Option<Audio::IAudioClient>>, Error> {
        if self.future_audio_client.borrow().is_none() {
            let control = self
                .control
                .as_ref()
                .ok_or_else(|| backend_error("Missing audio owner"))?;
            if !control.active() {
                return Err(backend_error("Audio activation was cancelled"));
            }
            let client = match &self.device {
                DeviceHandle::DefaultOutput => {
                    super::activation::render(control, activation_timeout)?
                }
                DeviceHandle::DefaultInput => {
                    let (client, origin) = super::activation::CaptureLease::acquire(control)?;
                    *self.capture_origin.borrow_mut() = Some(origin);
                    client
                }
                DeviceHandle::Specific { .. } => unsafe {
                    resolve_device(&self.device)?
                        .Activate(Com::CLSCTX_ALL, None)
                        .map_err(Error::from)?
                },
            };
            *self.future_audio_client.borrow_mut() = Some(client);
        }
        Ok(self.future_audio_client.borrow_mut())
    }

    /// Returns an uninitialized `IAudioClient`.
    pub(crate) fn build_audioclient(
        &self,
        activation_timeout: Option<Duration>,
    ) -> Result<Audio::IAudioClient, Error> {
        let mut lock = self.ensure_future_audio_client(activation_timeout)?;
        // ensure_future_audio_client always sets the Option to Some before returning Ok.
        Ok(lock
            .take()
            .ok_or_else(|| backend_error("Audio client was retired"))?)
    }

    // There is no way to query the list of all formats that are supported by the
    // audio processor, so instead we just trial some commonly supported formats.
    //
    // Common formats are trialed by first getting the default format (returned via
    // `GetMixFormat`) and then mutating that format with common sample rates and
    // querying them via `IsFormatSupported`.
    //
    // When calling `IsFormatSupported` with the shared-mode audio engine, only the default
    // number of channels seems to be supported. Any, more or less returns an invalid
    // parameter error. Thus, we just assume that the default number of channels is the only
    // number supported.
    fn supported_formats(&self) -> Result<SupportedInputConfigs, Error> {
        // initializing COM because we call `CoTaskMemFree` to release the format.

        // Retrieve the `IAudioClient`.
        let lock = self
            .ensure_future_audio_client(None)
            .context("Failed to get audio client")?;
        // ensure_future_audio_client always sets the Option to Some before returning Ok.
        let client = lock
            .as_ref()
            .ok_or_else(|| backend_error("Audio client was retired"))?;

        unsafe {
            // Retrieve the pointer to the default WAVEFORMATEX.
            let default_waveformatex_ptr = client
                .GetMixFormat()
                .map(WaveFormatExPtr)
                .context("Failed to get mix format")?;

            // If the default format can't succeed we have no hope of finding other formats.
            if !is_format_supported(client, default_waveformatex_ptr.0)? {
                return Err(Error::with_message(
                    ErrorKind::UnsupportedConfig,
                    "Could not determine support for default audio format",
                ));
            }

            let format = match format_from_waveformatex_ptr(default_waveformatex_ptr.0, client) {
                Some(fmt) => fmt,
                None => {
                    return Err(Error::with_message(
                        ErrorKind::UnsupportedConfig,
                        "Default audio format could not be mapped to a supported configuration",
                    ));
                }
            };

            // Output streams use AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM so Initialize accepts any
            // format regardless of what IsFormatSupported returns. Capture streams do not;
            // only native formats will work.
            let is_output = self.data_flow() == Audio::eRender;

            // For output, restrict to rates the MF Resampler can handle; for capture, probe all.
            let mut sample_rates: Vec<SampleRate> = COMMON_SAMPLE_RATES
                .iter()
                .copied()
                .filter(|&r| {
                    !is_output || (OUTPUT_MIN_SAMPLE_RATE..=OUTPUT_MAX_SAMPLE_RATE).contains(&r)
                })
                .collect();
            if !sample_rates.contains(&format.sample_rate) {
                sample_rates.push(format.sample_rate);
            }

            let mut default_period_hns: i64 = 0;
            let device_period_hns = if client
                .GetDevicePeriod(Some(&mut default_period_hns), None)
                .is_ok()
                && default_period_hns > 0
            {
                Some(default_period_hns)
            } else {
                None
            };

            let mut supported_formats = Vec::new();
            for sample_rate in sample_rates {
                let buffer_size = match format.buffer_size {
                    // Software stacks: substitute the device period expressed in frames
                    // at this sample rate.
                    SupportedBufferSize::Unknown => device_period_hns
                        .map(|p_hns| {
                            let frames = buffer_duration_to_frames(p_hns, sample_rate);
                            SupportedBufferSize::Range {
                                min: frames,
                                max: frames,
                            }
                        })
                        .unwrap_or(SupportedBufferSize::Unknown),
                    // Hardware stacks: report the hardware buffer size limits as-is.
                    other => other,
                };

                for sample_format in WAVEFORMATEXTENSIBLE_SAMPLE_FORMATS {
                    if let Some(waveformat) = config_to_waveformatextensible(
                        StreamConfig {
                            channels: format.channels,
                            sample_rate,
                            buffer_size: BufferSize::Default,
                        },
                        sample_format,
                    ) {
                        let usable = is_output
                            || is_format_supported(
                                client,
                                &waveformat.Format as *const Audio::WAVEFORMATEX,
                            )?;
                        if usable {
                            supported_formats.push(SupportedStreamConfigRange {
                                channels: format.channels,
                                min_sample_rate: sample_rate,
                                max_sample_rate: sample_rate,
                                buffer_size,
                                sample_format,
                            });
                        }
                    }
                }
            }
            Ok(supported_formats.into_iter())
        }
    }

    pub fn supported_input_configs(&self) -> Result<SupportedInputConfigs, Error> {
        if self.data_flow() == Audio::eCapture {
            self.supported_formats()
        // If it's an output device, assume no input formats.
        } else {
            Ok(vec![].into_iter())
        }
    }

    pub fn supported_output_configs(&self) -> Result<SupportedOutputConfigs, Error> {
        if self.data_flow() == Audio::eRender {
            self.supported_formats()
        // If it's an input device, assume no output formats.
        } else {
            Ok(vec![].into_iter())
        }
    }

    // We always create voices in shared mode, therefore all samples go through an audio
    // processor to mix them together.
    //
    // One format is guaranteed to be supported, the one returned by `GetMixFormat`.
    fn default_format(&self) -> Result<SupportedStreamConfig, Error> {
        // initializing COM because we call `CoTaskMemFree`

        let lock = self
            .ensure_future_audio_client(None)
            .context("Failed to get audio client")?;
        // ensure_future_audio_client always sets the Option to Some before returning Ok.
        let client = lock
            .as_ref()
            .ok_or_else(|| backend_error("Audio client was retired"))?;

        unsafe {
            let format_ptr = client
                .GetMixFormat()
                .map(WaveFormatExPtr)
                .context("Failed to get mix format")?;

            let mut config =
                format_from_waveformatex_ptr(format_ptr.0, client).ok_or_else(|| {
                    Error::with_message(
                        ErrorKind::UnsupportedConfig,
                        "Device audio format could not be mapped to a supported format",
                    )
                })?;

            if config.buffer_size == SupportedBufferSize::Unknown {
                let mut default_period_hns: i64 = 0;
                if client
                    .GetDevicePeriod(Some(&mut default_period_hns), None)
                    .is_ok()
                    && default_period_hns > 0
                {
                    let frames = buffer_duration_to_frames(default_period_hns, config.sample_rate);
                    config.buffer_size = SupportedBufferSize::Range {
                        min: frames,
                        max: frames,
                    };
                }
            }
            Ok(config)
        }
    }

    pub(crate) fn data_flow(&self) -> Audio::EDataFlow {
        match &self.device {
            DeviceHandle::DefaultOutput => Audio::eRender,
            DeviceHandle::DefaultInput => Audio::eCapture,
            DeviceHandle::Specific { flow, .. } => Audio::EDataFlow(*flow),
        }
    }

    pub fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        if self.data_flow() == Audio::eCapture {
            self.default_format()
        } else {
            Err(Error::with_message(
                ErrorKind::UnsupportedOperation,
                "Device does not support input",
            ))
        }
    }

    pub fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        let data_flow = self.data_flow();
        if data_flow == Audio::eRender {
            self.default_format()
        } else {
            Err(Error::with_message(
                ErrorKind::UnsupportedOperation,
                "Device does not support output",
            ))
        }
    }

    pub(crate) fn build_input_stream_raw_inner(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        activation_timeout: Option<Duration>,
    ) -> Result<StreamInner, Error> {
        if !self
            .control
            .as_ref()
            .is_some_and(super::owner::Control::active)
        {
            return Err(backend_error("Audio construction was cancelled"));
        }
        crate::validate_stream_config(&config)?;
        unsafe {
            // Making sure that COM is initialized.
            // It's not actually sure that this is required, but when in doubt do it.

            // Obtaining a `IAudioClient`.
            let audio_client = self
                .build_audioclient(activation_timeout)
                .context("Failed to build audio client")?;
            let mut native_build = NativeBuild {
                client: Some(audio_client),
                event: None,
            };
            let audio_client = native_build
                .client
                .as_ref()
                .ok_or_else(|| backend_error("Audio client missing"))?;

            // No further range validation: IAudioClient::Initialize accepts any positive duration
            // in shared mode. The callback period is always GetDevicePeriod() regardless of what
            // is requested here; the value only affects ring-buffer latency.
            let buffer_duration = buffer_size_to_duration(&config.buffer_size, config.sample_rate);

            let mut stream_flags = DEFAULT_FLAGS;

            if self.data_flow() == Audio::eRender {
                stream_flags |= Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;
            }

            // Computing the format and initializing the device.
            let waveformatex = {
                let format_attempt = config_to_waveformatextensible(config, sample_format)
                    .ok_or_else(|| {
                        Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Stream configuration could not be converted to a compatible format",
                        )
                    })?;
                let share_mode = Audio::AUDCLNT_SHAREMODE_SHARED;

                // Finally, initializing the audio client
                audio_client
                    .Initialize(
                        share_mode,
                        stream_flags,
                        buffer_duration,
                        0,
                        &format_attempt.Format,
                        None,
                    )
                    .context("Failed to initialize audio client")?;

                format_attempt.Format
            };

            // obtaining the size of the samples buffer in number of frames
            let max_frames_in_buffer = audio_client
                .GetBufferSize()
                .context("Failed to get buffer size")?;

            let period_frames =
                shared_mode_period_frames(&audio_client, config.sample_rate, max_frames_in_buffer);

            // Creating the event that will be signalled whenever we need to submit some samples.
            native_build.event = Some(OwnedEvent::new().context("Failed to create event")?);
            let event = native_build
                .event
                .as_ref()
                .ok_or_else(|| backend_error("Audio event missing"))?;

            audio_client
                .SetEventHandle(event.handle())
                .context("Failed to set event handle")?;

            // Building a `IAudioCaptureClient` that will be used to read captured samples.
            let capture_client = audio_client
                .GetService::<Audio::IAudioCaptureClient>()
                .context("Failed to get capture client")?;

            // Once we built the `StreamInner`, we add a command that will be picked up by the
            // `run()` method and added to the `RunContext`.
            let client_flow = AudioClientFlow::Capture { capture_client };

            let audio_clock = audio_client
                .GetService::<Audio::IAudioClock>()
                .context("Failed to get audio clock")?;

            let stream_latency = {
                let hns = audio_client
                    .GetStreamLatency()
                    .context("Failed to get stream latency")?;
                Duration::from_nanos(hns.max(0) as u64 * 100)
            };

            Ok(StreamInner {
                audio_client: native_build
                    .client
                    .take()
                    .ok_or_else(|| backend_error("Audio client missing"))?,
                audio_clock,
                client_flow,
                event: native_build
                    .event
                    .take()
                    .ok_or_else(|| backend_error("Audio event missing"))?,
                playing: false,
                max_frames_in_buffer,
                period_frames,
                bytes_per_frame: waveformatex.nBlockAlign,
                config,
                sample_format,
                stream_latency,
                cleanup_failure: std::cell::Cell::new(None),
            })
        }
    }

    pub(crate) fn build_output_stream_raw_inner(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        activation_timeout: Option<Duration>,
    ) -> Result<StreamInner, Error> {
        if !self
            .control
            .as_ref()
            .is_some_and(super::owner::Control::active)
        {
            return Err(backend_error("Audio construction was cancelled"));
        }
        crate::validate_stream_config(&config)?;
        unsafe {
            // Making sure that COM is initialized.
            // It's not actually sure that this is required, but when in doubt do it.

            // Obtaining a `IAudioClient`.
            let audio_client = self
                .build_audioclient(activation_timeout)
                .context("Failed to build audio client")?;
            let mut native_build = NativeBuild {
                client: Some(audio_client),
                event: None,
            };
            let audio_client = native_build
                .client
                .as_ref()
                .ok_or_else(|| backend_error("Audio client missing"))?;

            // No further range validation: IAudioClient::Initialize accepts any positive duration
            // in shared mode. The callback period is always GetDevicePeriod() regardless of what
            // is requested here; the value only affects ring-buffer latency.
            let buffer_duration = buffer_size_to_duration(&config.buffer_size, config.sample_rate);

            // Computing the format and initializing the device.
            let waveformatex = {
                let format_attempt = config_to_waveformatextensible(config, sample_format)
                    .ok_or_else(|| {
                        Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Stream configuration could not be converted to a compatible format",
                        )
                    })?;
                let share_mode = Audio::AUDCLNT_SHAREMODE_SHARED;

                // Finally, initializing the audio client
                audio_client
                    .Initialize(
                        share_mode,
                        DEFAULT_FLAGS
                            | Audio::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                            | Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                        buffer_duration,
                        0,
                        &format_attempt.Format,
                        None,
                    )
                    .context("Failed to initialize audio client")?;

                format_attempt.Format
            };

            // Creating the event that will be signalled whenever we need to submit some samples.
            native_build.event = Some(OwnedEvent::new().context("Failed to create event")?);
            let event = native_build
                .event
                .as_ref()
                .ok_or_else(|| backend_error("Audio event missing"))?;

            audio_client
                .SetEventHandle(event.handle())
                .context("Failed to set event handle")?;

            // obtaining the size of the samples buffer in number of frames
            let max_frames_in_buffer = audio_client
                .GetBufferSize()
                .context("Failed to get buffer size")?;

            let period_frames =
                shared_mode_period_frames(&audio_client, config.sample_rate, max_frames_in_buffer);

            // Building a `IAudioRenderClient` that will be used to fill the samples buffer.
            let render_client = audio_client
                .GetService::<IAudioRenderClient>()
                .context("Failed to get render client")?;

            // Once we built the `StreamInner`, we add a command that will be picked up by the
            // `run()` method and added to the `RunContext`.
            let client_flow = AudioClientFlow::Render { render_client };

            let audio_clock = audio_client
                .GetService::<Audio::IAudioClock>()
                .context("Failed to get audio clock")?;

            let stream_latency = {
                let hns = audio_client
                    .GetStreamLatency()
                    .context("Failed to get stream latency")?;
                Duration::from_nanos(hns.max(0) as u64 * 100)
            };

            Ok(StreamInner {
                audio_client: native_build
                    .client
                    .take()
                    .ok_or_else(|| backend_error("Audio client missing"))?,
                audio_clock,
                client_flow,
                event: native_build
                    .event
                    .take()
                    .ok_or_else(|| backend_error("Audio event missing"))?,
                playing: false,
                max_frames_in_buffer,
                period_frames,
                bytes_per_frame: waveformatex.nBlockAlign,
                config,
                sample_format,
                stream_latency,
                cleanup_failure: std::cell::Cell::new(None),
            })
        }
    }
}

impl PartialEq for Device {
    fn eq(&self, other: &Self) -> bool {
        self.device == other.device
    }
}
impl Eq for Device {}
impl std::hash::Hash for Device {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.device.hash(state);
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let desc = self.description().map_err(|_| fmt::Error)?;
        f.write_str(desc.name())
    }
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Device")
            .field("device", &self.device)
            .field("description", &self.description())
            .finish()
    }
}

fn get_enumerator() -> Result<Audio::IMMDeviceEnumerator, Error> {
    unsafe {
        Com::CoCreateInstance(&Audio::MMDeviceEnumerator, None, Com::CLSCTX_ALL)
            .map_err(Error::from)
    }
}
fn resolve_device(descriptor: &DeviceHandle) -> Result<Audio::IMMDevice, Error> {
    let enumerator = get_enumerator()?;
    unsafe {
        match descriptor {
            DeviceHandle::DefaultOutput => {
                enumerator.GetDefaultAudioEndpoint(Audio::eRender, Audio::eConsole)
            }
            DeviceHandle::DefaultInput => {
                enumerator.GetDefaultAudioEndpoint(Audio::eCapture, Audio::eConsole)
            }
            DeviceHandle::Specific { id, .. } => {
                let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr()))
            }
        }
        .map_err(Error::from)
    }
}
pub(super) fn current_default_endpoint(flow: Audio::EDataFlow) -> Option<Audio::IMMDevice> {
    unsafe {
        get_enumerator()
            .ok()?
            .GetDefaultAudioEndpoint(flow, Audio::eConsole)
            .ok()
    }
}

// Helper function to query a DWORD property from a WASAPI device property store
unsafe fn get_property_u32(
    property_store: &IPropertyStore,
    property_key: *const PROPERTYKEY,
) -> Option<u32> {
    let mut property_value = property_store.GetValue(property_key).ok()?;
    let prop_variant = &property_value.Anonymous.Anonymous;

    // Check if it's a UI4 (unsigned 32-bit integer)
    if prop_variant.vt != VT_UI4 {
        return None;
    }

    let value = *(&prop_variant.Anonymous as *const _ as *const u32);

    // Clean up the property
    StructuredStorage::PropVariantClear(&mut property_value).ok();

    Some(value)
}

// Helper function to query a string property from a WASAPI device property store
unsafe fn get_property_string(
    property_store: &IPropertyStore,
    property_key: *const PROPERTYKEY,
) -> Option<String> {
    let mut property_value = property_store.GetValue(property_key).ok()?;
    let prop_variant = &property_value.Anonymous.Anonymous;

    // Read the string from the union data field, expecting a *const u16.
    if prop_variant.vt != VT_LPWSTR {
        return None;
    }
    let ptr_utf16 = *(&prop_variant.Anonymous as *const _ as *const *const u16);

    // Find the length of the null-terminated string with a safety limit
    const MAX_STRING_LEN: usize = 32768; // 32K characters should be more than enough
    let mut len = 0;
    while len < MAX_STRING_LEN && *ptr_utf16.add(len) != 0 {
        len += 1;
    }

    // If we hit the limit, the string is likely malformed (not null-terminated)
    if len >= MAX_STRING_LEN {
        return None;
    }

    // Create the utf16 slice and convert it into a string.
    let string_slice = slice::from_raw_parts(ptr_utf16, len);
    let os_string: OsString = OsStringExt::from_wide(string_slice);
    let result = match os_string.into_string() {
        Ok(string) => Some(string),
        Err(os_string) => Some(os_string.to_string_lossy().into()),
    };

    // Clean up the property.
    StructuredStorage::PropVariantClear(&mut property_value).ok();

    result
}

/// Catalogue contains copied identities only, never apartment-bound objects.
pub struct Devices {
    devices: std::vec::IntoIter<Device>,
}
impl Devices {
    pub fn new() -> Result<Self, Error> {
        let descriptors = super::owner::query(|| unsafe {
            let collection =
                get_enumerator()?.EnumAudioEndpoints(Audio::eAll, Audio::DEVICE_STATE_ACTIVE)?;
            let count = collection.GetCount()?;
            if count > 512 {
                return Err(backend_error("Too many audio endpoints"));
            }
            let mut descriptors = Vec::with_capacity(count as usize);
            for index in 0..count {
                let device = collection.Item(index)?;
                let id = device.GetId()?;
                let _id = ComString(id);
                let text = id.to_string().map_err(|_| {
                    Error::with_message(ErrorKind::BackendError, "Invalid audio endpoint identity")
                })?;
                if text.is_empty() || text.encode_utf16().count() > 16384 || text.contains('\0') {
                    return Err(backend_error("Invalid audio endpoint identity"));
                }
                let endpoint: Audio::IMMEndpoint = device.cast()?;
                let flow = endpoint.GetDataFlow()?;
                if flow != Audio::eCapture && flow != Audio::eRender {
                    return Err(backend_error("Unknown audio endpoint direction"));
                }
                descriptors.push(DeviceHandle::Specific {
                    id: text,
                    flow: flow.0,
                });
            }
            Ok(descriptors)
        })?;
        Ok(Self {
            devices: descriptors
                .into_iter()
                .map(Device::new)
                .collect::<Vec<_>>()
                .into_iter(),
        })
    }
}
impl Iterator for Devices {
    type Item = Device;
    fn next(&mut self) -> Option<Device> {
        self.devices.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.devices.size_hint()
    }
}
pub fn default_input_device() -> Option<Device> {
    super::owner::query(|| Ok(current_default_endpoint(Audio::eCapture).is_some()))
        .ok()?
        .then(|| Device::new(DeviceHandle::DefaultInput))
}
pub fn default_output_device() -> Option<Device> {
    super::owner::query(|| Ok(current_default_endpoint(Audio::eRender).is_some()))
        .ok()?
        .then(|| Device::new(DeviceHandle::DefaultOutput))
}

impl From<Audio::EDataFlow> for DeviceDirection {
    fn from(data_flow: Audio::EDataFlow) -> Self {
        if data_flow == Audio::eCapture {
            DeviceDirection::Input
        } else if data_flow == Audio::eRender {
            DeviceDirection::Output
        } else {
            DeviceDirection::Unknown
        }
    }
}

// Sample rate range supported by the Media Foundation Resampler MFT used by AUTOCONVERTPCM.
const OUTPUT_MIN_SAMPLE_RATE: SampleRate = 8_000;
const OUTPUT_MAX_SAMPLE_RATE: SampleRate = 384_000;

// Formats encodable as WAVEFORMATEXTENSIBLE. U8/I16 map to WAVE_FORMAT_PCM; the rest use
// WAVE_FORMAT_EXTENSIBLE. Unsigned formats wider than 8 bits are omitted: KSDATAFORMAT_SUBTYPE_PCM
// is always signed for 16-bit and wider, so submitting unsigned data would produce a DC offset.
const WAVEFORMATEXTENSIBLE_SAMPLE_FORMATS: [SampleFormat; 7] = [
    SampleFormat::U8,
    SampleFormat::I16,
    SampleFormat::I24,
    SampleFormat::I32,
    SampleFormat::I64,
    SampleFormat::F32,
    SampleFormat::F64,
];

// Turns a `Format` into a `WAVEFORMATEXTENSIBLE`.
//
// Returns `None` if the WAVEFORMATEXTENSIBLE does not support the given format.
fn config_to_waveformatextensible(
    config: StreamConfig,
    sample_format: SampleFormat,
) -> Option<Audio::WAVEFORMATEXTENSIBLE> {
    let format_tag = match sample_format {
        SampleFormat::U8 | SampleFormat::I16 => Audio::WAVE_FORMAT_PCM,

        SampleFormat::I24
        | SampleFormat::I32
        | SampleFormat::I64
        | SampleFormat::F32
        | SampleFormat::F64 => KernelStreaming::WAVE_FORMAT_EXTENSIBLE,

        _ => return None,
    };
    let channels = config.channels;
    let sample_rate = config.sample_rate;
    let sample_bytes = sample_format.sample_size() as u16;
    let avg_bytes_per_sec = u32::from(channels) * sample_rate * u32::from(sample_bytes);
    let block_align = channels * sample_bytes;
    // wBitsPerSample is the container word size; wValidBitsPerSample is the actual bit depth.
    // For I24 the container is 32 bits (sample_size() == 4) but only 24 bits are significant.
    let container_bits = 8 * sample_bytes;
    let valid_bits = sample_format.bits_per_sample() as u16;

    let cb_size = if format_tag == Audio::WAVE_FORMAT_PCM {
        0
    } else {
        let extensible_size = mem::size_of::<Audio::WAVEFORMATEXTENSIBLE>();
        let ex_size = mem::size_of::<Audio::WAVEFORMATEX>();
        (extensible_size - ex_size) as u16
    };

    let waveformatex = Audio::WAVEFORMATEX {
        wFormatTag: format_tag as u16,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: avg_bytes_per_sec,
        nBlockAlign: block_align,
        wBitsPerSample: container_bits,
        cbSize: cb_size,
    };

    // CPAL does not care about speaker positions, so pass audio right through.
    let channel_mask = KernelStreaming::KSAUDIO_SPEAKER_DIRECTOUT;

    let sub_format = match sample_format {
        SampleFormat::U8
        | SampleFormat::I16
        | SampleFormat::I24
        | SampleFormat::I32
        | SampleFormat::I64 => KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM,

        SampleFormat::F32 | SampleFormat::F64 => Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        _ => return None,
    };

    let waveformatextensible = Audio::WAVEFORMATEXTENSIBLE {
        Format: waveformatex,
        Samples: Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: channel_mask,
        SubFormat: sub_format,
    };

    Some(waveformatextensible)
}

/// Get the default device period in frames for a shared-mode stream.
fn shared_mode_period_frames(
    audio_client: &Audio::IAudioClient,
    sample_rate: SampleRate,
    max_frames_in_buffer: FrameCount,
) -> FrameCount {
    let mut default_period = 0i64;
    if unsafe { audio_client.GetDevicePeriod(Some(&mut default_period), None) }.is_ok()
        && default_period > 0
    {
        buffer_duration_to_frames(default_period, sample_rate)
    } else {
        max_frames_in_buffer
    }
}

fn buffer_size_to_duration(buffer_size: &BufferSize, sample_rate: SampleRate) -> i64 {
    match buffer_size {
        // Round: a frame count and its 100ns duration are not exact multiples, so truncating here
        // and again on the way back drops common sizes (512, 1024, ...) by one frame.
        BufferSize::Fixed(frames) => {
            let rate = sample_rate as i64;
            (*frames as i64 * (1_000_000_000 / 100) + rate / 2) / rate
        }
        BufferSize::Default => 0,
    }
}

fn buffer_duration_to_frames(buffer_duration: i64, sample_rate: SampleRate) -> FrameCount {
    ((buffer_duration * sample_rate as i64 * 100 + 500_000_000) / 1_000_000_000) as FrameCount
}
