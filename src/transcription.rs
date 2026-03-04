use std::{
    path::Path,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use anyhow::Context;
use opus::{Channels as OpusChannels, Decoder as OpusDecoder};
use tracing::{error, info, warn};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const OPUS_SAMPLE_RATE_HZ: usize = 48_000;
const TRANSCRIPT_SAMPLE_RATE_HZ: usize = 16_000;
const MAX_OPUS_FRAME_SAMPLES_PER_CHANNEL: usize = 5_760;
const ENDPOINT_SILENCE_MS: f32 = 650.0;
const MIN_CHUNK_MS: f32 = 800.0;
const MAX_CHUNK_MS: f32 = 12_000.0;
const MIN_VOICED_MS: f32 = 250.0;
const VAD_RMS_THRESHOLD: f32 = 0.010;

#[derive(Debug, Clone)]
pub struct TranscriptIdentity {
    pub room_id: String,
    pub peer_id: String,
    pub track_id: String,
}

#[derive(Debug)]
struct TranscriptJob {
    identity: TranscriptIdentity,
    chunk_index: u64,
    pcm_16k_mono: Vec<f32>,
}

#[derive(Clone)]
pub struct LocalTranscriber {
    tx: Sender<TranscriptJob>,
}

impl LocalTranscriber {
    pub fn from_env() -> anyhow::Result<Option<Arc<Self>>> {
        let Some(model_path) = std::env::var("HUDDLETALK_WHISPER_MODEL_PATH").ok() else {
            info!("transcription disabled: set HUDDLETALK_WHISPER_MODEL_PATH to enable local STT");
            return Ok(None);
        };

        if !Path::new(&model_path).exists() {
            warn!(
                "transcription disabled: model path '{}' was not found",
                model_path
            );
            return Ok(None);
        }

        let language = std::env::var("HUDDLETALK_STT_LANGUAGE").ok();
        let threads = std::env::var("HUDDLETALK_STT_THREADS")
            .ok()
            .and_then(|raw| raw.parse::<i32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(4);

        let (tx, rx) = mpsc::channel::<TranscriptJob>();
        thread::Builder::new()
            .name("huddletalk-stt".to_owned())
            .spawn(move || run_transcription_worker(model_path, language, threads, rx))
            .context("failed to spawn transcription worker thread")?;

        info!("transcription enabled with local whisper model");
        Ok(Some(Arc::new(Self { tx })))
    }

    fn submit(&self, job: TranscriptJob) {
        if let Err(err) = self.tx.send(job) {
            warn!("failed to submit transcription job: {err}");
        }
    }
}

fn run_transcription_worker(
    model_path: String,
    language: Option<String>,
    threads: i32,
    rx: Receiver<TranscriptJob>,
) {
    let context_parameters = WhisperContextParameters::default();
    let context = match WhisperContext::new_with_params(&model_path, context_parameters) {
        Ok(context) => context,
        Err(err) => {
            error!(
                "failed to load whisper model '{}' for transcription: {err}",
                model_path
            );
            return;
        }
    };

    info!(
        "transcription worker ready (language={}, threads={})",
        language.as_deref().unwrap_or("auto"),
        threads
    );

    while let Ok(job) = rx.recv() {
        let mut state = match context.create_state() {
            Ok(state) => state,
            Err(err) => {
                warn!("failed to create whisper state: {err}");
                continue;
            }
        };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(threads);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_translate(false);
        if let Some(language) = language.as_deref() {
            params.set_language(Some(language));
        }

        if let Err(err) = state.full(params, &job.pcm_16k_mono) {
            warn!(
                "transcription failed room='{}' peer='{}' track='{}' chunk={} err={err}",
                job.identity.room_id, job.identity.peer_id, job.identity.track_id, job.chunk_index
            );
            continue;
        }

        let segment_count = match state.full_n_segments() {
            Ok(segment_count) => segment_count,
            Err(err) => {
                warn!(
                    "transcription failed to read segment count room='{}' peer='{}' track='{}' chunk={} err={err}",
                    job.identity.room_id,
                    job.identity.peer_id,
                    job.identity.track_id,
                    job.chunk_index
                );
                continue;
            }
        };
        let mut transcript = String::new();
        for segment_idx in 0..segment_count {
            match state.full_get_segment_text(segment_idx) {
                Ok(text) => {
                    transcript.push_str(&text);
                    transcript.push(' ');
                }
                Err(err) => {
                    warn!(
                        "transcription failed to read segment room='{}' peer='{}' track='{}' chunk={} segment={} err={err}",
                        job.identity.room_id,
                        job.identity.peer_id,
                        job.identity.track_id,
                        job.chunk_index,
                        segment_idx
                    );
                }
            }
        }

        let transcript = transcript.trim();
        if transcript.is_empty() {
            continue;
        }

        info!(
            "transcript room='{}' peer='{}' track='{}' chunk={} text={}",
            job.identity.room_id,
            job.identity.peer_id,
            job.identity.track_id,
            job.chunk_index,
            transcript
        );
    }
}

pub struct TrackAudioTranscriber {
    transcriber: Arc<LocalTranscriber>,
    identity: TranscriptIdentity,
    opus_decoder: OpusDecoder,
    channel_count: usize,
    decode_buffer: Vec<i16>,
    chunk_pcm_16k_mono: Vec<f32>,
    chunk_duration_ms: f32,
    chunk_voiced_ms: f32,
    trailing_silence_ms: f32,
    chunk_index: u64,
}

impl TrackAudioTranscriber {
    pub fn new(
        transcriber: Arc<LocalTranscriber>,
        identity: TranscriptIdentity,
        opus_channels: u16,
    ) -> anyhow::Result<Self> {
        let channel_layout = if opus_channels >= 2 {
            OpusChannels::Stereo
        } else {
            OpusChannels::Mono
        };
        let channel_count = match channel_layout {
            OpusChannels::Mono => 1,
            OpusChannels::Stereo => 2,
        };

        let opus_decoder = OpusDecoder::new(OPUS_SAMPLE_RATE_HZ as u32, channel_layout)
            .context("failed to create Opus decoder")?;

        Ok(Self {
            transcriber,
            identity,
            opus_decoder,
            channel_count,
            decode_buffer: vec![0; MAX_OPUS_FRAME_SAMPLES_PER_CHANNEL * channel_count],
            chunk_pcm_16k_mono: Vec::with_capacity(TRANSCRIPT_SAMPLE_RATE_HZ * 12),
            chunk_duration_ms: 0.0,
            chunk_voiced_ms: 0.0,
            trailing_silence_ms: 0.0,
            chunk_index: 0,
        })
    }

    pub fn ingest_opus_payload(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }

        let decoded_samples_per_channel =
            match self
                .opus_decoder
                .decode(payload, &mut self.decode_buffer, false)
            {
                Ok(samples) => samples,
                Err(err) => {
                    warn!(
                        "opus decode failed room='{}' peer='{}' track='{}': {err}",
                        self.identity.room_id, self.identity.peer_id, self.identity.track_id
                    );
                    return;
                }
            };

        if decoded_samples_per_channel == 0 {
            return;
        }

        let mono_48k = to_mono_f32(
            &self.decode_buffer,
            decoded_samples_per_channel,
            self.channel_count,
        );
        let mono_16k = downsample_48k_to_16k(&mono_48k);
        self.ingest_pcm_16k_frame(&mono_16k);
    }

    pub fn finish(&mut self) {
        self.flush_chunk(true);
    }

    fn ingest_pcm_16k_frame(&mut self, frame: &[f32]) {
        if frame.is_empty() {
            return;
        }

        let frame_duration_ms = (frame.len() as f32) * 1000.0 / (TRANSCRIPT_SAMPLE_RATE_HZ as f32);
        let frame_voiced = is_voiced(frame);

        if self.chunk_pcm_16k_mono.is_empty() && !frame_voiced {
            return;
        }

        self.chunk_pcm_16k_mono.extend_from_slice(frame);
        self.chunk_duration_ms += frame_duration_ms;

        if frame_voiced {
            self.chunk_voiced_ms += frame_duration_ms;
            self.trailing_silence_ms = 0.0;
        } else {
            self.trailing_silence_ms += frame_duration_ms;
        }

        let should_endpoint = self.chunk_duration_ms >= MIN_CHUNK_MS
            && self.trailing_silence_ms >= ENDPOINT_SILENCE_MS;
        let should_cut_for_size = self.chunk_duration_ms >= MAX_CHUNK_MS;
        if should_endpoint || should_cut_for_size {
            self.flush_chunk(false);
        }
    }

    fn flush_chunk(&mut self, force: bool) {
        if self.chunk_pcm_16k_mono.is_empty() {
            self.reset_chunk_tracking();
            return;
        }

        let long_enough = self.chunk_duration_ms >= MIN_CHUNK_MS;
        let enough_voiced = self.chunk_voiced_ms >= MIN_VOICED_MS;
        if force {
            if enough_voiced {
                self.submit_current_chunk();
            }
            self.reset_chunk_tracking();
            return;
        }

        if long_enough && enough_voiced {
            self.submit_current_chunk();
        }
        self.reset_chunk_tracking();
    }

    fn submit_current_chunk(&mut self) {
        let pcm_16k_mono = std::mem::take(&mut self.chunk_pcm_16k_mono);
        let job = TranscriptJob {
            identity: self.identity.clone(),
            chunk_index: self.chunk_index,
            pcm_16k_mono,
        };
        self.transcriber.submit(job);
        self.chunk_index += 1;
    }

    fn reset_chunk_tracking(&mut self) {
        self.chunk_duration_ms = 0.0;
        self.chunk_voiced_ms = 0.0;
        self.trailing_silence_ms = 0.0;
        self.chunk_pcm_16k_mono.clear();
    }
}

fn to_mono_f32(samples: &[i16], samples_per_channel: usize, channel_count: usize) -> Vec<f32> {
    if channel_count <= 1 {
        let mono = &samples[..samples_per_channel];
        return mono
            .iter()
            .map(|sample| *sample as f32 / 32_768.0)
            .collect();
    }

    let mut out = Vec::with_capacity(samples_per_channel);
    for idx in 0..samples_per_channel {
        let left = samples[idx * channel_count] as f32;
        let right = samples[idx * channel_count + 1] as f32;
        out.push(((left + right) * 0.5) / 32_768.0);
    }
    out
}

fn downsample_48k_to_16k(samples_48k: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(samples_48k.len() / 3);
    for chunk in samples_48k.chunks_exact(3) {
        out.push((chunk[0] + chunk[1] + chunk[2]) / 3.0);
    }
    out
}

fn is_voiced(frame: &[f32]) -> bool {
    if frame.is_empty() {
        return false;
    }

    let mut square_sum = 0.0f32;
    for sample in frame {
        square_sum += sample * sample;
    }
    let rms = (square_sum / frame.len() as f32).sqrt();
    rms >= VAD_RMS_THRESHOLD
}
