# MPV STT UDP Server

原 WhisperSubs UDP 服务器，现与 `mpv_stt_plugin_rs` 插件保持一致命名与协议，提供远程 Whisper 推理服务。

## 特性

- **异步架构**: 使用tokio实现高性能异步UDP通信
- **Worker池**: 多线程Whisper推理，支持并发请求
- **自动重组**: 处理分块的音频数据传输
- **取消支持**: 支持客户端取消正在进行的请求
- **Postcard协议**: 轻量的二进制序列化，兼容 mpv 插件

## 构建

### CPU 版（默认）
```bash
cargo build --release
```

### CUDA 版
```bash
cargo build --release --no-default-features --features stt_local_cuda
```

### 无系统 mpv 头文件的快速方式
`mpv-client-sys` 需要 `mpv/client.h`，但本服务端只依赖其接口类型。若系统未安装 mpv 开发包，可用随仓脚本自动拉取头文件后再运行任意 cargo 命令：
```bash
scripts/cargo-with-mpv.sh clippy --no-deps -- -D warnings
# 或
scripts/cargo-with-mpv.sh build --release
```
脚本会在 `target/mpv-headers` 下浅克隆 mpv 仓库，仅供 bindgen 使用；实际运行时无需 mpv 本体。

## 使用

### 基本用法
```bash
mpv-stt-server \
  --bind 0.0.0.0:9000 \
  --model-path /path/to/ggml-base.bin \
  --threads 8 \
  --language auto \
  --workers 4
```

### 推荐（含加密与鉴权）
```bash
mpv-stt-server \
  --bind 0.0.0.0:9000 \
  --model-path /path/to/ggml-base.bin \
  --threads 8 \
  --language auto \
  --workers 4 \
  --enable-encryption \
  --encryption-key "your-passphrase" \
  --auth-secret "shared-secret"
```

客户端（mpv_stt_plugin_rs）的远程配置示例 `~/.config/mpv/mpv_stt_plugin_rs.toml`：
```toml
[stt.remote_udp]
server_addr = "192.168.1.100:9000"
timeout_ms = 120000
max_retry = 3
enable_encryption = true
encryption_key = "your-passphrase"
auth_secret = "shared-secret"
```

### 完整参数
```
Options:
  -b, --bind <BIND>                UDP bind address [default: 0.0.0.0:9000]
  -m, --model-path <MODEL_PATH>    Path to Whisper model [default: ggml-base.bin]
  -t, --threads <THREADS>          CPU threads for inference [default: 8]
  -l, --language <LANGUAGE>        Language code (en/zh/auto) [default: auto]
      --gpu-device <GPU_DEVICE>    GPU device ID (CUDA only) [default: 0]
      --flash-attn                 Enable flash attention (CUDA only)
      --timeout-ms <TIMEOUT_MS>    Inference timeout [default: 120000]
  -w, --workers <WORKERS>          Number of worker threads [default: 4]
  -h, --help                       Print help
  -V, --version                    Print version
```

## 协议

### 消息类型（postcard + 可选 AES-GCM 加密）

音频分片固定使用 Opus 压缩，封包均为 postcard 序列化的枚举：

#### AudioChunk (客户端→服务器)
```rust
AudioChunk {
    request_id: u64,
    chunk_index: u32,
    total_chunks: u32,
    duration_ms: u64,
    data: Vec<u8>,
    auth_token: [u8; 32],       // 来自 auth_secret 计算
    compression: CompressionFormat, // 目前仅 Opus
}
```

#### Cancel (客户端→服务器)
```rust
Cancel {
    request_id: u64,
    auth_token: [u8; 32],
}
```

#### Result (服务器→客户端)
```rust
Result {
    request_id: u64,
    chunk_index: u32,
    total_chunks: u32,
    data: Vec<u8>,  // SRT格式
}
```

#### Error (服务器→客户端)
```rust
Error {
    request_id: u64,
    message: String,
}
```

> 说明：启用 `--enable-encryption` 时需同时在客户端配置相同的 `encryption_key`；启用鉴权则双方需设置一致的 `auth_secret`。

### 取消语义
- 客户端发送 `Cancel{request_id}` 后，服务端会：
  - 清理尚未重组完成的分片；
  - 通知工作线程跳过已进入队列的同 ID 任务；
  - 丢弃后续产生的结果/错误，避免超时重试。

## 架构

```
客户端请求
    ↓
UDP Socket (tokio)
    ↓
消息解析 (postcard)
    ↓
音频块重组
    ↓
Worker池 (mpsc channel)
    ↓
Whisper推理 (blocking thread)
    ↓
SRT结果分块
    ↓
UDP响应发送
```

## 环境变量

- `RUST_LOG`: 日志级别 (e.g., `info`, `debug`, `trace`)

## 示例

### 启动服务器
```bash
RUST_LOG=info mpv-stt-server \
  --bind 0.0.0.0:9000 \
  --model-path models/ggml-base.bin \
  --threads 8 \
  --workers 4
```

### 客户端配置 (mpv 插件配置，例如 ~/.config/mpv/mpv_stt_plugin_rs.toml)
```toml
whisper_server_addr = "192.168.1.100:9000"
udp_timeout_ms = 120000
udp_max_retry = 3
```

## 性能调优

- **workers**: 根据CPU核心数调整，建议为`threads/2`
- **threads**: Whisper推理线程数，影响单个请求速度
- **timeout_ms**: 根据音频长度和模型大小调整
