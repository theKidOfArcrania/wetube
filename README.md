# WeTube

A private DVD movie library for a home TrueNAS NAS. DESIGN.md contains the
reviewed architecture and safety requirements.

## Current scope

This is a runnable foundation, not a DVD player yet:

- Rust/Axum API with SQLite, local admin/viewer accounts, and cookie sessions.
- Leptos/WASM login, library search, and administrator diagnostic results.
- Persistent, restart-recoverable diagnostic jobs and a single-owner worker.
- Read-only checks for FFmpeg, ffprobe, DVD-Video support, and English Tesseract data.
- Local, read-only inspection of an explicitly selected DVD ISO title and angle.

The library starts empty. ISO importing, conversion, playback, captions, TMDB,
backups, and trash are not implemented. Only the local `inspect-dvd` command
opens an explicitly selected ISO; it never modifies or deletes the input.
Automatic ISO deletion remains disabled pending the design's safety gates.
Docker packaging is intentionally deferred.

## Build locally

Use Linux, a current stable Rust toolchain, and Trunk:

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
cargo build --locked --package wetube-server --package wetube-worker
cd crates/web
trunk build --release
cd ../..
```

Trunk produces the frontend in dist/. Serve it through the Rust server, not a
separate development origin. If multiple Rust distributions are installed, use
the same toolchain for Cargo, Trunk, and the WASM target; `rustup run stable`
can select the standard Rust toolchain.

Media tools are optional for starting the foundation. Diagnostics require
`ffmpeg` and `ffprobe` with the `dvdvideo` demuxer, plus `tesseract` with `eng`
trained data. Missing capabilities produce a failed diagnostic with details;
installed tools alone do not prove that a particular ISO can be imported.

## Initialize and run

Run these commands from the workspace root:

```sh
target/debug/wetube-server init
target/debug/wetube-server add-user --username admin --admin
target/debug/wetube-server add-user --username viewer
```

Account commands prompt for a hidden password of at least 12 characters. There
are no default credentials or self-registration. `--password-stdin` is available
for automation; do not put real passwords in command arguments or shell history.

Start the server and worker in separate terminals:

```sh
target/debug/wetube-server serve
```

```sh
target/debug/wetube-worker run
```

Open `http://localhost:8080` and sign in. An administrator can queue a read-only
diagnostic in the Media readiness panel. Jobs remain queued while the worker is
offline. The worker's `doctor` command runs the same checks without opening the
database, and `run --once` processes at most one queued job.

```sh
target/debug/wetube-worker doctor
target/debug/wetube-server reset-password --username viewer
```

Stop the server and worker before account maintenance; maintenance commands
and workers share an exclusive lock. Resetting a password revokes that account's
sessions. Do not remove the lock file to bypass a running process.

## Inspect a DVD copy

On Linux, point this local operator command at a finished, disposable ISO copy,
not the original archive. It needs no database or running server:

```sh
target/debug/wetube-worker inspect-dvd --iso /path/to/copy.iso --title 1 --angle 1 --copy-complete
```

`--title` is required (1-99); `--angle` defaults to 1 (1-9). Inspect other title
numbers explicitly when comparing candidates. The command does not enumerate
all titles or decide which is the main movie.

The JSON report retains ffprobe's video, audio, subtitle, format, and chapter
metadata, including any reported language, channel layout, and field order.
Chapter timing is approximate: full-disc preindexing is disabled. Stream
metadata alone is not proof of correct deinterlacing, decoding, or caption OCR.

Input is a non-empty regular `.iso` file, opened read-only with final-component
symlinks rejected. The child receives a pinned, seekable file descriptor.
Device/inode, size, and nanosecond modification/change times are checked after
inspection; a changed or replaced source fails without emitting a usable report.
The subprocess is limited to 120 seconds and 1 MiB per output stream; Ctrl-C
terminates it. Unreadable/invalid DVDs produce an error and retain the input.
An excessive-output failure or timeout may require a separate manual analysis.

`--copy-complete` is an operator acknowledgement, not an SMB writer-exclusion
guarantee. This inspection does not stage/hash an import, bypass encryption,
publish media, or authorize cleanup. Representative real DVDs are still needed
to prove feasibility before implementing conversion.

## Configuration

| Environment variable | Default | Purpose |
| --- | --- | --- |
| `WETUBE_STATE_DIR` | `.wetube` | Persistent database and maintenance lock; use the same path for both processes. |
| `WETUBE_BIND` | `127.0.0.1:8080` | Server listening address. |
| `WETUBE_ORIGIN` | `http://localhost:8080` | Exact browser origin and accepted Host authority. |
| `WETUBE_STATIC_DIR` | `dist` | Built frontend directory. |
| `RUST_LOG` | Rust tracing default | Optional local log filter. |

Equivalent flags are available with `--help`. Initialization is explicit:
server and worker startup refuse a missing or incompatible database.

For LAN access, bind to the appropriate interface and configure one dedicated
hostname/origin that clients resolve to the NAS. Requests using a different
hostname, IP address, port, or mutation origin are rejected. Frontend and API
must share that origin.

**HTTP does not protect passwords, cookies, or movie traffic from LAN observers.**
Use only a trusted LAN or the separately managed VPN, as specified in DESIGN.md;
do not expose this service directly to the internet. This preview is not a
production-ready TrueNAS deployment.

## Development

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --package wetube-web --target wasm32-unknown-unknown --locked -- -D warnings
```

The workspace separates portable API contracts (crates/core), backend
persistence/authentication/locking (crates/backend), HTTP/administration
(crates/server), diagnostics (crates/worker), and browser UI (crates/web).
