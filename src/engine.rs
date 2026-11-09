//! Audio engine (cpal / WASAPI shared mode).
//!
//!   mic stream ──ring──► cable output callback (master clock):
//!                         out = limit(mic*micGain + sounds*soundGain)  ──► CABLE Input
//!                         sounds ──ring──► monitor callback ──► your headphones
//!
//! The cable callback owns the playing voices, so the people hearing you and
//! your own headphones always hear the same sounds in sync.

use crate::mixer::{mix_frame, peak_decay, Clip, Linear, Voices};
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

pub enum Cmd {
    Play(Arc<Clip>),
    Stop(u64),
    StopAll,
}

/// Values shared between the UI and the audio threads.
pub struct Shared {
    mic_gain: AtomicU32,
    snd_gain: AtomicU32,
    mon_gain: AtomicU32,
    pub mic_muted: AtomicBool,
    mic_peak: AtomicU32,
    out_peak: AtomicU32,
    /// (clip id, progress) of playing sounds, refreshed by the cable callback.
    pub playing: Mutex<Vec<(u64, f32)>>,
    pub error: Mutex<Option<String>>,
}

fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}
fn store(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed)
}

impl Shared {
    pub fn new(mic: f32, snd: f32, mon: f32) -> Self {
        Self {
            mic_gain: AtomicU32::new(mic.to_bits()),
            snd_gain: AtomicU32::new(snd.to_bits()),
            mon_gain: AtomicU32::new(mon.to_bits()),
            mic_muted: AtomicBool::new(false),
            mic_peak: AtomicU32::new(0),
            out_peak: AtomicU32::new(0),
            playing: Mutex::new(Vec::with_capacity(64)),
            error: Mutex::new(None),
        }
    }
    pub fn set_mic_gain(&self, v: f32) {
        store(&self.mic_gain, v)
    }
    pub fn set_snd_gain(&self, v: f32) {
        store(&self.snd_gain, v)
    }
    pub fn set_mon_gain(&self, v: f32) {
        store(&self.mon_gain, v)
    }
    pub fn mic_peak(&self) -> f32 {
        load(&self.mic_peak)
    }
    pub fn out_peak(&self) -> f32 {
        load(&self.out_peak)
    }
    fn report(&self, msg: String) {
        if let Ok(mut e) = self.error.lock() {
            *e = Some(msg);
        }
    }
}

pub struct DeviceLists {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

pub fn list_devices() -> DeviceLists {
    let host = cpal::default_host();
    let names = |it: Option<Vec<cpal::Device>>| -> Vec<String> {
        it.unwrap_or_default().iter().filter_map(|d| d.name().ok()).collect()
    };
    DeviceLists {
        inputs: names(host.input_devices().ok().map(|i| i.collect())),
        outputs: names(host.output_devices().ok().map(|i| i.collect())),
        default_input: host.default_input_device().and_then(|d| d.name().ok()),
        default_output: host.default_output_device().and_then(|d| d.name().ok()),
    }
}

/// The VB-CABLE recording side ("CABLE Output ..."). Using it as PadMix's mic would
/// feed PadMix's own output back into itself, so it is never offered as a microphone.
pub fn is_cable_capture(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("cable output") || n.contains("vb-audio virtual cable")
}

/// The VB-CABLE playback side ("CABLE Input (VB-Audio Virtual Cable)").
pub fn is_cable_playback(name: &str) -> bool {
    name.to_lowercase().contains("cable input")
}

pub fn find_cable(outputs: &[String]) -> Option<String> {
    outputs.iter().find(|n| is_cable_playback(n)).cloned()
}

fn find_device(input: bool, name: &str) -> Result<cpal::Device> {
    let host = cpal::default_host();
    let mut devs: Vec<cpal::Device> = if input {
        host.input_devices()?.collect()
    } else {
        host.output_devices()?.collect()
    };
    let idx = devs
        .iter()
        .position(|d| d.name().map(|n| n == name).unwrap_or(false))
        .ok_or_else(|| anyhow!("device not found: {name}"))?;
    Ok(devs.swap_remove(idx))
}

pub struct Engine {
    _streams: Vec<cpal::Stream>,
    pub rate: u32,
    tx: Sender<Cmd>,
}

impl Engine {
    pub fn sender(&self) -> Sender<Cmd> {
        self.tx.clone()
    }
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// Start streams. `mic` and `monitor` are optional; `output` is the cable.
    pub fn start(mic: Option<&str>, output: &str, monitor: Option<&str>, shared: Arc<Shared>) -> Result<Self> {
        let out_dev = find_device(false, output)?;
        let out_cfg = out_dev.default_output_config().context("output config")?;
        let rate = out_cfg.sample_rate().0;

        // mic -> ring (mono, mic's own rate)
        let mut streams = Vec::new();
        let mut mic_side: Option<(HeapCons<f32>, u32)> = None;
        if let Some(name) = mic {
            let dev = find_device(true, name)?;
            let cfg = dev.default_input_config().context("mic config")?;
            let mic_rate = cfg.sample_rate().0;
            let rb = HeapRb::<f32>::new((mic_rate as usize).max(4800)); // 1 s headroom
            let (prod, cons) = rb.split();
            let s = match cfg.sample_format() {
                SampleFormat::F32 => build_mic::<f32>(&dev, &cfg.into(), prod, shared.clone())?,
                SampleFormat::I16 => build_mic::<i16>(&dev, &cfg.into(), prod, shared.clone())?,
                SampleFormat::I32 => build_mic::<i32>(&dev, &cfg.into(), prod, shared.clone())?,
                f => return Err(anyhow!("unsupported mic sample format {f:?}")),
            };
            streams.push(s);
            mic_side = Some((cons, mic_rate));
        }

        // cable output -> monitor ring (stereo sounds, engine rate)
        let mut mon_prod: Option<HeapProd<f32>> = None;
        let mut mon_cons: Option<HeapCons<f32>> = None;
        if monitor.is_some() {
            let rb = HeapRb::<f32>::new(rate as usize * 2); // 1 s stereo
            let (p, c) = rb.split();
            mon_prod = Some(p);
            mon_cons = Some(c);
        }

        let (tx, rx) = mpsc::channel();
        let out_stream = match out_cfg.sample_format() {
            SampleFormat::F32 => build_out::<f32>(&out_dev, &out_cfg.into(), rate, rx, mic_side, mon_prod, shared.clone())?,
            SampleFormat::I16 => build_out::<i16>(&out_dev, &out_cfg.into(), rate, rx, mic_side, mon_prod, shared.clone())?,
            SampleFormat::I32 => build_out::<i32>(&out_dev, &out_cfg.into(), rate, rx, mic_side, mon_prod, shared.clone())?,
            f => return Err(anyhow!("unsupported output sample format {f:?}")),
        };
        streams.push(out_stream);

        if let (Some(name), Some(cons)) = (monitor, mon_cons) {
            let dev = find_device(false, name)?;
            let cfg = dev.default_output_config().context("monitor config")?;
            let s = match cfg.sample_format() {
                SampleFormat::F32 => build_monitor::<f32>(&dev, &cfg.into(), rate, cons, shared.clone())?,
                SampleFormat::I16 => build_monitor::<i16>(&dev, &cfg.into(), rate, cons, shared.clone())?,
                SampleFormat::I32 => build_monitor::<i32>(&dev, &cfg.into(), rate, cons, shared.clone())?,
                f => return Err(anyhow!("unsupported monitor sample format {f:?}")),
            };
            streams.push(s);
        }

        for s in &streams {
            s.play().context("starting audio stream")?;
        }
        Ok(Self { _streams: streams, rate, tx })
    }
}

fn err_cb(shared: Arc<Shared>, what: &'static str) -> impl FnMut(cpal::StreamError) + Send + 'static {
    move |e| shared.report(format!("{what}: {e}"))
}

fn build_mic<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, mut prod: HeapProd<f32>, shared: Arc<Shared>) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = cfg.channels.max(1) as usize;
    let sh = shared.clone();
    let stream = dev.build_input_stream(
        cfg,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut peak = 0.0f32;
            for frame in data.chunks_exact(ch) {
                let mut s = 0.0f32;
                for &x in frame {
                    s += <f32 as FromSample<T>>::from_sample_(x);
                }
                let m = s / ch as f32;
                peak = peak.max(m.abs());
                let _ = prod.try_push(m); // drop if the ring is full
            }
            let old = load(&sh.mic_peak);
            store(&sh.mic_peak, peak_decay(old, peak));
        },
        err_cb(shared, "microphone"),
        None,
    )?;
    Ok(stream)
}

#[allow(clippy::too_many_arguments)]
fn build_out<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    rate: u32,
    rx: Receiver<Cmd>,
    mic: Option<(HeapCons<f32>, u32)>,
    mut mon: Option<HeapProd<f32>>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels.max(1) as usize;
    let mut voices = Voices::new();
    let (mut mic_cons, mic_rate) = match mic {
        Some((c, r)) => (Some(c), r),
        None => (None, rate),
    };
    let mut mic_rs = Linear::new(1, mic_rate, rate);
    // keep ~40 ms of mic audio buffered; trim if clocks drift and it grows past ~150 ms
    let mic_target = (mic_rate as usize * 40) / 1000;
    let mic_high = (mic_rate as usize * 150) / 1000;
    let sh = shared.clone();

    let stream = dev.build_output_stream(
        cfg,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Cmd::Play(c) => voices.play(c),
                    Cmd::Stop(id) => voices.stop(id),
                    Cmd::StopAll => voices.stop_all(),
                }
            }
            if let Some(c) = mic_cons.as_mut() {
                let occ = c.occupied_len();
                if occ > mic_high {
                    c.skip(occ - mic_target);
                }
            }
            let mic_gain = if sh.mic_muted.load(Ordering::Relaxed) { 0.0 } else { load(&sh.mic_gain) };
            let snd_gain = load(&sh.snd_gain);
            let mut peak = 0.0f32;
            let mut tmp = [0.0f32; 2];

            for frame in data.chunks_exact_mut(ch) {
                let mic = match mic_cons.as_mut() {
                    Some(c) => {
                        if mic_rs.next(&mut tmp, |f| c.try_pop().map(|s| f[0] = s).is_some()) {
                            tmp[0]
                        } else {
                            0.0
                        }
                    }
                    None => 0.0,
                };
                let snd = voices.next_frame();
                let (l, r) = mix_frame(mic, mic_gain, snd, snd_gain);
                peak = peak.max(l.abs()).max(r.abs());
                match ch {
                    1 => frame[0] = T::from_sample((l + r) * 0.5),
                    _ => {
                        frame[0] = T::from_sample(l);
                        frame[1] = T::from_sample(r);
                        for x in frame.iter_mut().skip(2) {
                            *x = T::from_sample(0.0f32);
                        }
                    }
                }
                if let Some(m) = mon.as_mut() {
                    if m.vacant_len() >= 2 {
                        let _ = m.try_push(snd.0);
                        let _ = m.try_push(snd.1);
                    }
                }
            }
            let old = load(&sh.out_peak);
            store(&sh.out_peak, peak_decay(old, peak));
            if let Ok(mut p) = sh.playing.try_lock() {
                p.clear();
                voices.for_each_progress(|id, prog| p.push((id, prog)));
            }
        },
        err_cb(shared, "cable output"),
        None,
    )?;
    Ok(stream)
}

fn build_monitor<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, engine_rate: u32, mut cons: HeapCons<f32>, shared: Arc<Shared>) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels.max(1) as usize;
    let dev_rate = cfg.sample_rate.0;
    let mut rs = Linear::new(2, engine_rate, dev_rate);
    let target = (engine_rate as usize * 2 * 40) / 1000;
    let high = (engine_rate as usize * 2 * 150) / 1000;
    let sh = shared.clone();
    let stream = dev.build_output_stream(
        cfg,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let occ = cons.occupied_len();
            if occ > high {
                let n = (occ - target) & !1; // keep L/R alignment
                cons.skip(n);
            }
            let g = load(&sh.mon_gain);
            let mut tmp = [0.0f32; 2];
            for frame in data.chunks_exact_mut(ch) {
                let ok = rs.next(&mut tmp, |f| {
                    if cons.occupied_len() < 2 {
                        return false;
                    }
                    f[0] = cons.try_pop().unwrap_or(0.0);
                    f[1] = cons.try_pop().unwrap_or(0.0);
                    true
                });
                let (l, r) = if ok { (tmp[0] * g, tmp[1] * g) } else { (0.0, 0.0) };
                match ch {
                    1 => frame[0] = T::from_sample((l + r) * 0.5),
                    _ => {
                        frame[0] = T::from_sample(l.clamp(-1.0, 1.0));
                        frame[1] = T::from_sample(r.clamp(-1.0, 1.0));
                        for x in frame.iter_mut().skip(2) {
                            *x = T::from_sample(0.0f32);
                        }
                    }
                }
            }
        },
        err_cb(shared, "monitor"),
        None,
    )?;
    Ok(stream)
}
