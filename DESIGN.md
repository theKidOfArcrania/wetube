# WeTube: Home NAS Movie Library

Status: Design draft approved by adversarial Claude Opus 5.5 review.
User approval and empirical/deployment gates remain pending. Requirements below
record agreed choices; proposals and deferred items are not deployment approvals.

No application code will be written until this design is reviewed and approved.

## 1. Goals and scope

Build a custom, Netflix-style household movie service running as an app on
TrueNAS Community Edition 25.10.8 (Goldeye).

The initial release provides:

- Responsive desktop and phone/tablet web interfaces.
- Thumbnail/poster browsing, search, genres, and custom collections.
- Individual user accounts with separate watch progress and resume playback.
- Up to four concurrent viewers.
- Main-movie playback, without original interactive DVD menus.
- Switchable English captions, including sidecar text and OCR of DVD subtitles.
- Multiple selectable audio languages and surround support, subject to client
  capabilities and a compatibility policy still to be approved.
- An administrator workflow for importing DVDs and correcting metadata.

Later milestones add native iOS/iPadOS and Android apps before Apple TV and
Android TV / Google TV / Fire TV clients. The order within each group is
deferred. Selectable DVD extras are also deferred.

Access is limited to the home network and remote devices connected over VPN.
Public internet exposure is not part of this design.

The project is private household software, provisionally named WeTube.

## 2. Known environment

| Item | Current information |
| --- | --- |
| CPU | Intel Core i7-950, four cores, 3.07 GHz |
| RAM | 11 GB total; application budget not yet established |
| GPU | NVIDIA GeForce GT 710; no acceleration dependency assumed |
| Internet speed test | About 94 Mbps download and 94 Mbps upload |
| NAS LAN throughput | Not yet measured; internet test does not establish it |
| Library | Hundreds of DVD ISOs, mostly movies |
| Incoming share | SMB, accessible to authenticated household devices |
| Processing | Ahead of time; continuous background imports permitted |
| Hardware changes | Upgrades permitted if measurements justify them |
| VPN | OpenVPN planned, not configured yet |

Existing ISOs play in VLC, but encryption status and unattended import
compatibility are unverified. Storage capacity, dataset paths, and spare RAM
remain unknown.

## 3. Agreed file ownership and lifecycle

The original archive exists elsewhere and must remain outside the app's access.
A household member copies an ISO into the incoming SMB share, then manually
selects the completed file for import.

The app converts this incoming copy into prepared media in a separate dataset.
It may automatically delete only that incoming copy, and only after conversion
and validation succeed. Failed imports retain their input.

Deleting the incoming ISO means future re-encoding may require copying the
original from the external archive again. The prepared media is not an
archival replacement for the ISO.

Use ordinary `.iso` filenames. Before import, the administrator explicitly
confirms that copying has completed. The worker makes a private staging copy
and checks source identity and content for changes. Reject or pause any input
that changes; file age alone is not proof of copy completion.

Automatic cleanup requires full output video/audio decode and package/track
validation. Pending OCR review does not block ISO deletion: retain original
subtitle images and timing so captions can be corrected without the ISO.
Missing or failed required media assets still prevent successful validation.

Source identity consists of device/inode, size, nanosecond mtime/ctime, and a
SHA-256 digest computed while copying to private staging. Open inputs relative
to the configured incoming directory without following symlinks. Compare
identity fields before and after the copy and verify staged content against
the recorded digest. This detects changes, not malicious writers who bypass
the trusted-household completion protocol.

Never unlink by the original SMB pathname. After output publication is durable,
atomically rename the cleanup candidate into a job-unique, worker-private
quarantine directory on the same incoming filesystem, with no overwrite.
Verify that the renamed object is the recorded device/inode and size/mtime,
and re-hash it before deletion. Account for the expected ctime change caused
by the rename; do not compare post-rename ctime blindly with the original.
Any unexpected identity/content change leaves the file quarantined for review;
do not overwrite a new file at the original path when recovering a mismatch.
Persist quarantine identity and location so cleanup retries operate only there.

A rename does not revoke outstanding SMB write handles. Automatic deletion
requires verified writer-exclusion semantics during quarantine verification
and unlink. If that cannot be established on the deployed SMB setup, retain
the quarantined ISO for operator intervention rather than pretending that a
hash followed by unlink is race-free. This is a feasibility/deployment gate.

Before enabling automatic cleanup, confirm that the external archive survives
the operator's copy workflow. Read-only household access to that archive is
recommended to prevent accidental cut/paste removal; the app never changes
archive permissions or treats incoming copies as the archive.

## 4. Proposed architecture

```text
Browser / future native clients
             |
        LAN or VPN
             |
  HTTP entry point / media serving
         |                |
      Web / API      Prepared media (read-only)
         |
  Catalog, users, progress, durable job queue
         |
  Background import worker
         |
  Incoming ISOs -> probe -> select -> convert / OCR -> validate -> publish
```

Deploy as one TrueNAS custom app containing a small number of containers.
The exact custom-app installation mechanism must be checked against the
installed TrueNAS release before implementation.

Proposed responsibilities:

| Component | Responsibility |
| --- | --- |
| Web UI | Browsing, search, player, account settings, import administration |
| API | Authentication, authorization, catalog, progress, job scheduling |
| Worker | DVD probing, extraction, encoding, captions, artwork, validation |
| Media server (part of server process) | Efficient authorized delivery of playlists, segments, and captions |
| Persistent database | Catalog, accounts, progress, import state, migrations |

Prefer a modular application over microservices. Start with one import worker
and one concurrent encoding job. Limit CPU, memory, temporary storage, and
process runtime; preserve resources for TrueNAS and active viewers.

No GPU access, host ISO mounts, Docker socket, or privileged container is assumed.
Verify that the selected DVD tools can operate directly on ISO files.

### Container build and delivery

Use a multi-stage Dockerfile with a builder stage and a slim runtime stage.
The builder has the Rust/native and WASM toolchains, frontend bundling tools,
headers, and other build-only dependencies. Build the server/worker executables,
WASM frontend, static assets, and the small player bridge there.

Copy only the required artifacts into a minimal supported runtime base image.
Include necessary dynamic libraries, CA certificates, and actual runtime
dependencies: DVD/media probing and conversion tools, OCR tools, and required
language data. These are not build dependencies and cannot be removed merely
to shrink the image. Select and verify their exact versions and ISO/subtitle
capabilities during the media feasibility work.

Ship a runtime build manifest recording media/DVD/OCR tool and library versions,
plus OCR language-data versions/checksums. An image rollback must restore those
dependencies as well as application binaries; record the encoding toolchain
identity with each prepared package.

Do not include Rust compilers, Cargo/Node tooling, source trees, headers, test
fixtures, build caches, credentials, or development-only packages in the final
image. Prefer a maintained slim distribution base over `scratch` while native
media/OCR tools need operating-system libraries. Run as a dedicated non-root
UID/GID, with writable paths restricted to the configured datasets and bounded
temporary storage.

Only the runtime image is pushed to the registry and pulled by TrueNAS.
The builder is an intermediate build stage, not a second deployed app or
normally published image. The initial app runs server and worker containers
from the same runtime image, selecting the role by command. The server also
serves compiled frontend assets and authorized media from the same origin.
This keeps deployment versions aligned without separate frontend/media images.

Publish immutable version tags and record the image digest; pin deployment and
rollback to a known digest rather than relying solely on `latest`. Registry,
repository, visibility, pull credentials, and build/push automation remain
deployment decisions. Keep registry/provider credentials out of build layers.
An upgrade must honor the worker-drain, schema, and backup protocol below.

Initially target the NAS's Linux amd64 architecture with an instruction-set
baseline compatible with the i7-950; never build release binaries with
`target-cpu=native` from a newer build host. Verify media-tool CPU dispatch on
the target too. Other architectures and GPU-specific images are deferred.

### Agreed implementation foundation

- Axum/Tokio Rust backend.
- Leptos client-rendered Rust/WASM frontend; no server-side rendering initially.
- A shared Rust core for appropriate cross-client/server domain logic and types.
- SQLite accessed through SQLx with a persistent job table; no Redis.
- A small JavaScript bridge and established HLS playback library where needed;
  prefer native HLS when the browser supports the required features.
- Versioned REST/JSON API with shared Rust request/response types.
- hls.js as the browser HLS library where native HLS is insufficient.
- Admin-created local accounts with administrator password resets; no email.
- TMDB as the preferred metadata provider, conditional on reviewing its terms.
  The operator can supply an API credential.

The shared core should contain portable domain types, API contracts, and useful
validation rules. Keep database access, filesystem operations, media subprocesses,
password handling, and provider credentials server-side. The backend remains
authoritative even when validation is also available in the frontend.

Rust/WASM does not remove the need to use browser media APIs and the approved
small JavaScript interop layer.

Proposed API organization: `/api/v1` resources for authentication, movies,
collections, progress, and administrator import jobs, with authorized media
delivery under the same origin. Keep job polling and progress updates simple
initially; no GraphQL or message broker is needed.

Proposed Cargo workspace boundaries:

```text
crates/core     Portable domain types, API contracts, shared validation
crates/server   Axum API, authentication, database, authorized media serving
crates/worker   Persistent job execution and media/OCR tool integration
crates/web      Leptos/WASM UI and browser player bridge
```

Server and worker can share server-only persistence helpers without exposing
them to the WASM dependency graph. Share useful logic rather than forcing
browser and server middleware into an artificial common abstraction.

Use SQLite WAL mode with bounded busy waits and short transactions on the
local app-state mount. One migration owner initializes the schema before
workers start. Job claims, leases, heartbeats, and retry counters live in the
database; never hold a transaction open while encoding a movie.

### Worker ownership and upgrades

The initial deployment has exactly one worker/maintenance owner. It must acquire
an exclusive OS `flock` on a persistent lock file in the local app-state mount
before recovering jobs or touching output, quarantine, or trash. Never replace
or remove that lock file while services run. Every worker container uses the
same lock-file inode; inability to acquire it means no work is performed.
After acquiring the lock, check schema/app compatibility before recovery or
any other job/maintenance operation.

Hold the lock for the worker lifetime and deliberately inherit its descriptor
into media/OCR child processes, so a crashed parent cannot release ownership
while a child still writes. Verify descriptor inheritance and lock exclusion
with the actual tools/container runtime. Children only write attempt-scoped
staging; they never publish packages or clean inputs. On cancellation/shutdown,
terminate and reap process groups before releasing the lock.

Run the worker under a minimal init process that forwards signals and reaps
children, included in the runtime image or enabled by the supported container
runtime setting. Give each media job a dedicated process group. Worker failure
must stop its container and descendants rather than restart only the parent
while old tools survive. Verify crash/restart and container-stop behavior leave
no orphaned lock-holding children before a successor can obtain ownership.

Only the lock owner may publish, purge, or delete incoming copies. The API
records requested operations instead of performing those filesystem mutations.
Lease heartbeats report liveness; lease expiry alone never authorizes another
worker. Recover stale jobs only after acquiring exclusive ownership. Staging
and package IDs are attempt-scoped and never reused by a retry.

Use one release version for server, worker, and schema. Stop/drain old workers
and stop the HTTP service before migration. A server maintenance command must
non-blockingly acquire the same exclusive maintenance lock and hold it through
the pre-upgrade backup and the entire migration; a one-off lock check is not
sufficient. If acquisition fails, abort the upgrade instead of racing a worker.
Start compatible HTTP/worker processes only after the command completes.
Server and worker refuse incompatible schema/app versions. Upgrade
orchestration must not serve requests with an old process against a migrated
schema; rolling mixed-version operation is not supported.

## 5. Proposed storage boundaries

Names below describe logical roles, not approved host paths.

| Dataset/directory | Purpose | Access |
| --- | --- | --- |
| External archive | Original ISO archive | Not mounted into the app |
| Incoming | Completed ISO copies selected for import | Worker read/delete; admin listing through API |
| Incoming quarantine | Claimed cleanup candidates; same filesystem as incoming | Worker only; never SMB-exported |
| Media | Published movie packages and artwork | Worker writes; playback service reads |
| Media staging | Unpublished conversion output | Worker only; same filesystem as final media |
| App state | Database, configuration, local secrets | Application services only; not SMB-shared |
| Subtitle assets | Original images/timing and versioned captions under app state | Worker writes; API serves authorized reads |
| Scratch | Bounded, disposable intermediate files | Worker only |

Scratch holds the private staged ISO. Access control is configured and verified
on the host filesystem; the service does not enforce Unix ownership or mode
bits. Capacity planning must include
the incoming ISO, staged ISO, intermediate files, and prepared output at once.
The staged ISO is temporary, not another permanent archive.

Use explicit host-path mounts with dedicated UID/GID and narrowly scoped ACLs.
An incoming SMB share must not expose app state or published media through
unauthorized paths.

Deployment gates: app state and its SQLite WAL/shared-memory files use a local
filesystem, not SMB/NFS; quarantine is inaccessible through every SMB export
and traversal path; the worker's inherited ACLs permit the required
rename/delete operations; and ZFS `sync=disabled` is forbidden on app-state,
incoming/quarantine, and media datasets. Verify actual flush/rename semantics,
worker locking, and outstanding SMB-writer exclusion before automatic cleanup.

Only the app's media mount is read-only for playback; ingestion still needs
write/delete access to its narrowly scoped incoming area.

## 6. Import workflow and recovery

Proposed persistent states:

```text
selected -> probing -> needs_review -> queued -> processing -> validating
         -> publishing -> published -> input_cleanup_pending -> complete
```

Errors transition to a recorded failed or needs-attention state. Cancellation
retains the input and never publishes partial output.

1. An administrator selects a completed ISO from the configured incoming root.
2. Record an import identity, require explicit copy-completion confirmation,
   and make a private staging copy. Verify source identity and content before
   and after staging; do not process a changing source.
3. Probe DVD titles, duration, chapters, audio, and subtitle tracks.
4. Suggest the main movie; require correction when selection is ambiguous.
   Longest-title selection alone is not sufficient for every DVD.
5. Search an online metadata provider using title/year; let the admin confirm
   or correct the match. Do not send ISO or video contents to the provider.
6. Prepare the approved movie title, audio tracks, captions, thumbnails, and
   streaming package in unpublished staging.
7. Fully decode prepared video/audio and validate required tracks, durations,
   subtitle asset retention, and package completeness. Pending OCR review is
   allowed but must remain visible to the administrator.
8. Publish through the durability protocol below; only a durable package and
   durable referenced subtitle assets may become visible or cleanup-eligible.
9. Verify the published package against its manifest, then use the quarantine
   protocol in section 3 to delete only the confirmed incoming object.
   Clean temporary staged inputs separately. Unexpected input identity/content
   or inability to exclude writers requires review, not deletion.

Filesystem publication, database commits, and input deletion are not one atomic
transaction. Persist checkpoints and reconcile after crashes. A published
package must be recoverable without starting over, and cleanup retries must
never target a new file placed at the same path.

### Publication durability and reconciliation

Before writing output, persist an attempt-scoped package UUID, intended staging
and final paths, and its owning job. After validation, create a self-describing
manifest containing package/movie IDs, source digest, selected title/tracks,
encoding profile, and every payload file's size and hash. Record the manifest
hash in the database; the manifest does not recursively hash itself. Subtitle
asset references identify durable content-addressed objects.

Flush all output files and the manifest with `fsync`, then flush directories
bottom-up and the staging parent. Atomically rename within the media filesystem
without overwriting an existing package and `fsync` the destination and source
parent directories. Flush subtitle assets before committing their references.
Only after these barriers commit catalog visibility and cleanup eligibility.
Database durability must not be used as evidence that another dataset's
unflushed files are durable.

Immediately before incoming cleanup, verify that the final package and all
referenced retained subtitle assets still match the committed manifest.
Missing or corrupt output disables cleanup. Source deletion is followed by
flushing the quarantine directory before recording cleanup completion.

On startup under the worker lock, reconcile publishing intents. A complete
final package matching the expected manifest can finish the visibility commit;
staging-only output is revalidated or retried. Missing/corrupt output enters
needs-attention and cannot trigger input cleanup. Unknown directories are
flagged, never silently adopted or deleted. Keep interrupted cleanup distinct
from an import needing re-encoding.

Jobs must be idempotent and recover stale worker leases after restart.
Skip exact same-ISO duplicates using a content digest; flag title/year matches
for review because different editions can be legitimate. Never silently replace
existing media. Skipping a duplicate is not authorization to delete its incoming
copy; duplicate cleanup requires an explicit policy or administrator action.
Retry transient failures at most twice with backoff. Unreadable/encrypted discs,
ambiguous titles, and invalid output require administrator action instead of
blind retries. Expose orphaned staging for review; do not silently delete
unrecognized files or skipped duplicate ISOs. Do not automatically delete failed
inputs.

## 7. Playback and media preparation

Proposed baseline: source-resolution H.264 video, HLS delivery, and
browser-compatible AAC stereo tracks. Do not upscale DVDs.

Preserve correct aspect ratio, chapter timing, language labels, and A/V sync.
Handle interlacing and telecine based on the source rather than applying a
blanket frame-rate conversion.

Choose a single rendition versus additional low-bandwidth renditions after
benchmarking representative DVDs. Do not depend on live transcoding for
four-viewer playback.

For multiple languages and surround, the administrator selects tracks during
import. Preselect English/default audio and available surround:

- Offer selectable language tracks.
- Provide compatible stereo fallbacks.
- Retain selected source surround tracks where appropriate.
- Offer surround playback only when the client, codec, and packaging support it.

Universal browser surround passthrough is not assumed. Exact codec/container
compatibility and conversion rules require a representative-client test,
including AAC multichannel and AC-3 passthrough candidates where supported.

Proposed captions: normalize supplied SRT/WebVTT to WebVTT; OCR selected English
DVD image subtitles and flag uncertain output for review. Retain the source
subtitle images and timing after ISO deletion. OCR correctness, forced subtitles,
and subtitle timing need explicit acceptance criteria.

Store subtitle images/timing and caption versions in the app-state subtitle
asset store, not solely in prepared-video packages. Assets are content-addressed
and immutable; corrections create new versions, with the current version
selected by a database reference. Durably flush assets before committing that
reference. Pending OCR output and the source images remain recoverable after
ISO cleanup.

Generate authorized HLS master playlists from the catalog's immutable media
package and current caption version. Declare captions as HLS subtitle
renditions, including the playlists/segments needed by native iPhone playback.
Caption edits therefore do not mutate a published video package. API and
playback services have read-only access to the relevant asset files.

DVD subtitle extraction and OCR tooling require a feasibility test. No encrypted
disc processing capability is promised based solely on VLC playback.

## 8. Catalog, accounts, and application behavior

Proposed core records:

- Movie, edition/media package, genre, collection, and artwork.
- Audio/subtitle track, language, chapter, and rendition.
- User, role, session, and per-user playback progress.
- Import job, input identity, processing attempt, and validation result.
- Metadata-provider reference and administrator overrides.

Keep movie identity separate from a particular encoded package so replacing
media does not erase user progress or manually corrected metadata.

An administrator-confirmed replacement atomically changes the movie's current
package reference without overwriting package files. Superseded packages enter
the same seven-day logical trash and active-session protection used for deleted
movies, while the movie and its new package remain visible. New sessions use
the current package; already-authorized sessions may finish the older one.

Roles: administrator and viewer. Administrators manage imports, metadata,
users, and deletion; viewers browse and watch. Only administrators create
accounts and reset passwords; no open registration or email service.

Only administrators can delete published movies. Retain deleted packages in
seven-day recoverable logical trash before permanent deletion. Hide trashed movies
from normal browsing, preserve their catalog identity/progress for restoration,
and prevent cleanup while an active playback session still requires the package.
Trash consumes media capacity and must be included in quota checks.

Trash is a database flag and purge deadline, not a filesystem move, so existing
segment URLs remain stable. Block new playback sessions for trashed movies;
existing authorized sessions may finish. Activity requires an authenticated
segment request or a playing heartbeat within two minutes; paused/abandoned
sessions stop renewing. Purge only after the seven-day deadline and expiration
of those sessions. Serialize session authorization and transition to purging
in the database so a new session cannot race deletion. After purge, an
authenticated former session receives a media-gone response and an explanation
in the player. Restoration before purge clears the flag/deadline.

Use a dark, poster-based interface with Continue Watching, Recently Added, and
genre rows. Search movie titles and descriptions; filter by genre, year, and
audio language; sort by title, year, and date added. Collections are
administrator-managed and shared across the household. Recommendation
algorithms and social features are out of scope.

Save progress approximately every ten seconds and on pause/exit when possible;
browser shutdown events are best effort. Mark a movie watched at 90% and provide
manual watched/unwatched controls and Start Over. For the same user/movie, the
latest active playback session controls progress; reject stale updates from an
older session rather than selecting the furthest position.

## 9. Security, operations, and backups

Network policy:

- OpenVPN is managed separately from WeTube; it is not part of the app.
- Use HTTP only for the initial LAN/VPN deployment, as explicitly requested.
- Do not configure public internet exposure or router port forwarding for WeTube.
- Confirm routing/firewall behavior before allowing remote VPN access.
- Give WeTube a dedicated private hostname, with host-only cookies and no
  Domain attribute; cookies are not isolated by TCP port. Reject unconfigured
  Host headers and cross-origin state-changing requests, in addition to CSRF
  tokens. Do not share the WeTube hostname with other NAS web applications.

HTTP leaves passwords, session cookies, and media unencrypted on local network
segments. A VPN protects its tunnel, not every LAN hop after termination.
This deployment accepts that limitation and assumes a trusted LAN; do not reuse
passwords from other services. Future native clients may require explicit
cleartext-network exceptions or a reconsideration of HTTPS. Features requiring
a browser secure context are not promised on an ordinary HTTP LAN origin.

Application safeguards:

- Require authentication even on LAN/VPN; authorize every media request.
- Keep HLS playlists, segments, subtitles, and artwork behind the appropriate
  access rules rather than exposing a public static media directory.
- Use server-side sessions, password hashing, and login rate limits. Cookies
  should be HttpOnly and SameSite; the Secure flag cannot protect HTTP sessions
  and must be enabled if HTTPS is introduced. Include CSRF protection for
  state-changing cookie-authenticated requests.
- Restrict imports to configured roots; reject path traversal and symlink escapes.
- Treat ISO contents, metadata, subtitles, and filenames as untrusted input.
- Run media tools as non-root with resource limits and no unnecessary network access.
- Keep credentials out of source control, API responses, logs, and media URLs.
- Preserve local playback if the external metadata provider is unavailable.

Provide import progress, actionable failure logs, disk-space checks, worker
health, and playback error reporting in the app. Keep bounded rotating logs for
up to fourteen days, without passwords, tokens, or provider credentials.
No telemetry, external push notifications, webhook alerts, or email service.

Back up the database, configuration, administrator overrides, subtitle images
and timing, and caption corrections. Use a database-consistent backup procedure
rather than copying an active database file blindly. Backups stay on the same
NAS/location; the exact path is undecided. Keep backup files separate from live
state so routine app replacement does not overwrite both. Same-location backups
do not protect against pool/NAS loss.

Run daily app-state backups, retain fourteen daily restore points, and create
a pre-upgrade backup. Include local subtitle assets and corrections, not just
catalog rows. Protect backup permissions because app state contains sensitive
configuration and authentication data.

The lock-owning worker coordinates daily backups and serializes asset garbage
collection with backup pinning/copying. The server maintenance command runs
pre-upgrade backups while holding the same lock with normal services stopped.
For a backup, pause asset garbage collection, produce a consistent SQLite
snapshot using the online backup API or equivalent, then copy immutable assets
referenced by that snapshot (a superset is acceptable). Freeze configuration
changes while capturing its files. Save a versioned backup manifest and verify
asset completeness/checksums before marking a restore point complete. Do not
count an interrupted backup toward retention, or delete older good backups
until its replacement is complete.

Asset garbage collection must preserve everything referenced by the live
catalog, an in-progress backup, or any retained daily/pre-upgrade restore point;
fourteen days alone is not sufficient if an older restore point remains.
Retained backup asset copies must be self-contained or explicitly pinned.
Also pin assets referenced by on-disk package manifests awaiting adoption or
review, including superseded/trashed packages not yet purged. After a restore,
complete the package inventory before enabling destructive asset collection;
unreadable or unresolved manifests require review rather than assumed absence
of references.

Do not back up prepared videos. Recover them by reimporting external originals
if needed; preserve movie identity and restored per-user progress during that
workflow. Rebuilding hundreds of movies may take substantial processing time.

Restore runs through the server maintenance command with HTTP and workers
stopped and the same exclusive maintenance lock held for the entire restore
and reconciliation. Resume normal services only after reconciliation succeeds.
After restoring app state, reconcile package manifests with the restored catalog.
Flag unknown packages for admin-reviewed re-adoption using their movie/package
IDs; do not automatically discard them. Mark missing packages as needs-reimport
without erasing restored metadata or progress. Invalidate restored worker leases
and pending destructive actions: require fresh input-completion confirmation and
all current validation/quarantine checks before resuming incoming deletion.
Never replay a backup's cleanup intent against an unverified current pathname.
Exercise missing packages, orphan packages, and interrupted jobs in the restore
rehearsal, including caption-asset retention for packages newer than the restored
database.

Version application images and schema migrations. Upgrade/rollback procedures
must account for database compatibility and include a restore rehearsal.

## 10. Open decisions

Some items need a user choice; others need measurements or verification rather
than another preference. Closed choices remain recorded in the sections above.

| ID | Decision | Recommendation / consideration |
| --- | --- | --- |
| D03 | Image publishing and TrueNAS installation | Multi-stage builder/slim runtime agreed; select registry/build automation and validate custom-app pull/install workflow |
| D04 | Dataset paths, free space, quotas, ACLs | Verify private quarantine, worker locking, writer exclusion, archive-copy safety, and durable local datasets |
| D08 | Output quality and renditions | Benchmark before choosing encode parameters and number of renditions |
| D09 | Audio compatibility matrix | Admin selection and stereo fallbacks agreed; test preserved surround by client |
| D10 | Metadata provider terms and credentials | TMDB preferred; review terms, attribution, and credential handling |
| D12 | Deployment network settings | External OpenVPN and HTTP-only agreed; dedicated hostname, Host allowlist, private port, and firewall rules |
| D16 | Backup destination and restore procedure | Contents, daily schedule, fourteen restore points, and pre-upgrade backup agreed; set path and rehearse recovery |
| D17 | Resource budget | Local-only monitoring agreed; measure RAM, CPU, throughput, and temporary space |
| D19 | Dependency and artwork licensing | Private household project is agreed; still review third-party terms |
| D20 | Media acceptance matrix | Browser set agreed below; representative DVD/remote playback tests remain required |

## 11. Proposed milestones and acceptance gates

1. Approve this design and close implementation-blocking decisions.
2. Prove import feasibility on representative DVDs: title selection,
   multilingual/surround audio, interlacing, subtitles/OCR, and failure handling.
   Measure encoding throughput, output size, CPU, RAM, and NAS LAN throughput.
3. Build the web library, accounts, metadata correction, playback, and progress.
4. Complete safe import publication/deletion, restart recovery, backups, and
   TrueNAS deployment. Test four simultaneous streams, including remote VPN
   playback, while an import runs. If OpenVPN runs on this NAS, include its CPU
   load in that test; do not infer VPN throughput from the internet speed test.
5. Add native phone/tablet clients before TV clients; add optional DVD extras
   in a later milestone.

Before calling the first release complete, demonstrate that an interrupted or
failed import cannot delete its source, unauthorized users cannot fetch media
segments, account progress remains separate, captions and audio selections
work on agreed browsers, and a backup can actually be restored.

### Agreed browser support

Support current stable desktop Chrome, Edge, Firefox, and Safari, plus
iPhone/iPad Safari and Android Chrome. Old browsers and embedded smart-TV
browsers are outside the first-release support set.

Test seeking, resume, English captions, language selection, stereo fallback,
and compatible surround behavior. The feasibility matrix must include a
simple movie, an ambiguous/multiple-title DVD, an interlaced or telecined source,
image subtitles, multilingual/surround audio, and an unreadable or incomplete
input. Preserve original subtitle assets even when OCR output is incorrect.

Fault-injection acceptance must cover power loss/crashes around every
publication and cleanup checkpoint, SMB replacement/concurrent writers,
overlapping workers and surviving child processes, caption edits during backup,
and restoration with missing or orphan packages. Automatic ISO deletion stays
disabled until these safety gates and the deployed filesystem/SMB assumptions
are demonstrated.

### Before implementation

Review and approve this document. Then perform the bounded media feasibility
work before committing to encoding profiles, OCR tooling, hardware upgrades,
or a bulk library conversion. Dataset paths and resource caps must be supplied
before deployment; actual four-stream performance remains a verification gate,
not an assumed property of the current hardware.
