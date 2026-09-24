# xtce-gs

[![CI](https://github.com/RustSpaceLab/xtce-gs/actions/workflows/ci.yml/badge.svg)](https://github.com/RustSpaceLab/xtce-gs/actions/workflows/ci.yml)

A ground station in one binary: a socket, an XTCE definition, and a window. Part of
[RustSpaceLab](https://github.com/RustSpaceLab), built on
[`xtce-rs`](https://github.com/RustSpaceLab/xtce-rs).

```console
$ xtce-gs run mission.xml --source udp://0.0.0.0:10015 \
      --framing tm --rs 32 --interleave 5 --derandomize
```

That is the CCSDS-standard downlink: 1115-octet transfer frames, RS(255,223) interleaved five
deep, randomised. `--framing packets` is the other half of the world, where the packets arrive
with nothing wrapped around them. Every flag combination that cannot mean anything is refused
at startup, by name, with the number that would have made it work.

No JVM, no Node, no database, no browser. The definition is read once into an arena, packets
are decoded by `xtce-decode` — already measured at ~100× the reference Python implementation —
and the interface is immediate-mode egui reading a store the decode thread writes.

| Crate | What it does |
|---|---|
| `xtce-gs-core` | owned samples, ring-buffered history, viewport decimation, limits, counters |
| `xtce-gs-link` | UDP/TCP/file sources, ASM sync, Reed-Solomon, derandomiser, TM transfer frames, packet assembly, CSP |
| `xtce-gs-engine` | the task graph: acquisition → decode → store, spacecraft time, recording, CSV export |
| `xtce-gs-gui` | the operator interface: parameter tree, table, plots, link status, event log |
| `xtce-gs-cli` | `run`, `replay`, `export`, `probe` |

Design decisions and what they cost are in [`ARCHITECTURE.md`](ARCHITECTURE.md). What is left
to do, and what each thing needs, is in [`TODO.md`](TODO.md).

## The four steps

Bytes arrive, frames are recovered from them, packets are recovered from the frames, and
parameters are recovered from the packets. Each step fails on its own, each has counters an
operator watches, and only the last one needs the definition.

```
 socket / file          xtce-gs-link                     xtce-gs-engine            xtce-gs-gui
┌──────────────┐   ┌────────────────────────┐   ┌─────────────────────────┐   ┌──────────────┐
│ UDP TCP file │──▶│ sync → derandom → RS   │──▶│ Decoder::decode         │──▶│ egui: tables │
│              │   │  → frame → assemble    │   │  → owned Sample         │   │  plots, log  │
│              │   │  → (CSP unwrap)        │   │  → ParameterStore       │   │              │
└──────────────┘   └────────────────────────┘   └─────────────────────────┘   └──────────────┘
      bytes            RawPacket (owned)            Batch, then the store         read-only
```

## Try it without a spacecraft

The recording in `testdata/` is a real JPSS downlink: 7 200 packets, one APID, no sequence
gaps. Provenance is in `testdata/SOURCES.md`.

```console
# What is in this stream? No definition needed.
$ xtce-gs probe testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1

# Replay it at 100 kB/s into the interface, as if it were arriving now.
$ xtce-gs replay testdata/jpss/jpss1_geolocation_xtce_v1.xml \
      testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1 --rate 100000

# Or decode the whole thing to CSV with no interface at all.
$ xtce-gs export testdata/jpss/jpss1_geolocation_xtce_v1.xml \
      testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1 -o jpss.csv
```

## Building

```console
$ cargo build --release
$ cargo test --workspace
$ cargo clippy --workspace --all-targets -- -D warnings
```

`xtce-rs` is pinned by revision in the workspace `Cargo.toml`, the way `xtce-flight` pins it:
this repository is built against one state of the decoder, and a moving dependency would mean
an unrelated commit there breaks a build here. Nothing else is needed to build — cargo fetches
it.

## Scope

What this is not, and why, is the shortest way to say what it is:

* **Not an archive.** History is a ring in memory; a recording is a file of bytes. A parameter
  archive is a different program.
* **Not a commanding system.** Uplink is [`xtce-flight`](https://github.com/RustSpaceLab/xtce-flight)'s
  half, and it needs a link that transmits.
* **Not a server.** One process, one operator, one window. Two operators want Yamcs.
