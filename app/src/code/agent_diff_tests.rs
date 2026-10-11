use super::*;

// In these tests `base` is the file before the agent, `before` the buffer
// with the agent's change, and `after` the buffer once the user has typed.

#[test]
fn typing_outside_the_agent_change_moves_the_base_along() {
    let base = "fn a() {}\nfn b() {}\n";
    let before = "fn a() {}\nfn agent() {}\nfn b() {}\n";
    let after = "fn a() {}\nfn agent() {}\nfn b(x: u8) {}\n";
    assert_eq!(
        carry_user_edit(base, before, after),
        "fn a() {}\nfn b(x: u8) {}\n"
    );
}

#[test]
fn typing_above_the_agent_change_moves_the_base_along() {
    let base = "one\ntwo\n";
    let before = "one\nagent\ntwo\n";
    let after = "one!\nagent\ntwo\n";
    assert_eq!(carry_user_edit(base, before, after), "one!\ntwo\n");
}

#[test]
fn typing_inside_the_agent_change_stays_part_of_it() {
    let base = "one\ntwo\n";
    let before = "one\nagent\ntwo\n";
    let after = "one\nagent!\ntwo\n";
    assert_eq!(carry_user_edit(base, before, after), base);
}

#[test]
fn a_new_line_typed_after_the_agent_change_is_the_user_s() {
    let base = "one\ntwo\n";
    let before = "one\nagent\ntwo\n";
    let after = "one\nagent\nmine\ntwo\n";
    assert_eq!(carry_user_edit(base, before, after), "one\nmine\ntwo\n");
}

#[test]
fn typing_at_the_end_of_the_file_is_the_user_s() {
    let base = "one\n";
    let before = "agent\none\n";
    let after = "agent\none\nmine";
    assert_eq!(carry_user_edit(base, before, after), "one\nmine");
}

#[test]
fn an_edit_that_crosses_into_the_agent_change_is_left_out() {
    let base = "one\ntwo\n";
    let before = "one\nagent\ntwo\n";
    // Deleting from the end of `one` into `agent` joins the two lines.
    let after = "onegent\ntwo\n";
    assert_eq!(carry_user_edit(base, before, after), base);
}

#[test]
fn edits_from_several_cursors_are_all_carried() {
    let base = "a\nb\nc\n";
    let before = "a\nb\nagent\nc\n";
    let after = "a1\nb\nagent\nc1\n";
    assert_eq!(carry_user_edit(base, before, after), "a1\nb\nc1\n");
}

#[test]
fn edits_in_lines_the_agent_removed_around_are_carried() {
    // The agent deleted `gone`; the user edits the line after it.
    let base = "keep\ngone\nnext\n";
    let before = "keep\nnext\n";
    let after = "keep\nnext line\n";
    assert_eq!(
        carry_user_edit(base, before, after),
        "keep\ngone\nnext line\n"
    );
}

#[test]
fn multibyte_text_lands_on_character_boundaries() {
    let base = "olá\nmundo\n";
    let before = "olá\nagente\nmundo\n";
    let after = "olá ção\nagente\nmundo\n";
    assert_eq!(carry_user_edit(base, before, after), "olá ção\nmundo\n");
}

#[test]
fn no_change_keeps_the_base() {
    assert_eq!(carry_user_edit("a\n", "b\n", "b\n"), "a\n");
}

#[test]
fn with_no_agent_change_the_base_simply_follows_the_buffer() {
    let text = "same\ntext\n";
    let after = "same\nnew text\n";
    assert_eq!(carry_user_edit(text, text, after), after);
}
