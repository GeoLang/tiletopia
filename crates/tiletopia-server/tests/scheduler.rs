//! Scheduled jobs through the real router and the real worker: an editor
//! schedules work the server can do unattended, the worker does it, and the row
//! says what happened.
//!
//! Its own binary because the auth middleware only enforces when
//! `TILETOPIA_JWT_SECRET` is set, which cannot be done in a binary whose other
//! cases expect it unset.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tiletopia_server::db::{JobRecord, JobStatus, ModelPlacement};
use tiletopia_server::scheduler::{MAX_RUN_ATTEMPTS, ScheduledAction, Scheduler};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tower::ServiceExt;
use uuid::Uuid;

const JWT_SECRET_ENV: &str = "TILETOPIA_JWT_SECRET";
const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef";

/// A cube the native mesh tiler takes, so a scheduled re-tile has real input to
/// submit.
const CUBE_OBJ: &str = "\
v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
f 1 2 3 4\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";

/// The shortest schedule a job may ask for, which is what lets a case wait out a
/// real due time instead of reaching past the worker.
const SHORT_INTERVAL_SECONDS: u64 = 1;

/// Long enough for a job on `SHORT_INTERVAL_SECONDS` to have come due.
const PAST_DUE: Duration = Duration::from_millis(1400);

/// How long a case waits for the background worker to get somewhere.
const WAIT_LIMIT: Duration = Duration::from_secs(60);

/// Put the auth middleware into its enforcing state, once for the binary.
fn signing_secret() -> &'static str {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // safe: the only writer in this test binary, before any request runs
        unsafe {
            std::env::set_var(JWT_SECRET_ENV, TEST_SECRET);
            std::env::remove_var("TILETOPIA_AUTH_DISABLED");
        }
    });
    TEST_SECRET
}

fn token(subject: &str, role: &str) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let claims = serde_json::json!({
        "sub": subject,
        "exp": chrono::Utc::now().timestamp() + 300,
        "role": role,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(signing_secret().as_bytes()),
    )
    .unwrap()
}

async fn test_state() -> Arc<AppState> {
    signing_secret();
    common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await
}

struct Answer {
    status: StatusCode,
    body: serde_json::Value,
    text: String,
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> Answer {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn json_request(method: &str, uri: &str, bearer: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn empty_request(method: &str, uri: &str, bearer: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap()
}

fn interval(seconds: u64) -> serde_json::Value {
    serde_json::json!({ "kind": "interval", "seconds": seconds })
}

fn one_shot_in(seconds: i64) -> serde_json::Value {
    serde_json::json!({
        "kind": "one_shot",
        "at": (chrono::Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339(),
    })
}

/// Schedule a job through the real route, answering the whole reply so a case
/// can read the row the server stored.
async fn schedule(
    state: &Arc<AppState>,
    bearer: &str,
    name: &str,
    action: serde_json::Value,
    schedule: serde_json::Value,
) -> Answer {
    let answer = send(
        state,
        json_request(
            "POST",
            "/api/v1/scheduler/jobs",
            bearer,
            serde_json::json!({ "name": name, "action": action, "schedule": schedule }),
        ),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.text);
    answer
}

/// The stored row, read back through the route.
async fn stored_job(state: &Arc<AppState>, bearer: &str, id: &str) -> serde_json::Value {
    let answer = send(
        state,
        empty_request("GET", &format!("/api/v1/scheduler/jobs/{id}"), bearer),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    answer.body["job"].clone()
}

fn job_id(answer: &Answer) -> String {
    answer.body["job"]["id"].as_str().unwrap().to_string()
}

/// An asset row owned by `owner`, with the cube staged where a tiling job reads
/// its input.
async fn staged_asset(state: &Arc<AppState>, owner: &str) -> Uuid {
    let asset = Asset {
        id: Uuid::new_v4(),
        name: "cube".into(),
        asset_type: AssetType::Model,
        status: AssetStatus::Ready,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: CUBE_OBJ.len() as u64,
        description: String::new(),
        tags: Vec::new(),
        owner_id: Some(owner.to_string()),
    };
    state.db.create_asset(&asset).await.unwrap();

    let input_dir = state.data_dir.join(asset.id.to_string()).join("input");
    std::fs::create_dir_all(&input_dir).unwrap();
    std::fs::write(input_dir.join("cube.obj"), CUBE_OBJ).unwrap();
    asset.id
}

/// A tiling job row that settled `days_ago`, which is what the prune action
/// deletes.
async fn settled_job_row(state: &Arc<AppState>, asset_id: Uuid, days_ago: i64) -> Uuid {
    let settled_at = chrono::Utc::now() - chrono::Duration::days(days_ago);
    let job = JobRecord {
        id: Uuid::new_v4(),
        asset_id,
        status: JobStatus::Done,
        progress: 1.0,
        input_path: "cube.obj".into(),
        output_format: "3dtiles".into(),
        created_at: settled_at,
        started_at: Some(settled_at),
        completed_at: Some(settled_at),
        error: None,
        points_processed: 0,
        tiles_written: 4,
        placement: ModelPlacement::default(),
    };
    state.db.create_job(&job).await.unwrap();
    state.db.update_job(&job).await.unwrap();
    job.id
}

/// A scheduler built fresh over the same database and data directory, which is
/// what the process gets on a restart.
fn restarted_scheduler(state: &Arc<AppState>) -> Scheduler {
    Scheduler::new(
        Arc::clone(&state.db),
        state.data_dir.clone(),
        Arc::clone(&state.job_queue),
    )
}

/// The round trip: an editor schedules an interval job, the worker the serve path
/// starts runs its real action, and the row carries the outcome.
#[tokio::test]
async fn an_interval_job_runs_its_action_and_the_row_records_the_outcome() {
    let state = test_state().await;
    let editor = token("editor-one", "editor");

    let asset_id = staged_asset(&state, "editor-one").await;
    let stale_job = settled_job_row(&state, asset_id, 3).await;

    let created = schedule(
        &state,
        &editor,
        "prune settled jobs",
        serde_json::json!({ "kind": "prune_finished_jobs", "older_than_days": 1 }),
        interval(SHORT_INTERVAL_SECONDS),
    )
    .await;
    let id = job_id(&created);
    assert_eq!(created.body["job"]["run_count"], 0);
    assert!(created.body["job"]["last_run"].is_null());
    assert!(created.body["job"]["next_run"].as_str().is_some());

    // the worker the serve path starts, not a hand-driven run
    let worker = Arc::clone(&state.scheduler).start();
    let deadline = std::time::Instant::now() + WAIT_LIMIT;
    let mut job = stored_job(&state, &editor, &id).await;
    while std::time::Instant::now() < deadline && job["run_count"] == 0 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        job = stored_job(&state, &editor, &id).await;
    }
    worker.abort();

    assert_eq!(job["run_count"], 1, "{job}");
    assert_eq!(job["last_outcome"]["outcome"], "success", "{job}");
    assert_eq!(
        job["last_outcome"]["detail"], "deleted 1 finished job rows",
        "{job}"
    );
    assert_eq!(job["consecutive_failures"], 0);
    assert_eq!(job["enabled"], true);

    // an interval job comes back, its seconds after the run that just finished
    let last_run = job["last_run"].as_str().unwrap();
    let next_run = job["next_run"].as_str().unwrap();
    assert!(next_run > last_run, "{last_run} then {next_run}");

    // and the action really deleted the row, so the job's own effect is visible
    let jobs = state.db.list_jobs_for_asset(asset_id).await.unwrap();
    assert!(
        jobs.iter().all(|job| job.id != stale_job),
        "the settled job row is still there"
    );
}

#[tokio::test]
async fn a_one_shot_runs_once_and_disables_itself() {
    let state = test_state().await;
    let editor = token("editor-two", "editor");

    let created = schedule(
        &state,
        &editor,
        "one prune",
        serde_json::json!({ "kind": "prune_export_files", "older_than_days": 7 }),
        one_shot_in(1),
    )
    .await;
    let id = job_id(&created);

    tokio::time::sleep(PAST_DUE).await;
    assert_eq!(state.scheduler.run_due().await, 1);

    let job = stored_job(&state, &editor, &id).await;
    assert_eq!(job["run_count"], 1, "{job}");
    assert_eq!(job["enabled"], false, "{job}");
    assert!(job["next_run"].is_null(), "{job}");
    assert_eq!(job["last_outcome"]["outcome"], "success");

    // nothing brings it back
    assert_eq!(state.scheduler.run_due().await, 0);
    tokio::time::sleep(PAST_DUE).await;
    assert_eq!(state.scheduler.run_due().await, 0);
    assert_eq!(stored_job(&state, &editor, &id).await["run_count"], 1);
}

#[tokio::test]
async fn a_disabled_job_never_runs_until_it_is_enabled() {
    let state = test_state().await;
    let editor = token("editor-three", "editor");

    let created = send(
        &state,
        json_request(
            "POST",
            "/api/v1/scheduler/jobs",
            &editor,
            serde_json::json!({
                "name": "paused prune",
                "action": { "kind": "prune_export_files", "older_than_days": 7 },
                "schedule": interval(SHORT_INTERVAL_SECONDS),
                "enabled": false,
            }),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
    let id = job_id(&created);

    tokio::time::sleep(PAST_DUE).await;
    assert_eq!(state.scheduler.run_due().await, 0);
    let job = stored_job(&state, &editor, &id).await;
    assert_eq!(job["run_count"], 0, "{job}");
    assert!(job["last_run"].is_null(), "{job}");

    let enabled = send(
        &state,
        json_request(
            "PUT",
            &format!("/api/v1/scheduler/jobs/{id}"),
            &editor,
            serde_json::json!({ "enabled": true }),
        ),
    )
    .await;
    assert_eq!(enabled.status, StatusCode::OK, "{}", enabled.text);
    // enabling recomputes the next run, so the time it sat disabled is not owed
    assert_eq!(state.scheduler.run_due().await, 0);

    tokio::time::sleep(PAST_DUE).await;
    assert_eq!(state.scheduler.run_due().await, 1);
    assert_eq!(stored_job(&state, &editor, &id).await["run_count"], 1);

    let paused = send(
        &state,
        json_request(
            "PUT",
            &format!("/api/v1/scheduler/jobs/{id}"),
            &editor,
            serde_json::json!({ "enabled": false }),
        ),
    )
    .await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.text);
    tokio::time::sleep(PAST_DUE).await;
    assert_eq!(state.scheduler.run_due().await, 0);
    assert_eq!(stored_job(&state, &editor, &id).await["run_count"], 1);
}

/// The next run is on the row, not in the worker: a scheduler built fresh over
/// the same database runs what is due and leaves what is not.
#[tokio::test]
async fn the_next_run_survives_a_restart() {
    let state = test_state().await;
    let editor = token("editor-four", "editor");

    let due_soon = schedule(
        &state,
        &editor,
        "due soon",
        serde_json::json!({ "kind": "prune_export_files", "older_than_days": 7 }),
        interval(SHORT_INTERVAL_SECONDS),
    )
    .await;
    let due_soon_id = job_id(&due_soon);

    let due_later = schedule(
        &state,
        &editor,
        "due in an hour",
        serde_json::json!({ "kind": "prune_export_files", "older_than_days": 7 }),
        interval(3600),
    )
    .await;
    let due_later_id = job_id(&due_later);

    // the row answers what the create response said, with no scheduler running
    assert_eq!(
        stored_job(&state, &editor, &due_soon_id).await["next_run"],
        due_soon.body["job"]["next_run"]
    );

    tokio::time::sleep(PAST_DUE).await;
    assert_eq!(restarted_scheduler(&state).run_due().await, 1);

    let ran = stored_job(&state, &editor, &due_soon_id).await;
    assert_eq!(ran["run_count"], 1, "{ran}");
    let waiting = stored_job(&state, &editor, &due_later_id).await;
    assert_eq!(waiting["run_count"], 0, "{waiting}");
    assert_eq!(
        waiting["next_run"], due_later.body["job"]["next_run"],
        "the untouched job's next run moved"
    );

    // and the run that happened put the next one an interval further out
    let last_run = chrono::DateTime::parse_from_rfc3339(ran["last_run"].as_str().unwrap()).unwrap();
    let next_run = chrono::DateTime::parse_from_rfc3339(ran["next_run"].as_str().unwrap()).unwrap();
    assert_eq!(
        (next_run - last_run).num_seconds(),
        SHORT_INTERVAL_SECONDS as i64
    );
}

/// A job whose action cannot be carried out fails, keeps its error on the row,
/// and is disabled once the attempts run out.
#[tokio::test]
async fn a_failing_job_records_its_error_and_is_disabled_at_the_attempt_bound() {
    let state = test_state().await;
    let editor = token("editor-five", "editor");

    let gone = Uuid::new_v4();
    let created = schedule(
        &state,
        &editor,
        "retile a missing asset",
        serde_json::json!({ "kind": "retile_asset", "asset_id": gone }),
        interval(SHORT_INTERVAL_SECONDS),
    )
    .await;
    let id = job_id(&created);

    tokio::time::sleep(PAST_DUE).await;
    for attempt in 1..=MAX_RUN_ATTEMPTS {
        // a failure comes back on the next run, so no waiting between attempts
        assert_eq!(state.scheduler.run_due().await, 1, "attempt {attempt}");
        let job = stored_job(&state, &editor, &id).await;
        assert_eq!(job["run_count"], attempt, "{job}");
        assert_eq!(job["consecutive_failures"], attempt, "{job}");
        assert_eq!(job["last_outcome"]["outcome"], "failure", "{job}");
        assert!(
            job["last_outcome"]["detail"]
                .as_str()
                .unwrap()
                .contains(&gone.to_string()),
            "{job}"
        );
    }

    let job = stored_job(&state, &editor, &id).await;
    assert_eq!(job["enabled"], false, "{job}");
    assert!(job["next_run"].is_null(), "{job}");
    assert_eq!(state.scheduler.run_due().await, 0);
}

#[tokio::test]
async fn the_write_routes_refuse_a_viewer_token() {
    let state = test_state().await;
    let editor = token("editor-six", "editor");
    let viewer = token("viewer", "viewer");

    let created = schedule(
        &state,
        &editor,
        "prune exports",
        serde_json::json!({ "kind": "prune_export_files", "older_than_days": 7 }),
        interval(3600),
    )
    .await;
    let id = job_id(&created);

    for request in [
        json_request(
            "POST",
            "/api/v1/scheduler/jobs",
            &viewer,
            serde_json::json!({
                "name": "mine now",
                "action": { "kind": "prune_export_files", "older_than_days": 7 },
                "schedule": interval(3600),
            }),
        ),
        json_request(
            "PUT",
            &format!("/api/v1/scheduler/jobs/{id}"),
            &viewer,
            serde_json::json!({ "enabled": false }),
        ),
        empty_request("DELETE", &format!("/api/v1/scheduler/jobs/{id}"), &viewer),
    ] {
        let uri = request.uri().to_string();
        let method = request.method().to_string();
        let answer = send(&state, request).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {uri}");
    }

    // nothing was created, removed or paused along the way
    let listing = send(
        &state,
        empty_request("GET", "/api/v1/scheduler/jobs", &editor),
    )
    .await;
    let jobs = listing.body["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{}", listing.text);
    assert_eq!(jobs[0]["id"], id);
    assert_eq!(jobs[0]["enabled"], true);
}

#[tokio::test]
async fn another_callers_job_is_not_there_to_read_or_change() {
    let state = test_state().await;
    let mine = token("editor-seven", "editor");
    let theirs = token("editor-eight", "editor");

    let created = schedule(
        &state,
        &theirs,
        "their prune",
        serde_json::json!({ "kind": "prune_export_files", "older_than_days": 7 }),
        interval(3600),
    )
    .await;
    let id = job_id(&created);

    for request in [
        empty_request("GET", &format!("/api/v1/scheduler/jobs/{id}"), &mine),
        json_request(
            "PUT",
            &format!("/api/v1/scheduler/jobs/{id}"),
            &mine,
            serde_json::json!({ "enabled": false }),
        ),
        empty_request("DELETE", &format!("/api/v1/scheduler/jobs/{id}"), &mine),
    ] {
        let uri = request.uri().to_string();
        let method = request.method().to_string();
        let answer = send(&state, request).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }

    // and it is not in my listing either, while an admin sees it
    let listing = send(
        &state,
        empty_request("GET", "/api/v1/scheduler/jobs", &mine),
    )
    .await;
    assert!(
        listing.body["jobs"].as_array().unwrap().is_empty(),
        "{}",
        listing.text
    );

    let admin = token("root", "admin");
    let all = send(
        &state,
        empty_request("GET", "/api/v1/scheduler/jobs", &admin),
    )
    .await;
    assert_eq!(
        all.body["jobs"].as_array().unwrap().len(),
        1,
        "{}",
        all.text
    );
    assert_eq!(stored_job(&state, &admin, &id).await["id"], id);

    // the creator can still delete it
    let gone = send(
        &state,
        empty_request("DELETE", &format!("/api/v1/scheduler/jobs/{id}"), &theirs),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.text);
    let after = send(
        &state,
        empty_request("GET", "/api/v1/scheduler/jobs", &admin),
    )
    .await;
    assert!(after.body["jobs"].as_array().unwrap().is_empty());
}

/// The actions route advertises exactly what the worker runs: one job per
/// advertised kind, and every one of them executes.
#[tokio::test]
async fn the_advertised_action_kinds_are_exactly_the_ones_the_worker_runs() {
    let state = test_state().await;
    let editor = token("editor-nine", "editor");
    let asset_id = staged_asset(&state, "editor-nine").await;

    let advertised = send(
        &state,
        empty_request("GET", "/api/v1/scheduler/actions", &editor),
    )
    .await;
    assert_eq!(advertised.status, StatusCode::OK, "{}", advertised.text);
    let kinds: Vec<String> = advertised.body["action_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind.as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ScheduledAction::KINDS);

    let mut scheduled = Vec::new();
    for kind in &kinds {
        // an advertised kind with no case here is a kind nothing schedules
        let action = match kind.as_str() {
            "retile_asset" => serde_json::json!({ "kind": kind, "asset_id": asset_id }),
            "prune_export_files" | "prune_finished_jobs" => {
                serde_json::json!({ "kind": kind, "older_than_days": 7 })
            }
            other => panic!("{other} is advertised and this case does not schedule it"),
        };
        let created = schedule(&state, &editor, kind, action, one_shot_in(1)).await;
        scheduled.push((kind.clone(), job_id(&created)));
    }

    tokio::time::sleep(PAST_DUE).await;
    assert_eq!(state.scheduler.run_due().await, kinds.len());

    for (kind, id) in &scheduled {
        let job = stored_job(&state, &editor, id).await;
        assert_eq!(job["run_count"], 1, "{kind}: {job}");
        assert_eq!(job["last_outcome"]["outcome"], "success", "{kind}: {job}");
    }

    // the re-tile really went on the tiling queue, which is the action's whole
    // effect
    let tiling = state.db.list_jobs_for_asset(asset_id).await.unwrap();
    assert_eq!(tiling.len(), 1, "{tiling:?}");
    assert_eq!(tiling[0].status, JobStatus::Queued);
}

#[tokio::test]
async fn scheduling_refuses_an_action_and_a_schedule_the_server_does_not_run() {
    let state = test_state().await;
    let editor = token("editor-ten", "editor");

    for (body, expected) in [
        (
            serde_json::json!({
                "name": "nightly terrain",
                "action": { "kind": "terrain_regeneration" },
                "schedule": interval(3600),
            }),
            "action is not one this server runs",
        ),
        (
            serde_json::json!({
                "name": "no age",
                "action": { "kind": "prune_export_files", "older_than_days": 0 },
                "schedule": interval(3600),
            }),
            "older_than_days must be at least 1",
        ),
        (
            serde_json::json!({
                "name": "every second",
                "action": { "kind": "prune_export_files", "older_than_days": 7 },
                "schedule": interval(0),
            }),
            "seconds must be at least 1",
        ),
        (
            serde_json::json!({
                "name": "bad cron",
                "action": { "kind": "prune_export_files", "older_than_days": 7 },
                "schedule": { "kind": "cron", "expression": "every night please" },
            }),
            "is not a cron expression",
        ),
        (
            serde_json::json!({
                "name": "already gone",
                "action": { "kind": "prune_export_files", "older_than_days": 7 },
                "schedule": one_shot_in(-60),
            }),
            "at must be in the future",
        ),
        (
            serde_json::json!({
                "name": "no schedule at all",
                "action": { "kind": "prune_export_files", "older_than_days": 7 },
                "schedule": { "kind": "whenever" },
            }),
            "schedule is not one this server keeps",
        ),
        (
            serde_json::json!({
                "name": "   ",
                "action": { "kind": "prune_export_files", "older_than_days": 7 },
                "schedule": interval(3600),
            }),
            "name must be 1 to 120 characters",
        ),
    ] {
        let answer = send(
            &state,
            json_request("POST", "/api/v1/scheduler/jobs", &editor, body),
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.text);
        assert!(
            answer.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{}",
            answer.text
        );
    }

    // a cron job the server can read is accepted, and lands on the minute
    let accepted = schedule(
        &state,
        &editor,
        "nightly",
        serde_json::json!({ "kind": "prune_finished_jobs", "older_than_days": 30 }),
        serde_json::json!({ "kind": "cron", "expression": "0 2 * * *" }),
    )
    .await;
    let next_run = accepted.body["job"]["next_run"].as_str().unwrap();
    assert!(next_run.contains("T02:00:00"), "{next_run}");
}

/// Every name the deleted scheduler had is gone from the tree, and the two routes
/// that answered fabricated stats and runs are not mounted.
#[tokio::test]
async fn the_deleted_scheduler_identifiers_are_absent() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(crates).unwrap().flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            collect_rust_sources(&src, &mut sources);
        }
    }
    assert!(sources.len() > 50, "found only {} sources", sources.len());

    // names the deleted module alone spelled
    for name in [
        "SchedulerStats",
        "next_cron_time",
        "TerrainRegeneration",
        "DemUpdate",
        "AnomalyMonitoring",
        "StorageMetrics",
        "CatalogRefresh",
        "CustomPipeline",
        "failure_count",
    ] {
        for path in &sources {
            let source = std::fs::read_to_string(path).unwrap();
            assert!(
                !source.contains(name),
                "{name} is still in {}",
                path.display()
            );
        }
    }

    // names other modules also spell, so only the scheduler's own file is checked
    let scheduler_source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("scheduler.rs"),
    )
    .unwrap();
    for name in [
        "JobType",
        "JobRun",
        "RunStatus",
        "Priority",
        "JobStatus",
        "ExportCleanup",
        "ChangeDetection",
        "demo_data",
        "duration_secs",
        // the three seeded jobs and their invented run counts
        "Nightly Terrain Refresh",
        "Structural Monitoring",
        "Weekly Export Cleanup",
    ] {
        assert!(
            !scheduler_source.contains(name),
            "{name} is still in scheduler.rs"
        );
    }

    let state = test_state().await;
    let editor = token("editor-eleven", "editor");
    for path in ["/api/v1/scheduler/stats", "/api/v1/scheduler/runs"] {
        let answer = send(&state, empty_request("GET", path, &editor)).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "GET {path}");
    }
}

fn collect_rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, into);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            into.push(path);
        }
    }
}
