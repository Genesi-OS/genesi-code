//! The seam between Bench's trigger and Bench's runner.
//!
//! The trigger is a pill that appears over a selection in the code editor; the
//! runner lives in the AI panel. Neither holds a handle to the other, and
//! neither should — the editor has no business knowing about the AI panel, and
//! the panel has no business reaching into an editor pane. This singleton is
//! the one thing they share: the panel writes the run's progress into it, the
//! pill reads it back.

use warpui::{AppContext, Entity, ModelContext, SingletonEntity};

/// What the pill over the selection should be showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BenchPopupState {
    /// Nothing has run for this selection: the pill offers to.
    #[default]
    Ready,
    /// Working out what the selection is, or running what it found. One state
    /// rather than two because the pill says the same thing either way, and the
    /// gap between them is a few milliseconds.
    Working,
    Passed {
        /// `parses_headers · 240 ms`
        label: String,
    },
    Failed {
        label: String,
    },
    /// Bench looked and found nothing runnable. Carries the reason so the pill
    /// can say why instead of just going quiet.
    Nothing {
        reason: String,
    },
}

impl BenchPopupState {
    /// Whether clicking the pill should open the full Bench surface rather than
    /// start a run. Once there is an outcome, the next click is the user asking
    /// to see the detail — output, stack trace, the generate-a-test flow.
    pub fn wants_details(&self) -> bool {
        matches!(
            self,
            Self::Passed { .. } | Self::Failed { .. } | Self::Nothing { .. }
        )
    }
}

/// The selection a verdict belongs to: the file on disk and the line Bench was
/// pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchAnchor {
    pub file: std::path::PathBuf,
    pub line: usize,
}

#[derive(Default)]
pub struct BenchStatus {
    state: BenchPopupState,
    /// What [`Self::state`] describes. A verdict is only shown while the
    /// selection that produced it is still the one under the pill — a green
    /// tick hovering over a function it never ran is the one way this could
    /// actively mislead.
    anchor: Option<BenchAnchor>,
}

impl Entity for BenchStatus {
    type Event = ();
}

impl SingletonEntity for BenchStatus {}

impl BenchStatus {
    /// What the pill should show for the selection currently under it.
    ///
    /// A verdict recorded for a different place answers [`BenchPopupState::Ready`]
    /// instead: the pill offers to run rather than reporting someone else's
    /// result.
    pub fn state_for(app: &AppContext, anchor: &BenchAnchor) -> BenchPopupState {
        let status = Self::as_ref(app);
        match &status.anchor {
            Some(recorded) if recorded == anchor => status.state.clone(),
            _ => BenchPopupState::Ready,
        }
    }

    pub fn set(
        &mut self,
        state: BenchPopupState,
        anchor: Option<BenchAnchor>,
        ctx: &mut ModelContext<Self>,
    ) {
        if self.state == state && self.anchor == anchor {
            return;
        }
        self.state = state;
        self.anchor = anchor;
        ctx.notify();
    }
}

pub fn init(app: &mut AppContext) {
    // Singletons panic when read before registration, and the pill reads this
    // one on any selection in any code editor — so it has to exist from start-up
    // rather than being created the first time Bench runs.
    app.add_singleton_model(|_| BenchStatus::default());
}
