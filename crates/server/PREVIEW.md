# Local experimental website preview

The website supports explicit administrator scan, inspection, conversion,
validation, and separate reviewed publication. Configure the same incoming,
scratch, prepared-media, and state roots for server and worker. See README.md
for the dropdown-based workflow and matching startup commands. On this host,
large staging/output/media artifacts stay under `/mnt/c/TempPrivate`; never use
Linux storage for ISO staging.

This local adapter is not the complete production asset-aware publication
protocol. It does not enable source deletion or OCR. Original audio and image
subtitles are preserved in a sidecar, not exposed as finished browser captions.
Failed or interrupted attempts remain unregistered and retained.

## Separate operator-only MP4 admission

The CLI below remains available independently of the website conversion queue.
The successful experimental attempt, staged ISO and source remain untouched.
Incomplete packages remain unregistered for operator review; finalized packages
without a committed database row are also never served.

Stop the server and worker first (the command takes both service/maintenance
locks). Initialize/upgrade state with `wetube-server --state-dir STATE init`.
Create a separate media directory outside state, frontend static files,
incoming files, and the experimental attempt directory.

```text
wetube-server --state-dir STATE admit-preview \
  --report SCRATCH/ATTEMPT_UUID/report.json --media-dir MEDIA \
  --title "Local preview" --movie-sha256 OPERATOR_CONFIRMED_SHA256
wetube-server --state-dir STATE serve --static-dir dist --media-dir MEDIA
```

Optional admission flags: `--year`, `--description`, `--incoming-dir`,
`--static-dir`, `--max-bytes` (default 8 GiB), and `--timeout-secs` (default
14400; bounded to 86400). Output is JSON containing `movie_id`, `package_id`,
and `local_preview:true`. Server defaults remain localhost-only.

Existing experimental reports contain no payload hash. Admission therefore
requires an explicit operator-confirmed SHA-256 of `movie.mp4` in the report's
attempt directory; that digest is not evidence that an earlier unobserved
payload was identical. Protect the attempt and preview directories against
concurrent writers. Admission rechecks source/report identities, copies
exclusively, independently probes and fully decodes the copy, compares
frame counts/layout/duration without rounding decoded video timestamps,
rehashes, fsyncs assets/manifest/checksum, and
uses an exclusively created UUID package and exclusive files before committing
eligibility. Failed package residue is not made playable.
The copied writer closes before readonly verification/fingerprinting; the
stored identity comes from a fresh readonly open of the finalized MP4.

## Generated fixture minimum report

Use a UUID-named attempt directory containing `report.json` and a real MP4.
For an 8-second, 24-fps, 160x90 H264 yuv420p/AAC stereo fixture:

```json
{
  "experimental": true,
  "published": false,
  "source_deletion_enabled": false,
  "status": "verified_experimental_output",
  "attempt_id": "UUID_MATCHING_PARENT_DIRECTORY",
  "validation": {
    "source_decoded_frames": 192,
    "output_decoded_frames": 192,
    "source_decoded_seconds": 8.0,
    "output_decoded_seconds": 8.0,
    "movie": {
      "format": {"duration": "8.000000"},
      "streams": [
        {"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p","width":160,"height":90},
        {"codec_type":"audio","codec_name":"aac","channels":2}
      ]
    }
  }
}
```

All report stream entries must match the actual MP4 in order. A forged report
does not bypass actual file decoding or the independently supplied hash.

Authenticated routes: `GET /api/v1/movies/{id}` returns `MovieDetails`;
`GET`/`HEAD .../{id}/video` stream only registered unchanged ready MP4s.
Single byte, suffix, and open-ended ranges are supported; malformed, multiple,
and unsatisfiable ranges return 416. `GET .../{id}/progress` returns the user's
position (initially zero). `PUT .../{id}/progress` takes
`{"position_seconds":N}` with the existing same-origin and `x-csrf-token`
requirements, and accepts only finite nonnegative positions. An explicit
0.25-second end-of-playback overshoot allowance handles browser timestamp
rounding; accepted positions are clamped to the movie duration before saving.
Responses remain `no-store`. Existing ready catalogue seeds remain visible;
only admitted previews have playable details/media.

Supported older schemas upgrade to schema 6 with a consistent pre-upgrade SQLite
snapshot. For schema 3 snapshots, retain the registered media unchanged and in
place: this backup contains no media files. This is not the design's complete
asset-aware backup/restore protocol.
