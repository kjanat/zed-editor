use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use client::{
    Client,
    sentry::{EventLevel, SentryEvent},
    telemetry::Telemetry,
};
use gpui::{AppContext, TasksIncluded, profiler};
use hang_telemetry::{HangTelemetry, ObservedHangIncident};
use ui::App;

use crate::STARTUP_TIME;

mod logging;
mod task_traces;

gpui::actions!(
    dev,
    [
        /// Causes a performance hang to test performance monitoring
        HangAction,
        /// Causes a performance hang to test performance monitoring
        HangBackground,
        /// Causes a performance hang to test performance monitoring
        HangForeground,
    ]
);

pub(crate) fn start(client: Arc<Client>, cx: &mut App) {
    let hang_time = hang_telemetry::hang_threshold();

    if cfg!(debug_assertions) {
        log::warn!("debug build, only reporting hangs longer then {hang_time:?}");
    }

    start_hang_detection(hang_time, client, cx);

    cx.on_action(move |_: &HangAction, _| {
        log::warn!(
            "Hanging the foreground for {hang_time:?} by blocking in an action. \
            Zed will be unresponsive for that time. This should trigger a report in the log",
        );
        thread::sleep(hang_time + Duration::from_micros(1));
        log::warn!("Hang ended");
    });
    cx.on_action(move |_: &HangBackground, cx| {
        cx.background_spawn(async move {
            log::warn!(
                "Hanging one background executor for {hang_time:?}. \
                This should trigger a report in the log",
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
    cx.on_action(move |_: &HangForeground, cx| {
        cx.spawn(async move |_| {
            log::warn!(
                "Hanging the foreground executor for {hang_time:?} seconds to test \
                performance monitoring! Zed will be unresponsive for that time. \
                This should trigger a report in the log"
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
}

fn start_hang_detection(report_longer_then: Duration, client: Arc<Client>, cx: &mut App) {
    let foreground_thread = thread::current().id();
    let monitor_interval = Duration::from_secs(1);
    let started = Instant::now();
    let startup = *STARTUP_TIME.get().unwrap_or(&started);
    // GPUI's final `Flush` poll runs during shutdown, concurrently with this
    // handler and within `SHUTDOWN_TIMEOUT`, so the last batch may miss this
    // flush.
    let hang_telemetry =
        HangTelemetry::new(startup, telemetry::send_event).with_incident_observer({
            let telemetry = client.telemetry().clone();
            move |incidents| report_hangs(&telemetry, incidents)
        });
    match hang_telemetry.start(cx) {
        Ok(()) => cx
            .on_app_quit(move |_| client.telemetry().flush_events())
            .detach(),
        Err(error) => log::error!("failed to start hang reporting: {error}"),
    }

    let mut log = logging::Reporter::new(monitor_interval, report_longer_then, foreground_thread);
    // An OS thread keeps the legacy hang logs and task traces working while
    // the foreground or background executors are hung.
    thread::Builder::new()
        .name("HangLogging".to_string())
        .spawn(move || {
            // allow "bad" tasks during startup. Not because we should but since here
            // they are not observed by the user and to lower on clutter from the reporter
            thread::sleep(Duration::from_millis(200));
            loop {
                thread::sleep(monitor_interval);
                let task_stats = profiler::take_all_stats(TasksIncluded::CompletedAndRunning);
                let action_stats = profiler::take_action_stats();

                let should_write_trace = log.check_and_report(&task_stats, &action_stats);
                if should_write_trace {
                    if let Some(path) = task_traces::save_any(foreground_thread) {
                        log::info!("Task trace has been saved to: {}", path.display());
                    }
                }
            }
        })
        .expect("App can always spawn threads");
}

fn report_hangs(telemetry: &Arc<Telemetry>, incidents: &[ObservedHangIncident]) {
    for incident in incidents {
        telemetry.report_diagnostic_event(hang_event(incident));
    }
}

fn hang_event(observed: &ObservedHangIncident) -> SentryEvent {
    let incident = &observed.incident;
    let top_contributor = observed.top_contributor.as_deref().unwrap_or("unknown");
    let trigger = incident.trigger.as_str();
    let mut event = SentryEvent::new(EventLevel::Warning, format!("Hang: {top_contributor}"));
    event.fingerprint = vec![trigger.to_string(), top_contributor.to_string()];
    event.tags = BTreeMap::from([
        ("trigger".to_string(), trigger.to_string()),
        ("phase".to_string(), incident.phase.to_string()),
        ("sealed_by".to_string(), incident.sealed_by.to_string()),
        (
            "during_active_use".to_string(),
            incident.during_active_use.to_string(),
        ),
        ("binary".to_string(), "zed".to_string()),
        (
            "hang.top_contributor".to_string(),
            top_contributor.to_string(),
        ),
    ]);
    match serde_json::to_value(incident) {
        Ok(context) => {
            event.contexts.insert("hang".to_string(), context);
        }
        Err(error) => log::error!("failed to serialize hang incident: {error}"),
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::sentry::{SentryDsn, TEST_DSN, recording_http_client};
    use gpui::TestAppContext;
    use gpui::profiler::hang::{HangTrigger, MEASUREMENT_VERSION, SerializedHangIncident};
    use serde_json::json;
    use settings::SettingsStore;

    fn observed(top_contributor: Option<&str>, stall_ms: f64) -> ObservedHangIncident {
        ObservedHangIncident {
            top_contributor: top_contributor.map(str::to_string),
            incident: SerializedHangIncident {
                measurement_version: MEASUREMENT_VERSION,
                phase: "steady",
                trigger: HangTrigger::LateFrame,
                during_active_use: true,
                start_ms: 1000.0,
                active_ms: stall_ms,
                stall_ms,
                dirty_to_present_ms: Some(stall_ms),
                sealed_by: "present",
                busy_fraction: 0.9,
                event_count: 3,
                small_poll_count: 0,
                small_poll_total_ms: 0.0,
                dropped_events: 0,
                journal_discontinuous: false,
                contributors: Vec::new(),
                contributors_elided: 0,
            },
        }
    }

    #[test]
    fn hang_events_group_by_trigger_and_top_contributor() {
        let event = hang_event(&observed(Some("action:editor::Paste"), 320.5));

        assert_eq!(event.level, EventLevel::Warning);
        assert_eq!(
            event.logentry.map(|entry| entry.formatted).as_deref(),
            Some("Hang: action:editor::Paste")
        );
        assert_eq!(event.fingerprint, ["late_frame", "action:editor::Paste"]);
        assert_eq!(
            event.tags,
            BTreeMap::from(
                [
                    ("trigger", "late_frame"),
                    ("phase", "steady"),
                    ("sealed_by", "present"),
                    ("during_active_use", "true"),
                    ("binary", "zed"),
                    ("hang.top_contributor", "action:editor::Paste"),
                ]
                .map(|(name, value)| (name.to_string(), value.to_string()))
            )
        );
        let context = &event.contexts["hang"];
        assert_eq!(context["stall_ms"], json!(320.5));
        assert_eq!(context["trigger"], json!("late_frame"));
        assert_eq!(context["sealed_by"], json!("present"));
    }

    #[test]
    fn hang_events_without_contributors_are_named_unknown() {
        let event = hang_event(&observed(None, 0.0));
        assert_eq!(event.fingerprint, ["late_frame", "unknown"]);
        assert_eq!(
            event.tags.get("hang.top_contributor").map(String::as_str),
            Some("unknown")
        );
    }

    #[gpui::test]
    async fn every_observed_hang_goes_to_sentry(cx: &mut TestAppContext) {
        let (http, requests) = recording_http_client();
        let telemetry = cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            Telemetry::new(
                Arc::new(clock::FakeSystemClock::new()),
                http,
                Some(SentryDsn::parse(TEST_DSN).unwrap()),
                cx,
            )
        });

        report_hangs(
            &telemetry,
            &[
                observed(Some("action:editor::Paste"), 320.5),
                observed(Some("draw"), 150.0),
            ],
        );
        cx.run_until_parked();

        let requests = requests.lock();
        assert_eq!(requests.len(), 2);
        let messages: Vec<_> = requests
            .iter()
            .map(|request| request.envelope_lines()[2]["logentry"]["formatted"].clone())
            .collect();
        assert!(messages.contains(&json!("Hang: action:editor::Paste")));
        assert!(messages.contains(&json!("Hang: draw")));
        for request in requests.iter() {
            assert_eq!(request.envelope_lines()[1]["type"], json!("event"));
        }
    }
}
