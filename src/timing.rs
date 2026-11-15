//! Job wall times. Read projections never write a heartbeat to SQLite.
use serde_json::{Value, json};

fn millis(value: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|v| v.timestamp_millis())
}
fn seconds(start: &Value, end: &Value) -> Option<f64> {
    Some((millis(end)? - millis(start)?).max(0) as f64 / 1000.)
}

pub fn measure(job: &mut Value, at: &str) {
    let live = crate::store::active(job);
    let m = &mut job["metadata"];
    if !m.is_object() {
        *m = json!({});
    }
    let end = if live {
        Value::from(at)
    } else {
        m.get("finishedAt")
            .or_else(|| m.get("completedAt"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    if m["timingVersion"] != 1 {
        // Old resourceWaitSeconds measured preparation too; it is not GPU wait.
        m["runSeconds"] = seconds(&m["startedAt"], &end).into();
        m["waitSeconds"] = Value::Null;
        m["resourceWaitSeconds"] = Value::Null;
        m["timingEstimated"] = true.into();
        m["timingMeasuredAt"] = end;
        return;
    }
    let queue_end = m.get("dequeuedAt").cloned().unwrap_or_else(|| end.clone());
    let queued = seconds(&m["queuedAt"], &queue_end);
    let resource_end = m.get("startedAt").cloned().unwrap_or_else(|| end.clone());
    let resource = if m["deviceWaitEnabled"] == false {
        Some(0.)
    } else {
        seconds(&m["dequeuedAt"], &resource_end)
    };
    let run = seconds(&m["startedAt"], &end);
    m["queueSeconds"] = queued.unwrap_or(0.).into();
    m["resourceWaitSeconds"] = resource.unwrap_or(0.).into();
    m["waitSeconds"] = (queued.unwrap_or(0.) + resource.unwrap_or(0.)).into();
    m["runSeconds"] = run.unwrap_or(0.).into();
    m["timingMeasuredAt"] = end;
}

pub fn snapshot(mut job: Value) -> Value {
    measure(&mut job, &crate::store::now());
    job
}

pub fn finish(job: &mut Value) {
    if job["metadata"]["finishedAt"].is_null() {
        job["metadata"]["finishedAt"] = crate::store::now().into();
    }
    let end = job["metadata"]["finishedAt"].as_str().unwrap().to_owned();
    measure(job, &end);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job() -> Value {
        json!({"status":"queued","metadata":{"timingVersion":1,"queuedAt":"2026-01-01T00:00:00Z"}})
    }
    #[test]
    fn queue_gpu_wait_and_processing_are_disjoint_and_terminal_frozen() {
        let mut j = job();
        measure(&mut j, "2026-01-01T00:00:05Z");
        assert_eq!(j["metadata"]["queueSeconds"], 5.);
        assert_eq!(j["metadata"]["runSeconds"], 0.);
        j["status"] = "running".into();
        j["metadata"]["dequeuedAt"] = "2026-01-01T00:00:05Z".into();
        measure(&mut j, "2026-01-01T00:00:08Z");
        assert_eq!(j["metadata"]["resourceWaitSeconds"], 3.);
        assert_eq!(j["metadata"]["runSeconds"], 0.);
        j["metadata"]["startedAt"] = "2026-01-01T00:00:08Z".into();
        measure(&mut j, "2026-01-01T00:00:10.125Z");
        assert_eq!(j["metadata"]["runSeconds"], 2.125);
        assert_eq!(j["metadata"]["waitSeconds"], 8.);
        for status in ["complete", "error", "cancelled", "interrupted"] {
            j["status"] = status.into();
            j["metadata"]["finishedAt"] = "2026-01-01T00:00:10.125Z".into();
            measure(&mut j, "2027-01-01T00:00:00Z");
            assert_eq!(j["metadata"]["runSeconds"], 2.125);
            assert_eq!(j["metadata"]["waitSeconds"], 8.);
        }
    }
    #[test]
    fn cancelled_queue_never_claims_to_have_run_and_negative_clock_is_clamped() {
        let mut j = job();
        j["status"] = "cancelled".into();
        j["metadata"]["finishedAt"] = "2026-01-01T00:00:07Z".into();
        measure(&mut j, "2026-02-01T00:00:00Z");
        assert_eq!(j["metadata"]["runSeconds"], 0.);
        assert_eq!(j["metadata"]["waitSeconds"], 7.);
        assert_eq!(
            seconds(
                &json!("2026-01-02T00:00:00Z"),
                &json!("2026-01-01T00:00:00Z")
            ),
            Some(0.)
        );
    }
    #[test]
    fn legacy_missing_end_is_unknown_and_old_preparation_is_not_subtracted() {
        let mut j = json!({"status":"complete","metadata":{"startedAt":"2026-01-01T00:00:00Z","resourceWaitSeconds":20}});
        measure(&mut j, "2026-01-02T00:00:00Z");
        assert!(j["metadata"]["runSeconds"].is_null());
        j["metadata"]["completedAt"] = "2026-01-01T00:00:30Z".into();
        measure(&mut j, "2026-01-02T00:00:00Z");
        assert_eq!(j["metadata"]["runSeconds"], 30.);
        assert!(j["metadata"]["resourceWaitSeconds"].is_null());
        assert_eq!(j["metadata"]["timingEstimated"], true);
    }
}
