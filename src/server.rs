use crate::protocol::{CompressionFormat, JobResult, Message, TranscriptionJob};
use crate::worker::WorkerPool;
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use mpv_stt_plugin_rs::{AuthToken, EncryptionKey, LocalModelConfig};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

const MAX_DATAGRAM_SIZE: usize = 65507;
const ASSEMBLY_TIMEOUT_SECS: u64 = 60;

pub struct ServerConfig {
    pub enable_encryption: bool,
    pub encryption_key: String,
    pub auth_secret: String,
}

struct PendingAssembly {
    chunks: HashMap<u32, Vec<u8>>,
    total_chunks: u32,
    duration_ms: u64,
    last_update: Instant,
    compression: crate::protocol::CompressionFormat,
}

struct ReceivedChunk {
    request_id: u64,
    chunk_index: u32,
    total_chunks: u32,
    duration_ms: u64,
    data: Vec<u8>,
    compression: crate::protocol::CompressionFormat,
    addr: SocketAddr,
}

pub struct UdpServer {
    socket: Arc<UdpSocket>,
    worker_pool: WorkerPool,
    assemblies: HashMap<u64, PendingAssembly>,
    client_addrs: HashMap<u64, SocketAddr>,
    cancelled: HashSet<u64>,
    encryption_key: Option<EncryptionKey>,
    expected_auth_token: Option<AuthToken>,
}

impl UdpServer {
    pub async fn bind(
        addr: &str,
        config: LocalModelConfig,
        num_workers: usize,
        server_config: ServerConfig,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(addr)
            .await
            .context("Failed to bind UDP socket")?;
        info!("UDP server listening on {}", socket.local_addr()?);

        let worker_pool = WorkerPool::new(config, num_workers);

        let encryption_key = if server_config.enable_encryption {
            Some(EncryptionKey::from_passphrase(
                &server_config.encryption_key,
            ))
        } else {
            None
        };

        let expected_auth_token = if !server_config.auth_secret.is_empty() {
            Some(AuthToken::from_secret(&server_config.auth_secret))
        } else {
            None
        };

        Ok(Self {
            socket: Arc::new(socket),
            worker_pool,
            assemblies: HashMap::new(),
            client_addrs: HashMap::new(),
            cancelled: HashSet::new(),
            encryption_key,
            expected_auth_token,
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let mut buf = vec![0u8; MAX_DATAGRAM_SIZE];
        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut buf) => {
                    match result {
                        Ok((len, addr)) => {
                            if let Err(e) = self.handle_packet(&buf[..len], addr).await {
                                error!("Error handling packet from {}: {}", addr, e);
                            }
                        }
                        Err(e) => {
                            error!("Socket recv error: {}", e);
                        }
                    }
                }

                Some(job_result) = self.worker_pool.next_result() => {
                    if let Err(e) = self.handle_job_result(job_result).await {
                        error!("Error handling job result: {}", e);
                    }
                }

                _ = cleanup_interval.tick() => {
                    self.cleanup_stale_assemblies();
                }
            }
        }
    }

    async fn handle_packet(&mut self, data: &[u8], addr: SocketAddr) -> Result<()> {
        let message = Message::decode(data, self.encryption_key.as_ref())
            .context("Failed to decode message")?;

        if let Some(expected_token) = &self.expected_auth_token {
            if let Some(received_token) = message.auth_token() {
                let received = AuthToken::from_bytes(*received_token);
                if &received != expected_token {
                    warn!("Authentication failed from {}", addr);
                    let error_msg = Message::Error {
                        request_id: message.request_id(),
                        message: "Unauthorized".to_string(),
                    };
                    self.send_message(&error_msg, addr).await?;
                    return Ok(());
                }
            } else {
                warn!("Missing auth token from {}", addr);
                return Ok(());
            }
        }

        match message {
            Message::AudioChunk {
                request_id,
                chunk_index,
                total_chunks,
                duration_ms,
                data,
                compression,
                ..
            } => {
                let chunk = ReceivedChunk {
                    request_id,
                    chunk_index,
                    total_chunks,
                    duration_ms,
                    data,
                    compression,
                    addr,
                };
                self.handle_audio_chunk(chunk).await?;
            }
            Message::Cancel { request_id, .. } => {
                self.handle_cancel(request_id, addr).await?;
            }
            _ => {
                warn!("Unexpected message type from {}", addr);
            }
        }

        Ok(())
    }

    async fn handle_audio_chunk(&mut self, chunk: ReceivedChunk) -> Result<()> {
        let request_id = chunk.request_id;
        let assembly = self.assemblies.entry(request_id).or_insert_with(|| {
            debug!(
                "Starting new assembly for request {} from {}",
                request_id, chunk.addr
            );
            self.client_addrs.entry(request_id).or_insert(chunk.addr);
            PendingAssembly {
                chunks: HashMap::new(),
                total_chunks: chunk.total_chunks,
                duration_ms: chunk.duration_ms,
                last_update: Instant::now(),
                compression: chunk.compression,
            }
        });

        if assembly.total_chunks != chunk.total_chunks {
            warn!(
                "Mismatched total_chunks for request {}: stored {}, incoming {}",
                request_id, assembly.total_chunks, chunk.total_chunks
            );
        }

        assembly.last_update = Instant::now();
        assembly.chunks.insert(chunk.chunk_index, chunk.data);

        let expected_total = assembly.total_chunks;

        if assembly.chunks.len() == expected_total as usize {
            let mut compressed_data = Vec::new();
            for i in 0..expected_total {
                if let Some(chunk) = assembly.chunks.get(&i) {
                    compressed_data.extend_from_slice(chunk);
                } else {
                    warn!("Missing chunk {} for request {}", i, request_id);
                    return Ok(());
                }
            }

            let audio_data = match assembly.compression {
                CompressionFormat::Opus => {
                    info!(
                        "Decompressing Opus data ({} bytes) for request {}",
                        compressed_data.len(),
                        request_id
                    );
                    Self::decompress_opus(&compressed_data)?
                }
            };

            info!(
                "Assembled request {} ({} bytes, compression: Opus) from {}",
                request_id,
                audio_data.len(),
                self.client_addrs
                    .get(&request_id)
                    .copied()
                    .unwrap_or(chunk.addr)
            );

            let job = TranscriptionJob {
                request_id,
                audio_data,
                duration_ms: assembly.duration_ms,
            };

            // Drop the buffered chunks but keep client address mapping
            self.assemblies.remove(&request_id);
            self.worker_pool.submit_job(job)?;
        }

        Ok(())
    }

    async fn handle_cancel(&mut self, request_id: u64, addr: SocketAddr) -> Result<()> {
        self.cancelled.insert(request_id);
        self.worker_pool.cancel_request(request_id);
        if self.assemblies.remove(&request_id).is_some() {
            info!("Cancelled pending assembly for request {}", request_id);
        }

        // Client address is no longer needed once we acknowledged cancellation
        self.client_addrs.remove(&request_id);

        let error_msg = Message::Error {
            request_id,
            message: "Cancelled".to_string(),
        };

        self.send_message(&error_msg, addr).await?;
        Ok(())
    }

    async fn handle_job_result(&mut self, result: JobResult) -> Result<()> {
        match result {
            JobResult::Success {
                request_id,
                srt_data,
            } => {
                if self.cancelled.contains(&request_id) {
                    info!("Dropping result for cancelled request {}", request_id);
                    return Ok(());
                }
                self.send_srt_result(request_id, srt_data).await?;
            }
            JobResult::Error {
                request_id,
                message,
            } => {
                error!("Job {} failed: {}", request_id, message);
                if self.cancelled.contains(&request_id) {
                    info!("Dropping error for cancelled request {}", request_id);
                    return Ok(());
                }
                let error_msg = Message::Error {
                    request_id,
                    message,
                };
                if let Some(addr) = self.client_addrs.get(&request_id).copied() {
                    self.send_message(&error_msg, addr).await?;
                    self.client_addrs.remove(&request_id);
                    self.cancelled.remove(&request_id);
                }
            }
        }
        Ok(())
    }

    async fn send_srt_result(&mut self, request_id: u64, srt_data: Vec<u8>) -> Result<()> {
        let max_chunk_size = MAX_DATAGRAM_SIZE - 1024;
        let chunks: Vec<Vec<u8>> = srt_data
            .chunks(max_chunk_size)
            .map(|c| c.to_vec())
            .collect();
        let total_chunks = chunks.len() as u32;

        let client_addr = self
            .client_addrs
            .get(&request_id)
            .copied()
            .context("Client address not found for request")?;

        for (index, chunk_data) in chunks.into_iter().enumerate() {
            let message = Message::Result {
                request_id,
                chunk_index: index as u32,
                total_chunks,
                data: chunk_data,
            };

            self.send_message(&message, client_addr).await?;
        }

        info!(
            "Sent {} result chunks for request {}",
            total_chunks, request_id
        );
        // Clean up address mapping once response is sent
        self.client_addrs.remove(&request_id);
        self.cancelled.remove(&request_id);
        Ok(())
    }

    async fn send_message(&self, message: &Message, addr: SocketAddr) -> Result<()> {
        let data = message.encode(self.encryption_key.as_ref())?;
        self.socket.send_to(&data, addr).await?;
        Ok(())
    }

    fn decompress_opus(compressed: &[u8]) -> Result<Vec<u8>> {
        use std::convert::TryInto;
        use std::ffi::CStr;
        use std::os::raw::c_int;

        use hound::WavWriter;
        use opus_static_sys as opus;
        use tempfile::NamedTempFile;

        const SAMPLE_RATE: c_int = 16_000;
        const CHANNELS: c_int = 1;
        // 120 ms @ 48k = 5760 samples; safe upper bound for 16k streams too.
        const MAX_FRAME_SIZE: usize = 5760;

        let mut err: c_int = 0;
        let decoder = unsafe { opus::opus_decoder_create(SAMPLE_RATE, CHANNELS, &mut err) };
        if decoder.is_null() || err != opus::OPUS_OK as c_int {
            let msg = unsafe {
                CStr::from_ptr(opus::opus_strerror(err))
                    .to_string_lossy()
                    .into_owned()
            };
            anyhow::bail!("Failed to create Opus decoder: {}", msg);
        }

        let mut samples = Vec::new();
        let mut pos = 0;

        while pos + 4 <= compressed.len() {
            let frame_len = u32::from_le_bytes(
                compressed[pos..pos + 4]
                    .try_into()
                    .expect("slice length validated"),
            ) as usize;
            pos += 4;

            if pos + frame_len > compressed.len() {
                unsafe { opus::opus_decoder_destroy(decoder) };
                anyhow::bail!("Invalid Opus frame length");
            }

            let frame = &compressed[pos..pos + frame_len];
            pos += frame_len;

            let mut output = vec![0i16; MAX_FRAME_SIZE];
            let decoded_samples = unsafe {
                opus::opus_decode(
                    decoder,
                    frame.as_ptr(),
                    frame_len as opus::opus_int32,
                    output.as_mut_ptr(),
                    MAX_FRAME_SIZE as c_int,
                    0,
                )
            };

            if decoded_samples < 0 {
                let msg = unsafe {
                    CStr::from_ptr(opus::opus_strerror(decoded_samples))
                        .to_string_lossy()
                        .into_owned()
                };
                unsafe { opus::opus_decoder_destroy(decoder) };
                anyhow::bail!("Opus decode failed: {}", msg);
            }

            samples.extend_from_slice(&output[..decoded_samples as usize]);
        }

        unsafe { opus::opus_decoder_destroy(decoder) };

        let temp_file = NamedTempFile::new().context("Failed to create temp file")?;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        {
            let mut writer =
                WavWriter::create(temp_file.path(), spec).context("Failed to create WAV writer")?;

            for sample in &samples {
                writer
                    .write_sample(*sample)
                    .context("Failed to write WAV sample")?;
            }

            writer.finalize().context("Failed to finalize WAV")?;
        }

        let wav_data = std::fs::read(temp_file.path()).context("Failed to read WAV file")?;

        info!(
            "Opus decompression: {} frames → {} samples → {} bytes WAV",
            (compressed.len() / 1024).max(1),
            samples.len(),
            wav_data.len()
        );

        Ok(wav_data)
    }

    fn cleanup_stale_assemblies(&mut self) {
        let now = Instant::now();
        let timeout = Duration::from_secs(ASSEMBLY_TIMEOUT_SECS);

        self.assemblies.retain(|request_id, assembly| {
            let elapsed = now.duration_since(assembly.last_update);
            let keep = elapsed < timeout;
            if !keep {
                warn!("Dropping stale assembly for request {}", request_id);
                self.client_addrs.remove(request_id);
                self.cancelled.remove(request_id);
            }
            keep
        });
    }
}
