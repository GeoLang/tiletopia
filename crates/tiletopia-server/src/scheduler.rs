//! Scheduled jobs: work the server carries out unattended, on an interval, on a
//! cron expression, or once at a time.
//!
//! A job is a row in SQLite holding an action, a schedule and what its runs did:
//! when it last ran, what happened, and how many times it has run. Nothing is
//! seeded, so a fresh server has no jobs.
//!
//! Three actions, each of them an entry point that already exists: re-tiling an
//! asset submits to [`crate::job_queue`], pruning export files removes what
//! [`crate::export`] left under the data directory, and pruning finished jobs
//! deletes settled rows from the `jobs` table. Nothing else is schedulable: an
//! action has to be something this process can finish with no caller waiting on
//! it.
//!
//! One worker task runs the whole schedule, so two runs of one job never
//! overlap. A run that fails comes back on the next tick, and a job that fails
//! [`MAX_RUN_ATTEMPTS`] times in a row is disabled with the failure left on the
//! row.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::Database;
use crate::job_queue::JobQueue;

/// How often the worker looks for a job that is due, which is also the finest
/// schedule granularity there is.
const TICK: Duration = Duration::from_secs(1);

/// Failures in a row a job gets before it is disabled. A failed run is retried
/// on the next tick, so this counts attempts and there is no backoff.
pub const MAX_RUN_ATTEMPTS: u32 = 3;

/// Shortest interval a job may ask for.
const MIN_INTERVAL_SECONDS: u64 = 1;

/// Youngest age a prune action may delete at, so no job can be configured to
/// delete what finished a moment ago.
const MIN_PRUNE_AGE_DAYS: u32 = 1;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// What a job does when it comes due. Every variant runs through an entry point
/// the server already has, and [`Scheduler`] is the only caller.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduledAction {
    /// Submit a tiling job for this asset, the same job `POST
    /// /api/v1/assets/{id}/tile` submits.
    RetileAsset { asset_id: Uuid },
    /// Delete finished export directories older than this many days.
    PruneExportFiles { older_than_days: u32 },
    /// Delete settled rows from the `jobs` table older than this many days.
    PruneFinishedJobs { older_than_days: u32 },
}

impl ScheduledAction {
    /// Every action a job may ask for, which is every action that executes. Same
    /// rule as [`crate::webhooks::WebhookEvent::ALL`]: a name here with no arm in
    /// [`ScheduledAction::kind`] does not compile.
    pub const KINDS: [&'static str; 3] =
        ["retile_asset", "prune_export_files", "prune_finished_jobs"];

    /// The `kind` tag this action serializes as and parses from.
    pub fn kind(&self) -> &'static str {
        match self {
            ScheduledAction::RetileAsset { .. } => "retile_asset",
            ScheduledAction::PruneExportFiles { .. } => "prune_export_files",
            ScheduledAction::PruneFinishedJobs { .. } => "prune_finished_jobs",
        }
    }

    /// Refuse a configuration the action cannot carry out.
    pub fn check(&self) -> Result<(), String> {
        match self {
            ScheduledAction::RetileAsset { .. } => Ok(()),
            ScheduledAction::PruneExportFiles { older_than_days }
            | ScheduledAction::PruneFinishedJobs { older_than_days } => {
                if *older_than_days < MIN_PRUNE_AGE_DAYS {
                    return Err(format!(
                        "older_than_days must be at least {MIN_PRUNE_AGE_DAYS}"
                    ));
                }
                Ok(())
            }
        }
    }
}

/// When a job runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Every `seconds`, measured from the end of the last run.
    Interval { seconds: u64 },
    /// Five standard cron fields, see [`parse_cron`].
    Cron { expression: String },
    /// Once, at this time.
    OneShot { at: DateTime<Utc> },
}

impl Schedule {
    /// Every schedule a job may ask for.
    pub const KINDS: [&'static str; 3] = ["interval", "cron", "one_shot"];

    /// The `kind` tag this schedule serializes as and parses from.
    pub fn kind(&self) -> &'static str {
        match self {
            Schedule::Interval { .. } => "interval",
            Schedule::Cron { .. } => "cron",
            Schedule::OneShot { .. } => "one_shot",
        }
    }

    /// When this schedule comes due after `after`, or `None` when it never does
    /// again.
    pub fn next_run_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Interval { seconds } => {
                Some(after + chrono::Duration::seconds(*seconds as i64))
            }
            Schedule::Cron { expression } => parse_cron(expression).ok()?.after(&after).next(),
            Schedule::OneShot { .. } => None,
        }
    }

    /// The first run this schedule asks for, or why it is not a schedule this
    /// server can keep.
    pub fn first_run(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        match self {
            Schedule::Interval { seconds } => {
                if *seconds < MIN_INTERVAL_SECONDS {
                    return Err(format!("seconds must be at least {MIN_INTERVAL_SECONDS}"));
                }
                Ok(now + chrono::Duration::seconds(*seconds as i64))
            }
            Schedule::Cron { expression } => parse_cron(expression)?
                .after(&now)
                .next()
                .ok_or_else(|| format!("'{expression}' has no run left in the future")),
            Schedule::OneShot { at } => {
                if *at <= now {
                    return Err("at must be in the future".to_string());
                }
                Ok(*at)
            }
        }
    }
}

/// Fields a cron expression must have: minute, hour, day of month, month, day of
/// week.
const CRON_FIELDS: usize = 5;

/// A cron expression as the `cron` crate reads it. Five standard fields, so a job
/// runs at second 0 of the minute it names, in UTC. The crate wants seconds and a
/// year of its own, which is what the padding supplies.
///
/// The count is checked first: four fields padded the same way is a valid
/// six-field expression to the crate, meaning something else entirely.
pub fn parse_cron(expression: &str) -> Result<cron::Schedule, String> {
    let fields = expression.split_whitespace().count();
    if fields != CRON_FIELDS {
        return Err(format!(
            "'{expression}' is not a cron expression: {CRON_FIELDS} fields expected, found {fields}"
        ));
    }
    cron::Schedule::from_str(&format!("0 {expression} *"))
        .map_err(|error| format!("'{expression}' is not a cron expression: {error}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Failure,
}

impl Outcome {
    pub const ALL: [Outcome; 2] = [Outcome::Success, Outcome::Failure];

    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
        }
    }

    pub fn from_name(name: &str) -> Option<Outcome> {
        Outcome::ALL
            .into_iter()
            .find(|outcome| outcome.name() == name)
    }
}

/// What the last run did. The next run overwrites it: the row keeps the last
/// outcome, not a history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunOutcome {
    pub outcome: Outcome,
    /// What the action did, or the error it failed with.
    pub detail: String,
}

/// A job as it is stored.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledJob {
    pub id: Uuid,
    pub name: String,
    pub action: ScheduledAction,
    pub schedule: Schedule,
    /// A disabled job is never due. A one-shot disables itself once it has run,
    /// and so does a job that has failed [`MAX_RUN_ATTEMPTS`] times in a row.
    pub enabled: bool,
    /// JWT `sub` of the editor who created it.
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    /// When the worker runs it next, `None` when nothing will.
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    pub last_outcome: Option<RunOutcome>,
    /// Runs the worker has finished, successful or not.
    pub run_count: u64,
    pub consecutive_failures: u32,
}

/// The schedule state a row takes after a run.
#[derive(Debug, PartialEq)]
struct NextState {
    next_run: Option<DateTime<Utc>>,
    enabled: bool,
    consecutive_failures: u32,
}

/// A success follows the schedule, and a schedule with nothing left disables the
/// job. A failure comes back on the next tick until the attempts run out, and
/// then the job is disabled too.
fn next_state(job: &ScheduledJob, outcome: Outcome, finished_at: DateTime<Utc>) -> NextState {
    match outcome {
        Outcome::Success => {
            let next_run = job.schedule.next_run_after(finished_at);
            NextState {
                enabled: next_run.is_some(),
                next_run,
                consecutive_failures: 0,
            }
        }
        Outcome::Failure => {
            let consecutive_failures = job.consecutive_failures + 1;
            if consecutive_failures >= MAX_RUN_ATTEMPTS {
                NextState {
                    next_run: None,
                    enabled: false,
                    consecutive_failures,
                }
            } else {
                NextState {
                    next_run: Some(finished_at),
                    enabled: true,
                    consecutive_failures,
                }
            }
        }
    }
}

/// Runs the jobs the database says are due.
pub struct Scheduler {
    db: Arc<Database>,
    data_dir: PathBuf,
    job_queue: Arc<JobQueue>,
}

impl Scheduler {
    pub fn new(db: Arc<Database>, data_dir: PathBuf, job_queue: Arc<JobQueue>) -> Self {
        Self {
            db,
            data_dir,
            job_queue,
        }
    }

    /// Start the background worker loop.
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.run_due().await;
                tokio::time::sleep(TICK).await;
            }
        })
    }

    /// Run every job that is due and write back what happened. Returns how many
    /// ran.
    pub async fn run_due(&self) -> usize {
        let due = match self.db.due_scheduled_jobs(Utc::now()).await {
            Ok(due) => due,
            Err(error) => {
                tracing::error!("reading the due scheduled jobs failed: {error}");
                return 0;
            }
        };

        let mut ran = 0;
        for job in due {
            let outcome = match self.execute(&job.action).await {
                Ok(detail) => RunOutcome {
                    outcome: Outcome::Success,
                    detail,
                },
                Err(detail) => {
                    tracing::warn!(
                        "scheduled job {} ({}) failed: {detail}",
                        job.id,
                        job.action.kind()
                    );
                    RunOutcome {
                        outcome: Outcome::Failure,
                        detail,
                    }
                }
            };

            let finished_at = Utc::now();
            let next = next_state(&job, outcome.outcome, finished_at);
            if !next.enabled {
                tracing::info!(
                    "scheduled job {} is disabled after this run: {}",
                    job.id,
                    outcome.detail
                );
            }

            let mut job = job;
            job.last_run = Some(finished_at);
            job.last_outcome = Some(outcome);
            job.run_count += 1;
            job.next_run = next.next_run;
            job.enabled = next.enabled;
            job.consecutive_failures = next.consecutive_failures;

            if let Err(error) = self.db.record_scheduled_run(&job).await {
                tracing::error!(
                    "recording the run of scheduled job {} failed: {error}",
                    job.id
                );
            }
            ran += 1;
        }
        ran
    }

    /// Carry out one action. `Ok` says what it did, `Err` says why it did not.
    async fn execute(&self, action: &ScheduledAction) -> Result<String, String> {
        match action {
            ScheduledAction::RetileAsset { asset_id } => self.retile_asset(*asset_id).await,
            ScheduledAction::PruneExportFiles { older_than_days } => {
                prune_export_files(&self.data_dir, *older_than_days).await
            }
            ScheduledAction::PruneFinishedJobs { older_than_days } => {
                let cutoff = Utc::now() - chrono::Duration::days(*older_than_days as i64);
                let deleted = self
                    .db
                    .delete_finished_jobs_before(cutoff)
                    .await
                    .map_err(|error| format!("deleting finished job rows failed: {error}"))?;
                Ok(format!("deleted {deleted} finished job rows"))
            }
        }
    }

    /// Submit a tiling job for the asset, carrying the placement its last job
    /// carried: a mesh needs longitude and latitude to be tiled at all, so a
    /// re-tile that dropped them would fail where the first tiling worked.
    async fn retile_asset(&self, asset_id: Uuid) -> Result<String, String> {
        if self
            .db
            .get_asset(asset_id)
            .await
            .map_err(|error| format!("reading asset {asset_id} failed: {error}"))?
            .is_none()
        {
            return Err(format!("asset {asset_id} is gone"));
        }

        let input_path = input_file(&self.data_dir, asset_id)
            .await
            .ok_or_else(|| format!("asset {asset_id} has no uploaded input to tile"))?;

        let placement = self
            .db
            .list_jobs_for_asset(asset_id)
            .await
            .map_err(|error| format!("reading the jobs of asset {asset_id} failed: {error}"))?
            .into_iter()
            .next()
            .map(|job| job.placement)
            .unwrap_or_default();

        let job = self
            .job_queue
            .submit(asset_id, input_path, placement)
            .await
            .map_err(|error| {
                format!("submitting a tiling job for asset {asset_id} failed: {error}")
            })?;
        Ok(format!("submitted tiling job {}", job.id))
    }
}

/// The uploaded file a tiling job reads, which the upload route wrote as the
/// only entry under the asset's `input` directory.
async fn input_file(data_dir: &Path, asset_id: Uuid) -> Option<String> {
    let input_dir = data_dir.join(asset_id.to_string()).join("input");
    let mut entries = tokio::fs::read_dir(&input_dir).await.ok()?;
    let entry = entries.next_entry().await.ok()??;
    Some(entry.path().to_string_lossy().into_owned())
}

/// Remove export directories whose newest file is older than `older_than_days`.
/// An export's job record is process memory, so a restart leaves the files with
/// nothing that would ever delete them.
///
/// Only directories named by a job id are considered, and one holding no file is
/// an export still being encoded, so it is left alone.
async fn prune_export_files(data_dir: &Path, older_than_days: u32) -> Result<String, String> {
    let exports = crate::export::exports_dir(data_dir);
    let read_failed =
        |error: std::io::Error| format!("reading {} failed: {error}", exports.display());

    let mut entries = match tokio::fs::read_dir(&exports).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok("nothing has been exported".to_string());
        }
        Err(error) => return Err(read_failed(error)),
    };

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            older_than_days as u64 * SECONDS_PER_DAY,
        ))
        .ok_or_else(|| format!("{older_than_days} days is further back than the clock goes"))?;

    let mut removed = 0;
    while let Some(entry) = entries.next_entry().await.map_err(read_failed)? {
        let path = entry.path();
        let named_by_a_job = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| Uuid::parse_str(name).is_ok());
        if !named_by_a_job {
            continue;
        }
        if !entry.path().is_dir() {
            continue;
        }
        match newest_file_time(&path).await? {
            Some(written) if written < cutoff => {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(|error| format!("removing {} failed: {error}", path.display()))?;
                removed += 1;
            }
            _ => continue,
        }
    }

    Ok(format!("removed {removed} expired export directories"))
}

/// When the newest file directly in `dir` was written, or `None` when it holds
/// no file.
async fn newest_file_time(dir: &Path) -> Result<Option<SystemTime>, String> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|error| format!("reading {} failed: {error}", dir.display()))?;

    let mut newest = None;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| format!("reading {} failed: {error}", dir.display()))?
    {
        let metadata = entry
            .metadata()
            .await
            .map_err(|error| format!("reading {} failed: {error}", entry.path().display()))?;
        if !metadata.is_file() {
            continue;
        }
        if let Ok(modified) = metadata.modified() {
            newest = Some(newest.map_or(modified, |seen: SystemTime| seen.max(modified)));
        }
    }
    Ok(newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(schedule: Schedule, consecutive_failures: u32) -> ScheduledJob {
        ScheduledJob {
            id: Uuid::new_v4(),
            name: "nightly".into(),
            action: ScheduledAction::PruneFinishedJobs {
                older_than_days: 30,
            },
            schedule,
            enabled: true,
            created_by: "editor".into(),
            created_at: Utc::now(),
            next_run: None,
            last_run: None,
            last_outcome: None,
            run_count: 0,
            consecutive_failures,
        }
    }

    #[test]
    fn every_advertised_action_kind_is_the_tag_it_serializes_as() {
        let actions = [
            ScheduledAction::RetileAsset {
                asset_id: Uuid::nil(),
            },
            ScheduledAction::PruneExportFiles { older_than_days: 7 },
            ScheduledAction::PruneFinishedJobs { older_than_days: 7 },
        ];
        let kinds: Vec<&str> = actions.iter().map(|action| action.kind()).collect();
        assert_eq!(kinds, ScheduledAction::KINDS);

        for action in &actions {
            let json: serde_json::Value = serde_json::to_value(action).unwrap();
            assert_eq!(json["kind"], action.kind());
            let parsed: ScheduledAction = serde_json::from_value(json).unwrap();
            assert_eq!(&parsed, action);
        }
    }

    #[test]
    fn an_unknown_action_kind_does_not_parse() {
        for json in [
            serde_json::json!({ "kind": "terrain_regeneration" }),
            serde_json::json!({ "kind": "dem_update" }),
            serde_json::json!({ "kind": "custom_pipeline", "script": "rm -rf /" }),
            // a real kind missing the field it needs
            serde_json::json!({ "kind": "retile_asset" }),
            serde_json::json!({ "kind": "prune_export_files" }),
        ] {
            assert!(
                serde_json::from_value::<ScheduledAction>(json.clone()).is_err(),
                "{json}"
            );
        }
    }

    #[test]
    fn a_prune_action_refuses_an_age_under_a_day() {
        assert!(
            ScheduledAction::PruneExportFiles { older_than_days: 0 }
                .check()
                .is_err()
        );
        assert!(
            ScheduledAction::PruneFinishedJobs { older_than_days: 0 }
                .check()
                .is_err()
        );
        assert!(
            ScheduledAction::PruneExportFiles { older_than_days: 1 }
                .check()
                .is_ok()
        );
    }

    #[test]
    fn an_interval_comes_due_its_seconds_after_the_last_run() {
        let now = Utc::now();
        let schedule = Schedule::Interval { seconds: 900 };
        assert_eq!(
            schedule.next_run_after(now),
            Some(now + chrono::Duration::seconds(900))
        );
        assert_eq!(
            schedule.first_run(now).unwrap(),
            now + chrono::Duration::seconds(900)
        );
        assert!(Schedule::Interval { seconds: 0 }.first_run(now).is_err());
    }

    #[test]
    fn a_one_shot_runs_at_its_time_and_never_again() {
        let now = Utc::now();
        let at = now + chrono::Duration::hours(2);
        let schedule = Schedule::OneShot { at };
        assert_eq!(schedule.first_run(now).unwrap(), at);
        assert_eq!(schedule.next_run_after(at), None);

        let past = Schedule::OneShot {
            at: now - chrono::Duration::seconds(1),
        };
        assert!(past.first_run(now).unwrap_err().contains("in the future"));
    }

    #[test]
    fn a_cron_schedule_reads_five_fields_and_refuses_anything_else() {
        let midnight = Schedule::Cron {
            expression: "0 0 * * *".into(),
        };
        let from = DateTime::parse_from_rfc3339("2026-08-24T09:15:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = midnight.next_run_after(from).unwrap();
        assert_eq!(next.to_rfc3339(), "2026-08-25T00:00:00+00:00");
        // the same answer whether it is the first run or a later one
        assert_eq!(midnight.first_run(from).unwrap(), next);

        let hourly = Schedule::Cron {
            expression: "30 * * * *".into(),
        };
        assert_eq!(
            hourly.next_run_after(from).unwrap().to_rfc3339(),
            "2026-08-24T09:30:00+00:00"
        );

        // four fields would pad into a valid six-field expression meaning
        // something else, and six would pad into eight
        for expression in [
            "",
            "* * * *",
            "0 0 * *",
            "0 0 * * * *",
            "@daily",
            "not cron",
            "99 0 * * *",
        ] {
            let refused = Schedule::Cron {
                expression: expression.into(),
            }
            .first_run(from)
            .unwrap_err();
            assert!(
                refused.contains("cron expression"),
                "{expression}: {refused}"
            );
        }
    }

    #[test]
    fn a_successful_run_follows_the_schedule_and_clears_the_failures() {
        let finished_at = Utc::now();
        let state = next_state(
            &job(Schedule::Interval { seconds: 60 }, 2),
            Outcome::Success,
            finished_at,
        );
        assert_eq!(
            state,
            NextState {
                next_run: Some(finished_at + chrono::Duration::seconds(60)),
                enabled: true,
                consecutive_failures: 0,
            }
        );
    }

    #[test]
    fn a_one_shot_disables_itself_once_it_has_run() {
        let finished_at = Utc::now();
        let state = next_state(
            &job(
                Schedule::OneShot {
                    at: finished_at - chrono::Duration::seconds(1),
                },
                0,
            ),
            Outcome::Success,
            finished_at,
        );
        assert_eq!(
            state,
            NextState {
                next_run: None,
                enabled: false,
                consecutive_failures: 0,
            }
        );
    }

    #[test]
    fn a_failing_run_comes_back_next_tick_until_the_attempts_run_out() {
        let finished_at = Utc::now();
        for failures_before in 0..MAX_RUN_ATTEMPTS - 1 {
            let state = next_state(
                &job(Schedule::Interval { seconds: 60 }, failures_before),
                Outcome::Failure,
                finished_at,
            );
            assert_eq!(
                state,
                NextState {
                    next_run: Some(finished_at),
                    enabled: true,
                    consecutive_failures: failures_before + 1,
                },
                "after {failures_before} failures"
            );
        }

        let state = next_state(
            &job(Schedule::Interval { seconds: 60 }, MAX_RUN_ATTEMPTS - 1),
            Outcome::Failure,
            finished_at,
        );
        assert_eq!(
            state,
            NextState {
                next_run: None,
                enabled: false,
                consecutive_failures: MAX_RUN_ATTEMPTS,
            }
        );
    }

    #[test]
    fn an_outcome_parses_from_the_name_it_stores_as() {
        for outcome in Outcome::ALL {
            assert_eq!(Outcome::from_name(outcome.name()), Some(outcome));
            assert_eq!(
                serde_json::to_string(&outcome).unwrap(),
                format!("\"{}\"", outcome.name())
            );
        }
        for name in ["", "Success", "ok", "failed"] {
            assert_eq!(Outcome::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn a_schedule_round_trips_through_json_under_its_advertised_kind() {
        let schedules = [
            Schedule::Interval { seconds: 3600 },
            Schedule::Cron {
                expression: "0 2 * * *".into(),
            },
            Schedule::OneShot { at: Utc::now() },
        ];
        let kinds: Vec<&str> = schedules.iter().map(|schedule| schedule.kind()).collect();
        assert_eq!(kinds, Schedule::KINDS);

        for schedule in schedules {
            let json = serde_json::to_value(&schedule).unwrap();
            assert_eq!(json["kind"], schedule.kind());
            assert_eq!(
                serde_json::from_value::<Schedule>(json.clone()).unwrap(),
                schedule,
                "{json}"
            );
        }
    }

    #[tokio::test]
    async fn pruning_export_files_removes_the_stale_directories_and_leaves_the_rest() {
        let dir = std::env::temp_dir().join(format!("tiletopia_prune_{}", Uuid::new_v4()));
        let exports = crate::export::exports_dir(&dir);
        std::fs::create_dir_all(&exports).unwrap();

        let stale = exports.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&stale).unwrap();
        let stale_file = stale.join("tiles.zip");
        std::fs::write(&stale_file, b"old").unwrap();
        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * SECONDS_PER_DAY);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale_file)
            .unwrap()
            .set_modified(three_days_ago)
            .unwrap();

        let fresh = exports.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join("tiles.zip"), b"new").unwrap();

        // an export still encoding has its directory but no file yet
        let encoding = exports.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&encoding).unwrap();

        // and something an operator dropped in by hand is not a job id
        let not_a_job = exports.join("scratch");
        std::fs::create_dir_all(&not_a_job).unwrap();

        let detail = prune_export_files(&dir, 1).await.unwrap();
        assert_eq!(detail, "removed 1 expired export directories");
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(encoding.exists());
        assert!(not_a_job.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An offline viewer bundle is retired on age like any other export, which
    /// is what keeps a copy of every tileset it holds from sitting on disk
    /// forever.
    #[tokio::test]
    async fn pruning_export_files_removes_an_aged_offline_viewer_bundle() {
        let dir = std::env::temp_dir().join(format!("tiletopia_prune_{}", Uuid::new_v4()));
        let asset_id = Uuid::new_v4();
        let asset_dir = dir.join(asset_id.to_string());
        std::fs::create_dir_all(asset_dir.join("tiles")).unwrap();
        std::fs::write(asset_dir.join("tileset.json"), r#"{"asset":{}}"#).unwrap();
        std::fs::write(asset_dir.join("tiles/0.pnts"), b"tile bytes").unwrap();

        let engine = crate::export::ExportEngine::new();
        let job = engine
            .create_export(
                Uuid::new_v4(),
                asset_id,
                crate::export::ExportFormat::OfflineViewer,
                None,
            )
            .await;
        let bundle = engine.execute_export(job.id, &dir).await.unwrap();

        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * SECONDS_PER_DAY);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&bundle)
            .unwrap()
            .set_modified(three_days_ago)
            .unwrap();

        let detail = prune_export_files(&dir, 1).await.unwrap();
        assert_eq!(detail, "removed 1 expired export directories");
        assert!(!bundle.exists());
        // the asset's own tiles are not an export, so they stay
        assert!(asset_dir.join("tileset.json").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn pruning_export_files_on_a_server_that_has_exported_nothing_is_not_a_failure() {
        let dir = std::env::temp_dir().join(format!("tiletopia_prune_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            prune_export_files(&dir, 7).await.unwrap(),
            "nothing has been exported"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
