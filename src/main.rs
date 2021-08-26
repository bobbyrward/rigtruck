use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Clap;
use gitlab::api::{projects, AsyncQuery};
use gitlab::types::{Job, Pipeline, Project, RepoCommitDetail, StatusState};
use gitlab::{AsyncGitlab, GitlabBuilder};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

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
struct Args {
    #[clap(long)]
    pat: Option<String>,

    #[clap(long, default_value = ".")]
    path: String,
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
    gl: Arc<AsyncGitlab>,
    project_id: u64,
    job_id: u64,
    progress: ProgressBar,
) -> Result<()> {
    loop {
        let job: Job = projects::jobs::Job::builder()
            .project(project_id)
            .job(job_id)
            .build()
            .map_err(|e| anyhow!("Gitlab error: {}", e))?
            .query_async(gl.as_ref())
            .await
            .context("Failed getting job")?;

        match job.status {
            StatusState::Pending | StatusState::Created | StatusState::Running => {
                progress.tick();
            }
            StatusState::Success => {
                progress.finish_and_clear();
                break;
            }
            _ => {
                progress.finish_with_message(format!("{:?}", job.status));
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    Ok(())
}

async fn poll_jobs(
    gl: Arc<AsyncGitlab>,
    project: u64,
    pipeline: u64,
) -> Result<HashMap<String, Vec<Job>>> {
    let jobs: Vec<Job> = projects::pipelines::PipelineJobs::builder()
        .project(project)
        .pipeline(pipeline)
        .build()
        .map_err(|e| anyhow!("Gitlab error: {}", e))?
        .query_async(gl.as_ref())
        .await
        .context("PipelineJobs failed")?;

    let mut result: HashMap<String, Vec<Job>> = HashMap::new();

    for job in jobs {
        result.entry(job.stage.clone()).or_default().push(job);
    }

    Ok(result)
}

async fn get_repo(path: &str) -> Result<git2::Repository> {
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let pat = if let Some(pat) = args.pat {
        pat
    } else {
        parse_netrc().await?
    };

    let gl = Arc::new(GitlabBuilder::new("gitlab.com", pat).build_async().await?);

    let repo = get_repo(&args.path).await?;
    let head = repo.head()?.peel_to_commit()?;
    let remote = repo.find_remote("origin")?;
    let remote_url = remote.url().ok_or_else(|| anyhow!("Missing origin url"))?;

    let project_path = get_project_path(remote_url)?;

    let project: Project = projects::Project::builder()
        .project(project_path)
        .build()
        .map_err(|e| anyhow!("Gitlab error: {}", e))?
        .query_async(gl.as_ref())
        .await
        .context("Failed getting project")?;

    let commit: RepoCommitDetail = projects::repository::commits::Commit::builder()
        .project(project.id.value())
        .commit(head.id().to_string())
        .build()
        .map_err(|e| anyhow!("Gitlab error: {}", e))?
        .query_async(gl.as_ref())
        .await
        .context("Failed getting commit")?;

    if let Some(ref last_pipeline) = commit.last_pipeline {
        println!(
            "{}\n\nPipeline {}: {}\nURL: {}\n",
            project.name_with_namespace,
            last_pipeline.id.value(),
            commit.title,
            last_pipeline.web_url,
        );

        let spinner_style = ProgressStyle::default_spinner()
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ")
            .template("  * [{msg}]: {spinner}");

        match last_pipeline.status {
            StatusState::Created
            | StatusState::Pending
            | StatusState::Running
            | StatusState::Success
            | StatusState::Failed
            | StatusState::Canceled => {
                for stage in STAGES {
                    let jobs =
                        poll_jobs(gl.clone(), project.id.value(), last_pipeline.id.value()).await?;

                    if let Some(stage_jobs) = jobs.get::<str>(stage) {
                        let stage_multi_pb = MultiProgress::new();
                        let stage_pb =
                            stage_multi_pb.add(ProgressBar::new(stage_jobs.len() as u64));
                        stage_pb.set_style(
                            ProgressStyle::default_bar()
                                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ")
                                .template(&format!("[{}]: {{spinner}}{{msg}}", stage)),
                        );

                        stage_pb.enable_steady_tick(300);

                        let stage_job_handles = stage_jobs
                            .iter()
                            .map(|job| {
                                let spinner = stage_multi_pb.add(ProgressBar::new_spinner());
                                spinner.set_style(spinner_style.clone());
                                // spinner.set_prefix(format!("
                                spinner.enable_steady_tick(300);
                                spinner.set_message(job.name.clone());

                                tokio::spawn(poll_job(
                                    gl.clone(),
                                    project.id.value(),
                                    job.id.value(),
                                    spinner,
                                ))
                            })
                            .collect::<Vec<_>>();

                        let handle_multi =
                            tokio::task::spawn_blocking(move || stage_multi_pb.join().unwrap()); // add this line

                        let result = futures_util::future::try_join_all(stage_job_handles).await;
                        result?;

                        stage_pb.finish_with_message(format!("Success"));
                        let _ = handle_multi.await;
                    } else {
                        continue;
                    }
                }

                let pipeline: Pipeline = projects::pipelines::Pipeline::builder()
                    .project(project.id.value())
                    .pipeline(last_pipeline.id.value())
                    .build()
                    .map_err(|e| anyhow!("Gitlab error: {}", e))?
                    .query_async(gl.as_ref())
                    .await
                    .context("Failed getting pipeline")?;

                // Give gitlab a chance to update the status of the pipeline
                tokio::time::sleep(Duration::from_millis(500)).await;

                println!("\nStatus: {:?}", pipeline.status);
            }
            _ => println!("\nStatus: {:?}", last_pipeline.status),
        };

        // Do stuff with last_pipeline
    } else {
        return Err(anyhow!("Commit {} has no associated pipelines", head.id()));
    }

    Ok(())
}
