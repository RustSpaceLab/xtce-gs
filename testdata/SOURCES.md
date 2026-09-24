# Test data provenance

One definition and one recording, vendored so that the tests that use them cannot silently
skip. They were reached through the sibling `xtce-rs` checkout until 2026-09-13; a test that
passes or skips depending on what else a developer happens to have cloned is a test that
stops being run.

## Upstream

| | |
|---|---|
| Project | [`lasp/space_packet_parser`](https://github.com/lasp/space_packet_parser) |
| Commit | `6de220ff25e75d0a8b6258086f9d99ce9eae820b` (2026-08-07) |
| Path | `tests/test_data/jpss/` |
| Licence | BSD 3-Clause, © 2023 University of Colorado — see `LICENSE.txt` |

The BSD 3-Clause licence permits redistribution provided the copyright notice and licence text
are retained, which `LICENSE.txt` does.

## Files

| File | What it is |
|---|---|
| `jpss/jpss1_geolocation_xtce_v1.xml` | the JPSS-1 geolocation definition: 27 parameters, container inheritance, a `ComparisonList`, IEEE-754 floats |
| `jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1` | a real downlink: 7 200 back-to-back space packets, APID 11, 71 octets each, no sequence gaps |
| `jpss/contrived_inheritance_structure.xml` | the same stream read through a second definition, whose extra concrete container leaves most of a packet undescribed — which is how the "this definition does not describe the whole packet" path is tested |

| `context_calibrators.xml` + `context_calibrators_stream.bin` | written for `xtce-rs`, not vendored from upstream: no mission file in reach has a calibrator at all, so this is the only way a *calibrated* engineering value — one that differs from the raw — reaches a test here |

Those numbers are what the tests assert against.

## What is *not* here

The other five definition/stream pairs `xtce-rs` vendors. This repository decodes through
`xtce-decode`, which is already differential-tested against the reference on all of them;
re-running that here would test somebody else's crate. What these two are for is the framing
below the decoder and the ownership boundary beside it, and one real pass exercises both.
