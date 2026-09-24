# TODO

Every entry here is a `// TODO(id)` in the code, and the code is where the detail lives — each
one says what is missing, **what has to be decided first**, and roughly what the change costs.
This file is the index: the identifier, where it is, and one line of what it is.

```console
$ grep -rn "TODO(gs-link-rs-erasures)" crates/     # the full text of any entry
$ grep -rn "TODO(" crates/ | wc -l                 # 31 of them, over 21 identifiers
```

Nothing here is a bug. The bugs found while building this are in
[`PROGRESS.md`](PROGRESS.md), fixed. These are the places where a decision was deferred rather
than guessed at, which is the difference between a gap and a mistake.

## If you want somewhere to start

Ordered by how much you learn per hour spent, not by importance.

| | Entry | Why it is a good one to pick up |
|---|---|---|
| 1 | `gs-link-frame-sh-zero` | A secondary header of length zero is malformed (§4.1.3.2 says 1–63 octets) and is accepted. Small, self-contained, and it makes you read one page of a Blue Book and decide what a malformed frame costs. |
| 2 | `gs-gui-layout` (`layout.rs:159`) | A panel the operator drags wider is narrow again next run. egui 0.35 has no public accessor for the dragged size, so the answer is to read it off the panel's own response rect — which means finding out how egui reports what it laid out. |
| 3 | `gs-engine-sctime-submilli` | A sub-millisecond field is refused on its width and not its value, so an out-of-range count pushes the instant past the millisecond it belongs in. Needs a ruling: drop the sample, or clamp it. |
| 4 | `gs-link-rs-shortened` | Virtual fill, CCSDS 131.0-B-5 §4.3.7. The arithmetic is a `pad` count; the work is threading a config field through `RsConfig` into the Chien search. Real Reed-Solomon work with a small blast radius. |
| 5 | `gs-link-frame-aos` | AOS transfer frames (CCSDS 732.0-B), which is what a great many CubeSats actually downlink. A second parser and a `Framing::AosFrames`, and the decision is whether the pipeline keeps one frame type with a discriminant or two. The biggest thing left that is not gated on hardware. |
| 6 | `gs-link-rs-erasures` | 2E erasures instead of E errors is most of the coding gain on a fading link. Needs a demodulator that reports confidence, which needs the link's input type to change. Do it when you have a radio that tells you. |

## `xtce-gs-link`

| Entry | Where | What |
|---|---|---|
| `gs-link-derand-degree-17` | `derand.rs:49` | The degree-17 randomiser CCSDS 131.0-B-5 §10.4.1 also allows (131 071 bits) is not implemented; a mission using it is refused, not silently misread. The decision is the configuration point — `derandomize` is a `bool` where three states are wanted. |
| `gs-link-serial` | `source.rs:8` | A fifth source variant for `serial:///dev/ttyUSB0?baud=115200`. Absent because `serialport` binds a C library on some platforms and the radios in reach speak UDP. |
| `gs-link-rs-erasures` | `rs.rs:795` | Erasure decoding. Needs somewhere for per-symbol confidence to come from — `Source` carries bytes and nothing else. |
| `gs-link-rs-shortened` | `rs.rs:804` | Virtual fill for shortened codeblocks. Needs a `virtual_fill` on `RsConfig` first; inferring it from the frame length is wrong. |
| `gs-link-frame-aos` | `frame.rs:220` | AOS frames (CCSDS 732.0-B) are refused at the version check rather than misread. Its VCID is six bits, so it needs a second parser, not a widened field. |
| `gs-link-frame-vca` | `frame.rs:298` | A frame with the sync flag set carries VCA service data, not packets, and its first-header-pointer is undefined. Decide whether the pipeline drops such frames or routes them. |
| `gs-link-frame-sh-zero` | `frame.rs:347` | A secondary header of length zero is malformed (§4.1.3.2 says 1–63 octets) and is accepted rather than refused. |
| `gs-link-frame-ocf-mismatch` | `frame.rs:377` | A frame whose OCF flag disagrees with the configuration is decoded by the flag and not reported. Decide: per frame (noisy) or latched per virtual channel. |
| `gs-link-packets` | `packets.rs:495` | Does any mission send only-idle-data frames *between* the frames of one long packet? Guessing wrong either splices two packets or loses one per idle frame. |
| `gs-link-csp` | `csp.rs:288` | An RDP packet is passed through with its five-octet trailer still on the payload, which desynchronises the packet stream behind it. Refuse or strip. |
| `gs-link-pipeline` | `pipeline.rs:324` | Shortened Reed-Solomon codeblocks are refused by the length check, because `ReedSolomon` has no shortening parameter. Pairs with `gs-link-rs-shortened`. |

## `xtce-gs-engine`

| Entry | Where | What |
|---|---|---|
| `gs-engine-decode-repeat` | `decode.rs:307` | A repeated sequence count is counted as nothing. A live operator and an analyst replaying a looping recording want opposite defaults, which is the decision. |
| `gs-engine-decode-headerless` | `decode.rs:521` | A definition whose root container has no CCSDS primary header yields a fabricated APID 0. Fixing it means `Option<u16>` in core, or refusing such packets. |
| `gs-engine-sctime-submilli` | `sctime.rs:161` | A sub-millisecond field is refused on its width, not its value, so an out-of-range count pushes the instant past the millisecond it belongs in. |
| `gs-engine-record-columns` | `record.rs:57` | The exported CSV has no `unit` and no `limit state` column. The unit is a compound list needing a separator; the limit state needs a `LimitSet` the function is not given. |
| `gs-engine-record` | `record.rs:161` | The recorder's time-bounded flush only fires when something is written, so a link that goes quiet holds its tail. The fix belongs in the session's `select!`. |
| `gs-engine-record` | `record.rs:497` | `export` reads back-to-back packets only; a recording of transfer frames has to go through `Pipeline` first. Decides whether the engine may depend on the link's framing. |
| `gs-engine-record` | `record.rs:505` | The exported `time` column repeats `received` because the function has no `SpacecraftClock`. |
| `gs-engine-session` | `session.rs:41` | Eviction on a full queue is one packet late; making it exact means replacing the channel with a `Mutex<VecDeque>` + `Condvar`. Decide against a measurement, not a principle. |
| `gs-engine-session` | `session.rs:382` | `describe_source` reports the configured address, so `udp://0.0.0.0:0` never shows the port actually bound. Returning the bound one costs an allocation per frame on the status bar. |
| `gs-engine-session` | `session.rs:1364` | Nothing can wait for the tasks to have finished — both are detached. A join that can hang is worse than no join, so the timeout is the decision. |

## `xtce-gs-gui`

| Entry | Where | What |
|---|---|---|
| `gs-gui-app` | `app.rs:497` | Nothing in `app.rs` is tested against a running session, because `Session` has no constructor that does not spawn tasks. A trait over the four accessors `App` uses would fix it, at a vtable per call. |
| `gs-gui-layout` | `layout.rs:159` | `tree_width` and `events_height` are written and never read back, so a panel the operator drags is narrow again next run. egui 0.35 has no public accessor for the dragged size. |
| `gs-gui-layout` | `layout.rs:254` | At version 2 the loader will refuse every version-1 layout and the station will not start until the operator deletes a file they did not write. Discard, migrate, or keep refusing. |
| `gs-gui-plots-monotonic` | `plots.rs:198` | The visible-range binary search assumes time never goes backwards. A spacecraft clock that steps back mid-pass truncates the window. Needs an ingest policy first. |

## `xtce-gs-cli`

| Entry | Where | What |
|---|---|---|
| `gs-cli-args` | `args.rs:485` | `--no-asm` is arguably a frame-only flag like `--derandomize`; moving it means deciding what `--framing packets --no-asm` should mean to someone scripting both. |
