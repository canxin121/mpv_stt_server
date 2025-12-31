use crate::protocol::{JobResult, TranscriptionJob};
use anyhow::Result;
use log::{debug, error, info};
use mpv_stt_plugin_rs::stt::SttBackend;
use mpv_stt_plugin_rs::{LocalModelConfig, SttRunner};
use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tempfile::NamedTempFile;
use tokio::sync::mpsc;

pub struct WorkerPool {
    job_tx: mpsc::UnboundedSender<TranscriptionJob>,
    result_rx: mpsc::UnboundedReceiver<JobResult>,
    cancelled: Arc<Mutex<HashSet<u64>>>,
}

impl WorkerPool {
    pub fn new(config: LocalModelConfig, num_workers: usize) -> Self {
        let (job_tx, job_rx) = mpsc::unbounded_channel();
        let (result_tx, result_rx) = mpsc::unbounded_channel();
        let cancelled = Arc::new(Mutex::new(HashSet::new()));

        let job_rx = Arc::new(tokio::sync::Mutex::new(job_rx));

        for id in 0..num_workers {
            let job_rx = Arc::clone(&job_rx);
            let result_tx = result_tx.clone();
            let config = config.clone();
            let cancelled = Arc::clone(&cancelled);

            tokio::task::spawn_blocking(move || {
                worker_thread(id, config, job_rx, result_tx, cancelled);
            });
        }

        Self {
            job_tx,
            result_rx,
            cancelled,
        }
    }

    pub fn submit_job(&self, job: TranscriptionJob) -> Result<()> {
        self.job_tx.send(job)?;
        Ok(())
    }

    pub fn cancel_request(&self, request_id: u64) {
        if let Ok(mut set) = self.cancelled.lock() {
            set.insert(request_id);
        }
    }

    pub async fn next_result(&mut self) -> Option<JobResult> {
        self.result_rx.recv().await
    }
}

fn worker_thread(
    worker_id: usize,
    config: LocalModelConfig,
    job_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<TranscriptionJob>>>,
    result_tx: mpsc::UnboundedSender<JobResult>,
    cancelled: Arc<Mutex<HashSet<u64>>>,
) {
    info!("Worker {} started", worker_id);

    let mut runner = SttRunner::new(config);

    loop {
        let job = {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut rx = job_rx.lock().await;
                rx.recv().await
            })
        };

        let Some(job) = job else {
            debug!("Worker {} shutting down (channel closed)", worker_id);
            break;
        };

        debug!(
            "Worker {} processing request {} ({} bytes)",
            worker_id,
            job.request_id,
            job.audio_data.len()
        );

        // Drop job early if it was cancelled after submission but before processing
        if is_cancelled(job.request_id, &cancelled) {
            debug!(
                "Worker {} skipping cancelled request {}",
                worker_id, job.request_id
            );
            continue;
        }

        let result = match process_job(&mut runner, &job) {
            Ok(srt_data) => JobResult::Success {
                request_id: job.request_id,
                srt_data,
            },
            Err(e) => JobResult::Error {
                request_id: job.request_id,
                message: e.to_string(),
            },
        };

        if result_tx.send(result).is_err() {
            error!("Worker {}: result channel closed", worker_id);
            break;
        }
    }

    info!("Worker {} stopped", worker_id);
}

fn is_cancelled(request_id: u64, cancelled: &Arc<Mutex<HashSet<u64>>>) -> bool {
    cancelled
        .lock()
        .map(|set| set.contains(&request_id))
        .unwrap_or(false)
}

fn process_job(runner: &mut SttRunner, job: &TranscriptionJob) -> Result<Vec<u8>> {
    let mut audio_file = NamedTempFile::new()?;
    audio_file.write_all(&job.audio_data)?;
    audio_file.flush()?;

    let duration_ms = if job.duration_ms > 0 {
        job.duration_ms
    } else {
        derive_duration_ms(audio_file.path()).unwrap_or(0)
    };

    runner.transcribe(audio_file.path(), audio_file.path(), duration_ms)?;

    let srt_path = audio_file.path().with_extension("srt");
    let srt_data = std::fs::read(&srt_path)?;

    Ok(srt_data)
}

fn derive_duration_ms(path: &std::path::Path) -> Option<u64> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    let samples = reader.len() as u64;
    let rate = spec.sample_rate as u64;
    if rate == 0 {
        return None;
    }
    Some(samples.saturating_mul(1000) / rate)
}
