# hls2dash

A high-performance HLS-to-DASH transpackaging proxy written in Rust. Converts Apple HLS (HTTP Live Streaming) manifests and segments to MPEG-DASH on the fly, without re-encoding media.

## Overview

`hls2dash` sits between a DASH player and an HLS origin server. It intercepts requests for `.m3u8` manifests, converts them to DASH MPDs, and transparently proxies media segments — optionally re-muxing MPEG-TS to fragmented MP4 via FFmpeg for maximum player compatibility.

```
DASH Player  ──►  hls2dash proxy  ──►  HLS Origin (CDN)
             ◄──  DASH MPD        ◄──  HLS M3U8
             ◄──  fMP4 segments   ◄──  TS/fMP4 segments
```

## Features

### Current

| Feature | Status |
|---|---|
| HLS master playlist → DASH MPD | ✅ |
| VOD streams (`EXT-X-ENDLIST`) | ✅ `type="static"` |
| Live / linear streams | ✅ `type="dynamic"` with `minimumUpdatePeriod` |
| Multiple video renditions | ✅ One `AdaptationSet`, multiple `Representation` |
| Multiple audio tracks (`EXT-X-MEDIA`) | ✅ Separate `AdaptationSet` per language |
| WebVTT subtitles | ✅ `contentType="text"` AdaptationSet |
| fMP4 / CMAF segments | ✅ Pass-through, `mimeType="video/mp4"` |
| MPEG-TS segments (pass-through) | ✅ `mimeType="video/MP2T"`, Shaka Player |
| MPEG-TS → fMP4 transmuxing (FFmpeg) | ✅ `TRANSMUX_TS=true`, all players incl. DASH.js |
| AES-128 key URI proxying | ✅ Key requests routed through proxy |
| Query string / signed URL passthrough | ✅ |
| In-memory playlist caching | ✅ Configurable TTL via `moka` |
| Streaming segment proxy (zero-copy) | ✅ No buffering for passthrough segments |
| `/dash/*path` DASH-native endpoint | ✅ No `.m3u8` in URL, works in all players |
| CORS headers | ✅ Permissive, configurable |
| Health check endpoint | ✅ `GET /health` |

### Roadmap

| Feature | Priority | Notes |
|---|---|---|
| **SCTE-35 ad markers** | High | Convert `EXT-X-DATERANGE` / SCTE-35 cues to DASH `EventStream` and `Period` boundaries |
| **Multi-DRM (Widevine / PlayReady / FairPlay)** | High | CPIX key exchange, `ContentProtection` elements, key rotation |
| **SAMPLE-AES / CBCS encryption** | High | Map HLS `KEYFORMAT="identity"` / `METHOD=SAMPLE-AES` to DASH `ContentProtection` |
| **Low-Latency DASH (LL-DASH)** | Medium | LL-HLS chunked transfer → DASH `availabilityTimeOffset`, `UTCTiming` |
| **Multi-period DASH** | Medium | Map `EXT-X-DISCONTINUITY` to DASH `Period` boundaries |
| **Segment-level caching** | Medium | Cache transmuxed fMP4 segments to avoid redundant FFmpeg calls |
| **Native TS→fMP4 muxer (no FFmpeg)** | Medium | Pure Rust implementation via `mpeg2ts` + `mp4` crates |
| **CMAF chunked encoding support** | Medium | Support `EXT-X-PART` for chunk-level addressing |
| **Prometheus metrics** | Low | Request count, segment latency, cache hit rate |
| **Kubernetes / Helm chart** | Low | Production deployment templates |
| **WebVTT subtitle segmentation** | Low | Proper segmented VTT handling for long-form VOD |
| **I-Frame playlists** | Low | `EXT-X-I-FRAMES-ONLY` → DASH trick-play |

## Architecture

```
src/
├── main.rs          — Server startup, routing, AppState
├── config.rs        — Configuration from environment variables
├── error.rs         — AppError with HTTP response mapping
├── cache.rs         — In-memory playlist cache (moka)
├── url_utils.rs     — URL resolution, proxy URL helpers, XML escaping
├── upstream.rs      — Upstream HTTP client (fetch + stream)
├── transmux.rs      — FFmpeg TS→fMP4 transmuxing, MP4 box parsing
├── hls/
│   └── mod.rs       — HLS M3U8 parsing (m3u8-rs), helpers
├── dash/
│   └── mod.rs       — DASH MPD XML generation
└── handlers/
    └── mod.rs       — Axum request handlers
```

### Request flow

```
GET /hls2dash/<host>/<path>/master.m3u8
  → fetch https://<host>/<path>/master.m3u8
  → parse HLS master playlist
  → fetch all variant + audio + subtitle media playlists (parallel)
  → generate DASH MPD
  → return application/dash+xml

GET /hls2dash/<host>/<path>/segment.ts   (TRANSMUX_TS=false)
  → stream https://<host>/<path>/segment.ts transparently

GET /hls2dash/<host>/<path>/segment.ts   (TRANSMUX_TS=true)
  → fetch https://<host>/<path>/segment.ts
  → pipe through ffmpeg (TS → fMP4)
  → strip moov, return moof+mdat as video/mp4

GET /hls2dash-init/<host>/<path>/segment.ts
  → fetch + transmux first TS segment
  → return ftyp+moov only (init segment for DASH.js)

GET /dash/<host>/<path>/manifest.mpd
  → rewrite .mpd → .m3u8, then same as manifest flow above
```

## Prerequisites

- [Rust](https://rustup.rs) 1.75+
- [FFmpeg](https://ffmpeg.org) in `PATH` (required only when `TRANSMUX_TS=true`)

## Quick start

```bash
git clone https://github.com/almeidadanilo/hls2dash.git
cd hls2dash
cp .env.example .env
cargo run
```

Test the health check:
```bash
curl http://localhost:3100/health
```

## Configuration

All settings are read from environment variables (or a `.env` file in the working directory).

| Variable | Default | Description |
|---|---|---|
| `PORT` | `3100` | TCP port to listen on |
| `PROXY_BASE` | _(empty)_ | Public base URL of this service (e.g. `https://hls2dash.example.com`). Used to generate absolute URLs in MPDs. Leave empty when behind a reverse proxy. |
| `TRANSMUX_TS` | `false` | Re-mux MPEG-TS segments to fMP4 via FFmpeg. Required for DASH.js compatibility with TS streams. |
| `CACHE_MAX_CAPACITY` | `500` | Maximum number of cached playlist entries |
| `UPSTREAM_TIMEOUT_SECS` | `15` | HTTP timeout for upstream HLS requests |
| `LOG_LEVEL` | `info` | Tracing log level: `error`, `warn`, `info`, `debug`, `trace` |

## API endpoints

### `GET /hls2dash/<host>/<path>`

Universal proxy endpoint. Behaviour depends on the file extension:

| Extension | Behaviour |
|---|---|
| `.m3u8` | Parse HLS, generate and return DASH MPD |
| `.ts` (with `TRANSMUX_TS=true`) | Fetch, transmux to fMP4, return media segment |
| anything else | Transparent byte-stream proxy |

**Example:**
```
GET /hls2dash/cdn.example.com/live/channel1/master.m3u8
→ returns DASH MPD (application/dash+xml)

GET /hls2dash/cdn.example.com/live/channel1/seg001.ts
→ returns fMP4 segment (video/mp4) or TS (video/MP2T)
```

### `GET /dash/<host>/<path>`

DASH-native endpoint — always returns a DASH MPD regardless of URL extension. Use this in DASH players to avoid `.m3u8` extension sniffing.

**Example:**
```
GET /dash/cdn.example.com/live/channel1/master.mpd
→ fetches master.m3u8 upstream, returns DASH MPD
```

### `GET /hls2dash-init/<host>/<path>`

Returns the fMP4 init segment (`ftyp+moov`) for a given TS segment URL. Automatically referenced in MPDs when `TRANSMUX_TS=true`.

### `GET /health`

Returns `200 OK` with body `OK`. Used for load balancer health checks.

## Player compatibility

| Player | fMP4 streams | TS streams (`TRANSMUX_TS=false`) | TS streams (`TRANSMUX_TS=true`) |
|---|---|---|---|
| DASH.js | ✅ | ❌ | ✅ |
| Shaka Player | ✅ | ✅ | ✅ |
| ExoPlayer (Android) | ✅ | ⚠️ limited | ✅ |
| AVPlayer (iOS/tvOS) | ✅ | ❌ | ✅ |
| Video.js (DASH plugin) | ✅ | ❌ | ✅ |

## Building for production

```bash
cargo build --release
./target/release/hls2dash
```

## Docker

```bash
docker build -t hls2dash .
docker run -d \
  --name hls2dash \
  --restart unless-stopped \
  -p 3100:3100 \
  -e PORT=3100 \
  -e PROXY_BASE=https://hls2dash.example.com \
  -e TRANSMUX_TS=true \
  hls2dash
```

## Deployment on AWS EC2 + Cloudflare

### 1. Launch EC2 instance

- AMI: **Ubuntu 24.04 LTS**
- Instance type: `t3.micro` (free tier eligible)
- Security group: open ports `22` (SSH, your IP), `80`, `443`

### 2. Install dependencies on the server

```bash
sudo apt update && sudo apt install -y ffmpeg docker.io
sudo usermod -aG docker ubuntu   # log out and back in
```

### 3. Deploy via Docker

```bash
git clone https://github.com/almeidadanilo/hls2dash.git && cd hls2dash
docker build -t hls2dash .
docker run -d \
  --name hls2dash \
  --restart unless-stopped \
  -p 80:3100 \
  -e PROXY_BASE=https://hls2dash.yourdomain.com \
  -e TRANSMUX_TS=true \
  hls2dash
```

### 4. Configure Cloudflare DNS

Add an **A record** in your Cloudflare dashboard:
- **Name:** `hls2dash`
- **Value:** EC2 public IP
- **Proxy:** ON (orange cloud)

Cloudflare handles HTTPS termination automatically — no SSL certificate setup required on the server.

Your API will be available at `https://hls2dash.yourdomain.com`.

## Known limitations

- MPEG-TS transmuxing buffers the full segment before piping to FFmpeg. For large segments (>30s), this increases memory usage transiently.
- `SegmentList`-based MPDs list every segment explicitly; for very long VOD content this can produce large MPDs. `SegmentTemplate` support is on the roadmap.
- AES-128 key URIs are proxied but key rotation across segments is not yet validated end-to-end.
- SCTE-35 cues embedded in TS PES or MPEG sections are parsed by FFmpeg during transmux but not yet surfaced in the MPD as `EventStream` elements.

## Technology stack

| Crate | Purpose |
|---|---|
| `axum` | HTTP server framework |
| `tokio` | Async runtime |
| `reqwest` | Upstream HTTP client |
| `m3u8-rs` | HLS M3U8 parser |
| `moka` | In-memory async cache |
| `tower-http` | CORS, tracing middleware |
| `url` | URL parsing and resolution |
| `chrono` | Live stream timing (availabilityStartTime) |
| `tracing` | Structured logging |
| FFmpeg | TS→fMP4 transmuxing (external binary) |

## License

Internal use.
