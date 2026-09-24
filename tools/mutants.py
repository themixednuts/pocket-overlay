"""Mutation check for the black-box tests: plant one realistic bug at a time and make
sure the tests notice. Every file is restored afterwards, even on Ctrl+C.

    python tools/mutants.py            # all mutants
    python tools/mutants.py arc paddle # only mutants whose name contains one of these
"""
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# (name, file, original, mutated, test binary, test filter)
MUTANTS = [
    ("stick y drawn inverted", "web/index.html",
     "y = g.cy - sy * TRAVEL", "y = g.cy + sy * TRAVEL", "blackbox_render", "sticks"),
    ("stick x drawn inverted", "web/index.html",
     "const x = g.cx + sx * TRAVEL", "const x = g.cx - sx * TRAVEL", "blackbox_render", "sticks"),
    ("direction arc mirrored", "web/index.html",
     "Math.atan2(-sy, sx)", "Math.atan2(sy, sx)", "blackbox_render", "sticks"),
    ("paddle leans the wrong way", "web/index.html",
     "rotate(${pos ? 14 : -14}", "rotate(${pos ? -14 : 14}", "blackbox_render", "switches"),
    ("nub heights reversed", "web/index.html",
     "NUB_H = [13, 20, 27]", "NUB_H = [27, 20, 13]", "blackbox_render", "switches"),
    ("channel bar on the wrong side", "web/index.html",
     "e.bar.setAttribute(\"x\", e.x + half + Math.min(0, len))", "e.bar.setAttribute(\"x\", e.x + half - Math.min(0, len))",
     "blackbox_render", "pot_channel"),
    ("button lit at zero", "web/index.html",
     "(s.channels[i + 8] ?? 0) > 0", "(s.channels[i + 8] ?? 0) >= 0", "blackbox_render", "disconnect_dims"),
    ("mode 1 throttle on the wrong stick", "web/index.html",
     "1: { THR: [\"right\", \"y\"]", "1: { THR: [\"left\", \"y\"]", "blackbox_render", "readout_follows"),
    ("left stick axes swapped", "src/overlay.rs",
     "left: stick(s.left_x, s.left_y)", "left: stick(s.left_y, s.left_x)", "blackbox_ws", "physical_controls"),
    ("3-pos switch ends swapped", "src/overlay.rs",
     "3 if v < -SWITCH_MIDDLE => 0,", "3 if v < -SWITCH_MIDDLE => 2,", "blackbox_ws", "physical_controls"),
    ("3-pos middle band too wide for low mix weights", "src/overlay.rs",
     "const SWITCH_MIDDLE: f32 = 0.1;", "const SWITCH_MIDDLE: f32 = 0.33;", "blackbox_ws", "low_mix_weights"),
    ("a small deadzone on the sticks", "src/overlay.rs",
     "let v = f32::from(r.channel(src.ch).unwrap_or(0)) / 1024.0;",
     "let v = f32::from(r.channel(src.ch).unwrap_or(0)) / 1024.0;\n    let v = if v.abs() < 0.03 { 0.0 } else { v };",
     "blackbox_ws", "not_deadzoned"),
    ("updates not capped per frame", "src/server.rs",
     "tokio::time::sleep_until(sent_at + MIN_FRAME).await;", "let _ = sent_at;",
     "blackbox_ws", "radio_rate_flood"),
    ("unplugging leaves the last position on screen", "src/input.rs",
     "tx.send_replace(RawState::default());\n                    // look again soon",
     "// look again soon", "hid_backend", "unplug_and_replug"),
    ("a quiet radio treated as unplugged", "src/input.rs",
     "Ok(0) => continue,", "Ok(0) => return true,", "hid_backend", "quiet_radio"),
    ("gives up when another program has the radio", "src/input.rs",
     'eprintln!("Found {name} but couldn\'t open it ({e}); retrying.");', "return;",
     "hid_backend", "locked"),
    ("wrong radio picked when two are plugged in", "src/input.rs",
     '.find(|(_, name)| name.contains("Pocket"))', '.find(|(_, name)| name.contains("no such radio"))',
     "hid_backend", "prefers_the_pocket"),
    ("axis scaling off by one", "src/hid.rs",
     "(scaled - 1024) as i16", "(scaled - 1023) as i16", "blackbox_ws", "every_channel"),
    ("button bits shifted", "src/hid.rs",
     "if byte >> (bit % 8) & 1 == 1", "if byte >> ((bit + 1) % 8) & 1 == 1", "blackbox_ws", "every_channel"),
    ("wizard records direction backwards", "src/learn.rs",
     "invert: v < 0", "invert: v > 0", "blackbox_ws", "wizard_learns_random_wiring_1"),
    ("wizard allows buttons for analog controls", "src/learn.rs",
     "(target.needs_analog() && ch > axes)", "(false && ch > axes)", "blackbox_ws", "wizard_refuses"),
    ("descriptor ignored", "src/input.rs",
     "state.layout = describe(&l);\n                    layout = l;", "state.layout = describe(&l);",
     "blackbox_ws", "uses_the_layout"),
]


def run(test, filt):
    r = subprocess.run(["cargo", "test", "-q", "--test", test, "--", filt],
                       cwd=ROOT, capture_output=True, text=True)
    return r.returncode == 0


def main():
    wanted = sys.argv[1:]
    results = []
    for name, rel, orig, mutated, test, filt in MUTANTS:
        if wanted and not any(w in name for w in wanted):
            continue
        path = ROOT / rel
        text = path.read_text(encoding="utf8")
        assert text.count(orig) == 1, f"{name}: pattern found {text.count(orig)} times in {rel}"
        try:
            path.write_text(text.replace(orig, mutated), encoding="utf8", newline="\n")
            passed = run(test, filt)
        finally:
            path.write_text(text, encoding="utf8", newline="\n")
        verdict = "SURVIVED (tests missed it)" if passed else "caught"
        print(f"{verdict:28} {name}", flush=True)
        results.append(passed)
    survivors = sum(results)
    print(f"\n{len(results) - survivors}/{len(results)} mutants caught")
    sys.exit(1 if survivors else 0)


if __name__ == "__main__":
    main()
