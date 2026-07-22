//! Turning frontend project source into something renderable.
//!
//! [`compiler`] parses HTML/CSS/JSX into a styled node tree; [`hover`] resolves
//! "what is under the cursor" into one of those trees. Neither knows anything
//! about the UI — the app supplies its own renderer for the tree — which is
//! what lets both be exercised by ordinary unit tests.

pub mod compiler;
pub mod hover;
