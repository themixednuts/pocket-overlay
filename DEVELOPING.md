# Developing pocket-overlay

## Building

Install [Rust](https://rustup.rs) and run `cargo build --release`. On Linux you also need `libudev-dev` and `pkg-config`. The app ends up in `target/release/`.

## How it works

A Rust server reads the radio's USB HID reports, decodes them, and pushes the result over a WebSocket to an SVG page (`web/index.html`) that OBS shows as a Browser Source.

- **`src/input.rs`**: finds the radio (USB `1209:4F54`, product "Radiomaster Pocket Joystick"). It reads the HID report descriptor the device announces and decodes with that layout (`src/hid.rs`), falling back to EdgeTX's fixed layout (`src/edgetx.rs`). It also has the `--demo` and `--replay` sources.
- **`src/engine.rs`**: owns the settings, the channel classifier (`src/detect.rs`) and the detection wizard (`src/learn.rs`), and publishes `OverlayState` (`src/overlay.rs`).
- **`src/server.rs`**: serves the page and the WebSocket. The page is built into the binary. A `web/` folder in the working directory overrides it, so you can edit and reload without rebuilding.

The Pocket's USB report is fixed: EdgeTX builds this radio without the configurable joystick extension (`USBJ_EX`). CH1-8 arrive as 8 analog axes (`channel + 1024`, 0..2048). CH9-32 arrive as 24 buttons, on when the channel is above 0. Any control can use any channel. On CH9-32 a stick, SB, SC or S1 shows only two positions (the middle of a 3-position switch reads like the end that's off).

SB and SC can instead have a channel per position (`up`, `mid`, `down` in the settings, two or three of them), which is how game setups often give each position its own button. With none of them on, the switch is in the position left out.

The detection wizard finds all of this. Sticks and S1 are found from one movement. Switches are held in each position in turn (toward you, away, then the middle), and the positions show how the switch is wired: one channel, or a channel per position. A middle that looks like one end on every channel (a switch on one on/off channel) is taken after 2 s without a change. A control mixed to several channels (a copy on a button for a game, or a stick mirrored for two aileron servos) is read from one of them, on CH1-8 when it can be.

### Sharing the radio with games

The app reads the radio and never writes to it: the `HidBackend`/`HidPort` traits in `src/input.rs` have no write method.

Other programs using the radio as a joystick keep working on every OS:

- **Windows:** hidapi opens with `FILE_SHARE_READ | FILE_SHARE_WRITE`, and each open handle gets its own copy of every report.
- **Linux:** several processes can open the same hidraw node, each with its own queue of reports. Games usually read the separate evdev node, and a game that grabs it (`EVIOCGRAB`) only shuts out other evdev readers, not hidraw. The `uhid` test checks all of this against the real kernel.
- **macOS:** hidapi *seizes* devices by default (`hid_init` sets `kIOHIDOptionsTypeSeizeDevice`). The `macos-shared-device` feature turns that off, `Hidapi::new` makes sure of it, and a macOS-only test checks it in CI.

If another program does hold the radio exclusively, the app says so once and keeps retrying.

### Rates

In joystick mode EdgeTX runs its mixer every 1 ms and sends a report each cycle, over an endpoint with `bInterval = 1`. That's up to 1000 reports a second; with the internal RF module active, the mixer follows the module's timing instead.

The reader handles every report. Each page gets at most one update per frame, always the newest (`server::MIN_FRAME`, about 120 a second, or about 64 on Windows because of its timer granularity). A page that stops reading for 5 s is dropped.

### Skins and the local server

A skin is a PNG in `skins/` next to the settings file. It's scaled to cover the body's bounding box and clipped to the traced outline (`#skinWrap`, `#bodyClip`); the shading, outline and every detail are drawn over it. The server stores skins (`GET/PUT/DELETE /skins/{name}`, PNG only, names `[A-Za-z0-9_-]{1,40}`, 20 MB max) and bumps `skin_rev` in the state so open pages reload them.

The server listens on 127.0.0.1 only and answers only requests whose `Host` (and `Origin`, when a browser sends one) is `127.0.0.1:<port>` or `localhost:<port>`. That keeps other websites in the same browser from reaching it, including over the WebSocket, which isn't covered by the browser's cross-site rules.

Settings live in `%APPDATA%\pocket-overlay\overlay.toml` (Windows) or `~/.config/pocket-overlay/overlay.toml`. The setup page (`/?setup=1`) writes them; `--config <file>` points somewhere else.

## Command-line options

```
pocket-overlay                      # real radio; opens the settings page in the default browser
pocket-overlay --no-browser         # ...without opening it
pocket-overlay --demo               # moving fake input
pocket-overlay --monitor            # raw CH1-32 in the terminal (~ analog, 2/3 = switch type)
pocket-overlay --record flight.txt  # real radio, and save every report
pocket-overlay --replay flight.txt  # play a recording back (`-` = stdin)
pocket-overlay --config <file> --port <n>
```

If the port is taken by a running pocket-overlay (it answers `/state`), a second copy opens that one's settings page and exits. If another program has the port from the settings file, it takes a random free port and saves it, so the OBS URL stays the same from then on; a port given with `--port` is used as is, or it exits with an error.

The OBS source is `http://127.0.0.1:7878/`, 680 wide and 830 tall (less with the readout or channel bars hidden; the setup page and the console show the size for the saved choice).

A recording is plain text, one item per line: `name <text>`, `descriptor <hex>`, `wait <ms>`, `disconnect`, or a report as hex.

Page options go in the query string: `accent=%23rrggbb`, `skin=name|none`, `channels=0|1`, `readout=0|1`, `sides=1` (side views of the paddles, SE and S1), `trail=0`, `debug=1`, `setup=1`. The look ones override what's saved (`accent`, `skin`, `show_readout`, `show_channels` in the settings file) for that one page.

The drawing is pinned to the top of the page (`preserveAspectRatio="xMidYMin meet"`) and the viewBox ends under whatever is shown, so hiding the strip under the radio never moves or resizes the radio in a scene; it only frees space at the bottom.

## Tests

```
git clone --depth 1 https://github.com/EdgeTX/edgetx.git ref/edgetx   # for the firmware checks
cargo test
cargo test --test blackbox_render -- --ignored   # screenshot gallery + evidence.json in target/tmp
python tools/mutants.py                          # plants bugs; fails if any test misses one (~15 min)
```

The tests treat the app as a black box. They act like a radio on one end and like a viewer on the other.

- **`edgetx_conformance`**: compares against the EdgeTX source.
  - The built-in descriptor equals `HID_JOYSTICK_ReportDesc` byte for byte.
  - The Pocket builds with `USBJ_EX OFF`, and the USB IDs, product name and switch types (`pocket.json`) match.
  - EdgeTX's own `usbJoystickUpdate()` is compiled into a harness. Its output decodes exactly for 508 channel vectors, and the Rust encoder mirror matches it byte for byte.
- **`blackbox_ws`**: models the physical radio (positions, then the model's mixes, then EdgeTX's encoder) and feeds the real binary its USB bytes on stdin.
  - The published state must read back the physical positions.
  - It also covers custom descriptors, unplugging, recordings and the stick-mode setting.
  - The detection wizard is driven like a person would drive it, against random channel wiring and reversing, a model with every switch and the pot on CH9-32, a button per switch position, and controls mixed to several channels.
  - Edge cases:
    - no deadzone (a single EdgeTX unit off centre comes through);
    - switches mixed at low weights;
    - rates: at the radio's ~1000 reports/s the last position is on screen within 60 ms (about 11 ms locally); a flood of tens of thousands per second still gives at most one update per frame and catches up within a second;
    - several viewers plus a frozen one;
    - rapid unplug cycles;
    - junk in the stream.
- **`hid_backend`**: runs the real USB read/reconnect loop against a scripted fake device:
  - 25 unplug/replug cycles, with nothing stale left on screen;
  - read errors from a glitchy cable, and a quiet radio that keeps its position;
  - garbage reports;
  - a radio locked by another program;
  - an unreadable descriptor;
  - two radios plugged in (it picks the Pocket);
  - reports never going backwards at 1000/s;
  - throughput (over 180,000 reports/s in a debug build).
- **`blackbox_render`**: loads the real page in headless Chrome/Edge and measures the drawing in screen pixels: knob position (including 1% deflections), the gimbals tilting like the real ones (the slot rolls the same way as the knob but less, and foreshortens), paddle lean, what's lit, bars, text, the setup page, the accent colour reaching OBS, hiding the strip under the radio at the real OBS size (680 × 830) without the radio moving, and that nothing ends up off screen at ten window sizes from a phone to 1920 × 1080 (on the setup page, with the settings scrolled to the bottom and the wizard on every step). Channel detection refuses to start without a radio or on `--demo`.
- **`uhid`** (Linux): plugs in a virtual Pocket through the kernel's uhid (same USB IDs, name and descriptor), so the real USB path runs against the real kernel HID stack.
  - Two overlays read it through hidraw while three "games" read it as a gamepad through evdev: one opened before the overlays, one opened later that grabs the gamepad for itself, one after unplugging and plugging back in.
  - Every overlay reads every report sent (compared byte for byte with its `--record` file), shows the last one, notices the unplug and comes back on its own.
  - Every game sees every report too. The kernel can split one report into several gamepad updates (it budgets about 16 events per update, a report can make about 56), so games may briefly see a mix of two reports; the overlay always gets whole reports.
  - Needs `/dev/uhid`: CI loads the module and installs the udev rule we ship. Elsewhere it prints `SKIPPED` and passes; `POCKET_UHID=1` makes that a failure.
- **`skins`**: uploads, lists, chooses and removes skins over HTTP and the WebSocket; rejects path-traversal names, non-PNGs and oversized files; refuses requests and WebSocket connections from other websites. `blackbox_render` checks a skin wraps the body inside the outline with every detail on top, and that `?skin=none` and a misspelt skin fall back to the drawing.
- **`tools/mutants.py`**: plants one realistic bug at a time (inverted axes, a gimbal that slides instead of tilting, swapped switch ends, off-by-one scaling, and so on) and checks that a test catches each.

Without the EdgeTX checkout or a C++ compiler, the tests use the Rust encoder mirror. Without Chrome or Edge, the render tests print `SKIPPED` and pass; at most four of them run at a time, since each starts its own browsers. CI (`.github/workflows/ci.yml`) fetches a pinned EdgeTX commit and runs everything on Windows, macOS and Linux.

## Releasing

Set `version` in `Cargo.toml`, commit, then push a matching tag:

```
git tag v0.1.1
git push origin v0.1.1
```

`.github/workflows/release.yml` builds Windows x86_64, a macOS universal binary (Apple Silicon and Intel), and Linux x86_64 (built on Ubuntu 22.04 for older glibc). It attaches them to a GitHub release together with the README and, for Linux, `packaging/99-radiomaster-pocket.rules`.

## Re-tracing the drawing

```
uv run --with opencv-python-headless --with numpy ref/trace.py   # writes web/traced.js + ref/trace_*.png previews
```

The photos come from `https://radiomasterrc.com/products/pocket-radio-controller-m2.json`: save `images[0]` as `ref/img0.jpg` and `images[6]` as `ref/img6.jpg`.
