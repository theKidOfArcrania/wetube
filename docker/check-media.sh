#!/bin/sh
set -eu

# Unknown FFmpeg help topics can exit zero; require the actual capability header.
for program in ffmpeg ffprobe; do
    "$program" -hide_banner -h demuxer=dvdvideo 2>&1 | grep -q '^Demuxer dvdvideo '
done
for codec in libx264 aac ffv1 pcm_s16le; do
    ffmpeg -hide_banner -h "encoder=$codec" 2>&1 | grep -q "^Encoder $codec "
done
for codec in mpeg2video ac3 dca dvdsub pcm_dvd; do
    ffmpeg -hide_banner -h "decoder=$codec" 2>&1 | grep -q "^Decoder $codec "
done
for muxer in mp4 matroska; do
    ffmpeg -hide_banner -h "muxer=$muxer" 2>&1 | grep -q "^Muxer $muxer "
done
ffmpeg -hide_banner -h filter=yadif 2>&1 | grep -q '^Filter yadif'
tesseract --list-langs 2>&1 | grep -qx eng
