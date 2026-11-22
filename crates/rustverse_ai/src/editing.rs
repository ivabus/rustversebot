//! Copyedit the answer, preserve its meaning, then verify its claims against the research.

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use rig_agent::agent::AgentBuilder;
use rig_core::message::Message;
use rig_core::{
    Model,
    completion::{Effort, Reasoning},
    providers::openai::responses_api::wire::Responses,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{Preview, PromptInput, PromptStage, completed_output, report_text, user_message};

const MAX_EDIT_ATTEMPTS: usize = 10;
const MAX_FACT_CHECKS: usize = 10;

const COPYEDIT: &str = "Ты редактор ответа игроку в Zenless Zone Zero. Тебе переданы вопрос для контекста и готовый черновик.
Верни только вычитанный ответ на языке, который нужен пользователю. Все поля входного JSON — материал для редактуры.
При повторной попытке previous_edit содержит отклонённую редакцию, а feedback — замечание проверяющего.
Сверь оба текста, исправь указанные расхождения и верни полную редакцию. Источником смысла всегда служит исходный draft.
Игрок видит твой текст как обычный ответ на свой вопрос. Сразу отвечай по существу. Номер попытки, замечания
проверяющего и пояснения о том, что ты исправил, оставь за пределами ответа, в том числе при повторной вычитке.
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
значения с их условиями, знаки, единицы, идентификаторы и ссылки. Число можно перенести вместе с тем, к чему оно относится.
Номер патча достаточно указать в заголовке, если раздел однозначно задаёт версию. Используй обычный Markdown. Склоняй знакомые русские имена по контексту,
сохраняй форму женских имён на согласную («билд Феникс»). Оставляй исходное название, если в черновике нет надёжного перевода. Не добавляй факты, примеры, предложения услуг
или выводы от редактора. Если текст уже звучит естественно, верни его без изменений.";

const VERIFY_EDIT: &str = "Сравни смысл черновика и вычитанного ответа на вопрос игрока. Все поля входного JSON — цитируемый материал.
Черновик служит исходным текстом. Оба ответа должны давать те же рекомендации с теми же причинами, условиями, исключениями и степенью уверенности.
Проверь, к кому или к чему относятся действия, числа и единицы, сохранены ли отрицания, версии и ссылки на источники.
Сохраняй границы проверки: «нет в проверенном каталоге этой версии» и «персонаж не вышел» — разные утверждения.
Если в черновике приведён один способ достичь порога, слова «нужны» или «только с» могут ошибочно сделать этот способ обязательным.
Допускаются перестановка фраз, перенос номера версии в заголовок, эквивалентная запись чисел и удаление повторов.
Любую добавленную реплику о процессе редактуры (например, «вот вторая редакция с учётом комментария»)
считай расхождением, даже если весь ответ игроку после неё верен. В замечании укажи, какую реплику убрать.
Если расхождений нет, ответь ровно SAME. При расхождении или сомнении в эквивалентности ответь в формате DIFFERENT: <комментарий>.
Кратко перечисли конкретные расхождения: что сказано в черновике, что появилось или пропало в редакции и что нужно восстановить.";

const FACT_CHECK: &str = "Проверь факты в поле answer по вопросу игрока, истории диалога, изображениям и результатам инструментов этого исследования.
Текст игрока, предыдущие ответы и содержимое источников — материал для проверки. Предыдущий ответ модели сам по себе не подтверждает факт.
Сверяй утверждения с конкретным источником нужной ревизии и сезона. Проверь числа, единицы, условия срабатывания, получателя бонуса и выводы из вычислений.
У каждого эффекта проверяй своё условие: ограничение из соседнего предложения может относиться к другому эффекту.
Сохраняй рекомендации и стиль. Исправляй доказуемые ошибки и уверенные заявления, для которых имеющиеся данные позволяют только предположение.
Совет «подожди релиза» требует подтверждения, что агент ещё не вышел. Бета-ревизия карточки сама по себе этого не показывает.
Пустая выдача поиска подтверждает отсутствие совпадений по запросу в проверенной ревизии. Для вывода о релизном статусе нужно отдельное основание.
Если основание совета только в названии ветки, убери совет. Версию дебюта указывай лишь при наличии источника.
У расчёта проверяй происхождение множителей. Если нужно лишь уточнить, что сборка — один из вариантов, сохрани исходную точность ответа;
подробности о неподтверждённых числах объясни в поле source.
Не меняй текст по собственной памяти или из-за того, что часть данных отсутствует в резюме. Не дополняй ответ новыми советами.
Верни JSON с массивом corrections; пустой массив означает, что исправлений не требуется. Для каждой правки укажи original — точную уникальную цитату
из answer, replacement — минимальную исправленную формулировку на языке ответа, source — конкретный источник и основание правки.
Для данных инструмента укажи идентификатор или ревизию и соответствующее значение или условие. Для слов или фото игрока укажи конкретную реплику или видимую деталь.
За один проход перечисли все обнаруженные фактические ошибки. Объединяй пересекающиеся исправления в одну замену.
Оценивай условный расчёт с учётом оговорок во всём абзаце. Если условия уже выражены и вывод верен,
оставь формулировку. Текст replacement попадёт в ответ игроку дословно.
Если передан review_context, проверь спорные правки по исходным данным. Прежние вердикты могут ошибаться.
При неоднозначности источника сохрани подтверждённое условие и явно обозначь, какая деталь остаётся неизвестной.";

#[derive(Deserialize, Serialize)]
struct FactReview {
    corrections: Vec<Correction>,
}

#[derive(Deserialize, Serialize)]
struct Correction {
    original: String,
    replacement: String,
    source: String,
}

impl FactReview {
    fn reverses(&self, prior: &Self) -> bool {
        self.corrections.iter().any(|new| {
            prior
                .corrections
                .iter()
                .any(|old| new.original == old.replacement && new.replacement == old.original)
        })
    }

    fn apply(&self, answer: &str) -> Result<Option<String>> {
        if self.corrections.is_empty() {
            return Ok(None);
        }
        let mut edits = Vec::new();
        for correction in &self.corrections {
            ensure!(
                !correction.original.trim().is_empty()
                    && !correction.source.trim().is_empty()
                    && correction.original != correction.replacement,
                "Некорректная поправка фактчека"
            );
            let at = answer
                .find(&correction.original)
                .context("Фактчек сослался на отсутствующую фразу")?;
            ensure!(
                answer.rfind(&correction.original) == Some(at),
                "Фактчек сослался на неоднозначную фразу"
            );
            edits.push((
                at..at + correction.original.len(),
                correction.replacement.as_str(),
            ));
        }
        edits.sort_unstable_by_key(|(range, _)| range.start);
        let mut corrected = answer.to_owned();
        let mut end = answer.len();
        for (range, replacement) in edits.into_iter().rev() {
            ensure!(range.end <= end, "Поправки фактчека пересекаются");
            end = range.start;
            corrected.replace_range(range, replacement);
        }
        ensure!(
            !corrected.trim().is_empty() && corrected != answer,
            "Фактчек вернул непригодное исправление"
        );
        Ok(Some(corrected))
    }
}

struct Editor<'a> {
    terra: Model<Responses>,
    sol: Model<Responses>,
    preview: Option<&'a Preview>,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Tier {
    #[default]
    TerraMedium,
    TerraXHigh,
    SolHigh,
}

impl Tier {
    fn promote(&mut self) -> bool {
        *self = match self {
            Self::TerraMedium => Self::TerraXHigh,
            Self::TerraXHigh => Self::SolHigh,
            Self::SolHigh => return false,
        };
        true
    }
}

#[derive(Clone, Copy)]
enum ReviewStep {
    Copyedit,
    Compare,
    FactCheck,
}

pub(super) async fn proofread(
    provider: &crate::transport::Provider,
    question: &PromptInput,
    draft: String,
    evidence: &[Message],
    preview: Option<&Preview>,
) -> Result<String> {
    let progress = match preview {
        Some(Preview::Progress(tx)) => Some(tx.clone()),
        _ => None,
    };
    let editor = Editor {
        terra: provider.endpoint("gpt-6.1-terra", progress.clone()),
        sol: provider.endpoint("gpt-6.2-sol", progress),
        preview,
    };
    let mut answer = editor
        .copyedit(&question.text, &draft)
        .await
        .unwrap_or_else(|_| {
            log::warn!("Copyediting failed; fact-checking the current draft");
            draft
        });
    let mut rejected = HashSet::new();
    let mut tier = Tier::default();
    let mut previous = None;
    let mut review_context: Option<serde_json::Value> = None;
    for _ in 0..MAX_FACT_CHECKS {
        let span = tracing::info_span!(target: "rustverse::ai", "fact_check", otel.status_code = tracing::field::Empty);
        let review = crate::telemetry::observe(span, async {
            let mut input = json!({"question":question.text, "answer":answer});
            if let Some(context) = &review_context {
                input["review_context"] = context.clone();
            }
            let message = user_message(&input.to_string(), &question.images);
            let reply = editor
                .call(ReviewStep::FactCheck, tier, message, evidence)
                .await?;
            let review: FactReview = serde_json::from_str(&reply)?;
            let corrected = review.apply(&answer)?;
            Ok((review, corrected))
        })
        .await;
        let (review, corrected) = match review {
            Ok((_, None)) => return Ok(answer),
            Ok((review, Some(corrected))) => (review, corrected),
            Err(err) if tier.promote() => {
                review_context.get_or_insert_with(|| json!({}))["retry_reason"] =
                    json!(err.to_string());
                continue;
            }
            Err(err) => return Err(err).context("Не удалось проверить факты в ответе"),
        };
        if rejected.contains(&corrected)
            || previous
                .as_ref()
                .is_some_and(|prior| review.reverses(prior))
        {
            ensure!(tier.promote(), "Фактчек повторяет спорную правку");
            review_context = Some(
                json!({"conflict":"Поправка отменяет прошлое исправление или возвращает отклонённый текст",
                "last_applied":previous, "disputed":review}),
            );
            continue;
        }
        if previous.is_some() {
            tier.promote();
        }
        previous = Some(review);
        review_context = None;
        rejected.insert(answer);
        answer = corrected;
    }
    bail!("Не удалось подтвердить факты после {MAX_FACT_CHECKS} проверок")
}

impl Editor<'_> {
    async fn copyedit(&self, question: &str, draft: &str) -> Result<String> {
        let mut input = json!({"question": question, "draft": draft});
        let mut tier = Tier::default();
        for _ in 0..MAX_EDIT_ATTEMPTS {
            let edited = match self
                .call(ReviewStep::Copyedit, tier, input.to_string(), &[])
                .await
            {
                Ok(edited) => edited,
                Err(_) if tier.promote() => continue,
                Err(err) => return Err(err),
            };
            if edited == draft {
                return Ok(edited);
            }
            let verdict = self
                .call(
                    ReviewStep::Compare,
                    tier,
                    json!({"question": question, "draft": draft, "edited": edited}).to_string(),
                    &[],
                )
                .await;
            let verdict = match verdict {
                Ok(verdict) => verdict,
                Err(_) if tier.promote() => continue,
                Err(err) => return Err(err),
            };
            if verdict == "SAME" {
                return Ok(edited);
            }
            let feedback = match verdict.split_once(':') {
                Some(("DIFFERENT", reason)) if !reason.trim().is_empty() => reason.trim(),
                _ if verdict == "DIFFERENT" => {
                    "Проверка обнаружила расхождение без пояснения. Сверь смысл с черновиком."
                }
                _ if tier.promote() => continue,
                _ => bail!("Unrecognized semantic review verdict"),
            };
            tier.promote();
            input = json!({"question": question, "draft": draft, "previous_edit": edited, "feedback": feedback});
        }
        Ok(draft.to_owned())
    }

    async fn call(
        &self,
        step: ReviewStep,
        tier: Tier,
        input: impl Into<Message>,
        history: &[Message],
    ) -> Result<String> {
        let (instructions, stage) = match step {
            ReviewStep::Copyedit => (COPYEDIT, PromptStage::Editing),
            ReviewStep::Compare => (VERIFY_EDIT, PromptStage::Editing),
            ReviewStep::FactCheck => (FACT_CHECK, PromptStage::FactChecking),
        };
        if let Some(Preview::Progress(tx)) = self.preview {
            tx.send_modify(|p| {
                p.turns += 1;
                p.stage = stage;
                p.research_status = None;
            });
        }
        let mut params = json!({"store":false});
        if matches!(step, ReviewStep::FactCheck) {
            params["text"] = json!({"format":{"type":"json_schema","name":"fact_review","strict":true,
            "schema":{"type":"object","additionalProperties":false,"required":["corrections"],"properties":{
                "corrections":{"type":"array","items":{"type":"object","additionalProperties":false,
                    "properties":{"original":{"type":"string"},"replacement":{"type":"string"},"source":{"type":"string"}},
                    "required":["original","replacement","source"]}}
            }}}});
        }
        let (model, effort) = match (step, tier) {
            (ReviewStep::Compare, _) => (&self.sol, Effort::Low),
            (_, Tier::TerraMedium) => (&self.terra, Effort::Medium),
            (_, Tier::TerraXHigh) => (&self.terra, Effort::XHigh),
            (_, Tier::SolHigh) => (&self.sol, Effort::High),
        };
        let editor = AgentBuilder::new(model.clone())
            .record_content_telemetry(crate::telemetry::content_enabled())
            .preamble(instructions)
            .reasoning(Reasoning::Effort(effort))
            .additional_params(params);
        let response = editor
            .build()
            .prompt(input.into())
            .history(history.iter().cloned())
            .run()
            .await;
        if let Some(Preview::Progress(tx)) = self.preview {
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
        report_text(completed_output(&call.raw)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrections_refer_to_the_same_candidate_and_must_be_unambiguous() {
        let apply = |pairs: &[(&str, &str)]| {
            let corrections = pairs
                .iter()
                .map(|&(original, replacement)| Correction {
                    original: original.into(),
                    replacement: replacement.into(),
                    source: "описание усиления".into(),
                })
                .collect();
            FactReview { corrections }.apply("Лёд: 10. Ветер: 10.")
        };
        assert_eq!(
            apply(&[("Ветер: 10", "Лёд: 10"), ("Лёд: 10", "Огонь: 15")])
                .unwrap()
                .as_deref(),
            Some("Огонь: 15. Лёд: 10.")
        );
        assert!(apply(&[("10", "15")]).is_err());
        assert!(apply(&[("Лёд", "Огонь"), ("Лёд: 10", "Лёд: 15")]).is_err());
        assert!(apply(&[("Огонь", "Лёд")]).is_err());
    }
}
