use ai::agent::action_result::{AnyFileContent, FileContext};
use ai::skills::{ParsedSkill, SkillPathOrigin, SkillReference};
use futures::future::{BoxFuture, FutureExt};
use warpui::{AppContext, Entity, ModelContext, ModelHandle, SingletonEntity};

use super::{ActionExecution, AnyActionExecution, ExecuteActionInput, PreprocessActionInput};
use crate::ai::agent::{AIAgentActionType, ReadSkillRequest, ReadSkillResult};
use crate::ai::blocklist::SessionContext;
use crate::ai::skills::{SkillManager, SkillTelemetryEvent};
use crate::send_telemetry_from_ctx;
use crate::terminal::model::session::active_session::ActiveSession;

pub struct ReadSkillExecutor {
    active_session: ModelHandle<ActiveSession>,
}

impl ReadSkillExecutor {
    pub fn new_with_active_session(active_session: ModelHandle<ActiveSession>) -> Self {
        Self { active_session }
    }

    pub(super) fn should_autoexecute(
        &self,
        _input: ExecuteActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> bool {
        // User-created skills are readable on demand.
        true
    }

    pub(super) fn execute(
        &mut self,
        input: ExecuteActionInput,
        ctx: &mut ModelContext<Self>,
    ) -> impl Into<AnyActionExecution> {
        let ExecuteActionInput { action, .. } = input;
        let AIAgentActionType::ReadSkill(ReadSkillRequest { skill: skill_ref }) = &action.action
        else {
            return ActionExecution::<Result<ParsedSkill, String>>::InvalidAction;
        };

        let skill_ref = skill_ref.clone();
        let skill_manager = SkillManager::as_ref(ctx);

        let path_origin =
            SessionContext::from_session(self.active_session.as_ref(ctx), ctx).skill_path_origin();

        let result = skill_manager
            .active_skill_by_reference_with_origin(&skill_ref, &path_origin, ctx)
            .cloned()
            .ok_or_else(|| {
                if matches!(&path_origin, SkillPathOrigin::Unavailable)
                    && matches!(&skill_ref, SkillReference::BundledSkillId(_))
                {
                    "Bundled skills are not available on this remote session".to_string()
                } else {
                    format!("Skill not found: {skill_ref:?}")
                }
            });
        ActionExecution::Sync(finish_skill_read(&skill_ref, result, ctx).into())
    }

    pub(super) fn preprocess_action(
        &mut self,
        _input: PreprocessActionInput,
        _ctx: &mut ModelContext<Self>,
    ) -> BoxFuture<'static, ()> {
        futures::future::ready(()).boxed()
    }
}

fn finish_skill_read(
    skill_ref: &SkillReference,
    result: Result<ParsedSkill, String>,
    ctx: &mut AppContext,
) -> ReadSkillResult {
    match result {
        Ok(skill) => {
            send_telemetry_from_ctx!(
                SkillTelemetryEvent::Read {
                    reference: skill_ref.clone(),
                    name: Some(skill.name.clone()),
                    scope: Some(skill.scope),
                    provider: Some(skill.provider),
                    error: false,
                },
                ctx
            );
            let content = FileContext::new(
                skill.path.display_path(),
                AnyFileContent::StringContent(skill.content),
                skill.line_range,
                None,
            );
            ReadSkillResult::Success { content }
        }
        Err(error) => {
            send_telemetry_from_ctx!(
                SkillTelemetryEvent::Read {
                    reference: skill_ref.clone(),
                    name: None,
                    scope: None,
                    provider: None,
                    error: true,
                },
                ctx
            );
            ReadSkillResult::Error(error)
        }
    }
}

impl Entity for ReadSkillExecutor {
    type Event = ();
}

#[cfg(test)]
#[path = "read_skill_tests.rs"]
mod tests;
