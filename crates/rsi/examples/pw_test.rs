#[cfg(not(feature = "pipewire-audio"))]
fn main() {
    eprintln!("PipeWire example disabled. Rebuild with --features pipewire-audio to run it.");
}

#[cfg(feature = "pipewire-audio")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    pipewire_example::run()
}

#[cfg(feature = "pipewire-audio")]
mod pipewire_example {
    use pipewire::spa;
    use pipewire::{
        context::ContextRc,
        keys,
        main_loop::MainLoopRc,
        properties,
        stream::{StreamBox, StreamFlags},
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        pipewire::init();

        let mainloop = MainLoopRc::new(None)?;
        let context = ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        let props = properties::properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_ROLE => "Music",
            *keys::STREAM_CAPTURE_SINK => "true",
        };

        let stream = StreamBox::new(&core, "test-capture", props)?;

        let frame_count = Arc::new(AtomicU64::new(0));
        let frame_count_clone = frame_count.clone();
        let byte_count = Arc::new(AtomicU64::new(0));
        let byte_count_clone = byte_count.clone();

        let buffer: Vec<f32> = Vec::new();

        let _listener = stream
            .add_local_listener_with_user_data(buffer)
            .state_changed(|_, _, old, new| {
                eprintln!("State: {:?} -> {:?}", old, new);
            })
            .param_changed(|_, _, id, param| {
                eprintln!("Param changed: id={}", id);
                if param.is_some() {
                    eprintln!("  (has param data)");
                }
            })
            .process(move |stream, _buf: &mut Vec<f32>| {
                frame_count_clone.fetch_add(1, Ordering::Relaxed);
                if let Some(mut buffer) = stream.dequeue_buffer() {
                    for data in buffer.datas_mut() {
                        if let Some(slice) = data.data() {
                            byte_count_clone.fetch_add(slice.len() as u64, Ordering::Relaxed);
                        }
                    }
                }
            })
            .register()?;

        let obj = spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaType,
                Id,
                spa::param::format::MediaType::Audio
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaSubtype,
                Id,
                spa::param::format::MediaSubtype::Raw
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::AudioFormat,
                Choice,
                Enum,
                Id,
                spa::param::audio::AudioFormat::F32LE,
                spa::param::audio::AudioFormat::F32LE,
                spa::param::audio::AudioFormat::S16LE
            ),
        );

        let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(obj),
        )
        .unwrap()
        .0
        .into_inner();
        let mut params = [spa::pod::Pod::from_bytes(&values).unwrap()];

        stream.connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;

        eprintln!("Stream connected, running for 3 seconds...");

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let mainloop_weak = mainloop.downgrade();
        let timer = mainloop.loop_().add_timer(move |_| {
            if shutdown_clone.load(Ordering::Relaxed) {
                if let Some(ml) = mainloop_weak.upgrade() {
                    ml.quit();
                }
            }
        });
        timer.update_timer(
            Some(std::time::Duration::from_millis(100)),
            Some(std::time::Duration::from_millis(100)),
        );

        let shutdown2 = shutdown.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            shutdown2.store(true, Ordering::Relaxed);
        });

        mainloop.run();

        eprintln!(
            "Done. process() called {} times, {} bytes received",
            frame_count.load(Ordering::Relaxed),
            byte_count.load(Ordering::Relaxed)
        );

        unsafe { pipewire::deinit() };
        Ok(())
    }
}
