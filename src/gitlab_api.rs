use anyhow::{anyhow, Context, Result};
use gitlab::api::{projects, ApiError, AsyncQuery};
use gitlab::{AsyncGitlab, GitlabBuilder};

pub use gitlab::types::{Job, Pipeline, Project, RepoCommitDetail, StatusState};

pub struct GitlabAPI {
    gl: AsyncGitlab,
}

impl GitlabAPI {
    pub async fn new(pat: &str) -> Result<Self> {
        Ok(Self {
            gl: GitlabBuilder::new("gitlab.com", pat).build_async().await?,
        })
    }

    pub async fn get_project(&self, project_path: &str) -> Result<Project> {
        projects::Project::builder()
            .project(project_path)
            .build()
            .map_err(|e| anyhow!("Gitlab error: {}", e))?
            .query_async(&self.gl)
            .await
            .context("Failed getting project")
    }

    pub async fn get_commit(&self, project_id: u64, sha: &str) -> Result<RepoCommitDetail> {
        projects::repository::commits::Commit::builder()
            .project(project_id)
            .commit(sha)
            .build()
            .map_err(|e| anyhow!("Gitlab error: {}", e))?
            .query_async(&self.gl)
            .await
            .context("Failed getting commit")
    }

    pub async fn get_pipeline(&self, project_id: u64, pipeline_id: u64) -> Result<Pipeline> {
        projects::pipelines::Pipeline::builder()
            .project(project_id)
            .pipeline(pipeline_id)
            .build()
            .map_err(|e| anyhow!("Gitlab error: {}", e))?
            .query_async(&self.gl)
            .await
            .context("Failed getting pipeline")
    }
    pub async fn get_job(&self, project_id: u64, job_id: u64) -> Result<Job> {
        projects::jobs::Job::builder()
            .project(project_id)
            .job(job_id)
            .build()
            .map_err(|e| anyhow!("Gitlab error: {}", e))?
            .query_async(&self.gl)
            .await
            .context("Failed getting job")
    }

    pub async fn get_pipeline_jobs(&self, project_id: u64, pipeline_id: u64) -> Result<Vec<Job>> {
        projects::pipelines::PipelineJobs::builder()
            .project(project_id)
            .pipeline(pipeline_id)
            .build()
            .map_err(|e| anyhow!("Gitlab error: {}", e))?
            .query_async(&self.gl)
            .await
            .context("PipelineJobs failed")
    }

    pub async fn get_job_trace(&self, project_id: u64, job_id: u64) -> Result<String> {
        let result: Result<(), _> = projects::jobs::JobTrace::builder()
            .project(project_id)
            .job(job_id)
            .build()
            .map_err(|e| anyhow!("Gitlab error: {}", e))?
            .query_async(&self.gl)
            .await;

        let err = result.expect_err("This should always be an error unless the gitlab crate stops parsing the response as json.  It's plain text");

        if let ApiError::GitlabService { status, data } = err {
            if status.as_u16() != 200 {
                panic!("This should return 200.  Something has changed.");
            }

            Ok(std::str::from_utf8(&data)
                .context("Job trace response is not valid utf8.")?
                .to_owned())
        } else {
            panic!("This should be a GitlabService error. The gitlab crate has changed.");
        }
    }
}
