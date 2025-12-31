use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CompressionFormat {
    Opus,
}

/// UDP消息类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// 音频数据块
    AudioChunk {
        request_id: u64,
        chunk_index: u32,
        total_chunks: u32,
        duration_ms: u64,
        data: Vec<u8>,
        auth_token: [u8; 32],
        compression: CompressionFormat,
    },
    /// 取消请求
    Cancel {
        request_id: u64,
        auth_token: [u8; 32],
    },
    /// 转录结果（SRT格式）
    Result {
        request_id: u64,
        chunk_index: u32,
        total_chunks: u32,
        data: Vec<u8>,
    },
    /// 错误响应
    Error { request_id: u64, message: String },
}

impl Message {
    pub fn request_id(&self) -> u64 {
        match self {
            Message::AudioChunk { request_id, .. }
            | Message::Cancel { request_id, .. }
            | Message::Result { request_id, .. }
            | Message::Error { request_id, .. } => *request_id,
        }
    }

    pub fn auth_token(&self) -> Option<&[u8; 32]> {
        match self {
            Message::AudioChunk { auth_token, .. } | Message::Cancel { auth_token, .. } => {
                Some(auth_token)
            }
            _ => None,
        }
    }

    pub fn encode(
        &self,
        encryption_key: Option<&mpv_stt_plugin_rs::EncryptionKey>,
    ) -> Result<Vec<u8>> {
        let serialized = postcard::to_allocvec(self).context("postcard encode failed")?;

        if let Some(key) = encryption_key {
            key.encrypt(&serialized).map_err(anyhow::Error::from)
        } else {
            Ok(serialized)
        }
    }

    pub fn decode(
        data: &[u8],
        encryption_key: Option<&mpv_stt_plugin_rs::EncryptionKey>,
    ) -> Result<Self> {
        let decrypted = if let Some(key) = encryption_key {
            key.decrypt(data).context("decrypt message failed")?
        } else {
            data.to_vec()
        };

        postcard::from_bytes(&decrypted).context("postcard decode failed")
    }
}

/// 推理任务
#[derive(Debug)]
pub struct TranscriptionJob {
    pub request_id: u64,
    pub audio_data: Vec<u8>,
    pub duration_ms: u64,
}

/// 推理结果
#[derive(Debug)]
pub enum JobResult {
    Success { request_id: u64, srt_data: Vec<u8> },
    Error { request_id: u64, message: String },
}
