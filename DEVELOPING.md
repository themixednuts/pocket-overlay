# Developing pocket-overlay

## How it works

A Rust server reads the radio's USB HID reports, decodes them, and pushes the result over a WebSocket to an SVG page (`web/index.html`) that OBS shows as a Browser Source.

- **`src/input.rs`**: finds the radio (USB `1209:4F54`, product "Radiomaster Pocket Joystick"). It reads the HID report descriptor the device announces and decodes with that layout (`src/hid.rs`), falling back to EdgeTX's fixed layout (`src/edgetx.rs`). It also has the `--demo` and `--replay` sources.
- **`src/engine.rs`**: owns the settings, the channel classifier (`src/detect.rs`) and the detection wizard (`src/learn.rs`), and publishes `OverlayState` (`src/overlay.rs`).
- **`src/server.rs`**: serves the page and the WebSocket. The page is built into the binary. A `web/` folder in the working directory overrides it, so you can edit and reload without rebuilding.

The Pocket's USB report is fixed: EdgeTX builds this radio without the configurable joystick extension (`USBJ_EX`). CH1-8 arrive as 8 analog axes (`channel + 1024`, 0..2048). CH9-32 arrive as 24 buttons, on when the channel is above 0. So SB, SC and S1 need one of CH1-8.

Settings live in `%APPDATA%\pocket-overlay\overlay.toml` (Windows) or `~/.config/pocket-overlay/overlay.toml`. The setup page (`/?setup=1`) writes them; `--config <file>` points somewhere else.

## Command-line options

```
pocket-overlay                      # real radio
pocket-overlay --demo               # moving fake input
pocket-overlay --monitor            # raw CH1-32 in the terminal (~ analog, 2/3 = switch type)
pocket-overlay --record flight.txt  # real radio, and save every report
pocket-overlay --replay flight.txt  # play a recording back (`-` = stdin)
pocket-overlay --config <file> --port <n>
```

A recording is plain text, one item per line: `name <text>`, `descriptor <hex>`, `wait <ms>`, `disconnect`, or a report as hex.

Page options go in the query string: `accent=%23rrggbb`, `sides=1` (side views of the paddles, SE and S1), `channels=0`, `readout=0`, `trail=0`, `debug=1`, `setup=1`.

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
  - The detection wizard is driven like a person would drive it, against random channel wiring and reversing.
- **`blackbox_render`**: loads the real page in headless Chrome/Edge and measures the drawing in screen pixels: knob position, direction glow, paddle lean, what's lit, bars, text, and the setup page.
- **`tools/mutants.py`**: plants one realistic bug at a time (inverted axes, mirrored glow, swapped switch ends, off-by-one scaling, and so on) and checks that a test catches each.

Without the EdgeTX checkout or a C++ compiler, the tests use the Rust encoder mirror. Without Chrome or Edge, the render tests print `SKIPPED` and pass. CI (`.github/workflows/ci.yml`) fetches a pinned EdgeTX commit and runs everything on Windows, macOS and Linux.

## Releasing

Push a version tag:

```
git tag v0.1.0
git push origin v0.1.0
```

`.github/workflows/release.yml` builds Windows x86_64, a macOS universal binary (Apple Silicon and Intel), and Linux x86_64 (built on Ubuntu 22.04 for older glibc). It attaches them to a GitHub release together with the README and, for Linux, `packaging/99-radiomaster-pocket.rules`.

## Re-tracing the drawing

```
uv run --with opencv-python-headless --with numpy ref/trace.py   # writes web/traced.js + ref/trace_*.png previews
```

The photos come from `https://radiomasterrc.com/products/pocket-radio-controller-m2.json`: save `images[0]` as `ref/img0.jpg` and `images[6]` as `ref/img6.jpg`.
