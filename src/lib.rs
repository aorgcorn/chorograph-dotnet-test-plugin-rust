// chorograph-dotnet-test-plugin-rust
//
// WASM plugin that runs `dotnet test`, parses its output line-by-line, and
// emits normalized test events to the Chorograph Swift host via
// `host_push_ai_event`.
//
// Normalized event protocol (sent as raw JSON strings, NOT wrapped in AIEvent):
//   {"type":"testRunStarted","framework":"dotnet-xunit","projectPath":"<cwd>"}
//   {"type":"testResult","outcome":"passed"|"failed"|"skipped",
//    "testClass":"Ns.ClassName","testName":"MethodName",
//    "duration":0.042,"message":"optional failure message"}
//   {"type":"testRunCompleted","passed":N,"failed":N,"skipped":N,"duration":S}
//
// The host's TelemetryManager.onAiEvent parses these directly.

use chorograph_plugin_sdk_rust::prelude::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// Plugin lifecycle
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn init() {
    let ui = json!([
        { "type": "label", "text": "dotnet test" },
        { "type": "button", "text": "Run Tests", "action": "run_tests" }
    ]);
    push_ui(&ui.to_string());
}

// ---------------------------------------------------------------------------
// Action handler
// ---------------------------------------------------------------------------

#[chorograph_plugin]
pub fn handle_action(action_id: String, payload: serde_json::Value) {
    if action_id != "run_tests" {
        return;
    }

    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("dotnet-test")
        .to_string();

    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let extra_args: Vec<String> = payload
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    run_dotnet_test(&session_id, cwd.as_deref(), &extra_args);
}

// ---------------------------------------------------------------------------
// Core runner
// ---------------------------------------------------------------------------

fn run_dotnet_test(session_id: &str, cwd: Option<&str>, extra_args: &[String]) {
    // Build args: ["test", "--verbosity", "detailed", ...extra_args]
    // We use "detailed" (not "normal") so that xUnit v3 / .NET 10 emits individual
    // per-test result lines.  With "normal", xUnit v3 only prints a summary.
    let mut args: Vec<&str> = vec!["test", "--verbosity", "detailed"];
    let extra_refs: Vec<&str> = extra_args.iter().map(|s| s.as_str()).collect();
    args.extend(extra_refs.iter());

    let project_path = cwd.unwrap_or(".");

    log!(
        "[dotnet-test] run_dotnet_test: session_id={} cwd={:?}",
        session_id,
        cwd
    );

    // Emit testRunStarted
    let started_json = json!({
        "type": "testRunStarted",
        "framework": "dotnet-xunit",
        "projectPath": project_path
    })
    .to_string();
    log!("[dotnet-test] emitting testRunStarted: {}", started_json);
    push_raw_event(session_id, &started_json);

    log!(
        "[dotnet-test] spawning: dotnet test --verbosity normal (cwd={:?})",
        cwd
    );
    let child = match ChildProcess::spawn("dotnet", args, cwd, std::collections::HashMap::new()) {
        Ok(c) => {
            log!("[dotnet-test] spawn succeeded");
            c
        }
        Err(e) => {
            log!("[dotnet-test] spawn FAILED: {:?}", e);
            let err_json = json!({
                "type": "testRunCompleted",
                "passed": 0,
                "failed": 0,
                "skipped": 0,
                "duration": 0.0,
                "error": "Failed to spawn dotnet test"
            })
            .to_string();
            push_raw_event(session_id, &err_json);
            return;
        }
    };

    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    let mut parser = DotnetTestParser::new();
    let start_ms = now_ms();
    let mut loop_iters: u32 = 0;

    // Single streaming loop.  We exit when any of these is true:
    //   1. ReadResult::EOF  — pipe closed cleanly (no MSBuild workers)
    //   2. parser.run_complete — we've seen "Test Run Successful/Failed." or
    //      the counts summary line; no more result lines will follow
    //   3. get_status() non-Running — main dotnet process has exited
    //
    // Strategy (2) is the primary path for dotnet test because:
    //   - MSBuild workers keep the pipe open long after dotnet prints its summary
    //   - get_status() tracks the main dotnet process but dotnet itself waits
    //     for its workers before exiting, so it can stay "Running" for seconds
    //     after all output has been printed
    loop {
        loop_iters += 1;
        if loop_iters % 10 == 1 {
            log!(
                "[dotnet-test] read loop iter={} run_complete={} parsed(p={} f={} s={})",
                loop_iters,
                parser.run_complete,
                parser.passed,
                parser.failed,
                parser.skipped
            );
        }
        child.wait_for_data(200);

        // Drain stderr — also check for "Test Run Failed/Successful." which
        // dotnet test writes to stderr (not stdout) in some configurations.
        loop {
            match child.read(PipeType::Stderr) {
                Ok(ReadResult::Data(data)) => {
                    stderr_buf.extend(&data);
                    // Scan newly received stderr bytes for the run-complete signal.
                    let text = String::from_utf8_lossy(&data);
                    for line in text.lines() {
                        let t = line.trim().to_lowercase();
                        if t.starts_with("test run successful") || t.starts_with("test run failed")
                        {
                            parser.run_complete = true;
                        }
                    }
                }
                _ => break,
            }
        }

        // Drain stdout and process complete lines.
        let mut got_eof = false;
        loop {
            match child.read(PipeType::Stdout) {
                Ok(ReadResult::Data(data)) => {
                    stdout_buf.extend(&data);
                    process_lines(&mut stdout_buf, &mut parser, session_id);
                }
                Ok(ReadResult::EOF) => {
                    got_eof = true;
                    break;
                }
                _ => break,
            }
        }

        if got_eof || parser.run_complete {
            log!(
                "[dotnet-test] exiting read loop: got_eof={} run_complete={} iters={}",
                got_eof,
                parser.run_complete,
                loop_iters
            );
            break;
        }

        // Fallback: exit once the main process has exited (covers edge cases
        // where the summary line is never printed, e.g. build errors).
        match child.get_status() {
            ProcessStatus::Running => {}
            s => {
                log!(
                    "[dotnet-test] process exited (status={:?}), breaking read loop after {} iters",
                    s,
                    loop_iters
                );
                break;
            }
        }
    }

    // Short post-summary drain: pick up any lines that arrived in the same
    // batch as the summary (e.g. "Total time: N Seconds").
    for _ in 0..5 {
        child.wait_for_data(100);
        loop {
            match child.read(PipeType::Stdout) {
                Ok(ReadResult::Data(data)) => {
                    stdout_buf.extend(&data);
                    process_lines(&mut stdout_buf, &mut parser, session_id);
                }
                Ok(ReadResult::EOF) => break,
                _ => break,
            }
        }
    }

    flush_partial(&mut stdout_buf, &mut parser, session_id);
    // Flush any pending failure that was still waiting for a boundary line
    // (e.g. when "Test Run Failed." came before the last failure's message block
    // was closed by a subsequent result or summary line).
    for event_json in parser.flush_pending() {
        push_raw_event(session_id, &event_json);
    }
    let elapsed_secs = (now_ms() - start_ms) as f64 / 1000.0;
    log!(
        "[dotnet-test] run finished: passed={} failed={} skipped={} elapsed={:.3}s",
        parser.passed,
        parser.failed,
        parser.skipped,
        elapsed_secs
    );
    emit_completed(&parser, elapsed_secs, session_id);

    let _ = stderr_buf;
}

/// Scan `buf` for complete newline-terminated lines, call the parser on each,
/// and emit any resulting events. Leaves a partial (non-terminated) tail in `buf`.
fn process_lines(buf: &mut Vec<u8>, parser: &mut DotnetTestParser, session_id: &str) {
    loop {
        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line_bytes).to_string();
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                log!("[dotnet-test] stdout: {}", trimmed);
            }
            push_log_event(session_id, &line);
            let events = parser.feed_line(&line);
            if !events.is_empty() {
                log!(
                    "[dotnet-test] parser produced {} event(s) for line: {}",
                    events.len(),
                    trimmed
                );
            }
            for event_json in events {
                push_raw_event(session_id, &event_json);
            }
        } else {
            break;
        }
    }
}

/// Emit a single raw stdout line as a testLog event.
fn push_log_event(session_id: &str, line: &str) {
    let trimmed = line.trim_end_matches(|c| c == '\n' || c == '\r');
    if trimmed.is_empty() {
        return;
    }
    let log_json = json!({
        "type": "testLog",
        "line": trimmed
    })
    .to_string();
    push_raw_event(session_id, &log_json);
}

/// Flush any partial (non-newline-terminated) tail remaining in `buf`.
fn flush_partial(buf: &mut Vec<u8>, parser: &mut DotnetTestParser, session_id: &str) {
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(buf).to_string();
        buf.clear();
        push_log_event(session_id, &line);
        for event_json in parser.feed_line(&line) {
            push_raw_event(session_id, &event_json);
        }
    }
}

/// Emit the testRunCompleted event.
fn emit_completed(parser: &DotnetTestParser, elapsed_secs: f64, session_id: &str) {
    let completed_json = json!({
        "type": "testRunCompleted",
        "passed":  parser.passed,
        "failed":  parser.failed,
        "skipped": parser.skipped,
        "duration": elapsed_secs
    })
    .to_string();
    push_raw_event(session_id, &completed_json);
}

// ---------------------------------------------------------------------------
// Raw-event helper (bypasses the typed AIEvent enum)
// ---------------------------------------------------------------------------

fn push_raw_event(session_id: &str, json: &str) {
    log!(
        "[dotnet-test] push_raw_event session={} json={}",
        session_id,
        json
    );
    unsafe {
        chorograph_plugin_sdk_rust::ffi::host_push_ai_event(
            session_id.as_ptr(),
            session_id.len() as i32,
            json.as_ptr(),
            json.len() as i32,
        );
    }
}

// ---------------------------------------------------------------------------
// Simple monotonic timestamp (ms) using host_wait_for_data as a proxy.
// WASM has no clock — we use std::time only when the target is native.
// ---------------------------------------------------------------------------

fn now_ms() -> u64 {
    // In a WASM32 environment without WASI, std::time is unavailable.
    // We track time via a zero-wait probe that returns instantly — this gives
    // us a rough elapsed count in tight loops. For the final duration we rely
    // on the host to have recorded the actual wall time via the testRunCompleted
    // event; we pass 0.0 as a fallback if timing is unavailable.
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    {
        0 // WASM32 without WASI: no wall clock
    }
}

// ---------------------------------------------------------------------------
// dotnet test output parser
// ---------------------------------------------------------------------------
//
// dotnet test --verbosity normal emits lines in these formats:
//
// xUnit:
//   "  Passed  MyNamespace.FooTests.SomeMethod [12 ms]"
//   "  Failed  MyNamespace.FooTests.OtherMethod [5 ms]"
//   "  Skipped MyNamespace.FooTests.SkipMethod"
//   "    Error Message:"
//   "      System.Exception : boom"
//
// MSTest:
//   "  Passed SomeName"
//   "  Failed SomeName"
//   "   Standard Output Messages:"
//   "     Error: ..."
//
// NUnit (via dotnet test adapter):
//   "Passed!  - Failed:     0, Passed:    10, Skipped:    0, ..."
//   "Failed!  - Failed:     2, Passed:     8, Skipped:    1, ..."
//   Individual test lines similar to xUnit.
//
// Summary line (all frameworks):
//   "Test Run Successful."
//   "Test Run Failed."
//   "Failed:     3, Passed:    10, Skipped:     0, Total:    13, Duration: 245 ms"

struct DotnetTestParser {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    /// Set to true when "Test Run Successful/Failed." or the counts summary line
    /// is seen — signals that no more result lines will follow.
    pub run_complete: bool,
    /// Pending failure info for a multi-line failure block
    current_failure: Option<PendingFailure>,
}

struct PendingFailure {
    test_class: String,
    test_name: String,
    duration: f64,
    message: Vec<String>,
    in_error_block: bool,
}

impl DotnetTestParser {
    fn new() -> Self {
        Self {
            passed: 0,
            failed: 0,
            skipped: 0,
            run_complete: false,
            current_failure: None,
        }
    }

    /// Flush any pending failure that hasn't been emitted yet.
    /// Call this after the last line has been processed and before emitting testRunCompleted.
    fn flush_pending(&mut self) -> Vec<String> {
        if let Some(pf) = self.current_failure.take() {
            let msg = pf.message.join(" ").trim().to_string();
            let event = make_test_event(
                "failed",
                &pf.test_class,
                &pf.test_name,
                pf.duration,
                Some(&msg),
            );
            vec![event]
        } else {
            vec![]
        }
    }

    /// Feed one line and return zero, one, or two JSON event strings that are ready to emit.
    /// Returns two events when flushing a pending failure block AND the triggering line
    /// is itself a result line (e.g. two consecutive test result lines with no gap).
    fn feed_line(&mut self, raw: &str) -> Vec<String> {
        let line = raw.trim_end_matches(|c| c == '\n' || c == '\r');

        // ----------------------------------------------------------------
        // Flush a pending failure block when we see the next test line or
        // a summary/blank line boundary.
        // ----------------------------------------------------------------
        let flush_on = is_result_line(line) || is_summary_line(line) || line.trim().is_empty();

        if flush_on {
            if let Some(pf) = self.current_failure.take() {
                let msg = pf.message.join(" ").trim().to_string();
                let flush_event = make_test_event(
                    "failed",
                    &pf.test_class,
                    &pf.test_name,
                    pf.duration,
                    Some(&msg),
                );
                // Also parse the triggering line — may produce a second event.
                let mut events = vec![flush_event];
                if let Some(next_event) = self.parse_line(line) {
                    events.push(next_event);
                }
                return events;
            }
        }

        self.parse_line(line).into_iter().collect()
    }

    fn parse_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        // Normalise to lowercase for prefix matching so we handle both
        // xUnit v2 ("Passed  Ns.Class.Method") and xUnit v3 ("passed Ns.Class.Method").
        let lower = trimmed.to_lowercase();

        // Helper: strip a prefix (lowercase match) and return the remainder
        // from the *original* trimmed string (preserving casing of the identifier).
        fn strip_ci<'a>(trimmed: &'a str, lower: &str, prefix: &str) -> Option<&'a str> {
            if lower.starts_with(prefix) {
                Some(trimmed[prefix.len()..].trim_start())
            } else {
                None
            }
        }

        // ── xUnit / MSTest individual test result lines ─────────────────
        if let Some(rest) =
            strip_ci(trimmed, &lower, "passed  ").or_else(|| strip_ci(trimmed, &lower, "passed "))
        {
            if !looks_like_test_identifier(rest) {
                return None;
            }
            let (class, name, dur) = split_test_identifier(rest);
            self.passed += 1;
            return Some(make_test_event("passed", &class, &name, dur, None));
        }

        if let Some(rest) =
            strip_ci(trimmed, &lower, "failed  ").or_else(|| strip_ci(trimmed, &lower, "failed "))
        {
            if !looks_like_test_identifier(rest) {
                return None;
            }
            let (class, name, dur) = split_test_identifier(rest);
            self.failed += 1;
            // Start a pending failure block to collect the error message.
            self.current_failure = Some(PendingFailure {
                test_class: class,
                test_name: name,
                duration: dur,
                message: Vec::new(),
                in_error_block: false,
            });
            return None; // wait for message lines
        }

        if let Some(rest) =
            strip_ci(trimmed, &lower, "skipped  ").or_else(|| strip_ci(trimmed, &lower, "skipped "))
        {
            if !looks_like_test_identifier(rest) {
                return None;
            }
            let (class, name, dur) = split_test_identifier(rest);
            self.skipped += 1;
            return Some(make_test_event("skipped", &class, &name, dur, None));
        }

        // ── Error message block (inside a Failed block) ──────────────────
        if let Some(ref mut pf) = self.current_failure {
            // xUnit v2: "Error Message:" / "Standard Output Messages:"
            // xUnit v3: "[FAIL]" marker
            let lower_trim = trimmed.to_lowercase();
            if trimmed == "Error Message:"
                || trimmed == "Standard Output Messages:"
                || lower_trim == "[fail]"
            {
                pf.in_error_block = true;
                return None;
            }
            if pf.in_error_block && !trimmed.is_empty() {
                // Collect up to 3 message lines
                if pf.message.len() < 3 {
                    pf.message.push(trimmed.to_string());
                }
                return None;
            }
            // End of error block: stack trace markers or xUnit v3 output markers
            if trimmed.starts_with("Stack Trace:")
                || trimmed.starts_with("at ")
                || trimmed.starts_with("[STACK]")
                || trimmed.starts_with("[OUTPUT]")
            {
                pf.in_error_block = false;
                return None;
            }
        }

        // ── Summary line: "Failed: N, Passed: N, Skipped: N, ..." ────────
        if lower.contains("failed:") && lower.contains("passed:") {
            // Parse the summary counts — these are authoritative.
            // Note: individual Passed/Failed/Skipped increments above may be
            // off if the verbosity level doesn't print all test results; the
            // summary overrides.
            if let (Some(p), Some(f), Some(s)) = (
                extract_count(&lower, "passed:"),
                extract_count(&lower, "failed:"),
                extract_count(&lower, "skipped:"),
            ) {
                self.passed = p;
                self.failed = f;
                self.skipped = s;
            }
            self.run_complete = true;
            return None;
        }

        // ── "Test Run Successful." / "Test Run Failed." ───────────────────
        if lower.starts_with("test run successful") || lower.starts_with("test run failed") {
            self.run_complete = true;
            return None;
        }

        // ── xUnit separate-line summary: "     Passed: 74" / "     Failed: 1" ──
        // These appear after "Test Run Failed/Successful." and carry authoritative counts.
        if self.run_complete {
            if let Some(rest) = lower.strip_prefix("passed:") {
                if let Ok(n) = rest.trim().parse::<u32>() {
                    self.passed = n;
                }
            } else if let Some(rest) = lower.strip_prefix("failed:") {
                if let Ok(n) = rest.trim().parse::<u32>() {
                    self.failed = n;
                }
            } else if let Some(rest) = lower.strip_prefix("skipped:") {
                if let Ok(n) = rest.trim().parse::<u32>() {
                    self.skipped = n;
                }
            }
            return None;
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Parser helpers
// ---------------------------------------------------------------------------

fn is_result_line(line: &str) -> bool {
    let t = line.trim();
    let lower = t.to_lowercase();
    fn rest_after_first_space<'a>(t: &'a str, lower: &str, prefix: &str) -> Option<&'a str> {
        if lower.starts_with(prefix) {
            Some(t[prefix.len()..].trim_start())
        } else {
            None
        }
    }
    let is_passed = rest_after_first_space(t, &lower, "passed  ")
        .or_else(|| rest_after_first_space(t, &lower, "passed "))
        .map(|r| looks_like_test_identifier(r))
        .unwrap_or(false);
    let is_failed = rest_after_first_space(t, &lower, "failed  ")
        .or_else(|| rest_after_first_space(t, &lower, "failed "))
        .map(|r| looks_like_test_identifier(r))
        .unwrap_or(false);
    let is_skipped = rest_after_first_space(t, &lower, "skipped  ")
        .or_else(|| rest_after_first_space(t, &lower, "skipped "))
        .map(|r| looks_like_test_identifier(r))
        .unwrap_or(false);
    is_passed || is_failed || is_skipped
}

/// Returns true if `s` looks like a test identifier rather than prose text.
/// A valid test identifier either:
///   - contains a '.' (dotted namespace, e.g. "Ns.Class.Method [12 ms]"), or
///   - ends with a timing bracket ("[N ms]" / "[N s]").
/// This guards against matching MSBuild lines like "Failed to load prune package...".
fn looks_like_test_identifier(s: &str) -> bool {
    let s = s.trim();
    s.contains('.') || s.ends_with(']')
}

fn is_summary_line(line: &str) -> bool {
    let t = line.trim().to_lowercase();
    t.starts_with("test run ") || (t.contains("failed:") && t.contains("passed:"))
}

/// Split "Ns.Class.Method [12 ms]" into (class, method, duration_secs).
fn split_test_identifier(s: &str) -> (String, String, f64) {
    // Strip trailing " [N ms]" or " [N s]"
    let (ident, dur) = if let Some(bracket) = s.rfind('[') {
        let dur_str = s[bracket..].trim_matches(|c| c == '[' || c == ']');
        let dur_secs = parse_duration_secs(dur_str);
        (s[..bracket].trim(), dur_secs)
    } else {
        (s.trim(), 0.0)
    };

    // Split on last '.' to get class vs method
    if let Some(dot) = ident.rfind('.') {
        let class = ident[..dot].to_string();
        let method = ident[dot + 1..].to_string();
        (class, method, dur)
    } else {
        ("Unknown".to_string(), ident.to_string(), dur)
    }
}

/// Parse "12 ms" → 0.012, "1.5 s" → 1.5, etc.
fn parse_duration_secs(s: &str) -> f64 {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 2 {
        return 0.0;
    }
    let val: f64 = parts[0].parse().unwrap_or(0.0);
    match parts[1] {
        "ms" => val / 1000.0,
        "s" => val,
        "m" => val * 60.0,
        _ => val / 1000.0,
    }
}

/// Extract a number after a label like "Passed:" from a summary line.
fn extract_count(line: &str, label: &str) -> Option<u32> {
    let pos = line.find(label)?;
    let rest = line[pos + label.len()..].trim_start();
    let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num_str.parse().ok()
}

/// Build a `testResult` JSON event.
fn make_test_event(
    outcome: &str,
    test_class: &str,
    test_name: &str,
    duration: f64,
    message: Option<&str>,
) -> String {
    let mut obj = json!({
        "type": "testResult",
        "outcome": outcome,
        "testClass": test_class,
        "testName": test_name,
        "duration": duration
    });
    if let Some(msg) = message {
        if !msg.is_empty() {
            obj["message"] = json!(msg);
        }
    }
    obj.to_string()
}
