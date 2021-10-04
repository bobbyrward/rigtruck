mod gitlab_api;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Clap;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::gitlab_api::{GitlabAPI, Job, StatusState};

const JOB_SPINNER_TEMPLATE: &'static str = "  * [{msg}]: {spinner}";

const STAGES: &'static [&'static str] = &[
    "prebuild",
    "test",
    "build",
    "postbuild",
    "rollout",
    "release",
    "deploy",
];

#[derive(Debug, Clone, Clap)]
#[clap(version=clap::crate_version!())]
struct Args {
    #[clap(long)]
    /// Gitlab personal access token.  If not provided, it will attempt to parse ~/.netrc.
    pat: Option<String>,

    #[clap(long, default_value = ".")]
    /// The path to the git repository to use for metadata
    path: String,

    #[clap(long, short)]
    failure_logs: bool,

    #[clap(long)]
    /// Disable notications on completion
    no_notification: bool,
}

impl Args {
    async fn get_pat(&self) -> Result<String> {
        if let Some(pat) = &self.pat {
            Ok(pat.clone())
        } else {
            parse_netrc().await
        }
    }
}

fn is_terminal_state(state: StatusState) -> bool {
    matches!(
        state,
        StatusState::Success | StatusState::Failed | StatusState::Canceled
    )
}

fn is_running_state(state: StatusState) -> bool {
    matches!(
        state,
        StatusState::Created | StatusState::Pending | StatusState::Running
    )
}

fn is_tailable_state(state: StatusState) -> bool {
    is_terminal_state(state) || is_running_state(state)
}

fn create_spinner(template: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style(template));
    pb
}

fn spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::default_spinner()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ")
        .template(template)
}

async fn parse_netrc() -> Result<String> {
    let mut path = dirs::home_dir().ok_or_else(|| anyhow!("Couldn't get home directory"))?;
    path.push(".netrc");

    let contents = tokio::fs::read_to_string(&path).await?;

    for line in contents.trim().lines() {
        let parts = line.split_whitespace().collect::<Vec<_>>();

        if parts.len() < 2 {
            continue;
        }

        if parts[0] == "login" {
            return Ok(parts[1].to_string());
        }
    }

    Err(anyhow!("unable to parse .netrc"))
}

async fn poll_job(
    api: Arc<GitlabAPI>,
    project_id: u64,
    job_id: u64,
    progress: ProgressBar,
) -> Result<(u64, String, StatusState)> {
    loop {
        let job = api.get_job(project_id, job_id).await?;

        match job.status {
            StatusState::Pending | StatusState::Created | StatusState::Running => {
                progress.tick();
            }
            StatusState::Success => {
                progress.finish_and_clear();
                return Ok((job_id, job.name, job.status));
            }
            _ => {
                progress.finish_with_message(format!("{:?}", job.status));
                return Ok((job_id, job.name, job.status));
            }
        }

        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}

async fn poll_jobs(
    api: Arc<GitlabAPI>,
    project_id: u64,
    pipeline_id: u64,
) -> Result<HashMap<String, Vec<Job>>> {
    let jobs = api.get_pipeline_jobs(project_id, pipeline_id).await?;

    let mut result: HashMap<String, Vec<Job>> = HashMap::new();

    for job in jobs {
        result.entry(job.stage.clone()).or_default().push(job);
    }

    Ok(result)
}

fn get_repo(path: &str) -> Result<git2::Repository> {
    Ok(git2::Repository::discover(path)?)
}

fn get_project_path(origin_url: &str) -> Result<&str> {
    if origin_url.starts_with("https://gitlab.com/") && origin_url.ends_with(".git") {
        Ok(&origin_url[19..(origin_url.len() - 4)])
    } else if origin_url.starts_with("git@gitlab.com:") && origin_url.ends_with(".git") {
        Ok(&origin_url[15..(origin_url.len() - 4)])
    } else {
        Err(anyhow!(
            "Unable to parse project from origin url: {}",
            origin_url
        ))
    }
}

#[derive(Debug, Clone, Default)]
struct StageResults {
    succeeded: Vec<(u64, String)>,
    failed: Vec<(u64, String)>,
}

async fn monitor_stage(
    api: Arc<GitlabAPI>,
    project_id: u64,
    pipeline_id: u64,
    stage: &str,
) -> Result<StageResults> {
    let jobs = poll_jobs(api.clone(), project_id, pipeline_id).await?;

    if let Some(stage_jobs) = jobs.get::<str>(stage) {
        let stage_multi_pb = Arc::new(MultiProgress::new());
        let stage_pb =
            stage_multi_pb.add(create_spinner(&format!("[{}]: {{spinner}}{{msg}}", stage)));

        let handle_multi = tokio::task::spawn_blocking({
            let bg_stage_multi_pb = stage_multi_pb.clone();

            move || bg_stage_multi_pb.join().unwrap()
        });

        // This needs to go after starting the multi in the background
        stage_pb.enable_steady_tick(300);

        let stage_job_handles = stage_jobs
            .iter()
            .map(|job| {
                let spinner = stage_multi_pb.add(create_spinner(JOB_SPINNER_TEMPLATE));
                spinner.set_message(job.name.clone());
                spinner.enable_steady_tick(300);

                tokio::spawn(poll_job(api.clone(), project_id, job.id.value(), spinner))
            })
            .collect::<Vec<_>>();

        let results = futures_util::future::try_join_all(stage_job_handles)
            .await?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        let mut succeeded = vec![];
        let mut failed = vec![];

        for (job_id, job_name, state) in results {
            match state {
                StatusState::Failed => failed.push((job_id, job_name)),
                _ => succeeded.push((job_id, job_name)),
            };
        }

        stage_pb.finish_with_message(format!("Success"));
        let _ = handle_multi.await;

        return Ok(StageResults { succeeded, failed });
    }

    Ok(StageResults::default())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let api = Arc::new(GitlabAPI::new(&args.get_pat().await?).await?);

    let repo = get_repo(&args.path)?;
    let head = repo.head()?.peel_to_commit()?;
    let remote = repo.find_remote("origin")?;
    let remote_url = remote.url().ok_or_else(|| anyhow!("Missing origin url"))?;

    let project = api.get_project(get_project_path(remote_url)?).await?;
    let commit = api
        .get_commit(project.id.value(), &head.id().to_string())
        .await?;

    let last_pipeline = commit
        .last_pipeline
        .ok_or_else(|| anyhow!("Commit {} has no associated pipelines", head.id()))?;

    println!(
        "{}\n\nPipeline {}: {}\nURL: {}\n",
        project.name_with_namespace,
        last_pipeline.id.value(),
        commit.title,
        last_pipeline.web_url,
    );

    if is_tailable_state(last_pipeline.status) {
        let initial_status = last_pipeline.status;

        let mut failed_jobs = vec![];

        for stage in STAGES {
            let mut stage_results = monitor_stage(
                api.clone(),
                project.id.value(),
                last_pipeline.id.value(),
                stage,
            )
            .await?;

            if !stage_results.failed.is_empty() {
                failed_jobs.append(&mut stage_results.failed);
            }
        }

        // Give gitlab a chance to update the status of the pipeline
        tokio::time::sleep(Duration::from_millis(500)).await;

        let pipeline = api
            .get_pipeline(project.id.value(), last_pipeline.id.value())
            .await?;

        println!("\nStatus: {:?}", pipeline.status);

        if !args.no_notification && is_terminal_state(initial_status) {
            // do notication
            let mut notification = notify_rust::Notification::new();

            notification
                .summary(&format!(
                    "{} pipeline finished: {:?}",
                    project.name, pipeline.status,
                ))
                .sound_name("dialog-information")
                .timeout(notify_rust::Timeout::Milliseconds(6000));

            if !failed_jobs.is_empty() {
                notification.body(&format!(
                    "The following jobs failed: \n* {}",
                    failed_jobs
                        .iter()
                        .map(|(_, name)| name.as_str())
                        .collect::<Vec<_>>()
                        .join("\n* ")
                ));
            }

            notification.show()?;
        }

        if args.failure_logs && !failed_jobs.is_empty() {
            for (job_id, job_name) in failed_jobs {
                println!(
                    "{}:\n{}\n\n",
                    job_name,
                    api.get_job_trace(project.id.value(), job_id).await?
                );
            }
        }
    } else {
        println!("\nStatus: {:?}", last_pipeline.status);
    }

    Ok(())
}
