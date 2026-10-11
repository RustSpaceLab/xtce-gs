# xtce-gs

[![CI](https://github.com/RustSpaceLab/xtce-gs/actions/workflows/ci.yml/badge.svg)](https://github.com/RustSpaceLab/xtce-gs/actions/workflows/ci.yml)

A ground station in one binary: a socket, an XTCE definition, and a window. Part of
[RustSpaceLab](https://github.com/RustSpaceLab), built on
[`xtce-rs`](https://github.com/RustSpaceLab/xtce-rs).

```console
$ xtce-gs run mission.xml --source udp://0.0.0.0:10015 \
      --framing tm --rs 32 --interleave 5 --derandomize
```

`mission.xml` stands for your own definition and the socket for your own radio, so that one is
the shape rather than something to paste. [Try it without a spacecraft](#try-it-without-a-spacecraft)
runs against a recording this repository ships.

That is the CCSDS-standard downlink: 1115-octet transfer frames, RS(255,223) interleaved five
deep, randomised. `--framing packets` is the other half of the world, where the packets arrive
with nothing wrapped around them. Every flag combination that cannot mean anything is refused
at startup, by name, with the number that would have made it work.

No JVM, no Node, no database, no browser. The definition is read once into an arena, packets
are decoded by `xtce-decode` — already measured at ~100× the reference Python implementation —
and the interface is immediate-mode egui reading a store the decode thread writes.

One binary, and here is what that is worth measured rather than claimed:

```console
$ cargo build --release -p xtce-gs-cli && ls -l target/release/xtce-gs
9928976        # 9.47 MiB, macOS x86_64, unstripped
$ strip target/release/xtce-gs && ls -l target/release/xtce-gs
8595272        # 8.20 MiB stripped
```

Measured with rustc 1.99.0; the toolchain and the dependency tree both move these by a few
kilobytes, so run the commands rather than trusting the figures.

Linux is bigger, and the release workflow is what says so rather than this paragraph: the
[v0.1.0 notes](https://github.com/RustSpaceLab/xtce-gs/releases/tag/v0.1.0) carry a table it
measured on the runner, where `x86_64-unknown-linux-gnu` is 13 685 384 bytes unstripped and
11 424 312 stripped — 10.90 MiB, against 8.19 for macOS arm64. The GUI toolkit links against
X11, Wayland and GTK there and against system frameworks on macOS, and that is most of the
difference.

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

A clone has no `xtce-gs` yet, so build it once — either onto your `PATH`:

```console
$ cargo install --path crates/xtce-gs-cli
```

or in place, substituting `cargo run --release -p xtce-gs-cli --` for `xtce-gs` below. Either
way the first build is a few minutes; eframe is most of it.

```console
# What is in this stream? No definition needed.
$ xtce-gs probe testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1

# Replay it into the interface at 100 kB/s, as if it were arriving now. The recording is
# 511 200 bytes, so that is 5.1 seconds of pass — `--repeat` starts it over at the end,
# which is what you want while looking at the plots.
$ xtce-gs replay testdata/jpss/jpss1_geolocation_xtce_v1.xml \
      testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1 --rate 100000 --repeat

# Or decode the whole thing to CSV with no interface at all.
$ xtce-gs export testdata/jpss/jpss1_geolocation_xtce_v1.xml \
      testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1 -o jpss.csv
```

![The station replaying a JPSS pass: a parameter tree, a table of current values, and plots of
GPS position and velocity over one orbit](docs/station.png)

That picture is not a screen grab. The window photographs itself, so there is a command that
makes it again rather than a file nobody can reproduce:

```console
$ xtce-gs replay testdata/jpss/jpss1_geolocation_xtce_v1.xml \
      testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1 \
      --rate 100000 --repeat --screenshot docs/station.png --screenshot-after 4
```

The replay opens with three plots already drawn — GPS position, GPS velocity and the attitude
quaternion, 7 200 points each, the third below the fold — because a layout ships beside the
definition as
`jpss1_geolocation_xtce_v1.xml.gs-layout.json`. That is not a special case for the demo: a
layout is a plain JSON file of qualified parameter names that the interface writes next to
whatever definition you open, so the arrangement belongs to the mission rather than to your
machine. Delete it and the station starts empty, which is what a definition with no layout
does.

## Building

```console
$ cargo build --release
$ cargo test --workspace
$ cargo clippy --workspace --all-targets -- -D warnings
```

`xtce-rs` is pinned by revision in the workspace `Cargo.toml`, the way `xtce-flight` pins it:
this repository is built against one state of the decoder, and a moving dependency would mean
an unrelated commit there breaks a build here. Cargo fetches it; there is nothing to install
by hand.

On Linux, eframe does need system libraries that cargo cannot fetch — the same seven CI
installs:

```console
$ sudo apt-get install -y libgtk-3-dev libxcb-render0-dev libxcb-shape0-dev \
      libxcb-xfixes0-dev libxkbcommon-dev libssl-dev libwayland-dev
```

macOS and Windows need none of it.

## Scope

What this is not, and why, is the shortest way to say what it is:

* **Not an archive.** History is a ring in memory; a recording is a file of bytes. A parameter
  archive is a different program.
* **Not a commanding system.** Uplink is [`xtce-flight`](https://github.com/RustSpaceLab/xtce-flight)'s
  half, and it needs a link that transmits.
* **Not a server.** One process, one operator, one window. Two operators want Yamcs.
