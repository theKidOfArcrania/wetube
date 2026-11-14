# WeTube

A private DVD movie library for a home TrueNAS NAS. DESIGN.md contains the
reviewed architecture and safety requirements.

## Current scope

This is a runnable local website preview, not a production TrueNAS deployment:

- Rust/Axum API with SQLite, local admin/viewer accounts, and cookie sessions.
- Leptos/WASM login, library search, authenticated MP4 playback/seeking, and
  per-user resume.
- Persistent, restart-recoverable diagnostic jobs and a single-owner worker.
- Read-only checks for required media capabilities and optional English OCR.
- DVD program discovery with duration/track information and title/angle dropdowns.
- Administrator-selected incoming copies, checksum-verified private staging,
  inspection, explicit conversion, and separate reviewed local-library publication.
- Preserved original audio/image-subtitle sidecars and retained failed attempts.
- Live, persisted media-phase progress with labeled stage-based workflow bars.

The library starts empty. The complete production publication protocol, captions,
TMDB, scheduled backups/restore, and trash are not implemented. Inspection never
modifies or deletes incoming files and does not add movies to the library.
Automatic ISO deletion remains disabled pending the design's safety gates.
Docker packaging is intentionally deferred.

A separate opt-in `import_dvd` Rust example exercises experimental conversion
and full decoding without enabling the production pipeline; see below.

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

Media tools are optional for starting the website. Scanning/conversion require
`ffmpeg` and `ffprobe` with DVD-Video support, H.264/AAC encoding, and the native
`libdvdnav`/`libdvdread` libraries. Discovery uses a bounded native helper, not a
mandatory `lsdvd` subprocess. Tesseract with `eng` trained data is optional and
does not determine conversion readiness; caption OCR itself is not implemented.
Installed tools alone do not prove that a particular ISO can be imported.

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
database. Jobs require the worker to be running.

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

## Scan, convert, and publish from the admin UI

Use the same finished-copy incoming directory for both processes, and a
separate scratch and prepared-media directories with filesystem-managed access
controls. Create the directories before starting either process. Never
configure the original archive
or an HTTP static directory as either root. The scratch parent must already
exist; a new scratch root requests mode 0700. Existing scratch roots must not be
symlinks or overlap incoming. The service does not enforce Unix ownership or
mode bits: configure and verify access through the host filesystem's ACLs,
including Windows ACLs for Windows-backed storage. For deployment, keep scratch
outside SMB exports, including ancestor shares; local mode bits alone do not
establish the NAS's deployed ACL behavior.

```sh
target/debug/wetube-server serve --incoming-dir /mnt/share/staging --scratch-dir /mnt/c/TempPrivate/website-staging --media-dir /mnt/c/TempPrivate/website-media
target/debug/wetube-worker run --incoming-dir /mnt/share/staging --scratch-dir /mnt/c/TempPrivate/website-staging --media-dir /mnt/c/TempPrivate/website-media
```

Use matching state, incoming, scratch, and media roots for both processes; keep
state, static assets, incoming, scratch, and media separate and non-overlapping.
On this development host, all large ISO, conversion, and publication artifacts
belong on C:, never in `/tmp` or another Linux staging directory. Small SQLite
state is separate.

1. Refresh incoming files, select an ordinary non-empty ISO from the root
   (no recursion/symlinks), acknowledge the completed copy, and choose **Scan DVD**.
2. Select a discovered DVD title using its duration, chapters, and tracks.
   The longest program is only a main-feature candidate, not an automatic choice.
   Choose an angle if the selected program has multiple angles.
3. Choose **Inspect selected title**. Review its report and warnings, explicitly
   select Preserve or Yadif, acknowledge the experimental treatment, and convert.
4. After full decoding and output validation succeed, review the title, optional
   year, description, and warnings. Confirm **Publish to local library** separately.
5. After publication succeeds, choose **Watch published movie** or open it from
   the library.

Listing is limited to 1,000 candidate ISOs / 10,000 directory entries; split
larger batches. Jobs are serialized by the worker. Missing worker staging
configuration leaves jobs queued; diagnostics still work.

The worker checks the confirmed source identity, persists a unique attempt ID,
copies through a pinned read-only descriptor while computing SHA-256, checks
the source handle and root-relative pathname again, fsyncs, and rehashes the
private staged bytes. Scan, selected inspection, and conversion reuse the same
staged input. Successful inspection ends at **Needs review**, never publication
or cleanup. Conversion fully decodes the source and output, records hashes, and
retains `movie.mp4`, `original-tracks.mkv`, reports, and logs in the attempt.
Publication independently verifies the copied package before registration.
Metadata/content checks
detect changes under the trusted-household protocol; they are not a guarantee
against malicious writers or proof of SMB writer exclusion.

Defaults admit ISOs up to 16 GiB, cap retained scratch file bytes at 32 GiB, and
require free space for the ISO plus a 1 GiB reserve before copying. These are
conservative admission checks, not a dataset quota or reservation against other
NAS workloads. Set real dataset quotas and choose deployment caps after
measuring available space. Copying plus rehashing has a ten-minute cooperative
budget checked between reads, configurable from 1 to 3,600 seconds with
`--staging-timeout-secs` (default 600). Allow for both copy and full rehash,
particularly over slower mounts; kernel filesystem I/O itself has no hard deadline.
ffprobe retains its separate 120-second / 1 MiB-per-stream limits.

All incoming files and attempt directories are retained, including failures.
Interrupted staging/probing becomes Failed on worker restart, not an automatic
retry; refresh and reconfirm to submit again. Retained attempts count against
scratch limits. The admin history shows the latest 20 jobs and their attempt
IDs; older rows remain in SQLite. An attempt that failed before directory
creation may have no corresponding directory. Inspect scratch/attempt-ID
manually, with services stopped, before any operator-managed cleanup. No
automatic cleanup exists. Failed conversion reports and their output files are
not accepted as validated media.

SIGTERM/Ctrl-C drains the worker's current work before releasing ownership,
including blocking copy I/O; allow time for staging and inspection to finish.
The worker heartbeat continues during active jobs. Force-killed jobs retain
their inputs; children retain the ownership lock until they exit.

## Upgrade local state

Stop both services before running `wetube-server init` with this release.
Initialization upgrades supported older schemas (1–4) to schema 5 only after a consistent `VACUUM INTO`
snapshot is written and synced in a private `pre-upgrade-UUID` directory under
the state root. Newer/unknown versions fail closed. Repeating initialization
does not create another snapshot once the upgrade succeeds.

Schema 5 adds nullable progress snapshots to scan, inspection and conversion
jobs. Stop **both** services, retain the state/media/attempt roots, run the matching
`wetube-server --state-dir <state-root> init`, then restart the matching server
and worker with the same roots and rebuilt frontend. Existing jobs have no
historical telemetry; interrupted jobs still fail closed and retain their last
observation rather than resuming a clock. Do not initialize a database while
either old service is running.

Progress bars count workflow stages, not equal amounts of work or remaining
time. Only measurable current phases show percentage, elapsed, average rate and
estimated phase remainder. DVD discovery/metadata inspection are indeterminate;
review waits and stale/failed observations have no live ETA. Publication includes
separate copy, decode and checksum phases for the movie and preservation assets.
Current phase fills are capped at 99% until validation advances the stage;
only durable job state establishes completion/readiness. Progress stdout logs
retain at most the initial 2 MiB per tool; final validation uses a bounded 2 MiB
tail while continuing to drain progress, so a long conversion cannot exhaust
memory or fill scratch with progress logs. Error logs keep their existing quota.

This is a narrow SQLite-only pre-upgrade safeguard, not the design's complete
daily, off-device, asset-aware backup/restore system. Schema 3 can already
reference media: retain those asset files unchanged and in place so its snapshot
can still refer to the original assets. Changing media roots is not a database
migration; files need independent verification/admission, not rewritten fingerprints.
Keep these snapshots until an operator has
verified recovery; there is no automatic retention or restore command. To
rehearse a snapshot, copy it into a separate private state directory
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
| `WETUBE_SCRATCH_DIR` | Disabled | Private staging root; configure the same root for server and worker. |
| `WETUBE_MEDIA_DIR` | Disabled | Prepared local-library packages; configure the same separate root for server and worker. |
| `WETUBE_MAX_ISO_BYTES` | `17179869184` | Worker's per-input staging byte limit (16 GiB). |
| `WETUBE_MAX_SCRATCH_BYTES` | `34359738368` | Worker's total retained scratch byte limit (32 GiB). |
| `WETUBE_STAGING_TIMEOUT_SECS` | `600` | Combined copy/rehash cooperative budget, 1-3,600 seconds. |
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

The local website now connects scanning through validated publication and
playback. Six Chromium scenarios include an authored two-title DVD end to end;
this is not acceptance of full-length real discs, production publication, or
TrueNAS. Before Docker/TrueNAS packaging, supply disposable, completed ISO copies
covering:

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

## Experimental full-import example

The standalone program at crates/worker/examples/import_dvd.rs exercises:
finished ISO copy -> verified staging -> full selected-title video/audio decode
-> experimental encoding -> output metadata/track checks -> full output decode.
It uses no application database or HTTP server and never publishes or deletes.
Stop other workers/examples using the same scratch root before running it.

```sh
cargo run --release --locked --package wetube-worker --example import_dvd -- \
  --iso /path/to/incoming/movie.iso \
  --scratch-dir /private/scratch \
  --title 1 --angle 1 --copy-complete \
  --video-treatment yadif \
  --staging-timeout-secs 1800 --tool-timeout-secs 14400 --threads 2
```

For this session, the ISO is /mnt/share/staging/COUNT_OF_MONTE_CRISTO.ISO and
the requested local scratch directory is /mnt/c/TempPrivate. This tests the
media pipeline only; filesystem access controls and deployment safety gates
must be verified separately.

`--video-treatment` is required: `yadif` uses single-rate deinterlacing;
`preserve` leaves frames unfiltered. Neither is an approved production profile
or an automatic telecine decision. The experimental encoder is software H.264,
CRF 20, veryfast, yuv420p; every source audio track gets an AAC stereo fallback
at 192 kbit/s. No GPU is required.

Each unique attempt retains:

- input.iso: checksum-verified staged ISO (or the retained ISO referenced by a resume report).
- movie.mp4: H.264 video, AAC stereo tracks, and chapters, for manual playback.
- original-tracks.mkv: stream-copied original audio and image subtitles, retaining
  their language metadata and chapters; omitted for video-only inputs.
- report.json and phase stdout/stderr logs: success/failure, parameters, source digest,
  stream metadata/packet counts, durations, and decoded frame counts.

Success requires decoding the entire selected title's video/all audio,
checking output codecs/dimensions/track counts/languages/chapters, and fully
decoding the MP4 plus original sidecar audio. Encoded video must have exactly
the source's decoded frame count. Duration comparisons allow a small tolerance
for approximate DVD timing. Media-tool errors, missing/empty encoded tracks,
truncation, timeouts, changed source identity, or byte-cap failures return a
nonzero exit code, including error-level stderr from a tool that exits zero.
Encoding preserves the filter's fine-grained timebase instead of rounding DVD
timestamps to the nominal frame rate; it does not regenerate a guessed cadence.
Inspection and media tools reuse the checksum-bound ISO descriptor, with
identity checks to reject a changed or replaced staged copy.
This does not perform subtitle OCR, assess visual/audio
quality or synchronization perceptually, or demonstrate browser playback.

Default limits are 16 GiB per ISO, 32 GiB retained scratch, and 8 GiB per output
file. Before encoding, capacity checks reserve room for two capped outputs plus
1 GiB free; `--max-output-bytes` and `--max-scratch-bytes` can adjust admission.
The FFmpeg byte caps may be exceeded slightly before detection; exceeding them
fails verification. They are not a substitute for dataset quotas. The staging
budget defaults to 600 seconds (maximum 3,600); each media tool defaults to
14,400 seconds (maximum 86,400) and 2 MiB captured output. Use `--help` for flags.

Phase stdout/stderr logs are written incrementally within the capture cap, so
timeouts, cancellation, and output-limit failures keep diagnostics captured so
far. Filesystem I/O is not hard-cancellable: an unavailable NAS can prevent
logging, report writes, or final synchronization.

Ctrl-C/SIGTERM cancels active media tools but drains uncancellable staging I/O
before releasing ownership. Completed staging metadata is recorded even when
cancellation stops the next phase, allowing a checksum-verified retry.
Partial attempts and failed reports are normally retained; there is no automatic
cleanup or production approval action. A forced kill or storage failure
can leave an attempt without a final report. Do not treat this example as the
production import, publication, browser compatibility, or source-deletion gate.

To retry an interrupted/failed experimental attempt without copying the ISO
again, append `--resume-attempt UUID` to the same command. Keep the same incoming
filename, title, angle, and scratch root. The program checks the earlier report,
rehashes its retained ISO against the recorded SHA-256, then creates a **new**
attempt directory for all output and logs. A partial MP4/sidecar is never trusted,
appended to, or overwritten. A failed/incomplete staging attempt with no verified
digest cannot be resumed.

The new report records `resumed_from` and `staged_attempt_id`. Keep the referenced
staged attempt while any resumed report/output is under review: resumed attempts
do not contain a second input.iso. A failure before attempt-directory allocation
may return an error without a new report. Repeating resume rehashes the original
retained ISO again, not a partial encoded artifact.

## Local website preview and browser E2E

The local preview connects the website to explicitly admitted, independently
revalidated experimental MP4s. It supports account login, library search,
authenticated playback/seeking, per-user saved position, and logout. This does
not enable the production conversion queue, subtitle OCR, or source deletion.
Run the standalone importer separately; failed or incomplete imports cannot
be admitted.

With services stopped, initialize local app state, create an account, and admit
a successful experiment into a separate media directory:

```sh
target/debug/wetube-server --state-dir /local/wetube-state init
target/debug/wetube-server --state-dir /local/wetube-state add-user --username admin --admin
sha256sum /scratch/ATTEMPT/movie.mp4
target/debug/wetube-server --state-dir /local/wetube-state admit-preview \
  --report /scratch/ATTEMPT/report.json --media-dir /local/wetube-media \
  --movie-sha256 SHA256_FROM_THE_PREVIOUS_COMMAND --title "Movie title"
target/debug/wetube-server --state-dir /local/wetube-state serve \
  --bind 127.0.0.1:8080 --origin http://localhost:8080 \
  --static-dir dist --media-dir /local/wetube-media
```

Create the dedicated media directory first; keep it separate from app state,
incoming, and HTTP static files. The explicit digest binds admission to the
operator-selected payload, and the copied MP4 is independently decoded and
validated. Admission leaves the original experiment and ISO untouched.
This is an experimental local playback adapter, not the complete production
publication/backup/subtitle-asset protocol.

The Chromium browser suite builds the real server and frontend, creates an
eight-second H.264/AAC fixture and a tiny two-title DVD ISO with isolated
accounts, and tests playback, saved resume, end/back navigation, save-failure
feedback, failed-scan retention, and successful scan/inspect/convert/publish
through the real worker. A separate mocked-contract case covers title/angle
dropdowns and review confirmations; it is not converter acceptance evidence.
It never touches the real NAS import or starts Docker:

```sh
cd tests/e2e
npm ci --ignore-scripts
npm run install-browser
WETUBE_E2E_ARTIFACTS_DIR=/mnt/c/TempPrivate npm test
```

This development harness requires an explicit, existing artifacts directory
under /mnt/c/TempPrivate and checks its resolved location before building or
generating media. It never falls back to Linux ISO staging. Only the small test
database uses Linux temporary storage.

The real-DVD fixture requires `dvdauthor` and `genisoimage`, in addition to the
existing FFmpeg and browser prerequisites. These authoring tools are test-only;
runtime DVD discovery uses the installed DVD navigation library. Stop and
explain any needed apt installation before proceeding.

The suite uses safe-exec to generate its bounded fixture report and sha256sum
to stream payload hashes without Rhai blob-size limits; native test
runtime APIs handle subprocesses, random credentials, owner-only temporary
directories, and cleanup. The ordinary run removes its own temporary state.
`npm run test:preview` instead retains a small preview state/media/incoming directory and
prints its location and a private credentials-file path, without printing the
password. Its test server and worker still stop at the end; use the retained
paths with `serve` and `wetube-worker run` to leave a local website running.
Traces may contain
ephemeral test credentials and are ignored by Git.

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

### Opt-in real-DVD staged inspection test

The ignored integration case uses an isolated temporary database and the real
worker executable. It requires a completed disposable ISO and explicitly
selected title; it retains the staged ISO and integration-report.json under a
unique attempt directory in scratch, even on inspection failure. No incoming
file is modified and no movie is published. Allow at least the ISO size plus
1 GiB free and review retained attempts manually afterward.

```sh
WETUBE_TEST_COPY_COMPLETE=1 \
WETUBE_TEST_ISO=/path/to/incoming/movie.iso \
WETUBE_TEST_SCRATCH=/private/scratch \
WETUBE_TEST_TITLE=1 \
WETUBE_TEST_STAGING_TIMEOUT_SECS=1800 \
cargo test --package wetube-worker --test inspect --locked \
  real_dvd_copy_reaches_review_without_changing_incoming -- --ignored --exact --nocapture
```

Only acknowledge completion after all writers are closed. The selected title
is a candidate, not an automatic main-feature choice. Configure and verify
scratch access controls on the host; the worker does not check Unix owner/mode:
an SMB mount with forced uid/gid and 0700 modes can still expose the same files
through another mount/export. Such a mount may be used for an explicitly
accepted, non-destructive test, but it does not pass the deployment privacy or
writer-exclusion gates.

The first real-DVD test reached Needs review for a 4,681,420,800-byte ISO,
title 1 / angle 1, in 687 seconds over the shared SMB test mount using an
explicit 1,800-second budget. The default 600-second run failed closed during
rehashing and retained its copy. The successful run verified staging,
unchanged incoming identity, persisted inspection metadata, and no catalog
publication. Its probe reported about 131 minutes, 720x480 MPEG-2 video with
top-field-first metadata, English AC-3 surround/stereo audio, and
English/Spanish/French image subtitles.

This is one disc's staging/probing result, not a full decode, a confirmed
main-feature selection, a deinterlacing decision, OCR validation, or a TrueNAS
performance benchmark. Keep the remaining acceptance matrix above open.
