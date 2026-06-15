use crate::model::ProjectStat;

/// Drop dash-separated prefix segments shared by every project name
/// ("alice/work/api" / "alice/blog" -> "work/api" / "blog") so the
/// distinctive tail survives narrow columns.
pub(super) fn strip_common_project_prefix(projects: &mut [ProjectStat]) {
    if projects.len() < 2 {
        return;
    }
    loop {
        let Some(first_segment) = projects[0].name.split('-').next().map(ToOwned::to_owned) else {
            return;
        };
        let prefix = format!("{first_segment}-");
        let all_share = projects
            .iter()
            .all(|project| project.name.starts_with(&prefix) && project.name.len() > prefix.len());
        if !all_share {
            return;
        }
        for project in projects.iter_mut() {
            project.name = project.name[prefix.len()..].to_owned();
        }
    }
}
