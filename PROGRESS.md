# Progress log

## The station, end to end

**Done.** Five crates, 531 tests, `clippy --workspace --all-targets -- -D warnings` clean,
`fmt` clean. A real JPSS downlink goes in one end and comes out the other:

```console
$ xtce-gs probe testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1
stream: file://testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1?chunk=4096
read:   511200 bytes
...
apids: 1
   apid    packets     min     max    gaps   missing
     11       7200      71      71       0         0

$ xtce-gs export testdata/jpss/jpss1_geolocation_xtce_v1.xml \
      testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1 -o jpss.csv
export: 7200 packets, 194400 rows, 0 refused

$ xtce-gs replay --headless --rate 0 testdata/jpss/jpss1_geolocation_xtce_v1.xml \
      testdata/jpss/J01_G011_LZ_2021-04-09T00-00-00Z_V01.DAT1
final 2026-09-24T08:36:08.761Z bytes=511200 packets=7200 decoded=7200 rejected=0 lost=0 \
gaps=0 missing=0 last=1.0s ago
```

Those packet counts came out of a twenty-line Python script over the same file before any of
this existed, which is why the agreement means something. The parameter values were diffed against
`xtce-cli decode` from `xtce-rs`: 27 of 27 identical on the first packet, which is agreement
with the Python reference by transitivity — `xtce-rs` is already proven equal to it on all
7 200.

### Correctness is inherited, not re-established

`xtce-decode` is the decoder and it is already differential-tested against
`space_packet_parser`. Nothing here re-litigates that. What this repository has to get right
is everything *around* it: the framing below, the ownership boundary beside it, and the
display above it.

So the tests here are about the seams. `xtce-gs-link` builds a wire image — JPSS packets
wrapped in transfer frames, Reed-Solomon encoded, randomised, with sync markers — and pushes
it through the pipeline at chunk sizes of 1, 7 and 65 536 octets, asserting the packets come
back identical and in order. The chunk boundary is where a streaming parser breaks, so that is
where the test aims.

### Three bugs, and what found each

**The coding stages were in the wrong order, and Reed-Solomon cannot tell you.** The pipeline
ran `sync → RS → derandomise`. CCSDS 131.0-B-5 §10 randomises the *codeblock*, check symbols
included, so a receiver derandomises first. The implementation followed a diagram in this
repository's own `ARCHITECTURE.md`, which was wrong — written before the standard was read.

What makes it worth recording is why no test caught it: the fixture that built the wire image
had the same misreading, so both sides agreed. And the obvious symptom is absent. The 255-octet
randomiser sequence is *itself a valid RS(255,223) codeword* — established here by re-encoding
its first 223 octets with the encoder that is verified against Phil Karn's — and a Reed-Solomon
code is linear, so a codeblock that has not been derandomised is another codeword. The decoder
returns `Ok(0)`, **no errors**, and hands on rubbish. A station with the stages swapped reports
a clean link and fails at the frame checksum two stages later.

The test that now pins it (`the_randomiser_sequence_is_a_codeword_so_the_wrong_order_decodes_
cleanly_and_lies`) asserts the property, not the behaviour: it re-encodes, it does not ask the
decoder whether the decoder is right.

**`Utc::civil` overflowed within a day of either end of its range.** It floor-divided by
multiplying the day count back out, and at `i64::MIN` that product is below `i64::MIN`: an
arithmetic panic in a debug build, on the drawing thread, reachable by dragging a plot axis.
`div_euclid`/`rem_euclid` cannot overflow for any input. Found by a TODO left in the interface
pointing back at core, not by a test.

**And a batch caught before any of it was implemented.** The skeleton went in first — every
signature, every doc comment, no bodies — and reading that against what each module was
supposed to do turned up, among others: `Session::shutdown` using `Notify::notify_waiters`, which wakes only tasks already
parked, so a shutdown landing during `read_chunk` was lost and the task read the socket
forever; a decode signature whose only possible body allocated a `Vec` and a hash map per
packet; a synchroniser returning an owned `Vec` per frame into a chain that corrects in place;
`bit_slip` and the flywheel unreachable from any configuration; and `packets_lost` with two
owners and two meanings. All settled before a body existed to be wrong. A clean `cargo check` on a
skeleton proves nothing — every `unimplemented!()` body coerces from `!` — which is why that
review happened at the signature stage and not after.

### Three more, all of them invisible to a test written beside the code

These came out of going back through the finished thing looking for what the tests could not
see — which, on this kind of code, is anything both the code and its test got wrong together:

**Idle frames never reached the packet assembler.** `Pipeline::push` counted a fill frame and
`continue`d, and `PacketAssembler::push_data`'s own contract says the opposite in so many
words — "the frame count is recorded even for an idle frame ... not recording it would
manufacture a gap on the next real frame". The assembler was right and nothing called it. On
any virtual channel that idles — which is what a channel does when it has nothing to send —
every real frame after a fill frame read as a discontinuity: a false loss counted, a warning
logged, and a packet that legitimately spanned the fill frame thrown away. The tests that
covered this called `push_frame` directly, below the guard that production applies. The new
test goes through `Pipeline` and fails without the fix.

**A restart marker could be evicted from the dispatch ring.** The marker travels in the queue
rather than beside it precisely so that it cannot overtake or be overtaken by the packets it
separates — and the eviction policy popped the front without looking at what it was. The one
source that is both lossy and able to restart is `tcp-listen://`, so the state is a feeder
reconnecting into a station whose decoder is behind: exactly when the ring is full. Losing the
marker leaves the sequence tracker comparing counts across a stream boundary, which reports
thousands of packets missing that were never sent.

**A layout from another build stopped the station.** The version field refused the session
instead of discarding the arrangement, and `deny_unknown_fields` fired before the version was
even looked at — so a field added in a later build produced a message about a key. The file is
written by the station itself, so a version bump would have stopped every station that had
ever been used, until its operator deleted a file they never wrote.

Beside those: the acquisition task's `Source::connect` was the one await a shutdown could not
cancel, so closing the window during a `tcp://` connect to an unrouted address left the task
and everything it held alive for the kernel's timeout; two breaks left the loop silently on
the only condition that means the decode thread has panicked, which is the one moment the
event log needs a line; and the waker fired on every wake-up including those that decoded
nothing, repainting the window at the packet rate with nothing on it able to change.

The interface got the test it did not have: `crates/xtce-gs-gui/tests/against_a_session.rs`
runs a real replay and drives every panel's `refresh`, which is how a station that "works" and
shows an empty tree gets caught. Writing it found one more — a layout naming a parameter in a
plot but not in its watch list draws an empty box forever, and a hand-edited layout file is
the expected case, not an abuse.

Going on through the rest, a file at a time, each fix waiting until its test had been watched
to fail first — a test never seen red is a test that proves the code compiles. Most of what
turned up was not behaviour at all but claims the code no longer kept: a module header promising no
allocation on a path that allocates per text value, an assertion that could not fail, four
consecutive Blue Book citations where two named the same section, and an operator-facing
refusal that attributed Reed-Solomon, the pseudo-randomiser and the insert zone to CCSDS
132.0-B-3, which defines none of them. That last one was settled by downloading the Blue Books
and grepping the extracted text: the attached sync marker is 131.0-B-5 §9.3, and the insert
zone is an AOS field from 732.0-B-4 that no TM frame has at all.

What did change behaviour was all in the command line, and all of it came out of measuring
rather than reading: the headless log used a wall clock as a cursor and skipped any event sharing a timestamp with the
last one printed — 93 258 of 100 000 back-to-back events share a stamp on this machine, so the
log was losing most of what it is for; `export` dropped a partial packet at end of stream with
no counter and a clean exit, and the session's own end-of-stream arm had the identical hole;
`--limit 0` exported one packet where the library exported none; and `export | head -1` exited
1 with "Broken pipe", which makes peeking at an export fail a script under `set -e`.

### What is deliberately not here

`TODO.md` indexes them, each a `// TODO(id)` in the code saying what is missing and, more
usefully, **what has to be decided first**. The pattern is deliberate: erasure decoding needs a
demodulator that reports confidence, and there is nowhere for that to come from while `Source`
carries bytes; virtual fill needs a config field, because inferring the fill from the frame
length is wrong for a mission that sends a short frame inside a full block. Neither is hard.
Both need an answer this repository does not have yet.

### Numbers, and what they are not

The export figure above includes framing, decoding, formatting and writing 194 400 CSV rows.
It is not a decoder benchmark — `xtce-rs` has those, and they are the honest ones. Nothing here
has a criterion suite yet; the first person to quote a throughput number for this repository
should write one first.

### What that left open

* The test data, still reached for in a sibling checkout rather than held here.
* The dependency on `xtce-rs`, still a path and not a revision.
* A criterion suite, so the next change to the pipeline can be measured rather than argued
  about.

The first two are done, below. The third is not.

## The loose ends

Twelve of the deferred decisions taken and written, with the code behind them. Two were
correctness:

**A frame carrying virtual channel access service data was taken apart as if it held
packets.** CCSDS 132.0-B-3 §4.1.2.7.2 sets the sync flag when the data field is not packets,
and §4.1.2.7.6.2 then leaves the first header pointer undefined — so an undefined eleven-bit
number was being used as an offset into a packet in flight. The frame now records its virtual
channel count, which is what stops the *next* packet frame from reading as a gap, and nothing
else about it is believed. The test builds the hostile case: a pointer of 200 in a frame
between two halves of one packet, and the packet has to come back whole.

**A CSP packet with `CSP_FRDP` set was passed through with its trailer on.** RDP appends five
octets to the payload, so the packet stage behind it read a length field out of a trailer —
which is still a number, so the desynchronisation was silent. Refused now, by name. Stripping
it instead would need RDP's own layout and would still leave a connection protocol whose
acknowledgements a receive-only station cannot send.

The rest: a link losing one packet in three no longer fills the event log — a dropout is named
once, with the sequence count that says where it began, a running total every sixty-fourth gap
after that, and one line when the link comes back; the acquisition task publishes the address
it really bound, so `udp://0.0.0.0:0` stops being what the status bar says; a closed session
and a dead decode thread no longer produce the same sentence; the table's context menu offers
the one item that does something and every plot as a target; the four link counters that had
no cell are in hover text rather than a second row; the event log has a substring filter; the
tree says when a parameter last arrived, and `never` when it never has; the plots shade the
band between the warning and the alarm bound, at a twelfth of the line's opacity so telemetry
stays on top; and `export` counts the rows it wrote rather than the samples it was offered.

**The test data is vendored.** Six tests reached into a sibling working copy, which meant they
passed or skipped depending on what else the developer happened to have cloned — and a test
that skips silently stops being run. 524 KB under `testdata/`, provenance in
`testdata/SOURCES.md`, and nothing skips now.

531 tests, `clippy --workspace --all-targets -- -D warnings` clean, `fmt` clean. The JPSS pass
still decodes 7 200 of 7 200.

## Published

Pinned `xtce-rs` by revision — `e45784b`, which is what `origin/main` carries — instead of by
path. The path worked only on a machine with both checkouts side by side, which is every
machine except the one that matters: a runner. CI lost the step that cloned the sibling and
copied it into place, and gained two that prove the thing end to end without a display, by
running `probe` and a headless `replay` over the recording and grepping the line they print.

**A missing fixture is now a panic.** Four tests would quietly pass when the recording was
absent — three returned early and one substituted packets made up on the spot, which kept it
green while it stopped testing a real pass. Nothing in the repository reads outside it any
more, so absence means deletion, and deletion should be red.

A newer toolchain (1.98.1) brought three lints the code had not met: two slice fills written
as loops and a decimal literal in a bitwise expression. Fixed rather than allowed; they were
right.

531 tests, `clippy --workspace --all-targets -- -D warnings` clean, `fmt` clean, and the pass
still decodes 7 200 of 7 200.
