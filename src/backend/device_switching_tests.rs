use super::*;
use cubeb_backend::{ffi, ContextOps, StreamParams, StreamPrefs};
use std::ffi::CString;
use std::process::Command;
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

fn pactl(args: &[&str]) -> String {
    let result = Command::new("pactl").args(args).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

fn assert_devices(stream: &PulseStream, input: &str, output: &str) -> bool {
    let _lock = stream.context.mainloop.lock_guard();
    stream
        .input_stream
        .as_ref()
        .unwrap()
        .get_device_name()
        .unwrap()
        .to_bytes()
        == input.as_bytes()
        && stream
            .output_stream
            .as_ref()
            .unwrap()
            .get_device_name()
            .unwrap()
            .to_bytes()
            == output.as_bytes()
}

#[test]
#[ignore = "requires an isolated PulseAudio server and pactl"]
fn default_device_switching() {
    let module_a = pactl(&["load-module", "module-null-sink", "sink_name=cubeb_test_a"]);
    let module_b = pactl(&["load-module", "module-null-sink", "sink_name=cubeb_test_b"]);
    pactl(&["set-default-sink", "cubeb_test_a"]);
    pactl(&["set-default-source", "cubeb_test_a.monitor"]);

    let ctx = PulseContext::init(Some(&CString::new("cubeb switching test").unwrap())).unwrap();
    let params = |prefs: StreamPrefs| {
        StreamParams::from(ffi::cubeb_stream_params {
            format: ffi::CUBEB_SAMPLE_FLOAT32NE,
            rate: 48000,
            channels: 2,
            layout: ffi::CUBEB_LAYOUT_STEREO,
            prefs: prefs.bits(),
            ..Default::default()
        })
    };
    let following = params(StreamPrefs::NONE);
    let pinned = params(StreamPrefs::DISABLE_DEVICE_SWITCHING);
    let name = CString::new("device switching").unwrap();
    let explicit_input = CString::new("cubeb_test_a.monitor").unwrap();
    let explicit_output = CString::new("cubeb_test_a").unwrap();
    let create =
        |input_device, input_params: &StreamParams, output_device, output_params: &StreamParams| {
            PulseStream::new(
                &ctx,
                Some(&name),
                input_device,
                Some(input_params),
                output_device,
                Some(output_params),
                1024,
                None,
                None,
                ptr::null_mut(),
            )
            .unwrap()
        };
    let streams = [
        create(ptr::null_mut(), &following, ptr::null_mut(), &following),
        create(ptr::null_mut(), &pinned, ptr::null_mut(), &pinned),
        create(
            explicit_input.as_ptr() as _,
            &following,
            explicit_output.as_ptr() as _,
            &following,
        ),
        create(ptr::null_mut(), &pinned, ptr::null_mut(), &following),
        create(ptr::null_mut(), &following, ptr::null_mut(), &pinned),
    ];
    for stream in &streams {
        assert!(assert_devices(
            stream,
            "cubeb_test_a.monitor",
            "cubeb_test_a"
        ));
    }
    pactl(&["set-default-sink", "cubeb_test_b"]);
    pactl(&["set-default-source", "cubeb_test_b.monitor"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let correct = assert_devices(&streams[0], "cubeb_test_b.monitor", "cubeb_test_b")
            && assert_devices(&streams[1], "cubeb_test_a.monitor", "cubeb_test_a")
            && assert_devices(&streams[2], "cubeb_test_a.monitor", "cubeb_test_a")
            && assert_devices(&streams[3], "cubeb_test_a.monitor", "cubeb_test_b")
            && assert_devices(&streams[4], "cubeb_test_b.monitor", "cubeb_test_a");
        if correct {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "streams did not follow their switching policy"
        );
        thread::sleep(Duration::from_millis(10));
    }
    drop(streams);
    drop(ctx);
    pactl(&["unload-module", &module_a]);
    pactl(&["unload-module", &module_b]);
}

#[test]
#[ignore = "requires an isolated PulseAudio server and pactl"]
fn pinned_device_removal_reports_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    extern "C" fn state_callback(
        _: *mut ffi::cubeb_stream,
        user: *mut std::os::raw::c_void,
        state: ffi::cubeb_state,
    ) {
        if state == ffi::CUBEB_STATE_ERROR {
            unsafe { &*(user as *const AtomicUsize) }.fetch_add(1, Ordering::SeqCst);
        }
    }
    let module = pactl(&[
        "load-module",
        "module-null-sink",
        "sink_name=cubeb_test_pinned",
    ]);
    let fallback = pactl(&[
        "load-module",
        "module-null-sink",
        "sink_name=cubeb_test_fallback",
    ]);
    pactl(&["set-default-sink", "cubeb_test_pinned"]);
    let ctx = PulseContext::init(Some(&CString::new("cubeb removal test").unwrap())).unwrap();
    let params = StreamParams::from(ffi::cubeb_stream_params {
        format: ffi::CUBEB_SAMPLE_FLOAT32NE,
        rate: 48000,
        channels: 2,
        layout: ffi::CUBEB_LAYOUT_STEREO,
        ..Default::default()
    });
    let pinned_params = StreamParams::from(ffi::cubeb_stream_params {
        prefs: ffi::CUBEB_STREAM_PREF_DISABLE_DEVICE_SWITCHING,
        ..unsafe { *params.as_ptr() }
    });
    let explicit_errors = AtomicUsize::new(0);
    let pinned_errors = AtomicUsize::new(0);
    let following_errors = AtomicUsize::new(0);
    let name = CString::new("device removal").unwrap();
    let device = CString::new("cubeb_test_pinned").unwrap();
    let create = |device, params: &StreamParams, errors: &AtomicUsize| {
        PulseStream::new(
            &ctx,
            Some(&name),
            ptr::null_mut(),
            None,
            device,
            Some(params),
            1024,
            None,
            Some(state_callback),
            errors as *const _ as *mut _,
        )
        .unwrap()
    };
    let explicit = create(device.as_ptr() as _, &params, &explicit_errors);
    let pinned = create(ptr::null_mut(), &pinned_params, &pinned_errors);
    let following = create(ptr::null_mut(), &params, &following_errors);
    pactl(&["set-default-sink", "cubeb_test_fallback"]);
    pactl(&["unload-module", &module]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while explicit_errors.load(Ordering::SeqCst) == 0 || pinned_errors.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "pinned device loss did not report an error"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(following_errors.load(Ordering::SeqCst), 0);
    drop((explicit, pinned, following));
    drop(ctx);
    pactl(&["unload-module", &fallback]);
}
