use ai::skills::{ParsedSkill, SkillProvider, SkillScope};
use warp_util::host_id::HostId;
use warp_util::local_or_remote_path::LocalOrRemotePath;

use super::*;

fn bundled_skill(content: &str) -> BundledSkill {
    let mut bundled_skill = BundledSkill::default();
    bundled_skill.insert_for_testing(
        "test-skill",
        ParsedSkill {
            name: "test-skill".to_string(),
            description: "Test skill".to_string(),
            path: LocalOrRemotePath::Local("/bundled/skills/test-skill/SKILL.md".into()),
            content: content.to_string(),
            line_range: None,
            provider: SkillProvider::Warp,
            scope: SkillScope::Bundled,
        },
        BundledSkillActivation::Always,
    );
    bundled_skill
}

fn ready_content<'a>(bundled_skills: &'a BundledSkills, host_id: &HostId) -> Option<&'a str> {
    bundled_skills
        .remote(host_id)?
        .skill("test-skill")
        .map(|skill| skill.content.as_str())
}

#[test]
fn remote_bootstrap_is_idempotent_until_host_is_removed() {
    let host_id = HostId::new("host".to_string());
    let mut bundled_skills = BundledSkills::default();

    let bootstrap = bundled_skills
        .begin_remote_bootstrap(host_id.clone())
        .expect("first bootstrap should start");
    assert!(bundled_skills
        .begin_remote_bootstrap(host_id.clone())
        .is_none());
    assert_eq!(ready_content(&bundled_skills, &host_id), None);

    assert!(bundled_skills.complete_remote_bootstrap(bootstrap, bundled_skill("ready")));
    assert_eq!(ready_content(&bundled_skills, &host_id), Some("ready"));
    assert!(bundled_skills.begin_remote_bootstrap(host_id).is_none());
}

#[test]
fn removing_host_discards_bootstrapping_and_ready_catalogs() {
    let bootstrapping_host_id = HostId::new("bootstrapping".to_string());
    let ready_host_id = HostId::new("ready".to_string());
    let mut bundled_skills = BundledSkills::default();

    let disconnected_bootstrap = bundled_skills
        .begin_remote_bootstrap(bootstrapping_host_id.clone())
        .expect("bootstrap should start");
    let ready_bootstrap = bundled_skills
        .begin_remote_bootstrap(ready_host_id.clone())
        .expect("bootstrap should start");
    assert!(bundled_skills.complete_remote_bootstrap(ready_bootstrap, bundled_skill("ready")));

    bundled_skills.remove_remote(&bootstrapping_host_id);
    bundled_skills.remove_remote(&ready_host_id);
    assert!(!bundled_skills
        .complete_remote_bootstrap(disconnected_bootstrap, bundled_skill("disconnected")));

    assert_eq!(ready_content(&bundled_skills, &bootstrapping_host_id), None);
    assert_eq!(ready_content(&bundled_skills, &ready_host_id), None);
    assert!(bundled_skills
        .begin_remote_bootstrap(bootstrapping_host_id)
        .is_some());
    assert!(bundled_skills
        .begin_remote_bootstrap(ready_host_id)
        .is_some());
}

#[test]
fn stale_completion_cannot_replace_catalog_after_reconnect() {
    let host_id = HostId::new("host".to_string());
    let mut bundled_skills = BundledSkills::default();

    let stale_bootstrap = bundled_skills
        .begin_remote_bootstrap(host_id.clone())
        .expect("initial bootstrap should start");
    bundled_skills.remove_remote(&host_id);
    let current_bootstrap = bundled_skills
        .begin_remote_bootstrap(host_id.clone())
        .expect("reconnect bootstrap should start");
    assert_ne!(stale_bootstrap.generation, current_bootstrap.generation);

    assert!(!bundled_skills.complete_remote_bootstrap(stale_bootstrap, bundled_skill("stale")));
    assert_eq!(ready_content(&bundled_skills, &host_id), None);

    assert!(bundled_skills.complete_remote_bootstrap(current_bootstrap, bundled_skill("current")));
    assert_eq!(ready_content(&bundled_skills, &host_id), Some("current"));
}

#[test]
fn local_and_remote_catalogs_are_isolated() {
    let first_host_id = HostId::new("first-host".to_string());
    let second_host_id = HostId::new("second-host".to_string());
    let mut bundled_skills = BundledSkills::default();
    bundled_skills.set_local(bundled_skill("local"));

    let first_bootstrap = bundled_skills
        .begin_remote_bootstrap(first_host_id.clone())
        .expect("first host bootstrap should start");
    let second_bootstrap = bundled_skills
        .begin_remote_bootstrap(second_host_id.clone())
        .expect("second host bootstrap should start");
    assert!(bundled_skills.complete_remote_bootstrap(first_bootstrap, bundled_skill("first")));
    assert!(bundled_skills.complete_remote_bootstrap(second_bootstrap, bundled_skill("second")));

    assert_eq!(
        bundled_skills
            .local()
            .skill("test-skill")
            .map(|skill| skill.content.as_str()),
        Some("local")
    );
    assert_eq!(ready_content(&bundled_skills, &first_host_id), Some("first"));
    assert_eq!(
        ready_content(&bundled_skills, &second_host_id),
        Some("second")
    );

    bundled_skills.remove_remote(&first_host_id);
    assert_eq!(
        bundled_skills
            .local()
            .skill("test-skill")
            .map(|skill| skill.content.as_str()),
        Some("local")
    );
    assert_eq!(ready_content(&bundled_skills, &first_host_id), None);
    assert_eq!(
        ready_content(&bundled_skills, &second_host_id),
        Some("second")
    );
}
