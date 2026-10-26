//! The decision card Mono posts when it asks the user: the question, why it
//! matters, the recommendation, alternatives, what is paused and what
//! continues, tied to the decision id. Choosing an option answers it; a typed
//! reply answers it only when unambiguous (see `Monos::user_reply`).
use super::cards::CardAction;
use super::*;
use serde_json::{Value, json};

fn row(label: &'static str, text: String) -> gpui::Div {
    div()
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
                .text_size(px(12.))
                .text_color(content(0.75))
                .child(text),
        )
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
        .rounded_lg()
        .border_1()
        .border_color(content(0.12))
        .bg(content(0.035))
        .p_3()
        .max_w(px(480.))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(icon("message-square", 16.))
                .child(if answered {
                    "Decision answered"
                } else {
                    "Needs your decision"
                }),
        )
        .child(div().text_size(px(13.)).child(text("question")))
        .child(row("Why it matters", text("why")))
        .child(row("Recommendation", recommendation))
        .child(row("Paused", text("paused")))
        .child(row("Continues", text("continues")))
        .when(answered, |d| d.child(row("Answer", text("answer"))))
        .children(choices.into_iter().enumerate().map(|(ix, choice)| {
            let card = json!({"type": "decision-answer", "decisionId": decision,
                "revision": revision, "text": choice});
            panels::button(("mono-decision-choice", ix), choice)
                .bg(content(0.06))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(CardAction(card.clone()))))
        }))
        .child(
            div()
                .text_size(px(11.))
                .text_color(content(0.45))
                .child(format!(
                    "Decision {decision} · or reply starting with \u{201c}{decision}:\u{201d}"
                )),
        )
        .into_any_element()
}
