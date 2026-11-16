//! The VU chain end to end, from a real child process to the pasted report.
//!
//! # Why this test exists
//!
//! The user's report said “METERING: TOUS les niveaux à 0.000” and nothing else:
//! a recorder that never spawns, one that dies at once, one whose blocks were all
//! dropped as unreadable (a stream read with the wrong sample format) and a
//! genuinely silent bus all left the *same* trace — a dark bar.  The report now
//! carries a CAPTURES section and the `metering_*` findings, and this file is
//! what pins them down **through the real path**: a `MeterCapture` spawns a child
//! (`sh` scripts standing in for `pw-cat`, so no audio server is needed), the
//! child's bytes go through the real reader, the registry and
//! [`Report::collect`], and the assertions are made on the text the user pastes.
//!
//! The nominal case (a real `pw-cat` recording a real sink) is
//! `capture::tests::a_live_capture_feeds_the_board`, which runs against a live
//! PipeWire when `HOLAF_METER_LIVE_SINK` / `HOLAF_METER_LIVE_WAV` are set.
#![cfg(feature = "engine-pw")]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use holaf_core::model::{Strip, StripType};
use holaf_ui::capture::MeterCapture;
use holaf_ui::diagnostics::{Inputs, Report, Severity};
use holaf_ui::meters::MeterBoard;

/// An executable `sh` script that writes `payload` to stdout for ever — a
/// `pw-cat --record` stand-in.
fn recorder_repeating(tag: &str, payload: &[u8]) -> String {
    let dir = std::env::temp_dir().join(format!("holaf-meter-report-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("block.bin");
    std::fs::write(&data, payload).unwrap();
    let script = dir.join("recorder.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nwhile :; do cat '{}'; done\n", data.display()),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    script.to_string_lossy().into_owned()
}

/// The 7 strips of the shipped config, reduced to the two the tests capture.
fn strips() -> Vec<Strip> {
    vec![
        Strip::with_uid("in-mic", "Mic", StripType::Input),
        Strip::with_uid("out-casque", "Casque", StripType::Output),
    ]
}

/// A `status()` payload matching those strips, so the report's ENGINE/GRAPH
/// sections are coherent (the findings are what this file checks).
fn status() -> serde_json::Value {
    serde_json::json!({
        "running": true,
        "degraded": false,
        "pending": [],
        "fx_host": {"mode": "external", "pid": 4242, "respawns": 0, "chains": ["Casque", "Mic"]},
        "holaf_nodes": ["Holaf_Bus_Mic"],
        "meter_sources": {
            "in-mic": "Holaf_Bus_Mic.monitor",
            "out-casque": "alsa_output.usb-PreSonus.analog-surround-40.monitor",
        },
        "strips": [
            {"uid": "in-mic", "label": "Mic", "node_name": "Holaf_Bus_Mic",
             "device_name": null, "volume": 0.85, "effects": {}},
            {"uid": "out-casque", "label": "Casque",
             "node_name": "Holaf_Bus_Casque",
             "device_name": "alsa_output.usb-PreSonus.analog-surround-40",
             "volume": 0.75, "effects": {"eq": {"active": true}}},
        ],
    })
}

/// Run `case` on a fresh capture (one source), wait for the reader to settle,
/// and return the report the user would paste.
fn report_for(capture: &MeterCapture, uid: &str, source: &str) -> Report {
    let strips = strips();
    let levels = vec![
        ("in-mic".to_string(), "Mic".to_string(), [0.0_f32, 0.0]),
        ("out-casque".to_string(), "Casque".to_string(), [0.0, 0.0]),
    ];
    let status = status();
    // The registry snapshot is taken *after* the wait, by `collect` below.
    let captures = capture.stats().summary();
    let sources: HashMap<String, String> = [(uid.to_string(), source.to_string())]
        .into_iter()
        .collect();
    Report::collect(&Inputs {
        status: Some(&status),
        status_error: None,
        devices: &[],
        meter_sources: &sources,
        strips: &strips,
        missing_devices: &[],
        meter_missing: &[],
        meter_levels: &levels,
        vu_gains: &[],
        captures: Some(&captures),
        config_path: None,
        now_unix: 1_700_000_000,
        daemon_pids: Some(Vec::new()),
    })
}

/// Like [`report_for`], but with the **live** level the board is drawing (so the
/// report's METERING section and the `metering_silent` finding reflect the real
/// bars, not a hard-coded zero).  Used by the conversion case, whose whole point
/// is that the bar moves.
fn report_for_live_level(capture: &MeterCapture, uid: &str, source: &str, level: [f32; 2]) -> Report {
    let strips = strips();
    let levels = vec![
        ("in-mic".to_string(), "Mic".to_string(), level),
        ("out-casque".to_string(), "Casque".to_string(), [0.0, 0.0]),
    ];
    let status = status();
    let captures = capture.stats().summary();
    let sources: HashMap<String, String> = [(uid.to_string(), source.to_string())]
        .into_iter()
        .collect();
    Report::collect(&Inputs {
        status: Some(&status),
        status_error: None,
        devices: &[],
        meter_sources: &sources,
        strips: &strips,
        missing_devices: &[],
        meter_missing: &[],
        meter_levels: &levels,
        vu_gains: &[],
        captures: Some(&captures),
        config_path: None,
        now_unix: 1_700_000_000,
        daemon_pids: Some(Vec::new()),
    })
}

/// The `blocks : N accepted` figure the report prints (`0` when absent).
///
/// Parsing the number beats substring matching: `!text.contains("0 accepted")`
/// also matches the perfectly normal “10 accepted” (and 20, 100, …), which made
/// the tests below fail on ~10 % of the runs — a CI-only red that is
/// unreproducible locally because the decoded-block count depends on scheduling.
fn accepted_blocks_in(text: &str) -> u64 {
    text.lines()
        .find_map(|line| {
            let (_, rest) = line.split_once(": ")?;
            let (count, tail) = rest.split_once(' ')?;
            if !tail.starts_with("accepted") {
                return None;
            }
            count.parse().ok()
        })
        .unwrap_or(0)
}

/// Start one capture and wait until `ready` holds (bounded: a hung child must
/// fail the test, not hang it).
fn capture_until(
    capture: &mut MeterCapture,
    board: &MeterBoard,
    uid: &str,
    source: &str,
    mut ready: impl FnMut(&MeterCapture) -> bool,
) {
    let slots = board.assign(&[uid.to_string()]);
    let mut wanted = HashMap::new();
    wanted.insert(uid.to_string(), source.to_string());
    capture.sync(&wanted, &slots, board);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready(capture) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "the capture never settled: {:?}\n{}",
        capture.stats().summary(),
        report_for(capture, uid, source).text()
    );
}

/// **Case A — `pw-cat` is not installed.**  `spawn` fails with ENOENT on Arch
/// when `pipewire-audio` is missing; the report must name the binary, the OS
/// error and the package, instead of leaving seven bars at 0.000.
#[test]
fn a_missing_recorder_is_described_by_the_report() {
    let board = MeterBoard::new();
    let mut capture = MeterCapture::with_recorder("/nonexistent/pw-cat");
    let uid = "in-mic";
    let source = "Holaf_Bus_Mic.monitor";
    capture_until(&mut capture, &board, uid, source, |capture| {
        !capture.stats().summary().is_empty()
    });
    let report = report_for(&capture, uid, source);
    let text = report.text();
    if std::env::var_os("HOLAF_REPORT_DUMP").is_some() {
        eprintln!("{text}");
    }
    capture.stop_all();

    assert!(
        text.contains("CAPTURES"),
        "the report must carry the capture section:\n{text}"
    );
    assert!(
        text.contains("source=Holaf_Bus_Mic.monitor -> target Holaf_Bus_Mic (sink monitor)"),
        "{text}"
    );
    assert!(text.contains("pid none · failed"), "{text}");
    assert!(text.contains("/nonexistent/pw-cat"), "{text}");
    assert!(text.contains("No such file"), "{text}");
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.id == "metering_dead")
        .unwrap_or_else(|| panic!("no metering_dead:\n{text}"));
    assert_eq!(finding.severity, Severity::Error);
    assert!(
        finding
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("could not be started"),
        "{finding:?}"
    );
}

/// **Case B — the recorder writes s16le (PipeWire 1.6, no `--raw`).**  The
/// reader detects the format from the bytes and **converts** every block, so the
/// VU works — and the report must say *which* format it detected and converted,
/// not that blocks were dropped (the user's report: “all levels 0.000” on a
/// machine whose pw-cat wrote s16le despite `--format f32`).
#[test]
fn an_s16le_stream_is_detected_converted_and_described_by_the_report() {
    // A −20 dBFS sine, encoded as interleaved s16le (what a pw-cat that ignores
    // `--format f32` writes).  10 ms of stereo at 2 B/sample = 1920 bytes.
    let mut payload = Vec::new();
    for n in 0..480 {
        let sample = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
        let value = (sample * 32_767.0) as i16;
        payload.extend_from_slice(&value.to_le_bytes());
        payload.extend_from_slice(&value.to_le_bytes());
    }
    let board = MeterBoard::new();
    let mut capture = MeterCapture::with_recorder(recorder_repeating("s16", &payload));
    let uid = "in-mic";
    let source = "Holaf_Bus_Mic.monitor";
    // Wait until the format is detected and blocks are drawn.
    capture_until(&mut capture, &board, uid, source, |capture| {
        capture
            .stats()
            .summary()
            .sources
            .first()
            .is_some_and(|facts| facts.blocks > 0 && facts.detected_format.is_some())
    });
    let level = board.read(uid).unwrap_or([0.0, 0.0]);
    let report = report_for_live_level(&capture, uid, source, level);
    let text = report.text();
    capture.stop_all();

    // The report names the detected format and says it was converted.
    assert!(
        text.contains("detected s16le and converted"),
        "the CAPTURES section must say what it detected:\n{text}"
    );
    assert!(text.contains("s16le"), "{text}");
    // Nothing was refused: a convertible stream is not a failure any more.
    assert!(
        !text.contains("implausible"),
        "a converted stream has no rejected blocks:\n{text}"
    );
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.id == "metering_implausible"),
        "no metering_implausible for a converted stream:\n{text}"
    );
    // …and the VU actually moves (the whole point): a −20 dBFS sine ≈ 0.56.
    assert!(
        (level[0] - 0.56).abs() < 0.05,
        "the converted stream must feed the bar (level {level:?}):\n{text}"
    );
    // The report's METERING section shows the converted level on Mic (the only
    // captured strip; Casque has no capture, so it legitimately reads 0.000).
    //
    // The expected string is built from the level the board actually holds, not
    // from a constant: the value is printed with 3 decimals, so pinning
    // `"L=0.560"` would depend on the last digit of a float (0.5604 today) and
    // fail on a machine that happens to read 0.5599 — while the assertion we
    // *want* is “the METERING section shows the bar's level, not 0.000”.  The
    // numeric guard above is what pins the value.
    assert!(
        text.contains(&format!("L={:.3} R={:.3}", level[0], level[1])),
        "the METERING section must show the converted level {level:?}:\n{text}"
    );
}

/// **Case B′ — the same s16le stream, but read as *denormals*.**  This is the
/// shape the “VU à zéro” report really had: an s16 block whose `f32` reading is
/// **not** implausible (measured peak 4.9e-30, RMS 1.4e-30 — the loudness guard
/// never fires), so it used to be accepted as `f32le` and drawn at exactly
/// 0.000.  The report must show the detected format *and* a non-zero level.
#[test]
fn a_denormal_s16le_stream_is_detected_converted_and_described_by_the_report() {
    // A −20 dBFS-RMS tone as interleaved s16le whose samples all stay **positive**
    // (a DC-offset sine): every high byte is ≤ 0x0F, so reading 4 bytes at a time
    // yields denormals instead of huge values.
    let offset = 0.0707_f32 / 1.5_f32.sqrt();
    let mut payload = Vec::new();
    for n in 0..480 {
        let sample = offset
            * (1.0 + (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin());
        let value = (sample * 32_767.0) as i16;
        payload.extend_from_slice(&value.to_le_bytes());
        payload.extend_from_slice(&value.to_le_bytes());
    }
    let board = MeterBoard::new();
    let mut capture = MeterCapture::with_recorder(recorder_repeating("s16-denormal", &payload));
    let uid = "in-mic";
    let source = "Holaf_Bus_Mic.monitor";
    capture_until(&mut capture, &board, uid, source, |capture| {
        capture
            .stats()
            .summary()
            .sources
            .first()
            .is_some_and(|facts| facts.blocks > 0 && facts.detected_format.is_some())
    });
    let level = board.read(uid).unwrap_or([0.0, 0.0]);
    let summary = capture.stats().summary();
    let report = report_for_live_level(&capture, uid, source, level);
    let text = report.text();
    if std::env::var_os("HOLAF_REPORT_DUMP").is_some() {
        eprintln!("{text}");
    }
    capture.stop_all();

    assert!(
        text.contains("detected s16le and converted"),
        "the CAPTURES section must say what it detected:\n{text}"
    );
    // The RMS is the converted one (−20 dBFS ≈ 0.0707), not the 1e-30 of the
    // misread — this is what tells “converted” from “read as silence”.
    let rms = summary.sources[0].last_rms.unwrap_or_default();
    assert!(
        (rms[0] - 0.0707).abs() < 5e-3,
        "the report must carry the converted RMS, got {rms:?}\n{text}"
    );
    assert_eq!(summary.rejected, 0, "nothing is refused: {summary:?}");
    // …and the VU moves: the very symptom was “L=0.000 R=0.000”.  Same rule as
    // above: the string is derived from the level the board holds.
    assert!(
        text.contains(&format!("L={:.3} R={:.3}", level[0], level[1])),
        "the METERING section must show the converted level {level:?}:\n{text}"
    );
    assert!(
        !report.findings.iter().any(|finding| finding.id == "metering_implausible"
            || finding.id == "metering_silent"),
        "no failure finding for a working stream:\n{text}"
    );
}

/// **Case C — the recorder dies immediately** (`pw-cat` refusing its `--target`,
/// a device that vanished).  The report says the stream ended and why.
#[test]
fn a_recorder_that_exits_at_once_is_described_by_the_report() {
    let board = MeterBoard::new();
    // `true` exits 0 without a byte, exactly like a refused target.
    let mut capture = MeterCapture::with_recorder("true");
    let uid = "out-casque";
    let source = "alsa_output.usb-PreSonus.analog-surround-40.monitor";
    capture_until(&mut capture, &board, uid, source, |capture| {
        capture
            .stats()
            .summary()
            .sources
            .first()
            .is_some_and(|facts| facts.dead())
    });
    let report = report_for(&capture, uid, source);
    let text = report.text();
    capture.stop_all();

    assert!(text.contains("uid=out-casque"), "{text}");
    assert!(
        text.contains("source=alsa_output.usb-PreSonus.analog-surround-40.monitor")
            && text.contains("target alsa_output.usb-PreSonus.analog-surround-40"),
        "{text}"
    );
    assert!(text.contains("· ended ·"), "{text}");
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.id == "metering_dead")
        .unwrap_or_else(|| panic!("no metering_dead:\n{text}"));
    assert!(
        finding
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("ended as soon as it started"),
        "{finding:?}"
    );
}

/// **Case D — the nominal chain.**  A real f32le block (a −20 dBFS sine) is drawn
/// and counted, and the report says so: blocks accepted, last RMS/peak, no
/// finding at all.  Without this case every assertion above could be satisfied by
/// a chain that *never* works.
#[test]
fn a_working_capture_is_counted_and_raises_nothing() {
    let mut payload = Vec::new();
    for n in 0..480 {
        let sample = 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / 48_000.0).sin();
        payload.extend_from_slice(&sample.to_le_bytes());
        payload.extend_from_slice(&sample.to_le_bytes());
    }
    let board = MeterBoard::new();
    let mut capture = MeterCapture::with_recorder(recorder_repeating("tone", &payload));
    let uid = "in-mic";
    let source = "Holaf_Bus_Mic.monitor";
    capture_until(&mut capture, &board, uid, source, |capture| {
        capture.stats().summary().blocks > 0
    });
    let summary = capture.stats().summary();
    let report = report_for(&capture, uid, source);
    capture.stop_all();

    assert!(summary.blocks > 0, "{summary:?}");
    assert_eq!(summary.rejected, 0, "{summary:?}");
    let text = report.text();
    assert!(text.contains("pid "), "{text}");
    assert!(text.contains("· streaming ·"), "{text}");
    assert!(text.contains("last rms 0.0707"), "{text}");
    // The recorded RMS is −23 dBFS → 0.56 on the bar; the report prints the
    // level the capture would draw (`METERING` is the *drawn* envelope, which is
    // 0.000 here because nothing polls the board in this test).
    //
    // The block count is read back **by value** (see [`accepted_blocks_in`]).
    // The report is built from a snapshot taken after `summary`, so its count can
    // only be equal or larger — and it must never be 0 on a working chain.
    let accepted = accepted_blocks_in(&text);
    assert!(
        accepted > 0,
        "the CAPTURES section must not report 0 accepted blocks:\n{text}"
    );
    assert!(
        accepted >= summary.blocks,
        "the report ({accepted}) must not under-count the decoded blocks ({}):\n{text}",
        summary.blocks
    );
    for id in ["metering_dead", "metering_implausible", "metering_pinned"] {
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.id == id),
            "{id} must not fire on a working chain:\n{text}"
        );
    }
}

/// **Case E — the recorder is a bare name that is not in `PATH`.**  This is the
/// Arch case (`pw-cat` ships with `pipewire-audio`, not with the base
/// `pipewire`), and it must be described exactly like an explicit path: the
/// report names the command and the OS error.
#[test]
fn a_recorder_missing_from_path_is_described_by_the_report() {
    let board = MeterBoard::new();
    let mut capture = MeterCapture::with_recorder("holaf-no-such-pw-cat");
    let uid = "in-mic";
    let source = "Holaf_Bus_Mic.monitor";
    capture_until(&mut capture, &board, uid, source, |capture| {
        !capture.stats().summary().is_empty()
    });
    let report = report_for(&capture, uid, source);
    let text = report.text();
    capture.stop_all();
    assert!(text.contains("holaf-no-such-pw-cat"), "{text}");
    assert!(text.contains("pid none · failed"), "{text}");
    let finding = report
        .findings
        .iter()
        .find(|finding| finding.id == "metering_dead")
        .expect("metering_dead");
    assert!(
        finding
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("holaf-no-such-pw-cat: No such file"),
        "{finding:?}"
    );
}

/// **The nominal chain, with a real recorder** (opt-in): a tone played into a
/// sink must show up as a numbered, non-zero level in the report — the case that
/// proves the whole section is not just describing failures.
///
/// Skipped unless both variables are set, so `cargo test` stays hermetic:
///
/// ```bash
/// eval "$(./scripts/start-pipewire-headless.sh --export)"
/// pw-cli create-node adapter '{ factory.name=support.null-audio-sink \
///   node.name=Holaf_Test_Sink media.class=Audio/Sink object.linger=1 \
///   audio.position=[FL FR] }'
/// pw-cat --playback --target Holaf_Test_Sink /tmp/tone.wav &
/// HOLAF_METER_LIVE_SINK=Holaf_Test_Sink cargo test -p holaf-ui \
///   --test metering_report a_live_recorder -- --nocapture
/// ```
#[test]
fn a_live_recorder_is_counted_with_a_real_level() {
    let Ok(sink) = std::env::var("HOLAF_METER_LIVE_SINK") else {
        eprintln!("SKIP a_live_recorder_is_counted_with_a_real_level: set HOLAF_METER_LIVE_SINK");
        return;
    };
    // Something to record: a tone into the sink, played by `pw-cat` itself.
    let wav = std::env::var("HOLAF_METER_LIVE_WAV").unwrap_or_else(|_| "/tmp/tone.wav".to_string());
    let mut player = std::process::Command::new(holaf_ui::capture::default_recorder())
        .args(["--playback", "--target", &sink, &wav])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("playback child");
    std::thread::sleep(Duration::from_millis(500));

    let board = MeterBoard::new();
    let mut capture = MeterCapture::new();
    let uid = "in-mic";
    let source = format!("{sink}.monitor");
    // Wait for a *non-silent* block: the stream opens (and delivers zeros)
    // before the playback child has been linked to the sink, so "blocks > 0" is
    // not enough to prove the chain carries audio.
    capture_until(&mut capture, &board, uid, &source, |capture| {
        capture
            .stats()
            .summary()
            .sources
            .first()
            .and_then(|facts| facts.last_rms)
            .is_some_and(|rms| rms[0] > 0.005)
    });
    let summary = capture.stats().summary();
    let report = report_for(&capture, uid, &source);
    let text = report.text();
    if std::env::var_os("HOLAF_REPORT_DUMP").is_some() {
        eprintln!("{text}");
    }
    capture.stop_all();
    let _ = player.kill();
    let _ = player.wait();

    let facts = &summary.sources[0];
    assert!(facts.blocks > 0, "{facts:?}");
    let rms = facts.last_rms.expect("a block was decoded")[0];
    assert!(rms > 0.01, "the monitor of {sink} recorded silence: {facts:?}");
    assert!(text.contains("· streaming ·"), "{text}");
    // By value, not by substring (see [`accepted_blocks_in`]).
    let accepted = accepted_blocks_in(&text);
    assert!(
        accepted > 0,
        "the CAPTURES section must not report 0 accepted blocks:\n{text}"
    );
    assert!(
        accepted >= summary.blocks,
        "the report ({accepted}) must not under-count the decoded blocks ({}):\n{text}",
        summary.blocks
    );
    for id in ["metering_dead", "metering_implausible"] {
        assert!(
            !report.findings.iter().any(|finding| finding.id == id),
            "{id} on a live chain:\n{text}"
        );
    }
}
