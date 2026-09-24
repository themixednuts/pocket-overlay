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
     "y = g.cy - sy * TRAVEL_Y; // up", "y = g.cy + sy * TRAVEL_Y; // up", "blackbox_render", "sticks"),
    ("stick x drawn inverted", "web/index.html",
     "const x = g.cx + sx * TRAVEL_X, y", "const x = g.cx - sx * TRAVEL_X, y", "blackbox_render", "sticks"),
    ("gimbal drum doesn't tilt with the stick", "web/index.html",
     "const phi = Math.asin(sy * SWING);", "const phi = 0;", "blackbox_render", "sticks"),
    ("gimbal drum tilts the wrong way", "web/index.html",
     "const phi = Math.asin(sy * SWING);", "const phi = Math.asin(-sy * SWING);", "blackbox_render", "sticks"),
    ("gimbal slot slides instead of tilting (no foreshortening)", "web/index.html",
     "return [cy - DRUM_R * Math.sin(phi + d), cy - DRUM_R * Math.sin(phi - d)];",
     "return [cy - DRUM_R * Math.sin(phi) - half, cy - DRUM_R * Math.sin(phi) + half];",
     "blackbox_render", "sticks"),
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
    ("skin wrap not cropped to the radio's outline", "web/index.html",
     '<g id="skinWrap" clip-path="url(#bodyClip)">', '<g id="skinWrap">',
     "blackbox_render", "a_skin_wraps"),
    ("a missing skin blanks the radio", "web/index.html",
     "probe.onerror = () => { if (shownSkin === key) show(null); };",
     "probe.onerror = () => { if (shownSkin === key) show(href, name); };",
     "blackbox_render", "the_saved_skin"),
    ("skin names not checked (path traversal)", "src/skins.rs",
     "        if valid_name(name) {\n            Ok(self.dir.join", "        if true {\n            Ok(self.dir.join",
     "skins", "bad_skins"),
    ("other websites let in", "src/server.rs",
     "    host_ok && origin_ok\n", "    let _ = (host_ok, origin_ok);\n    true\n",
     "skins", "other_websites"),
    ("axis scaling off by one", "src/hid.rs",
     "(scaled - 1024) as i16", "(scaled - 1023) as i16", "blackbox_ws", "every_channel"),
    ("button bits shifted", "src/hid.rs",
     "if byte >> (bit % 8) & 1 == 1", "if byte >> ((bit + 1) % 8) & 1 == 1", "blackbox_ws", "every_channel"),
    ("wizard records direction backwards", "src/learn.rs",
     "invert: v < 0", "invert: v > 0", "blackbox_ws", "wizard_learns_random_wiring_1"),
    ("wizard allows buttons for analog controls", "src/learn.rs",
     "(target.needs_analog() && ch > axes)", "(false && ch > axes)", "blackbox_ws", "wizard_refuses"),
    ("accent colour not checked (it goes into the page's CSS)", "src/engine.rs",
     "let ok = accent.as_deref().is_none_or(crate::config::valid_colour);", "let ok = true;",
     "blackbox_ws", "accent_colour"),
    ("OBS ignores the saved accent colour", "web/index.html",
     "if (!ACCENT_PARAM) applyAccent(", "if (false) applyAccent(", "blackbox_render", "accent_picked"),
    ("OBS ignores the saved strip choice", "web/index.html",
     'const showPart = (param, saved) => (param === null ? saved : param !== "0");',
     'const showPart = (param, saved) => (param === null ? true : param !== "0");',
     "blackbox_render", "the_strip"),
    ("?channels= and ?readout= ignored", "web/index.html",
     'const showPart = (param, saved) => (param === null ? saved : param !== "0");',
     "const showPart = (param, saved) => saved;", "blackbox_render", "the_strip"),
    ("radio moves when the strip is hidden", "web/index.html",
     ' preserveAspectRatio="xMidYMin meet">', ">", "blackbox_render", "the_strip"),
    ("bars leave a gap where the readout was", "web/index.html",
     'readout ? "" : "translate(0 -38)"', '""', "blackbox_render", "the_strip"),
    ("wizard runs on the demo's made-up input", "src/engine.rs",
     "if !self.raw.connected || self.raw.demo {", "if !self.raw.connected {", "blackbox_ws", "no_channel_detection"),
    ("wizard starts with no radio", "src/engine.rs",
     "if !self.raw.connected || self.raw.demo {", "if self.raw.demo {", "blackbox_ws", "no_channel_detection"),
    ("setup page lets you start detection when it can't work", "web/index.html",
     '$("bStart").disabled = running || !!why;', '$("bStart").disabled = running;',
     "blackbox_render", "channel_detection_says"),
    ("setup page's radio scrolls away with the settings", "web/index.html",
     "body.setup { background: #0e0f11; display: grid; height: 100vh; }",
     "body.setup { background: #0e0f11; overflow: auto; }", "blackbox_render", "everything_stays"),
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
