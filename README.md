# Pocket Overlay

Shows a RadioMaster Pocket's sticks and switches live in OBS, read straight from the radio over USB.

![The overlay: both sticks, all switches, and the channel values](docs/screenshot.png)

## Download

Get the file for your computer from the [latest release](../../releases/latest) and unzip it:

| Computer | File |
|---|---|
| Windows | `pocket-overlay-…-windows-x86_64.zip` |
| Mac (Apple Silicon or Intel) | `pocket-overlay-…-macos-universal.tar.gz` |
| Linux | `pocket-overlay-…-linux-x86_64.tar.gz` |

## Use it

1. Plug the Pocket into the computer with a USB cable. On the radio, choose **USB Joystick (HID)**.
2. Start **pocket-overlay** (on Windows, double-click it). Its window shows the address to use: `http://127.0.0.1:7878/`.
3. In OBS, add a **Browser** source with that address and a size of **680 × 830**.

Leave pocket-overlay running while you stream. It finds the radio by itself, including after you unplug it and plug it back in.

## If a stick or switch shows the wrong thing

The overlay shows what your model sends, so it depends on the model's mixes. Open `http://127.0.0.1:7878/?setup=1` in a web browser and press **Detect channels**. It asks you to move each stick and switch in turn and remembers what it finds. The same page sets your stick mode (which side the throttle is on).

## Good to know

- **It only listens.** You can fly a sim or play a game with the radio as a joystick while the overlay runs. Both see every movement, and the overlay never sends anything to the radio.
- It shows exactly what the radio sends, with no deadzone or smoothing, up to the radio's full rate of 1000 updates a second.
- Only the sticks, switches and the S1 wheel reach the computer. The menu buttons and trim buttons don't, although trims show up as a small shift in the stick position.
- No radio yet? Run `pocket-overlay --demo` and everything moves by itself, so you can arrange the OBS scene.
- Port 7878 already taken? Run `pocket-overlay --port 7879` and use that number in OBS.
- To change the colour, add `?accent=%23ff8800` (any colour code, with `%23` in place of `#`) to the address in OBS. To hide the channel bars, add `?channels=0`.

**First run on Windows:** if you see "Windows protected your PC", click **More info**, then **Run anyway**. The app isn't signed.

**First run on a Mac:** the app isn't signed by Apple. Right-click `pocket-overlay`, choose **Open**, then **Open** again.

**Linux:** give your user access to the radio once, then unplug it and plug it back in:

```
sudo cp 99-radiomaster-pocket.rules /etc/udev/rules.d/
sudo udevadm control --reload
```

## Building it yourself

With [Rust](https://rustup.rs) installed, run `cargo build --release`. On Linux you also need `libudev-dev` and `pkg-config`. The app ends up in `target/release/`.

[DEVELOPING.md](DEVELOPING.md) covers how it works, the tests, and making a release.
