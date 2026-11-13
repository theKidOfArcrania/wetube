let report = #{
    fixture: true,
    experimental: true,
    published: false,
    source_deletion_enabled: false,
    status: "verified_experimental_output",
    attempt_id: attempt,
    validation: #{
        source_decoded_frames: 192,
        output_decoded_frames: 192,
        source_decoded_seconds: 8.0,
        output_decoded_seconds: 8.0,
        movie: #{
            format: #{ duration: "8.000000" },
            streams: [
                #{ codec_type: "video", codec_name: "h264", pix_fmt: "yuv420p", width: 160, height: 90 },
                #{ codec_type: "audio", codec_name: "aac", channels: 2 }
            ]
        }
    }
};
fs::write_text(MOUNTS[0], json::stringify_pretty(report));
fs::write_text(MOUNTS[1], "Synthetic invalid DVD fixture\n");
