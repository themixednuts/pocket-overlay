# Pocket Overlay

Show your RadioMaster Pocket's sticks and switches live on stream.

![The overlay: both sticks, all switches, and the channel values](docs/screenshot.png)

## Setup

1. **Download** the file for your computer from the [latest release](../../releases/latest) and unzip it.
   Windows: `…windows-x86_64.zip` · Mac: `…macos-universal.tar.gz` · Linux: `…linux-x86_64.tar.gz`
2. **Plug in** the Pocket with a USB cable. On the radio, choose **USB Joystick (HID)**.
3. **Run** `pocket-overlay`. On Windows, double-click it. If Windows warns you, click **More info**, then **Run anyway**.
4. **In OBS**, add a **Browser** source with the URL `http://127.0.0.1:7878/`, width **680** and height **830**.

That's it. Keep `pocket-overlay` open while you stream. You can still use the radio in a sim or game at the same time.

## Something looks wrong?

- **A stick or switch moves the wrong thing:** open <http://127.0.0.1:7878/?setup=1> in your browser, click **Detect channels**, and follow the prompts.
- **Nothing moves:** check the radio is in **USB Joystick (HID)** mode and the `pocket-overlay` window says **Radio connected**.

<details>
<summary><b>Mac</b></summary>

The app isn't signed by Apple. The first time, right-click `pocket-overlay`, choose **Open**, then **Open** again.
</details>

<details>
<summary><b>Linux</b></summary>

Give your user access to the radio once, then unplug it and plug it back in:

```
sudo cp 99-radiomaster-pocket.rules /etc/udev/rules.d/
sudo udevadm control --reload
```

Then run `./pocket-overlay` from the unzipped folder.
</details>

<details>
<summary><b>Extras</b></summary>

- `pocket-overlay --demo`: everything moves by itself, so you can arrange the OBS scene without the radio.
- `pocket-overlay --port 7879`: use another port if 7878 is taken, and put the same number in the OBS URL.
- Add `?accent=%23ff8800` to the OBS URL for another colour, or `?channels=0` to hide the channel bars.
- Only the sticks, switches and S1 reach the computer. The menu and trim buttons don't, although trims show as a small shift in the stick position.
</details>

Building it yourself, or curious how it works? See [DEVELOPING.md](DEVELOPING.md).
