use std::cell::Cell;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, RequestBuilder, multipart};
use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use super::photo_set::for_each_photo;
use super::{COORDS_FILE, PhotogrammetryPreset, TEXTURING_DIR, stop_if_deleted};
use crate::db::JobRecord;

pub const NODE_ODM_URL_ENV: &str = "TILETOPIA_NODEODM_URL";
pub const TIMEOUT_ENV: &str = "TILETOPIA_PHOTOGRAMMETRY_TIMEOUT_SECS";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const CLEANUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_LINES_IN_ERROR: usize = 20;
const DOWNLOAD_CHUNK_BYTES: usize = 64 * 1024;

// nodeodm's TaskInfo status codes
const QUEUED: i64 = 10;
const RUNNING: i64 = 20;
const FAILED: i64 = 30;
const COMPLETED: i64 = 40;
const CANCELED: i64 = 50;

const UPLOAD_SHARE: f64 = 0.1;
const PROCESSING_SHARE: f64 = 0.8;

#[derive(Debug, Clone)]
pub struct NodeOdm {
    pub base_url: String,
    pub timeout: Duration,
    pub poll_interval: Duration,
}

impl NodeOdm {
    pub fn from_env() -> Result<Option<Self>, String> {
        let Ok(base_url) = std::env::var(NODE_ODM_URL_ENV) else {
            return Ok(None);
        };
        reqwest::Url::parse(base_url.trim())
            .map_err(|e| format!("{NODE_ODM_URL_ENV}={base_url}: {e}"))?;
        let timeout = match std::env::var(TIMEOUT_ENV) {
            Err(_) => DEFAULT_TIMEOUT,
            Ok(seconds) => seconds
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|seconds| *seconds > 0)
                .map(Duration::from_secs)
                .ok_or_else(|| {
                    format!("{TIMEOUT_ENV}={seconds}: expected a whole number of seconds above 0")
                })?,
        };
        Ok(Some(Self {
            base_url: base_url.trim().to_string(),
            timeout,
            poll_interval: POLL_INTERVAL,
        }))
    }
}

struct Session<'a> {
    node: &'a NodeOdm,
    client: Client,
    deadline: Cell<Instant>,
    cancellation: &'a CancellationToken,
}

pub(super) fn run_task(
    node: &NodeOdm,
    job: &JobRecord,
    result_zip: &Path,
    cancellation: &CancellationToken,
    photo_transfer_slot: OwnedSemaphorePermit,
    report_progress: &dyn Fn(f64),
) -> Result<(), String> {
    let session = Session {
        node,
        client: Client::new(),
        deadline: Cell::new(Instant::now() + node.timeout),
        cancellation,
    };
    let uuid = session.init(job.photogrammetry_preset)?;
    let outcome = session.process(
        &uuid,
        Path::new(&job.input_path),
        result_zip,
        photo_transfer_slot,
        report_progress,
    );
    // cancelling makes nodeodm kill the odm process tree
    if outcome.is_err() {
        session.clean_up("task/cancel", &uuid);
    }
    session.clean_up("task/remove", &uuid);
    outcome
}

fn deadline_after_status(
    status_code: i64,
    now: Instant,
    timeout: Duration,
    deadline: Instant,
) -> Instant {
    // nodeodm runs only parallel_queue_processing tasks at once, the rest wait unstarted
    if status_code == QUEUED {
        now + timeout
    } else {
        deadline
    }
}

impl Session<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.node.base_url.trim_end_matches('/'))
    }

    fn time_left(&self) -> Result<Duration, String> {
        stop_if_deleted(self.cancellation)?;
        let left = self
            .deadline
            .get()
            .saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "the reconstruction ran past {} seconds and was cancelled, {TIMEOUT_ENV} sets the limit",
                self.node.timeout.as_secs()
            ));
        }
        Ok(left)
    }

    fn request_failure(&self, what: &str, error: reqwest::Error) -> String {
        // requests time out at the reconstruction deadline
        if error.is_timeout()
            && let Err(past_deadline) = self.time_left()
        {
            return past_deadline;
        }
        format!("NodeODM {what}: {error}")
    }

    fn call(
        &self,
        request: RequestBuilder,
        timeout: Duration,
        what: &str,
    ) -> Result<Value, String> {
        let response = request
            .timeout(timeout)
            .send()
            .map_err(|error| self.request_failure(what, error))?;
        let status = response.status();
        let body: Value = response
            .json()
            .map_err(|e| format!("NodeODM {what} answered {status} with no JSON: {e}"))?;
        // nodeodm answers a refused call with 200 and an error field
        if let Some(error) = body.get("error").and_then(Value::as_str) {
            return Err(format!("NodeODM {what}: {error}"));
        }
        if !status.is_success() {
            return Err(format!("NodeODM {what} answered {status}: {body}"));
        }
        Ok(body)
    }

    fn require_success(
        &self,
        request: RequestBuilder,
        timeout: Duration,
        what: &str,
    ) -> Result<(), String> {
        let body = self.call(request, timeout, what)?;
        if body.get("success") != Some(&Value::Bool(true)) {
            return Err(format!("NodeODM {what} answered {body}"));
        }
        Ok(())
    }

    fn init(&self, preset: PhotogrammetryPreset) -> Result<String, String> {
        let form = multipart::Form::new()
            .text("options", preset.odm_options().to_string())
            .text(
                "outputs",
                serde_json::json!([TEXTURING_DIR, COORDS_FILE]).to_string(),
            )
            .text("skipPostProcessing", "true");
        let body = self.call(
            self.client.post(self.url("task/new/init")).multipart(form),
            self.time_left()?,
            "task init",
        )?;
        body.get("uuid")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("NodeODM task init answered {body} with no uuid"))
    }

    fn process(
        &self,
        uuid: &str,
        photo_zip: &Path,
        result_zip: &Path,
        photo_transfer_slot: OwnedSemaphorePermit,
        report_progress: &dyn Fn(f64),
    ) -> Result<(), String> {
        let upload_url = self.url(&format!("task/new/upload/{uuid}"));
        for_each_photo(photo_zip, |name, photo, done| {
            let part = multipart::Part::bytes(photo)
                .file_name(name.to_string())
                .mime_str("image/jpeg")
                .map_err(|e| e.to_string())?;
            self.require_success(
                self.client
                    .post(&upload_url)
                    .multipart(multipart::Form::new().part("images", part)),
                self.time_left()?,
                "photo upload",
            )?;
            report_progress(UPLOAD_SHARE * done);
            Ok(())
        })?;
        self.call(
            self.client
                .post(self.url(&format!("task/new/commit/{uuid}"))),
            self.time_left()?,
            "task commit",
        )?;
        drop(photo_transfer_slot);

        let info_url = self.url(&format!("task/{uuid}/info"));
        loop {
            let info = self.call(self.client.get(&info_url), self.time_left()?, "task info")?;
            let code = info["status"]["code"]
                .as_i64()
                .ok_or_else(|| format!("NodeODM task info {info} has no status code"))?;
            match code {
                QUEUED | RUNNING => {
                    let percent = info["progress"]
                        .as_f64()
                        .ok_or_else(|| format!("NodeODM task info {info} has no progress"))?;
                    report_progress(UPLOAD_SHARE + PROCESSING_SHARE * percent / 100.0);
                    self.deadline.set(deadline_after_status(
                        code,
                        Instant::now(),
                        self.node.timeout,
                        self.deadline.get(),
                    ));
                    std::thread::sleep(self.node.poll_interval.min(self.time_left()?));
                }
                COMPLETED => break,
                FAILED => return Err(self.failure(uuid, &info)),
                CANCELED => return Err(format!("NodeODM task {uuid} was cancelled on the node")),
                other => {
                    return Err(format!(
                        "NodeODM task {uuid} has status code {other}, which its API does not define"
                    ));
                }
            }
        }

        self.download(uuid, result_zip)?;
        report_progress(UPLOAD_SHARE + PROCESSING_SHARE);
        Ok(())
    }

    fn failure(&self, uuid: &str, info: &Value) -> String {
        let reason = match info["status"]["errorMessage"].as_str() {
            Some(message) => format!("NodeODM task {uuid} failed: {message}"),
            None => format!("NodeODM task {uuid} failed"),
        };
        let output = self.call(
            self.client.get(self.url(&format!("task/{uuid}/output"))),
            CLEANUP_REQUEST_TIMEOUT,
            "console output",
        );
        let tail = match output {
            Ok(Value::Array(lines)) => {
                let start = lines.len().saturating_sub(OUTPUT_LINES_IN_ERROR);
                lines[start..]
                    .iter()
                    .map(|line| {
                        line.as_str()
                            .map_or_else(|| line.to_string(), str::to_string)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Ok(other) => format!("console output was {other}"),
            Err(error) => error,
        };
        format!("{reason}\n{tail}")
    }

    fn download(&self, uuid: &str, result_zip: &Path) -> Result<(), String> {
        let mut response = self
            .client
            .get(self.url(&format!("task/{uuid}/download/all.zip")))
            .timeout(self.time_left()?)
            .send()
            .map_err(|error| self.request_failure("download", error))?;
        // nodeodm answers a download it cannot serve with 200 and a JSON error
        let is_zip = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"application/zip"));
        if !is_zip {
            let status = response.status();
            let body = response
                .text()
                .map_err(|e| format!("NodeODM download: {e}"))?;
            return Err(format!("NodeODM download answered {status}: {body}"));
        }
        stop_if_deleted(self.cancellation)?;
        let mut file = std::fs::File::create(result_zip)
            .map_err(|e| format!("could not create {}: {e}", result_zip.display()))?;
        let mut chunk = vec![0u8; DOWNLOAD_CHUNK_BYTES];
        loop {
            self.time_left()?;
            let read = response
                .read(&mut chunk)
                .map_err(|e| format!("NodeODM download: {e}"))?;
            if read == 0 {
                return Ok(());
            }
            file.write_all(&chunk[..read])
                .map_err(|e| format!("could not write {}: {e}", result_zip.display()))?;
        }
    }

    fn clean_up(&self, path: &str, uuid: &str) {
        let request = self.client.post(self.url(path)).form(&[("uuid", uuid)]);
        if let Err(error) = self.require_success(request, CLEANUP_REQUEST_TIMEOUT, path) {
            tracing::warn!("NodeODM task {uuid} was left on the node: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(60);

    #[test]
    fn a_queued_task_gets_a_full_timeout_from_now_even_past_its_old_deadline() {
        let deadline = Instant::now();
        let now = deadline + Duration::from_secs(3600);
        assert_eq!(
            deadline_after_status(QUEUED, now, TIMEOUT, deadline),
            now + TIMEOUT
        );
    }

    #[test]
    fn a_running_task_keeps_its_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(5);
        assert_eq!(
            deadline_after_status(RUNNING, now, TIMEOUT, deadline),
            deadline
        );
    }
}
