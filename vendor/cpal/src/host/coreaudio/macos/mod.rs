pub use self::enumerate::{default_input_device, default_output_device, Devices};
use super::{asbd_from_config, check_os_status, host_time_to_stream_instant, OSStatus};
use crate::{
    traits::{HostTrait, StreamTrait},
    Error, FrameCount, StreamInstant,
};
mod callback_gate;
mod device;
pub mod enumerate;
mod loopback;
mod owned;
pub use device::Device;
pub use owned::{CoreAudioClosePhase, OwnedCoreAudioCloseError};

#[derive(Debug)]
pub struct Host;
impl Host {
    pub fn new() -> Result<Self, Error> {
        Ok(Self)
    }
}
impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;
    fn is_available() -> bool {
        true
    }
    fn devices(&self) -> Result<Devices, Error> {
        Devices::new()
    }
    fn default_input_device(&self) -> Option<Device> {
        default_input_device()
    }
    fn default_output_device(&self) -> Option<Device> {
        default_output_device()
    }
}

pub struct Stream(owned::Stream);
impl Stream {
    fn build<F>(build: F) -> Result<Self, Error>
    where
        F: FnOnce(&mut owned::Resources) -> Result<owned::Setup, Error> + Send + 'static,
    {
        owned::Stream::build(build).map(Self)
    }
    /// Blocking native-owner close; the caller must retain a non-UI worker until this returns.
    /// A failed/timed-out close leaves its unit/listeners/thread owned and blocks replacement.
    pub fn close_owned(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(), OwnedCoreAudioCloseError> {
        self.0.close_owned(timeout)
    }
}
impl StreamTrait for Stream {
    fn play(&self) -> Result<(), Error> {
        self.0.play()
    }
    fn pause(&self) -> Result<(), Error> {
        self.0.pause()
    }
    fn buffer_size(&self) -> Result<FrameCount, Error> {
        self.0.buffer_size()
    }
    fn now(&self) -> StreamInstant {
        let time = unsafe { mach2::mach_time::mach_absolute_time() };
        host_time_to_stream_instant(time).expect("mach_timebase_info failed")
    }
}
pub(crate) fn drain_owned(timeout: std::time::Duration) -> Result<(), OwnedCoreAudioCloseError> {
    owned::drain_owned(timeout, true)
}

#[cfg(test)]
mod test {
    use crate::{
        default_host,
        traits::{DeviceTrait, HostTrait, StreamTrait},
        InputCallbackInfo, OutputCallbackInfo, Sample,
    };

    #[test]
    fn test_play() {
        let host = default_host();
        let device = host.default_output_device().unwrap();

        let mut supported_configs_range = device.supported_output_configs().unwrap();
        let supported_config = supported_configs_range
            .next()
            .unwrap()
            .with_max_sample_rate();
        let config = supported_config.config();

        let stream = device
            .build_output_stream(
                config,
                write_silence::<f32>,
                move |err| println!("Error: {err}"),
                None, // None=blocking, Some(Duration)=timeout
            )
            .unwrap();
        stream.play().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_record() {
        let host = default_host();
        let device = host.default_input_device().unwrap();
        println!("Device: {:?}", device.description());

        let mut supported_configs_range = device.supported_input_configs().unwrap();
        println!("Supported configs:");
        for config in supported_configs_range.clone() {
            println!("{:?}", config)
        }
        let supported_config = supported_configs_range
            .next()
            .unwrap()
            .with_max_sample_rate();
        let config = supported_config.config();

        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _: &InputCallbackInfo| {
                    // react to stream events and read or write stream data here.
                    println!("Got data: {:?}", &data[..25]);
                },
                move |err| println!("Error: {err}"),
                None, // None=blocking, Some(Duration)=timeout
            )
            .unwrap();
        stream.play().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_record_output() {
        if std::env::var("CI").is_ok() {
            println!("Skipping test_record_output in CI environment due to permissions");
            return;
        }

        let host = default_host();
        let device = host.default_output_device().unwrap();

        let mut supported_configs_range = device.supported_output_configs().unwrap();
        let supported_config = supported_configs_range
            .next()
            .unwrap()
            .with_max_sample_rate();
        let config = supported_config.config();

        println!("Building input stream");
        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _: &InputCallbackInfo| {
                    // react to stream events and read or write stream data here.
                    println!("Got data: {:?}", &data[..25]);
                },
                move |err| println!("Error: {err}"),
                None, // None=blocking, Some(Duration)=timeout
            )
            .unwrap();
        stream.play().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    fn write_silence<T: Sample>(data: &mut [T], _: &OutputCallbackInfo) {
        for sample in data.iter_mut() {
            *sample = Sample::EQUILIBRIUM;
        }
    }
}

pub(crate) fn drain_retired(timeout: std::time::Duration) -> Result<(), OwnedCoreAudioCloseError> {
    owned::drain_owned(timeout, false)
}
