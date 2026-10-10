use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{TrySendError, sync_channel},
    },
    thread,
    time::Duration,
};

use morpheus_native_video_image_poc::{rgb_buffer, synthetic_frame};
use morpheus_rtc::VideoRotation;
use slint::{ComponentHandle, Image, Rgb8Pixel, SharedPixelBuffer, Timer, TimerMode};

slint::slint! {
    export component VideoWindow inherits Window {
        title: "Morpheus native video image proof";
        preferred-width: 700px;
        preferred-height: 490px;
        background: #0c1220;
        in-out property <image> frame;
        in-out property <string> status: "Waiting for first synthetic frame";

        VerticalLayout {
            padding: 18px;
            spacing: 12px;
            Text {
                text: "Native I420 → RGB → Slint Image";
                color: #e8eeff;
                font-size: 19px;
            }
            Rectangle {
                vertical-stretch: 1;
                background: #080b14;
                border-radius: 10px;
                Image {
                    width: parent.width;
                    height: parent.height;
                    source: root.frame;
                    image-fit: contain;
                }
            }
            Text {
                text: root.status;
                color: #a5f3cf;
                font-size: 13px;
            }
            Text {
                text: "Synthetic pixels only · No call, camera, screen capture, or WebView";
                color: #9ba8c8;
                font-size: 12px;
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let window = VideoWindow::new()?;
    let (sender, receiver) = sync_channel::<(SharedPixelBuffer<Rgb8Pixel>, u32, VideoRotation)>(1);
    let running = Arc::new(AtomicBool::new(true));
    let worker_running = Arc::clone(&running);
    let worker = thread::spawn(move || {
        let mut index = 0;
        while worker_running.load(Ordering::Relaxed) {
            let source = synthetic_frame(index);
            if let Ok(pixels) = rgb_buffer(&source) {
                match sender.try_send((pixels, index, source.rotation)) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            index += 1;
            thread::sleep(Duration::from_millis(66));
        }
    });

    let weak = window.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
        if let Ok((pixels, index, rotation)) = receiver.try_recv() {
            if let Some(window) = weak.upgrade() {
                window.set_frame(Image::from_rgb8(pixels));
                let degrees = match rotation {
                    VideoRotation::Deg0 => 0,
                    VideoRotation::Deg90 => 90,
                    VideoRotation::Deg180 => 180,
                    VideoRotation::Deg270 => 270,
                };
                window.set_status(
                    format!("Frame {index} · {degrees}° rotation · 320×180 input").into(),
                );
            }
        }
    });

    let result = window.run();
    running.store(false, Ordering::Relaxed);
    worker.join().expect("synthetic frame worker panicked");
    result?;
    Ok(())
}
