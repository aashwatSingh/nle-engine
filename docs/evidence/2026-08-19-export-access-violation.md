# STATUS_ACCESS_VIOLATION in export_sequence tests (2026-08-19)

Reproduced 1 time in 6 full-workspace runs; 0 in 12 isolated runs of the same binary.

## Crash tail
```

Caused by:
  process didn't exit successfully: `C:\Users\aashw\Downloads\nle-engine\target\debug\deps\export_sequence-b1b963a4a8ec458d.exe` (exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
note: test exited abnormally; to see the full output pass --no-capture to the harness.
```

## Tests that completed before the crash (the other 9 were still running)
```
running 18 tests
test an_empty_sequence_is_rejected_rather_than_producing_an_empty_video ... ok
test an_inverted_or_empty_range_is_refused_rather_than_writing_an_empty_file ... ok
test cancelling_stops_early_and_leaves_no_partial_file ... ok
test missing_media_is_counted_not_silently_ignored ... ok
test an_export_with_no_audio_reports_no_loudness_rather_than_a_silence_reading ... ok
test exporting_at_half_scale_produces_a_half_sized_file ... ok
test a_sequence_with_no_audio_clips_gets_no_audio_stream ... ok
test muting_the_audio_track_produces_a_silent_but_present_stream ... ok
test clip_gain_and_pan_survive_all_the_way_into_the_file ... ok
```
