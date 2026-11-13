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
- Administrator-selected incoming copies, durable staging/inspection jobs,
  checksum verification, and retained reports for manual review.

The library starts empty. Conversion, publication, playback, captions, TMDB,
scheduled backups/restore, and trash are not implemented. Inspection never
modifies or deletes incoming files and does not add movies to the library.
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
database, and `run --once` processes at most one diagnostic and one inspection.

```sh
target/debug/wetube-worker doctor
target/debug/wetube-server reset-password --username viewer
```

Stop the server and worker before account maintenance; maintenance commands
are excluded by service-lifetime locks; workers also hold an exclusive,
child-inherited ownership lock. Resetting a password revokes that account's
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

## Stage and inspect from the admin UI

Use the same finished-copy incoming directory for both processes, and a
separate worker-private scratch directory. Never configure the original archive
or an HTTP static directory as either root. The scratch parent must already
exist; a new scratch root is created with mode 0700. Existing scratch roots must
be owned by the worker UID with no group/other access, not be symlinks, and must
not overlap incoming. Keep scratch outside SMB exports, including ancestor
shares; local mode bits alone do not establish the NAS's deployed ACL behavior.

```sh
target/debug/wetube-server serve --incoming-dir /path/to/incoming
target/debug/wetube-worker run --incoming-dir /path/to/incoming --scratch-dir /private/path/to/scratch
```

In the admin panel, refresh incoming files, select an ordinary non-empty ISO
from the root (no recursion/symlinks), specify a title and angle, and acknowledge
copy completion. There is no automatic longest-title selection. Listing is
limited to 1,000 candidate ISOs / 10,000 directory entries; split larger batches.
Only one inspection may be queued or active. Missing worker staging configuration
leaves inspection jobs queued; diagnostics still work.

The worker checks the confirmed source identity, persists a unique attempt ID,
copies through a pinned read-only descriptor while computing SHA-256, checks
the source handle and root-relative pathname again, fsyncs, and rehashes the
private staged bytes. ffprobe then inspects that copy. Successful inspection
ends at **Needs review**, never publication or cleanup. Metadata/content checks
detect changes under the trusted-household protocol; they are not a guarantee
against malicious writers or proof of SMB writer exclusion.

Defaults admit ISOs up to 16 GiB, cap retained scratch file bytes at 32 GiB, and
require free space for the ISO plus a 1 GiB reserve before copying. These are
conservative admission checks, not a dataset quota or reservation against other
NAS workloads. Set real dataset quotas and choose deployment caps after
measuring available space. Copying plus rehashing has a ten-minute cooperative
budget checked between reads; kernel filesystem I/O itself has no hard deadline.
ffprobe retains its separate 120-second / 1 MiB-per-stream limits.

All incoming files and attempt directories are retained, including failures.
Interrupted staging/probing becomes Failed on worker restart, not an automatic
retry; refresh and reconfirm to submit again. Retained attempts count against
scratch limits. The admin history shows the latest 20 jobs and their attempt
IDs; older rows remain in SQLite. An attempt that failed before directory
creation may have no corresponding directory. Inspect scratch/attempt-ID
manually, with services stopped, before any operator-managed cleanup. No
automatic cleanup or report-approval action exists yet.

SIGTERM/Ctrl-C drains the worker's current work before releasing ownership,
including blocking copy I/O; allow time for staging and inspection to finish.
The worker heartbeat continues during active jobs. Force-killed jobs retain
their inputs; children retain the ownership lock until they exit.

## Upgrade the foundation state

Stop both services before running `wetube-server init` with this release.
Initialization upgrades schema 1 to 2 only after a consistent `VACUUM INTO`
snapshot is written and synced in a private `pre-upgrade-UUID` directory under
the state root. Newer/unknown versions fail closed. Repeating initialization
does not create another snapshot once the upgrade succeeds.

This is a narrow pre-upgrade safeguard for the foundation schema, which has no
subtitle assets. It is not the design's complete daily, off-device,
asset-aware backup/restore system. Keep these snapshots until an operator has
verified recovery; there is no automatic retention or restore command. To
rehearse a schema-1 snapshot, copy it into a separate private state directory
with services stopped and use the matching old release, not the live database.
Do not roll an old binary forward onto the upgraded live state.

## Configuration

| Environment variable | Default | Purpose |
| --- | --- | --- |
| `WETUBE_STATE_DIR` | `.wetube` | Persistent database and maintenance lock; use the same path for both processes. |
| `WETUBE_BIND` | `127.0.0.1:8080` | Server listening address. |
| `WETUBE_ORIGIN` | `http://localhost:8080` | Exact browser origin and accepted Host authority. |
| `WETUBE_STATIC_DIR` | `dist` | Built frontend directory. |
| `WETUBE_INCOMING_DIR` | Disabled | Finished-copy root for server and worker; same directory on both. |
| `WETUBE_SCRATCH_DIR` | Disabled | Worker-private staging root; required together with worker incoming. |
| `WETUBE_MAX_ISO_BYTES` | `17179869184` | Worker's per-input staging byte limit (16 GiB). |
| `WETUBE_MAX_SCRATCH_BYTES` | `34359738368` | Worker's total retained scratch byte limit (32 GiB). |
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

## Next integration checkpoint

Local implementation stops at staged inspection. Before encoding profiles or
Docker/TrueNAS packaging, supply disposable, completed ISO copies covering:

| Input | Evidence to collect |
| --- | --- |
| Simple movie | Correct title/angle, duration, chapters, and verified staged hash. |
| Multiple/ambiguous titles | Compare explicitly selected titles; choose the feature manually. |
| Interlaced/telecined movie | Inspect field order, then evaluate real decoding/deinterlacing. |
| Multilingual/surround audio | Track language/layout, client compatibility, stereo fallback requirements. |
| Image subtitles | Extract original images/timing and assess English OCR quality; Tesseract plus `eng` data required. |
| Incomplete/unreadable copy | Failed job, unchanged source, retained attempt, actionable error. |

On TrueNAS, first verify dataset paths/ACLs, private scratch and capacity, pinned
input identity stability, locking across workers/children, SMB rename/replacement
and outstanding-writer behavior, and the original archive's isolation. Rehearse
crashes/restarts and snapshot recovery without touching live state. Do not
enable deletion based only on these metadata probes.

Measure encoding CPU/RAM, throughput, temporary/output space, and NAS LAN/VPN
throughput before choosing renditions. Later playback acceptance includes
desktop Chrome/Edge/Firefox/Safari, iOS/iPadOS Safari, Android Chrome, seeking,
per-user resume, captions, audio selection, and four streams during an import.
Docker remains deferred; its next step is the reviewed builder/slim-runtime
multi-stage image and a verified TrueNAS app pull/install workflow, not an
assumption that this preview is deployable.

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
