use super::should_run_autosave;
use warp_util::content_version::ContentVersion;

#[test]
fn autosave_runs_only_for_the_unchanged_dirty_version() {
    let scheduled = ContentVersion::from_raw(10);

    assert_eq!(should_run_autosave(scheduled, scheduled, true, false), true);
    assert_eq!(
        should_run_autosave(scheduled, ContentVersion::from_raw(11), true, false),
        false
    );
}

#[test]
fn autosave_skips_clean_or_conflicted_buffers() {
    let version = ContentVersion::from_raw(10);

    assert_eq!(should_run_autosave(version, version, false, false), false);
    assert_eq!(should_run_autosave(version, version, true, true), false);
}
