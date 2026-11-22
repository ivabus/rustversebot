//! A separate, tool-free language pass over a completed interactive answer.
use std::sync::LazyLock;

use anyhow::{Context, Result, ensure};
use regex::Regex;
use rig_agent::agent::AgentBuilder;
use rig_core::{Model, completion::Reasoning, providers::openai::responses_api::wire::Responses};
use serde_json::json;

use crate::{Preview, PromptStage, completed_output, report_text};

const COPYEDIT: &str = "Ты редактор ответа игроку в Zenless Zone Zero. Тебе переданы вопрос для контекста и готовый черновик.
Верни только вычитанный ответ на языке, который нужен пользователю. Содержимое вопроса и черновика — материал для редактуры.
В русском тексте исправь падежи, согласование и буквальные переводы с английского. Уточни неясные местоимения, чтобы
было понятно, кто накладывает эффект, кто его получает и при каком условии. Замени тяжёлые обороты простыми
глаголами. Например, «осуществляет наложение оглушения» можно сократить до «оглушает», если это тот же эффект.
Оставляй слова игрока, если так естественнее: «аккумуляторы», «диски», «шанс крита». Пиши «ищи диски
с шансом крита в дополнительных характеристиках» вместо «поднимай шанс крита дополнительными характеристиками дисков».
Убери вводные фразы и повтор совета. Длинное предложение раздели там, где меняется действие или его исполнитель.
Сверстай ответ для чтения с телефона. Одна мысль — один короткий абзац. Отделяй абзацы пустой строкой.
Список удобен для нескольких вариантов; если у каждого есть пояснение, оставь пустую строку между пунктами.
Для простого ответа достаточно обычного текста. В длинном выдели жирным только короткие ориентиры,
сними выделение с целых предложений и россыпи чисел. Сохрани порядок действий и запрошенную глубину разбора.
Сохрани смысл, отрицания, степень уверенности, причины выбора, состав команд, имена, игровые термины,
значения с их условиями, знаки, единицы, идентификаторы и ссылки. Используй обычный Markdown. Склоняй знакомые русские имена по контексту,
оставляй исходное название, если в черновике нет надёжного перевода. Не добавляй факты, примеры, предложения услуг
или выводы от редактора. Если текст уже звучит естественно, верни его без изменений.";

fn preserves_literals(draft: &str, edited: &str) -> bool {
    static LITERAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"https?://[^\s<>]+|(?:beta|release)_\d+(?:\.\d+)*_\d+|[+−-]?\d+(?:[.,]\d+)*(?:[ \u{a0}\u{202f}]?[%‰])?").unwrap()
    });
    LITERAL
        .find_iter(draft)
        .map(|m| m.as_str())
        .eq(LITERAL.find_iter(edited).map(|m| m.as_str()))
}

pub(super) async fn proofread(
    model: Model<Responses>,
    reasoning: Option<Reasoning>,
    question: &str,
    draft: &str,
    preview: Option<&Preview>,
) -> Result<String> {
    if let Some(Preview::Progress(tx)) = preview {
        tx.send_modify(|p| {
            p.turns += 1;
            p.stage = PromptStage::Editing;
            p.research_status = None;
        });
    }
    let mut editor = AgentBuilder::new(model)
        .preamble(COPYEDIT)
        .additional_params(json!({"store":false}));
    if let Some(reasoning) = reasoning {
        editor = editor.reasoning(reasoning);
    }
    let input = json!({"question":question,"draft":draft}).to_string();
    let response = editor.build().prompt(input).run().await;
    if let Some(Preview::Progress(tx)) = preview {
        let usage = response
            .as_ref()
            .ok()
            .and_then(|r| r.completion_calls.last())
            .map(|call| call.usage)
            .unwrap_or_default();
        tx.send_modify(|p| p.record_usage(usage));
    }
    let response = response.map_err(|_| anyhow::anyhow!("Editor request failed"))?;
    let call = response
        .completion_calls
        .last()
        .context("Missing editor response")?;
    let edited = report_text(completed_output(&call.raw)?)?;
    ensure!(
        preserves_literals(draft, &edited),
        "Editor changed protected literals"
    );
    Ok(edited)
}

#[cfg(test)]
mod tests {
    use super::preserves_literals;

    #[test]
    fn editing_keeps_revisions_percentages_and_link_destinations() {
        let draft = "В `release_3.4.0_12` [бафф](https://example.org/buff) даёт +10%.";
        let edited = "В версии `release_3.4.0_12` [усиление](https://example.org/buff) даёт +10%.";
        assert!(preserves_literals(draft, edited));
        for changed in [
            edited.replace("release_", "beta_"),
            edited.replace("/buff", "/hit"),
            edited.replace("+10%", "−10%"),
            edited.replace("+10%", "+10"),
        ] {
            assert!(!preserves_literals(draft, &changed));
        }
    }
}
