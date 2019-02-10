use crate::config::Config;
use crate::errors::RunError;
use crate::traces::{CoverageStat, TraceMap};
use coveralls_api::*;
use log::{info, warn};
use std::collections::HashMap;
use std::path::Path;

fn get_git_info(manifest_path: &Path) -> Result<GitInfo, String> {
    let dir_path = manifest_path
        .parent()
        .ok_or_else(|| format!("failed to get parent for path: {}", manifest_path.display()))?;
    let repo = git2::Repository::discover(dir_path).map_err(|err| {
        format!(
            "failed to open git repository: {}: {}",
            dir_path.display(),
            err
        )
    })?;

    let head = repo
        .head()
        .map_err(|err| format!("failed to get repository head: {}", err))?;
    let branch = git2::Branch::wrap(head);
    let branch_name = branch
        .name()
        .map_err(|err| format!("failed to get branch name: {}", err))?;
    let get_string = |data: Option<&str>| match data {
        Some(str) => Ok(str.to_string()),
        None => Err("string is not valid utf-8".to_string()),
    };
    let branch_name = get_string(branch_name)?;
    let commit = repo
        .head()
        .unwrap()
        .peel_to_commit()
        .map_err(|err| format!("failed to get commit: {}", err))?;

    let author = commit.author();
    let committer = commit.committer();
    Ok(GitInfo {
        head: Head {
            id: commit.id().to_string(),
            author_name: get_string(author.name())?,
            author_email: get_string(author.email())?,
            committer_name: get_string(committer.name())?,
            committer_email: get_string(committer.email())?,
            message: get_string(commit.message())?,
        },
        branch: branch_name,
        remotes: Vec::new(),
    })
}

pub fn export(coverage_data: &TraceMap, config: &Config) -> Result<(), RunError> {
    if let Some(ref key) = config.coveralls {
        let id = match config.ci_tool {
            Some(ref service) => Identity::ServiceToken(Service {
                service_name: service.clone(),
                service_job_id: key.clone(),
            }),
            _ => Identity::RepoToken(key.clone()),
        };
        let mut report = CoverallsReport::new(id);
        for file in &coverage_data.files() {
            let rel_path = config.strip_project_path(file);
            let mut lines: HashMap<usize, usize> = HashMap::new();
            let fcov = coverage_data.get_child_traces(file);

            for c in &fcov {
                match c.stats {
                    CoverageStat::Line(hits) => {
                        lines.insert(c.line as usize, hits as usize);
                    }
                    _ => {
                        info!("Support for coverage statistic not implemented or supported for coveralls.io");
                    }
                }
            }
            if let Ok(source) = Source::new(&rel_path, file, &lines, &None, false) {
                report.add_source(source);
            }
        }

        match get_git_info(&config.manifest) {
            Ok(git_info) => {
                report.set_detailed_git_info(git_info);
                info!("Git info collected");
            }
            Err(err) => warn!("Failed to collect git info: {}", err),
        }

        let res = match config.report_uri {
            Some(ref uri) => {
                info!("Sending report to endpoint: {}", uri);
                report.send_to_endpoint(uri)
            }
            None => {
                info!("Sending coverage data to coveralls.io");
                report.send_to_coveralls()
            }
        };

        match res {
            Ok(_) => Ok(()),
            Err(e) => Err(RunError::CovReport(format!("Coveralls send failed. {}", e))),
        }
    } else {
        Err(RunError::CovReport(
            "No coveralls key specified.".to_string(),
        ))
    }
}
