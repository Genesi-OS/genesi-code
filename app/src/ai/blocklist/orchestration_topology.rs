//! Shared helpers for walking the orchestration topology of conversations.
//!
//! The topology is stored as a parent → children index on
//! [`BlocklistAIHistoryModel`]. These helpers are factored out of the
//! orchestration pill bar so other surfaces (e.g. the agent-mode usage
//! footer's credit rollup) can walk the same tree without duplicating the
//! traversal.

use crate::ai::agent::conversation::{AIConversation, AIConversationId, ConversationStatus};
use crate::ai::blocklist::BlocklistAIHistoryModel;

/// Returns all locally-known descendants (children, grandchildren, …) of
/// `parent_id`, flattened in pre-order with each parent's child registration
/// order preserved.
///
/// This walks `BlocklistAIHistoryModel::child_conversation_ids_of`
/// transitively. The walker only consults the `children_by_parent` index, so
/// it works even before child `AIConversation`s have been loaded into
/// `conversations_by_id`. Unloaded descendants are still returned by id;
/// callers can filter them out via `history.conversation(&id)` as needed.
pub fn descendant_conversation_ids_in_spawn_order(
    history: &BlocklistAIHistoryModel,
    parent_id: AIConversationId,
) -> Vec<AIConversationId> {
    let mut descendants = Vec::new();
    collect_descendant_conversation_ids_in_spawn_order(history, parent_id, &mut descendants);
    descendants
}

/// Recursive worker for [`descendant_conversation_ids_in_spawn_order`]. Kept
/// separate so it can be invoked from existing call sites that already own a
/// buffer.
pub fn collect_descendant_conversation_ids_in_spawn_order(
    history: &BlocklistAIHistoryModel,
    parent_id: AIConversationId,
    descendants: &mut Vec<AIConversationId>,
) {
    for child_id in history.child_conversation_ids_of(&parent_id) {
        descendants.push(*child_id);
        collect_descendant_conversation_ids_in_spawn_order(history, *child_id, descendants);
    }
}

/// Returns a `ConversationStatus` that summarises the orchestrator's state
/// across the whole orchestration tree (orchestrator + all known descendants).
///
/// The orchestrator's own [`ConversationStatus`] only reflects its last
/// exchange's outcome — it flips to `Success` as soon as its own streaming
/// turn finishes, even though child agents may still be running. This helper
/// fixes that mismatch so surfaces like the orchestration pill bar can show a
/// status that matches what the user expects to see while children are still
/// in flight.
///
/// Aggregation precedence (highest wins):
///   1. `InProgress` — any node in the tree is actively running, **unless**
///      the orchestrator itself yielded into `WaitingForEvents`. The parent's
///      waiting state is a more specific and useful signal to the user than
///      "something somewhere is running".
///   2. `Blocked` — at least one node is waiting on user input. The
///      `blocked_action` from the first blocked node encountered is preserved
///      so callers can display it.
///   3. `WaitingForEvents` — at least one node yielded via `wait_for_events`
///      and is listening for inbound input. The run is quiescent but not
///      terminal — the driver stays alive until something resumes it.
///      Carve-out: when the orchestrator itself is `Cancelled` or `Error`,
///      the parent's terminal status wins over a descendant `WaitingForEvents`
///      so the pill does not falsely advertise a resumable run.
///   4. `Error` — at least one node finished with an error.
///   5. `Cancelled` — at least one node was cancelled.
///   6. `Success` — everything finished successfully.
///
/// Returns `Success` if the orchestrator is not loaded and has no descendants.
pub fn aggregated_orchestrator_status(
    history: &BlocklistAIHistoryModel,
    orchestrator_id: AIConversationId,
) -> ConversationStatus {
    let mut orchestrator_status: Option<ConversationStatus> = None;
    let mut first_blocked: Option<ConversationStatus> = None;
    let mut any_in_progress = false;
    let mut any_waiting = false;
    let mut any_error = false;
    let mut any_cancelled = false;

    for id in std::iter::once(orchestrator_id).chain(descendant_conversation_ids_in_spawn_order(
        history,
        orchestrator_id,
    )) {
        let Some(status) = history.conversation(&id).map(|c| c.status().clone()) else {
            continue;
        };
        if id == orchestrator_id {
            orchestrator_status = Some(status.clone());
        }
        match status {
            ConversationStatus::InProgress => any_in_progress = true,
            ConversationStatus::WaitingForEvents => any_waiting = true,
            ConversationStatus::Blocked { .. } => {
                if first_blocked.is_none() {
                    first_blocked = Some(status);
                }
            }
            ConversationStatus::Error => any_error = true,
            ConversationStatus::Cancelled => any_cancelled = true,
            ConversationStatus::Success => {}
        }
    }

    if any_in_progress {
        // Parent's own waiting state outranks descendant in-progress so
        // the pill reflects that THIS conversation is paused.
        if matches!(
            orchestrator_status,
            Some(ConversationStatus::WaitingForEvents)
        ) {
            return ConversationStatus::WaitingForEvents;
        }
        return ConversationStatus::InProgress;
    }
    if let Some(blocked) = first_blocked {
        return blocked;
    }
    if any_waiting {
        // Parent's terminal status beats descendant waiting — a
        // finalized run can't resume, so surface the parent's outcome.
        match orchestrator_status {
            Some(ConversationStatus::Cancelled) => return ConversationStatus::Cancelled,
            Some(ConversationStatus::Error) => return ConversationStatus::Error,
            _ => return ConversationStatus::WaitingForEvents,
        }
    }
    if any_error {
        return ConversationStatus::Error;
    }
    if any_cancelled {
        return ConversationStatus::Cancelled;
    }
    ConversationStatus::Success
}

/// Returns a conversation's direct status, or the aggregated subtree status
/// ([`aggregated_orchestrator_status`]) when it's a known orchestration parent.
///
/// Used by top-level chrome (tab/header icons, status rows) so the badge keeps
/// reflecting active children after the orchestrator's own turn finishes.
pub fn orchestration_aware_conversation_status(
    history: &BlocklistAIHistoryModel,
    conversation: &AIConversation,
) -> ConversationStatus {
    if history
        .child_conversation_ids_of(&conversation.id())
        .is_empty()
    {
        conversation.status().clone()
    } else {
        aggregated_orchestrator_status(history, conversation.id())
    }
}

#[cfg(test)]
#[path = "orchestration_topology_tests.rs"]
mod tests;
