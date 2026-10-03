# Kurogane Showcase

**A Rust app that owns its window and its loop, with Chromium as one participant in the frame, never the owner.**

The centre of the window is a wgpu compute galaxy of millions of stars, as many as the GPU's memory holds. Around it sit three live Chromium panes, each a real web page: one steers the galaxy, one plots what it is doing and one shows what the loop reports. Everything runs on one thread, in one winit event loop, and Chromium is pumped only when it asks to be.

Press **E** to pull the window apart and see which part is which.

## Run it

With the [Kurogane CLI](https://github.com/0x48piraj/kurogane) installed:

```bash
kurogane showcase
```

Or from a clone of this repository:

```bash
kurogane run
```

The CLI resolves and stages the Chromium runtime for you. On Linux the window must be X11 (XWayland is fine in a Wayland session), because CEF can only parent a browser to an X11 window.

### Controls

| Input | Effect |
|---|---|
| Sliders in the controls pane | Star count, gravity, swirl, turbulence, colour, camera spin, star size |
| **Burst** button or **Space** | Pushes the stars outward for a moment |
| **Pull apart** button or **E** | Separates the window into its layers |

## What you are looking at

| Piece | Owned by | What it does |
|---|---|---|
| Window and event loop | **winit** (`src/host.rs`) | One thread, one loop, owned by the app |
| The galaxy | **wgpu** (`src/galaxy.rs`, `src/step.wgsl`, `src/draw.wgsl`) | A compute shader moves the stars and counts them into two histograms; a render pass draws them as additive glowing discs |
| Frame chrome and HUD | **egui** (`src/ui.rs`) | The wires between panes, labels and per-frame cost readouts |
| The three panes | **Chromium, through Kurogane** | `panes/controls.html`, `panes/charts.html` and `panes/console.html`, real web pages placed inside the window |

wgpu does all the simulation and drawing. Kurogane does none of it. Kurogane is what puts real Chromium pages inside the same native window and gives them a way to talk to the Rust code.

```
winit window (the app's)
 ├─ wgpu surface ............ galaxy + egui chrome   <- the app's GPU frame
 ├─ child window: controls ─┐
 ├─ child window: charts    ├─ Chromium (Alloy, windowed), moved every frame
 └─ child window: console  ─┘
UI thread:          winit events → frame() → pump Chromium when it asks
Renderer processes: HTML/JS + injected `kurogane` ⇄ process messages ⇄ Rust closures
```

## How Chromium is used

### 1. Each pane is a windowed child browser, not offscreen rendering

Each pane is created with `AppInstance::create_child_browser(&window, bounds, url)`. Kurogane turns that into a CEF browser with:

- **`set_as_child(parent, rect)`**: CEF creates a native child window (an HWND on Windows, an NSView on macOS, an X11 window on Linux) parented to the winit window. Chromium composites into it through its own GPU process.
- **`RuntimeStyle::ALLOY`**: the bare content-only style, with no tabs, toolbar or browser chrome.

Two things follow from this:

- **Pane pixels never pass through the app's wgpu code.** The galaxy draws into the main window's surface. The panes are separate OS windows on top of it, and the OS compositor combines them. The panes cost the wgpu frame nothing.
- **Moving a pane means moving a window.** CEF has no move call for windowed browsers (`WasResized` is only for offscreen ones), so `BrowserHandle::set_bounds` moves the child window directly, the way CEF's sample client does. The pull-apart animation calls it every frame with spring-animated rects (`place_panes` in `src/host.rs`).

The alternative is CEF's offscreen rendering, where Chromium hands over pixel buffers to upload as textures. That would let the panes blend into the 3D scene, but every frame would cost a copy. This demo deliberately doesn't do that.

### 2. Chromium's message loop is pumped by the app's loop

CEF normally wants to own the thread with its own message loop. `App::start_embedded()` starts it with CEF's **external message pump** instead:

1. When Chromium has work queued, CEF calls `on_schedule_message_pump_work(delay)`.
2. Kurogane passes that to the app as a `PumpRequest`. The `.scheduler(...)` callback in `src/main.rs` forwards it to the winit loop as a deadline.
3. The loop calls `instance.pump()` once the deadline passes, and at least every 33 ms as a safety net, the same longest wait cefclient uses. A pump is one slice of Chromium's own loop, run on the app's thread.

The UI thread therefore runs both winit and Chromium's browser-side work, and the loop measures what each pump costs. The console's *"Chromium asked for work N times, pumped M times, X ms in all"* line reports exactly this, every second.

### 3. Chromium is still multi-process

Only Chromium's **browser process** shares the app's thread. The HTML, JavaScript and layout run in Chromium's usual **renderer processes**, alongside a GPU process.

- **Where `kurogane.*` in JavaScript comes from.** In each renderer process, Kurogane injects a bridge object into every page's V8 context as it is created.
- **How a call reaches Rust.** `kurogane.invoke('sim.set', params)` is packed into a CEF process message and sent from the renderer to the browser process. The Rust closure runs there during a pump. Replies, events and stream chunks go back the same way, and large payloads can travel through shared memory instead of being copied.
- **Where the pages are loaded from.** `app://app/controls.html` is neither a file path nor an HTTP server. Kurogane registers an `app` scheme handler in the browser process that serves the `panes/` folder.

## The three channels between the panes and Rust

All of them are declared on the `App` builder in `src/main.rs`.

| Channel | JavaScript side | Rust side | Used for |
|---|---|---|---|
| **Commands** (request/response) | `kurogane.invoke('sim.set', params)` | `.command("sim.set", \|params, _\| ...)` | Sliders, Burst, Pull apart, reading the current params |
| **Streams** (binary, every frame) | `kurogane.openStream('telemetry')` | `.stream("telemetry", ...)` → `StreamResponder` | Frame costs, star count and both GPU histograms as raw `f32` bytes |
| **Events** (Rust to JavaScript) | `kurogane.on('console.line', ...)` | Emitted from the loop | Console lines |

Keys work across the whole window too. `.on_key(...)` lets Rust catch **E** even when a pane has keyboard focus, unless the focus is in an editable field.

## One frame, end to end

Dragging the Stars slider:

1. `panes/controls.html` calls `kurogane.invoke('sim.set', params)`, at most once per animation frame.
2. The call crosses from the renderer process to the browser process. During the next pump, the `sim.set` closure writes the new params into `Shared`.
3. The loop's next `frame()` steps the layout springs, places the panes, then runs the wgpu compute step with the new star count (adding star buffers until GPU memory is full) and draws.
4. The histograms come back from the GPU a frame or two later through a mapped buffer, so the frame never stalls.
5. The loop sends them, with the frame's costs, on the `telemetry` stream to `panes/charts.html`, which plots them.

## Benchmarking

The showcase can measure itself: each frame's CPU stages as tracing spans, and each GPU pass with timestamp queries. A benchmark steers the scene the same way on every run, so a run before a change can be compared with one after it. Its switches go after `--`; Chromium ignores them.

```bash
# Step the stars up by √2 from 65,536 until GPU memory is full (about 95 s)
kurogane run --release -- --bench=baseline

# Replay perf/drag.input.json: drags across the whole range, bursts, a pull-apart, a bigger star size (about 40 s)
kurogane run --release -- --bench=baseline-drag --replay=drag
```

While a benchmark runs it ignores the controls, and it closes the window when it is done. It prints a table and writes to `perf/`:

| File | What it holds |
|---|---|
| `perf/<name>.json` | The summary: for each star count, frame rate and frame-time percentiles, the loop's CPU stages and the GPU passes |
| `perf/<name>.jsonl` | The trace, one JSON line each: a `frame` record per frame, a `gpu` record per timed frame, and Kurogane's, wgpu's and egui's own log lines among them |
| `perf/<name>.input.json` | A recording to replay |

### After a change

Run the same benchmark under a new name. It prints its own numbers, then the change from the baseline:

```bash
kurogane run --release -- --bench=one-triangle                     # against baseline
kurogane run --release -- --bench=one-triangle-drag --replay=drag  # against baseline-drag
kurogane run --release -- --compare=baseline,one-triangle          # any two runs, no window
```

`--against=<name>` compares with a run other than the baseline.

### Your own session

```bash
kurogane run --release -- --record=mine    # use the controls, then close the window
kurogane run --release -- --bench=baseline-mine --replay=mine
kurogane run --release -- --trace=session  # trace an ordinary session, without a benchmark
```

### Reading the table

| Column | Meaning |
|---|---|
| secs | How long the run stayed at that star count; a replay lists the counts it stayed at for a second or more |
| fps, on time | Frames a second, and the share of frames within 1.25 refresh periods |
| frame p95, worst | Milliseconds between frames: the 95th percentile and the slowest |
| cpu | The loop's own work in a frame, without waiting on the GPU |
| pump | Chromium's pumps between frames |
| wait | Waiting for the surface's next texture: long when the GPU is behind |
| gpu, compute, draw | GPU milliseconds a frame: every pass, the simulation, and the stars drawn |

- Compare release builds with release builds. A debug build mostly measures unoptimised Rust and wgpu's validation.
- Keep the window the same size, and close other GPU-heavy apps. How many stars fit depends on the GPU memory everything else is using.
- Below about a million stars the GPU lowers its clocks, so its timings there are noisy.

The HUD over the galaxy shows the GPU timings live as well.
