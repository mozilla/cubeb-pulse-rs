// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

use backend::*;
use backend::cork_state::CorkState;
use cubeb;
use pulse::{self, CVolumeExt, ChannelMapExt, SampleSpecExt, USecExt};
use pulse_ffi::*;
use std::ffi::{CStr, CString};
use std::os::raw::{c_long, c_void};
use std::ptr;

const PULSE_NO_GAIN: f32 = -1.0;

fn cubeb_channel_to_pa_channel(channel: cubeb::Channel) -> pa_channel_position_t {
    assert_ne!(channel, cubeb::CHANNEL_INVALID);

    // This variable may be used for multiple times, so we should avoid to
    // allocate it in stack, or it will be created and removed repeatedly.
    // Use static to allocate this local variable in data space instead of stack.
    static MAP: [pa_channel_position_t; 10] = [
        // PA_CHANNEL_POSITION_INVALID,      // CHANNEL_INVALID
        PA_CHANNEL_POSITION_MONO,         // CHANNEL_MONO
        PA_CHANNEL_POSITION_FRONT_LEFT,   // CHANNEL_LEFT
        PA_CHANNEL_POSITION_FRONT_RIGHT,  // CHANNEL_RIGHT
        PA_CHANNEL_POSITION_FRONT_CENTER, // CHANNEL_CENTER
        PA_CHANNEL_POSITION_SIDE_LEFT,    // CHANNEL_LS
        PA_CHANNEL_POSITION_SIDE_RIGHT,   // CHANNEL_RS
        PA_CHANNEL_POSITION_REAR_LEFT,    // CHANNEL_RLS
        PA_CHANNEL_POSITION_REAR_CENTER,  // CHANNEL_RCENTER
        PA_CHANNEL_POSITION_REAR_RIGHT,   // CHANNEL_RRS
        PA_CHANNEL_POSITION_LFE           // CHANNEL_LFE
    ];

    let idx: i32 = channel.into();
    MAP[idx as usize]
}

fn layout_to_channel_map(layout: cubeb::ChannelLayout) -> pulse::ChannelMap {
    assert_ne!(layout, cubeb::LAYOUT_UNDEFINED);

    let order = cubeb::mixer::channel_index_to_order(layout);

    let mut cm = pulse::ChannelMap::init();
    cm.channels = order.len() as u8;
    for (s, d) in order.iter().zip(cm.map.iter_mut()) {
        *d = cubeb_channel_to_pa_channel(*s);
    }
    cm
}

pub struct Device(cubeb::Device);

impl Drop for Device {
    fn drop(&mut self) {
        let _ = unsafe { CString::from_raw(self.0.input_name) };
        let _ = unsafe { CString::from_raw(self.0.output_name) };
    }
}

#[derive(Debug)]
pub struct Stream<'ctx> {
    context: &'ctx Context,
    output_stream: pulse::Stream,
    input_stream: pulse::Stream,
    data_callback: cubeb::DataCallback,
    state_callback: cubeb::StateCallback,
    user_ptr: *mut c_void,
    drain_timer: *mut pa_time_event,
    output_sample_spec: pulse::SampleSpec,
    input_sample_spec: pulse::SampleSpec,
    shutdown: bool,
    volume: f32,
    state: cubeb::State,
}

impl<'ctx> Drop for Stream<'ctx> {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl<'ctx> Stream<'ctx> {
    pub fn new(context: &'ctx Context,
               stream_name: &CStr,
               input_device: cubeb::DeviceId,
               input_stream_params: Option<cubeb::StreamParams>,
               output_device: cubeb::DeviceId,
               output_stream_params: Option<cubeb::StreamParams>,
               latency_frames: u32,
               data_callback: cubeb::DataCallback,
               state_callback: cubeb::StateCallback,
               user_ptr: *mut c_void)
               -> Result<Box<Stream<'ctx>>> {

        fn check_error(s: &pulse::Stream, u: *mut c_void) {
            let stm = unsafe { &mut *(u as *mut Stream) };
            if !s.get_state().is_good() {
                stm.state_change_callback(cubeb::STATE_ERROR);
            }
            stm.context.mainloop.signal();
        }

        fn read_data(s: &pulse::Stream, nbytes: usize, u: *mut c_void) {
            fn read_from_input(s: &pulse::Stream) -> pulse::Result<(*const c_void, usize)> {
                try!(s.readable_size());
                s.peek()
            }

            logv!("Input callback buffer size {}", nbytes);
            let mut stm = unsafe { &mut *(u as *mut Stream) };
            if stm.shutdown {
                return;
            }

            while let Ok((read_data, read_size)) = read_from_input(s) {
                /* read_data can be NULL in case of a hole. */
                if !read_data.is_null() {
                    let in_frame_size = stm.input_sample_spec.frame_size();
                    let read_frames = read_size / in_frame_size;

                    if !stm.output_stream.is_null() {
                        // input/capture + output/playback operation
                        let out_frame_size = stm.output_sample_spec.frame_size();
                        let write_size = read_frames * out_frame_size;
                        // Offer full duplex data for writing
                        let stream = &stm.output_stream as *const _;
                        stm.trigger_user_callback(stream, read_data, write_size);
                    } else {
                        // input/capture only operation. Call callback directly
                        let got = unsafe {
                            stm.data_callback.unwrap()(stm as *mut _ as *mut _,
                                                       stm.user_ptr,
                                                       read_data,
                                                       ptr::null_mut(),
                                                       read_frames as c_long)
                        };

                        if got < 0 || got as usize != read_frames {
                            let _ = s.cancel_write();
                            stm.shutdown = true;
                            break;
                        }
                    }
                }

                if read_size > 0 {
                    let _ = s.drop_record();
                }

                if stm.shutdown {
                    return;
                }
            }
        }

        fn write_data(s: &pulse::Stream, nbytes: usize, u: *mut c_void) {
            logv!("Output callback to be written buffer size {}", nbytes);
            let mut stm = unsafe { &mut *(u as *mut Stream) };
            if stm.shutdown || stm.state != cubeb::STATE_STARTED {
                return;
            }

            if stm.input_stream.is_null() {
                // Output/playback only operation.
                // Write directly to output
                debug_assert!(!stm.output_stream.is_null());
                stm.trigger_user_callback(s, ptr::null(), nbytes);
            }
        }

        let mut stm = Box::new(Stream {
                                   context: context,
                                   output_stream: pulse::Stream::default(),
                                   input_stream: pulse::Stream::default(),
                                   data_callback: data_callback,
                                   state_callback: state_callback,
                                   user_ptr: user_ptr,
                                   drain_timer: ptr::null_mut(),
                                   output_sample_spec: pulse::SampleSpec::default(),
                                   input_sample_spec: pulse::SampleSpec::default(),
                                   shutdown: false,
                                   volume: PULSE_NO_GAIN,
                                   state: cubeb::STATE_ERROR,
                               });

        {
            stm.context.mainloop.lock();

            // Setup output stream
            if let Some(ref stream_params) = output_stream_params {
                match Stream::stream_init(&stm.context.context, stream_params, stream_name) {
                    Ok(s) => {
                        stm.output_sample_spec = *s.get_sample_spec();

                        s.set_state_callback(check_error, stm.as_mut() as *mut _ as *mut _);
                        s.set_write_callback(write_data, stm.as_mut() as *mut _ as *mut _);

                        let battr = set_buffering_attribute(latency_frames, &stm.output_sample_spec);
                        let device_name = if output_device.is_null() {
                            None
                        } else {
                            unsafe { Some(CStr::from_ptr(output_device as *const _)) }
                        };
                        let _ = s.connect_playback(device_name,
                                                   &battr,
                                                   pulse::STREAM_AUTO_TIMING_UPDATE | pulse::STREAM_INTERPOLATE_TIMING |
                                                   pulse::STREAM_START_CORKED |
                                                   pulse::STREAM_ADJUST_LATENCY,
                                                   None,
                                                   None);

                        stm.output_stream = s;
                    },
                    Err(e) => {
                        stm.context.mainloop.unlock();
                        stm.destroy();
                        return Err(e);
                    },
                }

            }

            // Set up input stream
            if let Some(ref stream_params) = input_stream_params {
                match Stream::stream_init(&stm.context.context, stream_params, stream_name) {
                    Ok(s) => {
                        stm.input_sample_spec = *s.get_sample_spec();

                        s.set_state_callback(check_error, stm.as_mut() as *mut _ as *mut _);
                        s.set_read_callback(read_data, stm.as_mut() as *mut _ as *mut _);

                        let battr = set_buffering_attribute(latency_frames, &stm.input_sample_spec);
                        let device_name = if input_device.is_null() {
                            None
                        } else {
                            unsafe { Some(CStr::from_ptr(output_device as *const _)) }
                        };
                        let _ = s.connect_record(device_name,
                                                 &battr,
                                                 pulse::STREAM_AUTO_TIMING_UPDATE | pulse::STREAM_INTERPOLATE_TIMING |
                                                 pulse::STREAM_START_CORKED |
                                                 pulse::STREAM_ADJUST_LATENCY);

                        stm.input_stream = s;
                    },
                    Err(e) => {
                        stm.context.mainloop.unlock();
                        stm.destroy();
                        return Err(e);
                    },
                }

            }

            let r = if stm.wait_until_ready() {
                /* force a timing update now, otherwise timing info does not become valid
                until some point after initialization has completed. */
                stm.update_timing_info()
            } else {
                false
            };

            stm.context.mainloop.unlock();

            if !r {
                stm.destroy();
                return Err(cubeb::ERROR);
            }

            if cubeb::log_enabled() {
                if output_stream_params.is_some() {
                    let output_att = stm.output_stream.get_buffer_attr();
                    log!("Output buffer attributes maxlength {}, tlength {}, \
                         prebuf {}, minreq {}, fragsize {}",
                         output_att.maxlength,
                         output_att.tlength,
                         output_att.prebuf,
                         output_att.minreq,
                         output_att.fragsize);
                }

                if input_stream_params.is_some() {
                    let input_att = stm.input_stream.get_buffer_attr();
                    log!("Input buffer attributes maxlength {}, tlength {}, \
                          prebuf {}, minreq {}, fragsize {}",
                         input_att.maxlength,
                         input_att.tlength,
                         input_att.prebuf,
                         input_att.minreq,
                         input_att.fragsize);
                }
            }
        }

        Ok(stm)
    }

    fn destroy(&mut self) {
        self.cork(CorkState::cork());

        self.context.mainloop.lock();

        if !self.output_stream.is_null() {
            if !self.drain_timer.is_null() {
                /* there's no pa_rttime_free, so use this instead. */
                self.context
                    .mainloop
                    .get_api()
                    .time_free(self.drain_timer);
            }

            self.output_stream.clear_state_callback();
            self.output_stream.clear_write_callback();
            let _ = self.output_stream.disconnect();
            self.output_stream = pulse::Stream::default();
        }

        if !self.input_stream.is_null() {
            self.input_stream.clear_state_callback();
            self.input_stream.clear_read_callback();
            let _ = self.input_stream.disconnect();
            self.input_stream = pulse::Stream::default();
        }

        self.context.mainloop.unlock();
    }

    pub fn start(&mut self) -> i32 {
        fn output_preroll(_: &pulse::MainloopApi, u: *mut c_void) {
            let mut stm = unsafe { &mut *(u as *mut Stream) };
            if !stm.shutdown {
                if let Ok(size) = stm.output_stream.writable_size() {
                    let stream = &stm.output_stream as *const _;
                    stm.trigger_user_callback(stream, ptr::null_mut(), size);
                }
            }
        }

        self.shutdown = false;
        self.cork(CorkState::uncork() | CorkState::notify());

        if !self.output_stream.is_null() && self.input_stream.is_null() {
            /* On output only case need to manually call user cb once in order to make
             * things roll. This is done via a defer event in order to execute it
             * from PA server thread. */
            self.context.mainloop.lock();
            self.context
                .mainloop
                .get_api()
                .once(output_preroll, self as *mut _ as *mut _);
            self.context.mainloop.unlock();
        }

        cubeb::OK
    }

    pub fn stop(&mut self) -> i32 {
        {
            self.context.mainloop.lock();
            self.shutdown = true;
            // If draining is taking place wait to finish
            while !self.drain_timer.is_null() {
                self.context.mainloop.wait();
            }
            self.context.mainloop.unlock();
        }
        self.cork(CorkState::cork() | CorkState::notify());

        cubeb::OK
    }

    pub fn position(&self) -> Result<u64> {
        let in_thread = self.context.mainloop.in_thread();

        if !in_thread {
            self.context.mainloop.lock();
        }

        let r = if self.output_stream.is_null() {
            return Err(cubeb::ERROR);
        } else {
            match self.output_stream.get_time() {
                Ok(r_usec) => {
                    let bytes = r_usec.to_bytes(&self.output_sample_spec);
                    Ok((bytes / self.output_sample_spec.frame_size()) as u64)
                },
                Err(_) => Err(cubeb::ERROR),
            }
        };

        if !in_thread {
            self.context.mainloop.unlock();
        }

        r
    }

    pub fn latency(&self) -> Result<u32> {
        if self.output_stream.is_null() {
            Err(cubeb::ERROR)
        } else {
            match self.output_stream.get_latency() {
                Ok((r_usec, negative)) => {
                    debug_assert!(negative);
                    let latency = (r_usec * self.output_sample_spec.rate as pa_usec_t / PA_USEC_PER_SEC) as u32;
                    Ok(latency)
                },
                Err(_) => Err(cubeb::ERROR),
            }
        }
    }

    pub fn set_volume(&mut self, volume: f32) -> i32 {
        if self.output_stream.is_null() {
            return cubeb::ERROR;
        }

        {
            self.context.mainloop.lock();

            let mut cvol: pa_cvolume = Default::default();

            /* if the pulse daemon is configured to use flat volumes,
             * apply our own gain instead of changing the input volume on the sink. */
            let flags = {
                match self.context.default_sink_info {
                    Some(ref info) => info.flags,
                    _ => pulse::SinkFlags::empty(),
                }
            };

            if flags.contains(pulse::SINK_FLAT_VOLUME) {
                self.volume = volume;
            } else {
                let channels = self.output_stream.get_sample_spec().channels;
                let vol = pulse::sw_volume_from_linear(volume as f64);
                cvol.set(channels as u32, vol);

                let index = self.output_stream.get_index();

                let context_ptr = self.context as *const _ as *mut _;
                if let Ok(o) = self.context
                       .context
                       .set_sink_input_volume(index, &cvol, context_success, context_ptr) {
                    self.context.operation_wait(&self.output_stream, &o);
                }
            }

            self.context.mainloop.unlock();
        }
        cubeb::OK
    }

    pub fn set_panning(&mut self, panning: f32) -> i32 {
        #[repr(C)]
        struct SinkInputInfoResult<'a> {
            pub cvol: pulse::CVolume,
            pub mainloop: &'a pulse::ThreadedMainloop,
        }

        fn get_input_volume(_: &pulse::Context, info: *const pulse::SinkInputInfo, eol: i32, u: *mut c_void) {
            let mut r = unsafe { &mut *(u as *mut SinkInputInfoResult) };
            if eol == 0 {
                let info = unsafe { *info };
                r.cvol = info.volume;
            }
            r.mainloop.signal();
        }

        if self.output_stream.is_null() {
            cubeb::ERROR
        } else {
            self.context.mainloop.lock();

            let map = self.output_stream.get_channel_map();
            if !map.can_balance() {
                self.context.mainloop.unlock();
                return cubeb::ERROR;
            }

            let index = self.output_stream.get_index();

            let mut r = SinkInputInfoResult {
                cvol: pulse::CVolume::default(),
                mainloop: &self.context.mainloop,
            };

            if let Ok(o) = self.context
                   .context
                   .get_sink_input_info(index, get_input_volume, &mut r as *mut _ as *mut _) {
                self.context.operation_wait(&self.output_stream, &o);
            }

            r.cvol.set_balance(map, panning);

            let context_ptr = self.context as *const _ as *mut _;
            if let Ok(o) = self.context
                   .context
                   .set_sink_input_volume(index, &r.cvol, context_success, context_ptr) {
                self.context.operation_wait(&self.output_stream, &o);
            }

            self.context.mainloop.unlock();

            cubeb::OK
        }
    }

    pub fn current_device(&self) -> Result<Box<cubeb::Device>> {
        if self.context.version_0_9_8 {
            let mut dev = Box::new(cubeb::Device::default());

            if !self.input_stream.is_null() {
                dev.input_name = match self.input_stream.get_device_name() {
                    Ok(name) => name.to_owned().into_raw(),
                    Err(_) => {
                        return Err(cubeb::ERROR);
                    },
                }
            }

            if !self.output_stream.is_null() {
                dev.output_name = match self.output_stream.get_device_name() {
                    Ok(name) => name.to_owned().into_raw(),
                    Err(_) => {
                        return Err(cubeb::ERROR);
                    },
                }
            }

            Ok(dev)
        } else {
            Err(cubeb::ERROR_NOT_SUPPORTED)
        }
    }

    fn stream_init(context: &pulse::Context,
                   stream_params: &cubeb::StreamParams,
                   stream_name: &CStr)
                   -> Result<pulse::Stream> {

        fn to_pulse_format(format: cubeb::SampleFormat) -> pulse::SampleFormat {
            match format {
                cubeb::SAMPLE_S16LE => pulse::SampleFormat::Signed16LE,
                cubeb::SAMPLE_S16BE => pulse::SampleFormat::Signed16BE,
                cubeb::SAMPLE_FLOAT32LE => pulse::SampleFormat::Float32LE,
                cubeb::SAMPLE_FLOAT32BE => pulse::SampleFormat::Float32BE,
                _ => pulse::SampleFormat::Invalid,
            }
        }

        let fmt = to_pulse_format(stream_params.format);
        if fmt == pulse::SampleFormat::Invalid {
            return Err(cubeb::ERROR_INVALID_FORMAT);
        }

        let ss = pulse::SampleSpec {
            channels: stream_params.channels as u8,
            format: fmt.into(),
            rate: stream_params.rate,
        };

        let cm: Option<pa_channel_map> = match stream_params.layout {
            cubeb::LAYOUT_UNDEFINED => None,
            _ => Some(layout_to_channel_map(stream_params.layout)),
        };

        let stream = pulse::Stream::new(context, stream_name, &ss, cm.as_ref());

        if !stream.is_null() {
            Ok(stream)
        } else {
            Err(cubeb::ERROR)
        }
    }

    pub fn cork_stream(&self, stream: &pulse::Stream, state: CorkState) {
        if !stream.is_null() {
            if let Ok(o) = stream.cork(state.is_cork() as i32,
                                       stream_success,
                                       self as *const _ as *mut _) {
                self.context.operation_wait(stream, &o);
            }
        }
    }

    fn cork(&mut self, state: CorkState) {
        {
            self.context.mainloop.lock();
            self.cork_stream(&self.output_stream, state);
            self.cork_stream(&self.input_stream, state);
            self.context.mainloop.unlock()
        }

        if state.is_notify() {
            self.state_change_callback(if state.is_cork() {
                                           cubeb::STATE_STOPPED
                                       } else {
                                           cubeb::STATE_STARTED
                                       });
        }
    }

    fn update_timing_info(&self) -> bool {
        let mut r = false;

        if !self.output_stream.is_null() {
            if let Ok(o) = self.output_stream
                   .update_timing_info(stream_success, self as *const _ as *mut _) {
                r = self.context.operation_wait(&self.output_stream, &o);
            }

            if !r {
                return r;
            }
        }

        if !self.input_stream.is_null() {
            if let Ok(o) = self.input_stream
                   .update_timing_info(stream_success, self as *const _ as *mut _) {

                r = self.context.operation_wait(&self.input_stream, &o);
            }
        }

        r
    }

    pub fn state_change_callback(&mut self, s: cubeb::State) {
        self.state = s;
        unsafe {
            (self.state_callback.unwrap())(self as *mut Stream as *mut cubeb::Stream, self.user_ptr, s);
        }
    }

    fn wait_until_ready(&self) -> bool {
        fn wait_until_io_stream_ready(stm: &pulse::Stream, mainloop: &pulse::ThreadedMainloop) -> bool {
            if stm.is_null() || mainloop.is_null() {
                return false;
            }

            loop {
                let state = stm.get_state();
                if !state.is_good() {
                    return false;
                }
                if state == pulse::StreamState::Ready {
                    break;
                }
                mainloop.wait();
            }

            true
        }

        if !self.output_stream.is_null() && !wait_until_io_stream_ready(&self.output_stream, &self.context.mainloop) {
            return false;
        }

        if !self.input_stream.is_null() && !wait_until_io_stream_ready(&self.input_stream, &self.context.mainloop) {
            return false;
        }

        true
    }

    fn trigger_user_callback(&mut self, stream: *const pulse::Stream, input_data: *const c_void, nbytes: usize) {
        fn drained_cb(a: &pulse::MainloopApi, e: *mut pa_time_event, _tv: &pulse::TimeVal, u: *mut c_void) {
            let mut stm = unsafe { &mut *(u as *mut Stream) };
            debug_assert_eq!(stm.drain_timer, e);
            stm.state_change_callback(cubeb::STATE_DRAINED);
            /* there's no pa_rttime_free, so use this instead. */
            a.time_free(stm.drain_timer);
            stm.drain_timer = ptr::null_mut();
            stm.context.mainloop.signal();
        }

        let s = unsafe { &*stream };

        let frame_size = self.output_sample_spec.frame_size();
        debug_assert_eq!(nbytes % frame_size, 0);

        let mut towrite = nbytes;
        let mut read_offset = 0usize;
        while towrite > 0 {
            match s.begin_write(towrite) {
                Err(e) => {
                    panic!("Failed to write data: {}", e);
                },
                Ok((buffer, size)) => {
                    debug_assert!(size > 0);
                    debug_assert_eq!(size % frame_size, 0);

                    logv!("Trigger user callback with output buffer size={}, read_offset={}",
                          size,
                          read_offset);
                    let read_ptr = unsafe { (input_data as *const u8).offset(read_offset as isize) };
                    let got = unsafe {
                        self.data_callback.unwrap()(self as *const _ as *mut _,
                                                    self.user_ptr,
                                                    read_ptr as *const _ as *mut _,
                                                    buffer,
                                                    (size / frame_size) as c_long)
                    };
                    if got < 0 {
                        let _ = s.cancel_write();
                        self.shutdown = true;
                        return;
                    }

                    // If more iterations move offset of read buffer
                    if !input_data.is_null() {
                        let in_frame_size = self.input_sample_spec.frame_size();
                        read_offset += (size / frame_size) * in_frame_size;
                    }

                    if self.volume != PULSE_NO_GAIN {
                        let samples = (self.output_sample_spec.channels as usize * size / frame_size) as isize;

                        if self.output_sample_spec.format == PA_SAMPLE_S16BE ||
                           self.output_sample_spec.format == PA_SAMPLE_S16LE {
                            let b = buffer as *mut i16;
                            for i in 0..samples {
                                unsafe { *b.offset(i) *= self.volume as i16 };
                            }
                        } else {
                            let b = buffer as *mut f32;
                            for i in 0..samples {
                                unsafe { *b.offset(i) *= self.volume };
                            }
                        }
                    }

                    let r = s.write(buffer,
                                    got as usize * frame_size,
                                    0,
                                    pulse::SeekMode::Relative);
                    debug_assert!(r.is_ok());

                    if (got as usize) < size / frame_size {
                        let latency = match s.get_latency() {
                            Ok((l, _)) => l,
                            Err(e) => {
                                debug_assert_eq!(e, pulse::ErrorCode::from_error_code(PA_ERR_NODATA));
                                /* this needs a better guess. */
                                100 * PA_USEC_PER_MSEC
                            },
                        };

                        /* pa_stream_drain is useless, see PA bug# 866. this is a workaround. */
                        /* arbitrary safety margin: double the current latency. */
                        debug_assert!(self.drain_timer.is_null());
                        let stream_ptr = self as *const _ as *mut _;
                        self.drain_timer = self.context
                            .context
                            .rttime_new(pulse::rtclock_now() + 2 * latency, drained_cb, stream_ptr);
                        self.shutdown = true;
                        return;
                    }

                    towrite -= size;
                },
            }
        }

        debug_assert_eq!(towrite, 0);
    }
}

fn stream_success(_: &pulse::Stream, success: i32, u: *mut c_void) {
    let stm = unsafe { &*(u as *mut Stream) };
    debug_assert_ne!(success, 0);
    stm.context.mainloop.signal();
}

fn context_success(_: &pulse::Context, success: i32, u: *mut c_void) {
    let ctx = unsafe { &*(u as *mut Context) };
    debug_assert_ne!(success, 0);
    ctx.mainloop.signal();
}

fn set_buffering_attribute(latency_frames: u32, sample_spec: &pa_sample_spec) -> pa_buffer_attr {
    let tlength = latency_frames * sample_spec.frame_size() as u32;
    let minreq = tlength / 4;
    let battr = pa_buffer_attr {
        maxlength: u32::max_value(),
        prebuf: u32::max_value(),
        tlength: tlength,
        minreq: minreq,
        fragsize: minreq,
    };

    log!("Requested buffer attributes maxlength {}, tlength {}, prebuf {}, minreq {}, fragsize {}",
         battr.maxlength,
         battr.tlength,
         battr.prebuf,
         battr.minreq,
         battr.fragsize);

    battr
}
