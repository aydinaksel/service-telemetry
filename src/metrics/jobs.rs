use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const JOB_RUNS_TOTAL: &str = "job_runs_total";
pub const JOB_DURATION_SECONDS: &str = "job_duration_seconds";
pub const JOB_LAST_SUCCESS_TIMESTAMP_SECONDS: &str = "job_last_success_timestamp_seconds";

const JOB_LABEL: &str = "job_name";
const OUTCOME_LABEL: &str = "outcome";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    Success,
    Failure,
}

impl JobOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug)]
#[must_use = "a job run records nothing until it is finished"]
pub struct JobRun {
    job: &'static str,
    started_at: Instant,
}

impl JobRun {
    pub fn start(job: &'static str) -> Self {
        Self {
            job,
            started_at: Instant::now(),
        }
    }

    pub fn finish(self, outcome: JobOutcome) {
        let labels = [(JOB_LABEL, self.job), (OUTCOME_LABEL, outcome.label())];
        ::metrics::counter!(JOB_RUNS_TOTAL, &labels).increment(1);
        ::metrics::histogram!(JOB_DURATION_SECONDS, &labels).record(self.started_at.elapsed());
        if outcome == JobOutcome::Success
            && let Ok(since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH)
        {
            record_last_success(self.job, since_epoch.as_secs());
        }
    }
}

pub fn record_last_success(job: &'static str, unix_seconds: u64) {
    ::metrics::gauge!(JOB_LAST_SUCCESS_TIMESTAMP_SECONDS, JOB_LABEL => job)
        .set(Seconds(unix_seconds));
}

struct Seconds(u64);

impl ::metrics::IntoF64 for Seconds {
    #[expect(
        clippy::cast_precision_loss,
        reason = "Unix seconds stay below 2^53 for the next 285 million years"
    )]
    #[expect(
        clippy::as_conversions,
        reason = "u64 to f64 has no lossless conversion in std"
    )]
    fn into_f64(self) -> f64 {
        self.0 as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::tests::{scrape, test_recorder};

    const PERSISTED_SUCCESS_UNIX_SECONDS: u64 = 1_763_000_000;

    #[tokio::test]
    async fn runs_are_counted_and_timed_by_job_and_outcome() {
        let recorder = test_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            JobRun::start("floor_and_decor_sync").finish(JobOutcome::Success);
            JobRun::start("floor_and_decor_sync").finish(JobOutcome::Failure);
            JobRun::start("floor_and_decor_sync").finish(JobOutcome::Failure);
        });

        let text = scrape(recorder.handle()).await;

        assert!(
            text.contains(r#"job_runs_total{job_name="floor_and_decor_sync",outcome="success"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"job_runs_total{job_name="floor_and_decor_sync",outcome="failure"} 2"#),
            "{text}"
        );
        assert!(
            text.contains(
                r#"job_duration_seconds_count{job_name="floor_and_decor_sync",outcome="failure"} 2"#
            ),
            "{text}"
        );
    }

    #[tokio::test]
    async fn labels_never_clash_with_the_scrape_target_labels() {
        let recorder = test_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            JobRun::start("floor_and_decor_sync").finish(JobOutcome::Success);
        });

        let text = scrape(recorder.handle()).await;

        for reserved_label in ["job", "instance"] {
            assert!(!text.contains(&format!("{{{reserved_label}=\"")), "{text}");
            assert!(!text.contains(&format!(",{reserved_label}=\"")), "{text}");
        }
    }

    #[tokio::test]
    async fn only_a_success_moves_the_last_success_time() {
        let recorder = test_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            record_last_success("eta_etd_handler", PERSISTED_SUCCESS_UNIX_SECONDS);
            JobRun::start("eta_etd_handler").finish(JobOutcome::Failure);
        });

        let text = scrape(recorder.handle()).await;

        assert!(
            text.contains(&format!(
                r#"job_last_success_timestamp_seconds{{job_name="eta_etd_handler"}} {PERSISTED_SUCCESS_UNIX_SECONDS}"#
            )),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_success_sets_the_last_success_time_to_now() {
        let recorder = test_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            JobRun::start("source_image_sync").finish(JobOutcome::Success);
        });

        let text = scrape(recorder.handle()).await;

        let recorded_unix_seconds: f64 = text
            .lines()
            .find_map(|line| {
                line.strip_prefix(
                    r#"job_last_success_timestamp_seconds{job_name="source_image_sync"} "#,
                )
            })
            .unwrap()
            .parse()
            .unwrap();
        assert!(recorded_unix_seconds > 1_700_000_000.0, "{text}");
    }
}
