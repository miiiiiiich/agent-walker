use crate::model::ProjectStat;

/// Drop leading path segments shared by every project name
/// ("work/api", "work/blog" -> "api", "blog") so the distinctive tail
/// survives narrow columns. Project names keep real path separators
/// (see `collector::project_from_cwd`), so the shared prefix is slash-delimited.
pub(super) fn strip_common_project_prefix(projects: &mut [ProjectStat]) {
    if projects.len() < 2 {
        return;
    }
    loop {
        let Some((first_segment, _)) = projects[0].name.split_once('/') else {
            return;
        };
        let prefix = format!("{first_segment}/");
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

#[cfg(test)]
mod tests {
    use super::strip_common_project_prefix;
    use crate::model::{ProjectStat, TokenUsage};

    fn project(name: &str) -> ProjectStat {
        ProjectStat {
            name: name.to_owned(),
            usage: TokenUsage::default(),
        }
    }

    #[test]
    fn strips_segments_shared_by_every_project() {
        let mut projects = vec![
            project("ghq/github.com/me/api"),
            project("ghq/github.com/me/web"),
        ];
        strip_common_project_prefix(&mut projects);
        assert_eq!(projects[0].name, "api");
        assert_eq!(projects[1].name, "web");
    }

    #[test]
    fn keeps_names_when_top_segment_differs() {
        let mut projects = vec![project("work/api"), project("play/api")];
        strip_common_project_prefix(&mut projects);
        assert_eq!(projects[0].name, "work/api");
        assert_eq!(projects[1].name, "play/api");
    }

    #[test]
    fn never_strips_a_name_down_to_nothing() {
        let mut projects = vec![project("code/app"), project("code")];
        strip_common_project_prefix(&mut projects);
        assert_eq!(projects[0].name, "code/app");
        assert_eq!(projects[1].name, "code");
    }
}
