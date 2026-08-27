use warp_core::ui::appearance::Appearance;
use warpui_core::elements::{
    Align, Container, CrossAxisAlignment, Empty, Flex, MainAxisSize, ParentElement, Shrinkable,
};
use warpui_core::Element;

pub fn onboarding_bottom_nav(
    _appearance: &Appearance,
    _step_index: usize,
    _step_count: usize,
    back_button: Option<Box<dyn Element>>,
    next_button: Option<Box<dyn Element>>,
) -> Box<dyn Element> {
    let back_button = back_button.unwrap_or_else(|| Empty::new().finish());
    let next_button = next_button.unwrap_or_else(|| Empty::new().finish());

    // Equal-size flex slots keep Back pinned left and the primary button pinned
    // right. There is no progress indicator between them: the dots came from
    // Warp's longer flow, and on a four-slide, account-free onboarding they read
    // as clutter more than as orientation.
    let left = Shrinkable::new(1., Align::new(back_button).left().finish()).finish();
    let right = Shrinkable::new(1., Align::new(next_button).right().finish()).finish();

    Container::new(
        Flex::row()
            .with_main_axis_size(MainAxisSize::Max)
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_child(left)
            .with_child(right)
            .finish(),
    )
    .finish()
}
