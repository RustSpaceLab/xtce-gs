# Architecture

A ground station is four things in a row: bytes arrive, frames are recovered from them,
packets are recovered from the frames, and parameters are recovered from the packets. Each
step can fail on its own, each one has counters an operator watches, and only the last one
needs the XTCE definition. That sequence is the crate split.

```
 socket / file          xtce-gs-link                     xtce-gs-engine            xtce-gs-gui
┌──────────────┐   ┌────────────────────────┐   ┌─────────────────────────┐   ┌──────────────┐
│ UDP TCP file │──▶│ sync → derandom → RS   │──▶│ Decoder::decode         │──▶│ egui: tables │
│              │   │  → frame → assemble    │   │  → owned Sample         │   │  plots, log  │
│              │   │  → (CSP unwrap)        │   │  → ParameterStore       │   │              │
└──────────────┘   └────────────────────────┘   └─────────────────────────┘   └──────────────┘
      bytes            RawPacket (owned)            Batch, then the store         read-only
```

## The decision the rest follows from

`xtce-decode` hands back `DecodedPacket<'db, 'p>`: parameter values that borrow the packet
buffer *and* the definition. Nothing with that type can cross a channel to a drawing thread,
sit in a history, or outlive the datagram it came from.

So there is exactly one place where the borrow ends — `xtce_gs_engine::decode`, immediately
after `Decoder::decode` returns — and `xtce-gs-core::Value` is what it ends into. Every type
downstream of that point owns what it refers to. This is the whole reason `xtce-gs-core`
exists, and the reason it is a crate rather than a module: the interface must be able to
depend on the data model without depending on tokio or on a decoder.

The definition itself is different: `XtceDb` is built once, never mutated, and is `Send +
Sync`, so it is an `Arc<XtceDb>` and not an `Arc<RwLock<…>>`. A lock around something nothing
writes is a lock that only costs.

The `Decoder` borrows the db, so it is *not* stored in a struct beside it — a self-referential
field would need `unsafe` or a crate to hide the `unsafe`. It is created inside the decode
task, where the `Arc` it borrows from outlives it by construction.

## Threads

Three, and they are not symmetric.

| Thread | Owns | Blocks on |
|---|---|---|
| tokio runtime (n workers) | the source, the framing pipeline | the socket |
| decode | `Decoder`, sequence counters | an `mpsc` of `RawPacket` |
| interface (main) | egui, the layout | the display's vblank |

The decode task holds the store's write lock once per *batch*, not once per packet: a lock
taken 10 000 times a second is a lock the interface never gets. The interface takes the read
lock once per frame and copies out only what it draws.

`LinkStats` is atomics rather than a locked struct, because the status bar reads eighteen
counters every frame and the link writes several per frame. Nothing is published through
them, so `Relaxed` is the right ordering and not a shortcut.

## Repainting

egui is immediate mode: it redraws when something asks it to. Asking on a timer wastes a
laptop battery on a stream that sends one packet a minute; never asking means the operator
watches a frozen number.

So the engine holds a waker — `Arc<dyn Fn() + Send + Sync>`, which the interface fills with
`egui::Context::request_repaint` — and calls it after a batch has been ingested. An idle
station repaints once a second for the clock, and a loaded one repaints at the frame rate
because the store's generation counter has moved. The interface compares that counter with
what it drew last and skips the work when nothing changed.

## Decimation

A plot is at most a few thousand pixels wide and a history is up to a few hundred thousand
points. Handing all of them to `egui_plot` means tessellating a line with a hundred thousand
segments per frame, on the CPU, sixty times a second.

`xtce_gs_core::decimate` reduces the points in the visible x-range to roughly the plot's
width in pixels before they reach the widget, with Largest-Triangle-Three-Buckets. LTTB is
chosen over averaging because averaging removes exactly what an operator is watching for — a
spike one sample wide is a transient, not noise — and over plain min/max because LTTB keeps
the *shape* between the extremes. The decimation runs on the raw ring under the read lock and
writes into a buffer the plot owns, so nothing is allocated per frame.

## What the link layer must not assume

* **A datagram is not a frame.** UDP delivers message boundaries, TCP does not, and a file is
  neither. The synchroniser is fed bytes and finds its own frames, so the same code path
  serves all three.
* **Sync can be lost.** A station that finds one ASM and then trusts the frame length forever
  reports zero errors on a stream it has silently desynchronised from. The synchroniser has
  two states and counts the transitions.
* **Reed-Solomon can mis-correct.** Sixteen symbols is the limit for RS(255,223); beyond it a
  decoder can produce a clean-looking codeword that is not the transmitted one. Uncorrectable
  must be reported, not guessed at, and the count is on the status bar.
* **Reed-Solomon cannot tell you the stages are in the wrong order.** The randomiser is
  applied to the whole codeblock, check symbols included (CCSDS 131.0-B-5 §10), so it comes
  off before the decoder runs. Get that backwards and the decode still succeeds reporting
  *zero errors*: the 255-octet randomiser sequence is itself a valid RS(255,223) codeword and
  the code is linear, so a codeblock that has not been derandomised is another codeword. The
  station then reports a clean link and hands rubbish to the frame parser. The first draft of
  this document had the diagram the other way round, which is how the implementation got it
  wrong; the test that now pins it is named after the property, not after the code.
* **A packet spans frames.** The first-header-pointer says where the first packet *starts* in
  a frame; the bytes before it belong to the packet from the previous frame. Dropping them is
  how a station loses one packet in every few and calls the link clean.

## Crates

| Crate | Depends on | Contains |
|---|---|---|
| `xtce-gs-core` | `xtce-model`, `xtce-decode` | `Value`, `Sample`, `Batch`, `RawPacket`, `Utc`, `RingBuffer`, `ParameterStore`, `LinkStats`, `EventLog`, `decimate`, `limits` |
| `xtce-gs-link` | core, tokio | sources, ASM sync, derandomiser, Reed-Solomon, TM transfer frames, packet assembly, CSP, the pipeline that composes them |
| `xtce-gs-engine` | core, link, model, decode, tokio | session configuration, the task graph, borrowed-to-owned conversion, spacecraft time, sequence tracking, CSV export, recording |
| `xtce-gs-gui` | core, engine, egui | the window: parameter table, plots, link status, event log, saved layout |
| `xtce-gs-cli` | everything | `xtce-gs run`, `replay`, `export`, `probe` |

## Deliberate omissions

* **No database.** History is a ring in memory and a recording is a file of bytes. A station
  that needs a parameter archive should hand the recording to something that is one.
* **No serial port.** `serialport` is a C-library binding on some platforms and the radios in
  reach speak UDP. A TODO in `source.rs` says where it would go.
* **No commanding.** Uplink is `xtce-flight`'s half of the problem — it already encodes a
  telecommand from the same definition — and wiring it in needs a link that can transmit,
  which is a different conversation from a link that can receive.
* **No alarm definitions from XTCE.** `xtce-model` does not model `<AlarmSet>`, so limits are
  a JSON file keyed by qualified parameter name. When the model grows alarms, `limits.rs`
  gains a constructor and the file becomes an override.
