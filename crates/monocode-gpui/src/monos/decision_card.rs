//! The decision card Mono posts when it asks the user: the question, why it
//! matters, the recommendation, alternatives, what is paused and what
//! continues, tied to the decision id. Choosing an option answers it; a typed
//! reply answers it only when unambiguous (see `Monos::user_reply`).
use super::cards::CardAction;
use super::*;
use serde_json::{Value, json};

fn row(label: &'static str, text: String) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(
            div()
                .text_size(px(11.))
                .text_color(content(0.45))
                .child(label),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(12.))
                .text_color(content(0.75))
                .child(text),
        )
}

fn useful_context(value: &Value) -> Option<String> {
    let paused = value["paused"].as_str().unwrap_or_default();
    let continues = value["continues"].as_str().unwrap_or_default();
    let paused = if paused == "New launches for the whole task; nothing else" {
        "New task launches"
    } else {
        paused
    };
    let continues = (!continues.is_empty() && continues != "nothing else").then_some(continues);
    match (!paused.is_empty(), continues) {
        (true, Some(continues)) => Some(format!("Paused: {paused} · Continuing: {continues}")),
        (true, None) => Some(format!("Paused: {paused}")),
        (false, Some(continues)) => Some(format!("Continuing: {continues}")),
        (false, None) => None,
    }
}

pub(super) fn render(
    value: &Value,
    cx: &mut Context<crate::transcript::TranscriptView>,
) -> AnyElement {
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
    let decision = text("decisionId");
    let revision = value["revision"].as_u64().unwrap_or_default();
    let recommendation = text("recommendation");
    let answered = value["state"] == "answered";
    let context = useful_context(value);
    let accent = crate::theme::accent_color();
    let choices: Vec<String> = std::iter::once(recommendation.clone())
        .chain(
            value["alternatives"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(str::to_owned)),
        )
        .filter(|c| !c.is_empty() && !answered)
        .collect();
    div()
        .debug_selector(|| "mono-decision-card".into())
        .rounded_lg()
        .border_1()
        .border_color(gpui::Hsla { a: 0.18, ..accent })
        .bg(gpui::Hsla { a: 0.025, ..accent })
        .p_3()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap_1p5()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(12.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if answered { content(0.7) } else { accent })
                .child(icon("message-square", 16.))
                .child(if answered {
                    "Decision answered"
                } else {
                    "Decision needed"
                }),
        )
        .child(
            div()
                .text_size(px(14.))
                .line_height(px(19.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(text("question")),
        )
        .when(!text("why").is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(11.))
                    .line_height(px(16.))
                    .text_color(content(0.55))
                    .child(text("why")),
            )
        })
        .when(answered, |d| d.child(row("Answer", text("answer"))))
        .children(choices.into_iter().enumerate().map(|(ix, choice)| {
            let card = json!({"type": "decision-answer", "decisionId": decision,
                "revision": revision, "text": choice});
            div()
                .id(("mono-decision-choice", ix))
                .debug_selector(move || format!("mono-decision-option-{ix}"))
                .role(Role::Button)
                .aria_label(choice.clone())
                .w_full()
                .min_w_0()
                .cursor_pointer()
                .px_2()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(if ix == 0 {
                    gpui::Hsla { a: 0.4, ..accent }
                } else {
                    content(0.08)
                })
                .bg(if ix == 0 {
                    gpui::Hsla { a: 0.09, ..accent }
                } else {
                    content(0.018)
                })
                .hover(move |s| {
                    s.bg(if ix == 0 {
                        gpui::Hsla { a: 0.14, ..accent }
                    } else {
                        content(0.07)
                    })
                })
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(if ix == 0 { accent } else { content(0.5) })
                        .child(if ix == 0 {
                            "✓".to_owned()
                        } else {
                            format!("{}", ix + 1)
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .w_full()
                                .text_size(px(12.))
                                .line_height(px(17.))
                                .child(choice),
                        )
                        .when(ix == 0, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(gpui::Hsla { a: 0.12, ..accent })
                                    .text_size(px(9.))
                                    .text_color(accent)
                                    .child("RECOMMENDED"),
                            )
                        }),
                )
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(CardAction(card.clone()))))
        }))
        .when_some(context, |d, context| {
            d.child(
                div()
                    .id(SharedString::from(format!("decision-context-{decision}")))
                    .w_full()
                    .min_w_0()
                    .mt_1()
                    .pt_2()
                    .border_t_1()
                    .border_color(content(0.08))
                    .text_size(px(10.))
                    .text_color(content(0.45))
                    .child(context),
            )
        })
        .when(!answered, |d| {
            d.child(
                div()
                    .text_size(px(11.))
                    .text_color(content(0.45))
                    .child("Choose an option, or reply in the chat."),
            )
        })
        .into_any_element()
}
