use lsp_types::{ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation};

use super::*;

fn signature(label: &str, parameters: Vec<ParameterLabel>) -> SignatureInformation {
    SignatureInformation {
        label: label.to_string(),
        documentation: None,
        parameters: Some(
            parameters
                .into_iter()
                .map(|label| ParameterInformation {
                    label,
                    documentation: None,
                })
                .collect(),
        ),
        active_parameter: None,
    }
}

fn help(signatures: Vec<SignatureInformation>, active: u32) -> SignatureHelp {
    SignatureHelp {
        signatures,
        active_signature: Some(0),
        active_parameter: Some(active),
    }
}

fn highlighted(info: &SignatureHelpInfo) -> String {
    let range = info
        .active_parameter
        .clone()
        .expect("a parameter is active");
    info.label
        .chars()
        .skip(range.start)
        .take(range.len())
        .collect()
}

#[test]
fn label_offsets_select_the_active_parameter() {
    let info = SignatureHelpInfo::from_lsp(help(
        vec![signature(
            "fn add(a: u32, b: u32)",
            vec![
                ParameterLabel::LabelOffsets([7, 13]),
                ParameterLabel::LabelOffsets([15, 21]),
            ],
        )],
        1,
    ))
    .unwrap();
    assert_eq!(highlighted(&info), "b: u32");
}

#[test]
fn a_parameter_named_like_the_function_highlights_the_parameter() {
    // Searching the whole label would find the function's own name first.
    let info = SignatureHelpInfo::from_lsp(help(
        vec![signature(
            "id(id: string)",
            vec![ParameterLabel::Simple("id: string".to_string())],
        )],
        0,
    ))
    .unwrap();
    assert_eq!(info.active_parameter, Some(3..13));

    let info = SignatureHelpInfo::from_lsp(help(
        vec![signature(
            "id(id)",
            vec![ParameterLabel::Simple("id".to_string())],
        )],
        0,
    ))
    .unwrap();
    assert_eq!(
        info.active_parameter,
        Some(3..5),
        "not the `id` before the paren"
    );
}

#[test]
fn label_offsets_are_utf16_units() {
    // The emoji is two UTF-16 units but one char, so the parameter that follows
    // it sits one char earlier than its offset suggests: `x: u8` is UTF-16
    // units 10..15 but chars 9..14.
    let label = "f(\u{1F600}: u8, x: u8)";
    let info = SignatureHelpInfo::from_lsp(help(
        vec![signature(
            label,
            vec![
                ParameterLabel::LabelOffsets([2, 8]),
                ParameterLabel::LabelOffsets([10, 15]),
            ],
        )],
        1,
    ))
    .unwrap();
    assert_eq!(highlighted(&info), "x: u8");
}

#[test]
fn an_empty_response_means_close_the_popup() {
    assert_eq!(
        SignatureHelpInfo::from_lsp(SignatureHelp {
            signatures: vec![],
            active_signature: None,
            active_parameter: None,
        }),
        None
    );
}

#[test]
fn an_out_of_range_active_signature_is_clamped() {
    let info = SignatureHelpInfo::from_lsp(SignatureHelp {
        signatures: vec![signature("f()", vec![]), signature("f(x)", vec![])],
        active_signature: Some(9),
        active_parameter: None,
    })
    .unwrap();
    assert_eq!((info.signature_index, info.signature_count), (1, 2));
}
