use super::*;
use crate::{
    ImagePromptTurn, Preview, PromptProgress, PromptStage, run_agent, seasons::SeasonTools,
};
use rig_core::http_client::{
    self, Bytes, DynHttpClient, HttpClientExt, LazyBody, MultipartForm, Request, Response,
    StreamingResponse,
};
use rig_core::providers::openai::OpenAIConfig;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::watch;

const SOURCE: &str = "agents/1601/en/beta_1.0.0_2/12/max";
const QUESTION: &str = "Что изменится для второй комнаты?";
const NOTES: &str = "Выбрали Кафку для второй комнаты из-за слабости к электричеству.";
const FINAL: &str = "Для Кафки во второй комнате сезона 62053 проверено значение 42.";
const EDITED: &str = "Проверенное значение для Кафки — 42 (вторая комната сезона 62053).";
const WRONG: &str = "В первой комнате сезона 62053 значение для Кафки равно 42.";
const STILL_WRONG: &str = "Во второй комнате сезона 62053 значение для Кафки равно 24.";
const WRONG_ROOM: &str = "DIFFERENT: Верни вторую комнату из черновика.";
const WRONG_VALUE: &str = "DIFFERENT: Верни значение 42 из черновика.";
const VERIFIED: &str = r#"{"corrections":[]}"#;
const CORRECTION: &str = r#"{"corrections":[{"original":"42","replacement":"63","source":"Последний compute вернул 63 для утроенного значения."}]}"#;
const CORRECTED: &str = "Проверенное значение для Кафки — 63 (вторая комната сезона 62053).";

#[derive(Clone)]
struct Fixture {
    question: &'static str,
    route: Option<(&'static str, &'static str)>,
    turns: Arc<AtomicUsize>,
    summaries: Arc<AtomicUsize>,
    progress: watch::Receiver<PromptProgress>,
    editing_calls: Arc<AtomicUsize>,
    edit_attempts: Arc<AtomicUsize>,
    editing_replies: Vec<Option<&'static str>>,
    fact_calls: Arc<AtomicUsize>,
    fact_replies: Vec<(String, Option<&'static str>)>,
    fact_routes: Vec<&'static str>,
}

// A vision model with a smaller context window than the 384k compaction threshold.
const MODEL: &str = "gpt-4o";
const INPUT_LIMIT: usize = 128_000 - 8192;

fn tokens(body: &Value) -> usize {
    6400 + 30_000
        * body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["type"] == "function_call_output")
            .count()
}

fn message(text: &str) -> Value {
    json!({"type":"message","id":"msg","phase":"final_answer","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":text,"annotations":[]}]})
}

fn call(n: usize, name: &str, mut args: Value) -> Value {
    args["research_status"] = Value::Null;
    json!({"type":"function_call","id":format!("fc_{n}"),"call_id":format!("call_{n}"),"name":name,
        "arguments":args.to_string(),"status":"completed"})
}

fn response(item: &Value, id: &str) -> Value {
    json!({"id":id,"object":"response","status":"completed","model":MODEL,"output":[item],
        "usage":{"input_tokens":18,"output_tokens":2,"total_tokens":20}})
}

// Non-streaming report generation is used by both the scheduler and CLI.
#[derive(Default)]
struct ReportFixture(AtomicUsize);

impl HttpClientExt for ReportFixture {
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody>>> + Send + 'static {
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let draft = "### Базовая атака\n- Урон вырос с 20 до 24.";
        let corrected = draft.replace("24", "21");
        let reply = match request.uri().path() {
            "/v1/responses/input_tokens" => json!({"input_tokens": 1000}),
            "/v1/responses" => {
                let step = self.0.fetch_add(1, Ordering::Relaxed);
                let history = body["input"].as_array().unwrap();
                let input = &history.last().unwrap()["content"][0]["text"];
                if step == 0 {
                    let task = input.as_str().unwrap();
                    assert!(task.contains(crate::REPORT_TASK) && task.contains("beta_1.0.0_2"));
                    assert!(!task.contains("before") && !task.contains("damage"));
                    let mut reply = response(&Value::Null, "lookup");
                    reply["output"] = ["en", "cn"].iter().enumerate().map(|(i, lang)| call(i, "get_gachabase_entry",
                        json!({"category":"agents","id":1601,"version":"beta_1.0.0_2","language":lang,
                               "selector":"@changes","offset":null,"skill_level":null,"core_level":null}))).collect();
                    reply
                } else {
                    let answer = match step {
                        1 => draft,
                        2 => {
                            let input: Value =
                                serde_json::from_str(input.as_str().unwrap()).unwrap();
                            assert_eq!(input["draft"], draft);
                            draft
                        }
                        3 | 4 => {
                            let input: Value =
                                serde_json::from_str(input.as_str().unwrap()).unwrap();
                            let outputs: Vec<_> = history
                                .iter()
                                .filter_map(|m| {
                                    m["content"][0]["text"]
                                        .as_str()?
                                        .strip_prefix("[tool get_gachabase_entry result] ")
                                })
                                .collect();
                            assert_eq!(outputs.len(), 2);
                            let mut languages = BTreeSet::new();
                            for output in outputs {
                                let data: Value = serde_json::from_str(output).unwrap();
                                languages.insert(data["language"].as_str().unwrap().to_owned());
                                assert_eq!(data["result"][0]["changes"][0]["before"], 20);
                                assert_eq!(data["result"][0]["changes"][0]["after"], 21);
                            }
                            assert_eq!(languages, BTreeSet::from(["en".into(), "cn".into()]));
                            if step == 3 {
                                assert_eq!(input["answer"], draft);
                                r#"{"corrections":[{"original":"24","replacement":"21","source":"en/cn /damage, beta_1.0.0_2: 21"}]}"#
                            } else {
                                assert_eq!(input["answer"], corrected);
                                VERIFIED
                            }
                        }
                        _ => panic!("unexpected report step {step}"),
                    };
                    response(&message(answer), "report")
                }
            }
            path => panic!("unexpected report request {path}"),
        };
        async move {
            let body: LazyBody = Box::pin(async move { Ok(Bytes::from(reply.to_string())) });
            Ok(Response::builder()
                .header("content-type", "application/json")
                .body(body)?)
        }
    }

    async fn send_streaming(&self, _: Request<Bytes>) -> http_client::Result<StreamingResponse> {
        panic!("unexpected streaming report request")
    }

    fn send_multipart(
        &self,
        _: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody>>> + Send + 'static {
        std::future::ready(Err(http_client::Error::permanent("unexpected multipart")))
    }
}

#[tokio::test]
async fn diff_report_reads_changes_through_tools_before_checking_the_text() {
    let page = Comparison::parse(
        "https://zzz.gachabase.net/diff/agents/1601/test?prev=beta_1.0.0_1&cur=beta_1.0.0_2",
    )
    .unwrap();
    let snaps = || Snapshots {
        prev: json!({"agent":{"damage":20}}),
        cur: json!({"agent":{"damage":21}}),
    };
    let mut tools = KitTools::new(Client::new());
    let input = tools.report_input(&page, snaps(), snaps()).unwrap();
    let provider = crate::transport::Provider {
        config: OpenAIConfig::new("mock"),
        client: DynHttpClient::new(ReportFixture::default()),
    };
    let report = crate::write_report(tools, input, provider, MODEL, None)
        .await
        .unwrap();
    assert_eq!(report, "### Базовая атака\n- Урон вырос с 20 до 21.");
}

impl HttpClientExt for Fixture {
    fn send(
        &self,
        request: Request<Bytes>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody>>> + Send + 'static {
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let mut status = 200;
        let reply = match request.uri().path() {
            "/v1/responses/input_tokens" => json!({"input_tokens":tokens(&body)}),
            "/v1/decisions" => {
                let (reply, _) = self.route.expect("explicit model must bypass routing");
                assert_eq!(body["model"], "gpt-6-luna");
                assert_eq!(request.headers()["authorization"], "Bearer mock");
                assert_eq!(body["questions"][0]["type"], "choice");
                assert_eq!(body["questions"][0]["name"], "research_tier");
                assert_eq!(body["questions"][0]["choices"].as_array().unwrap().len(), 3);
                let parts = &body["input"][0]["content"];
                let text: Value = serde_json::from_str(parts[0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(text["question"], self.question);
                assert_eq!(text["recent"][0]["answer"], NOTES);
                assert_eq!(parts[1]["image_url"], "data:image/jpeg;base64,/9j/");
                assert_eq!(parts[2]["image_url"], "data:image/png;base64,iVBORw0KGgo=");
                let answer = match reply {
                    "refusal" => json!({"type":"refusal","name":"research_tier"}),
                    tier => {
                        json!({"type":"choice","name":"research_tier","choice":tier,"confidence":0.8,
                        "probabilities":[{"value":tier,"probability":0.9}]})
                    }
                };
                if reply == "http_error" {
                    status = 403;
                    json!({"error":{"message":"private upstream diagnostic"}})
                } else {
                    json!({"model":"gpt-6-luna","answers":[answer],"usage":{
                    "input_tokens":20,"output_tokens":0,"total_tokens":20,
                    "input_tokens_details":{"cached_tokens":5,"cache_write_tokens":2}}})
                }
            }
            "/v1/responses" if self.progress.borrow().stage == PromptStage::FactChecking => {
                let step = self.fact_calls.fetch_add(1, Ordering::Relaxed);
                let (expected, reply) = &self.fact_replies[step];
                let retried = self.fact_replies[..step]
                    .iter()
                    .filter(|(answer, _)| answer == expected)
                    .count();
                let (slug, effort) = self.fact_routes[step].split_once('@').unwrap();
                assert_eq!(body["model"], slug);
                assert_eq!(body["reasoning"]["effort"], effort);
                assert_eq!(body["text"]["format"]["strict"], true);
                let input = body["input"].as_array().unwrap();
                let user = input.last().unwrap();
                let payload: Value =
                    serde_json::from_str(user["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(payload["question"], self.question);
                assert_eq!(payload["answer"], expected.as_str());
                if retried > 0 {
                    assert!(payload.get("review_context").is_some());
                }
                assert_eq!(user["content"][2]["type"], "input_image");
                let history = body["input"].to_string();
                assert!(
                    history.contains(NOTES) && history.contains(SOURCE) && history.contains("63.0"),
                    "{history}"
                );
                response(&message(reply.unwrap_or("invalid verdict")), "fact_check")
            }
            "/v1/responses" if self.progress.borrow().stage == PromptStage::Editing => {
                assert!(
                    body.get("tools")
                        .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
                );
                assert_eq!(body["store"], false);
                let input = body["input"].to_string();
                assert_eq!(self.fact_calls.load(Ordering::Relaxed), 0);
                assert!(input.contains(self.question) && input.contains(FINAL));
                assert!(!input.contains(SOURCE) && !input.contains(NOTES));
                let step = self.editing_calls.fetch_add(1, Ordering::Relaxed);
                let payload: Value = serde_json::from_str(
                    body["input"].as_array().unwrap().last().unwrap()["content"][0]["text"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
                let id = if payload.get("edited").is_some() {
                    assert_eq!(body["model"], "gpt-6.2-sol");
                    assert_eq!(body["reasoning"]["effort"], "low");
                    assert!(input.contains(self.editing_replies[step - 1].unwrap()));
                    assert!(!input.contains("previous_edit") && !input.contains("feedback"));
                    "semantic_review"
                } else {
                    let attempt = self.edit_attempts.fetch_add(1, Ordering::Relaxed);
                    let route = match attempt {
                        0 => TERRA_HIGH,
                        1 => SOL_MEDIUM,
                        _ => SOL_HIGH,
                    };
                    let (model, effort) = route.split_once('@').unwrap();
                    assert_eq!(body["model"], model);
                    assert_eq!(body["reasoning"]["effort"], effort);
                    if step >= 2
                        && self.editing_replies[step - 1]
                            .is_some_and(|s| s.starts_with("DIFFERENT"))
                    {
                        let rejected = self.editing_replies[step - 2].unwrap();
                        let verdict = self.editing_replies[step - 1].unwrap();
                        assert!(input.contains("previous_edit") && input.contains(rejected));
                        assert!(input.contains("feedback"));
                        if let Some(comment) = verdict.strip_prefix("DIFFERENT: ") {
                            assert!(input.contains(comment));
                        }
                    }
                    "copyedit"
                };
                self.editing_replies[step]
                    .map(|text| response(&message(text), id))
                    .unwrap_or_else(|| {
                        json!({"id":id,"object":"response","model":MODEL,
                    "status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},
                    "usage":{"input_tokens":18,"output_tokens":2,"total_tokens":20},"output":[]})
                    })
            }
            "/v1/responses" => {
                assert!(body.get("tools").is_none());
                if body["model"] == MODEL {
                    assert!(tokens(&body) < INPUT_LIMIT);
                }
                let previous = self.summaries.fetch_add(1, Ordering::Relaxed);
                assert_eq!(self.progress.borrow().stage, PromptStage::Compacting);
                let data = body["input"].as_str().unwrap();
                assert!(!data.contains("base64"));
                assert!(data.contains(NOTES));
                if previous == 0 {
                    assert!(data.contains("62053"));
                }
                response(&message(NOTES), "summary")
            }
            path => panic!("Unexpected in-memory request {path}"),
        };
        async move {
            let bytes = Bytes::from(reply.to_string());
            let body: LazyBody = Box::pin(async move { Ok(bytes) });
            Ok(Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(body)?)
        }
    }

    fn send_multipart(
        &self,
        _: Request<MultipartForm>,
    ) -> impl Future<Output = http_client::Result<Response<LazyBody>>> + Send + 'static {
        std::future::ready(Err(http_client::Error::permanent(
            "unexpected multipart request",
        )))
    }

    async fn send_streaming(
        &self,
        request: Request<Bytes>,
    ) -> http_client::Result<StreamingResponse> {
        assert_eq!(request.uri().path(), "/v1/responses");
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        if let Some((_, selected)) = self.route {
            let (model, effort) = selected.split_once('@').unwrap();
            assert_eq!(body["model"], model);
            assert_eq!(body["reasoning"]["effort"], effort);
            assert_eq!(
                self.progress.borrow().research_model.as_deref(),
                Some(selected)
            );
        } else {
            assert_eq!(body["model"], MODEL);
            assert!(tokens(&body) < INPUT_LIMIT);
        }
        let frames = self.frames(&body);
        let stream: http_client::BoxedStream =
            Box::pin(futures_util::stream::iter([Ok(frames.into())]));
        Ok(Response::builder()
            .header("content-type", "text/event-stream")
            .body(stream)?)
    }
}

impl Fixture {
    fn frames(&self, body: &Value) -> String {
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 8);
        assert!(tools.iter().any(|t| t["name"] == "list_revision_timeline"));
        let n = self.turns.fetch_add(1, Ordering::Relaxed) + 1;
        let input = body["input"].to_string();
        assert!(input.contains(self.question));
        let question = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .rfind(|v| v["role"] == "user")
            .unwrap();
        assert_eq!(question["content"][1]["type"], "input_image", "{question}");
        assert_eq!(
            question["content"][1]["image_url"],
            "data:image/jpeg;base64,/9j/"
        );
        assert_eq!(
            question["content"][2]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
        assert!(input.contains(NOTES));
        if n > 2 {
            assert!(input.contains("62053"));
        }
        if n > 4 {
            assert!(input.contains(SOURCE));
        }
        let item = match n {
            1 => call(
                n,
                "list_endgame_seasons",
                json!({"mode":null,"offset":null}),
            ),
            2 => {
                assert!(input.contains("deadly_assault") && input.contains("shiyu_defense"));
                call(
                    n,
                    "get_endgame_seasons",
                    json!({"requests":[
                        {"mode":"shiyu_defense","season":"current","offset":null,"selector":null},
                        {"mode":"deadly_assault","season":"69999","offset":null,"selector":null}
                    ]}),
                )
            }
            3 => {
                assert!(input.contains("Test enemy"));
                call(
                    n,
                    "get_gachabase_entry",
                    json!({"category":"agents","id":1601,"language":"en","selector":"/damage",
            "version":"beta_1.0.0_2","offset":null,"core_level":null,"skill_level":null}),
                )
            }
            4 => call(
                n,
                "compute",
                json!({"script":"x0","bindings":[{"source":"missing","path":"/damage"}]}),
            ),
            5..=13 => call(
                n,
                "compute",
                json!({"script":"damage * 2.0","bindings":[{"name":"damage","source":SOURCE,"path":"/damage"}]}),
            ),
            14 => call(
                n,
                "compute",
                json!({"script":"calls[2].result.result.value * 3.0","bindings":[]}),
            ),
            15 => {
                assert!(input.contains("63.0"));
                message(FINAL)
            }
            _ => panic!("unexpected extra turn"),
        };
        let mut completed = response(&item, &format!("r{n}"));
        let output = completed["output"].as_array_mut().unwrap();
        if n == 4 {
            output.push(call(
                104,
                "get_endgame_season",
                json!({"mode":"deadly_assault","season":"69999","offset":null,"selector":null}),
            ));
        }
        let mut events = Vec::new();
        for (index, item) in output.iter().enumerate() {
            let id = &item["id"];
            if item["type"] == "function_call" {
                let mut empty = item.clone();
                empty["arguments"] = json!("");
                events.push(
                    json!({"type":"response.output_item.added","output_index":index,"item":empty}),
                );
                events.push(json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":id,"delta":item["arguments"]}));
            } else {
                events.push(json!({"type":"response.output_item.added","output_index":index,"item":{"type":"message","role":"assistant","id":id,"content":[]}}));
                events.push(json!({"type":"response.output_text.delta","output_index":index,"content_index":0,"item_id":id,"delta":FINAL}));
            }
            events
                .push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        }
        events.push(json!({"type":"response.completed","response":completed}));
        events
            .into_iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect()
    }
}

#[tokio::test]
async fn batch_seasons_keep_order_references_and_full_compute_sources() {
    let mut kit = KitTools::for_question(Client::new(), QUESTION);
    kit.seasons = Some(SeasonTools::fixture());
    let kit = Mutex::new(kit);
    let path = "/zones/0/zone/monster_level";
    let page = json!({"mode":"shiyu_defense","season":"62053","offset":0,"selector":path});
    let batch = KitTools::dispatch(
        &kit,
        "get_endgame_seasons",
        json!({"requests":[
            {"mode":"shiyu_defense","season":"current","selector":"/zones/bad"},
            {"mode":"deadly_assault","season":"69999"},
            page,
            {"mode":"shiyu_defense","season":"current","offset":1}
        ]}),
    )
    .await
    .unwrap();
    let rows = batch["results"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].get("error").is_some());
    assert_eq!(rows[1]["status"], "unavailable");
    assert_eq!(rows[2]["value"], 60);
    assert_eq!(rows[3]["offset"], 1);
    let computed = KitTools::dispatch(
        &kit,
        "compute",
        json!({"script":"x0+1.0","bindings":[{"source":rows[2]["source"],"path":path}]}),
    )
    .await
    .unwrap();
    assert_eq!(computed["result"], 61.0);
    for count in [0, 9] {
        assert!(
            KitTools::dispatch(
                &kit,
                "get_endgame_seasons",
                json!({"requests":vec![page.clone();count]})
            )
            .await
            .is_err()
        );
    }
    let refs = kit.lock().await.references.lock().await.clone();
    assert_eq!(refs.seasons.len(), 2);
    assert!(
        refs.seasons
            .iter()
            .any(|s| s.page == 1 && s.requested == "current")
    );
    assert!(
        refs.seasons
            .iter()
            .any(|s| s.page == 0 && s.path.as_deref() == Some(path))
    );
    let single = KitTools::dispatch(&kit, "get_endgame_season", page)
        .await
        .unwrap();
    assert_eq!(rows[2], single);
}

#[tokio::test]
async fn compute_can_combine_call_history_and_explicit_bindings() {
    let mut kit = KitTools::for_question(Client::new(), QUESTION);
    kit.seasons = Some(SeasonTools::fixture());
    let kit = Mutex::new(kit);
    let initial = KitTools::dispatch(
        &kit,
        "compute",
        json!({
            "script":"[len(calls), 2]", "bindings":[]
        }),
    )
    .await
    .unwrap();
    assert_eq!(initial["result"], json!([0, 2]));
    let path = "/zones/0/zone/monster_level";
    let page = json!({"mode":"shiyu_defense","season":"62053","selector":path});
    let batch = KitTools::dispatch(
        &kit,
        "get_endgame_seasons",
        json!({
            "requests":[page,{"mode":"shiyu_defense","season":"62053","selector":"/zones/bad"}]
        }),
    )
    .await
    .unwrap();
    let err = KitTools::dispatch(&kit, "get_endgame_seasons", json!({"requests":[]}))
        .await
        .unwrap_err();
    let script = r#"
        let matches = 0;
        for c in calls {
            if c.name == "get_endgame_seasons" && c.result.error == () {
                for row in c.result.results {
                    if row.status != "unavailable" && row.value != () { matches += row.value; }
                }
            }
        }
        [len(calls), matches, sum(calls[0].result.result), len(calls[1].args.requests), x0]
    "#;
    let summary = KitTools::dispatch(
        &kit,
        "compute",
        json!({
            "script":script,"bindings":[{"source":batch["results"][0]["source"],"path":path}]
        }),
    )
    .await
    .unwrap();
    assert_eq!(summary["result"], json!([3, 60, 2.0, 2, 60.0]));
    assert_eq!(
        kit.lock().await.calls[2]["result"]["error"],
        err.to_string()
    );
}

#[tokio::test]
async fn long_research_keeps_the_branch_and_numeric_sources_across_compactions() {
    check_answer(&[Some(EDITED), Some("SAME")], EDITED).await;
}

#[tokio::test]
async fn langfuse_records_multimodal_research_tools_compaction_and_editing() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};
    use tracing::Instrument;

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(crate::telemetry::SessionProcessor(
            SimpleSpanProcessor::new(exporter.clone()),
        ))
        .build();
    let _guard = tracing::subscriber::set_default(crate::telemetry::subscriber(&provider));
    let root = tracing::info_span!(target: "rustverse::ai", "prompt");
    let session_id = "020d2bcc-6631-4535-9399-c439f12c0374";
    crate::telemetry::set_prompt_session(&root, session_id.to_owned());
    check_answer(&[Some(EDITED), Some("SAME")], EDITED)
        .instrument(root)
        .await;
    let spans = exporter.get_finished_spans().unwrap();
    let root = spans.iter().find(|s| s.name == "prompt").unwrap();
    assert!(spans.iter().all(|s| {
        s.attributes
            .iter()
            .any(|kv| kv.key.as_str() == "langfuse.session.id" && kv.value.as_str() == session_id)
    }));
    assert!(
        spans
            .iter()
            .all(|s| s.span_context.trace_id() == root.span_context.trace_id())
    );
    for name in [
        "chat",
        "execute_tool",
        "compact_context",
        "proofread",
        "fact_check",
    ] {
        assert!(spans.iter().any(|s| s.name == name), "missing {name}");
    }
    let editor = spans.iter().find(|s| s.name == "proofread").unwrap();
    let fact_check = spans.iter().find(|s| s.name == "fact_check").unwrap();
    assert_eq!(fact_check.parent_span_id, editor.span_context.span_id());
    let attribute = |s: &opentelemetry_sdk::trace::SpanData, key: &str| {
        s.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    };
    let ttft = "langfuse.observation.completion_start_time";
    for chat in spans.iter().filter(|s| s.name == "chat") {
        if chat.parent_span_id == editor.span_context.span_id()
            || chat.parent_span_id == fact_check.span_context.span_id()
        {
            assert!(attribute(chat, ttft).is_none());
        } else {
            let first = chrono::DateTime::parse_from_rfc3339(&attribute(chat, ttft).unwrap())
                .unwrap()
                .to_utc();
            assert!(
                (chat.start_time..=chat.end_time).contains(&std::time::SystemTime::from(first))
            );
        }
    }
    let tools: Vec<_> = spans.iter().filter(|s| s.name == "execute_tool").collect();
    assert_eq!(tools.len(), 15);
    for tool in tools {
        let id = attribute(tool, "gen_ai.tool.call.id").unwrap();
        let generation_id = attribute(tool, "langfuse.observation.metadata.generation_id").unwrap();
        let generation = spans
            .iter()
            .find(|s| s.name == "chat" && s.span_context.span_id().to_string() == generation_id)
            .unwrap();
        assert!(
            tool.links
                .iter()
                .any(|l| l.span_context == generation.span_context)
        );
        assert!(
            attribute(generation, "gen_ai.output.messages")
                .unwrap()
                .contains(&format!("\"{id}\""))
        );
        assert_eq!(
            attribute(tool, "gen_ai.tool.call.outcome").as_deref(),
            Some("success")
        );
        let status = match attribute(tool, "gen_ai.tool.name").unwrap().as_str() {
            "get_endgame_seasons" => "partial",
            "get_endgame_season" => "unavailable",
            "compute"
                if attribute(tool, "gen_ai.tool.call.arguments")
                    .unwrap()
                    .contains("missing") =>
            {
                "error"
            }
            _ => "success",
        };
        assert_eq!(
            attribute(tool, "langfuse.observation.metadata.result_status").as_deref(),
            Some(status)
        );
    }
    assert!(
        spans
            .iter()
            .any(|s| s.name == "chat" && s.parent_span_id == editor.span_context.span_id())
    );
    for (operation, field, expected) in [
        ("chat", "gen_ai.system_instructions", "Answer the question"),
        ("chat", "gen_ai.input.messages", QUESTION),
        ("chat", "gen_ai.input.messages", "/9j/"),
        ("chat", "gen_ai.input.messages", "iVBORw0KGgo="),
        ("chat", "gen_ai.output.messages", FINAL),
        ("chat", "gen_ai.output.messages", EDITED),
        ("chat", "gen_ai.usage.input_tokens", "18"),
        ("execute_tool", "gen_ai.tool.call.arguments", "damage * 2.0"),
        ("execute_tool", "gen_ai.tool.call.result", "42"),
        ("compact_context", "langfuse.observation.input", NOTES),
        ("compact_context", "langfuse.observation.output", NOTES),
    ] {
        assert!(
            spans.iter().any(|s| s.name == operation
                && s.attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == field && kv.value.as_str().contains(expected))),
            "missing {field} on {operation}"
        );
    }
}

#[tokio::test]
async fn rejected_edits_use_the_latest_feedback_and_stop_after_ten_attempts() {
    let mut replies = [
        Some(WRONG),
        Some(WRONG_ROOM),
        Some(STILL_WRONG),
        Some(WRONG_VALUE),
    ]
    .repeat(5);
    check_answer(&replies, FINAL).await;
    replies[18..].copy_from_slice(&[Some(EDITED), Some("SAME")]);
    check_answer(&replies, EDITED).await;
    check_answer(&[Some(WRONG), Some("DIFFERENT"), Some(FINAL)], FINAL).await;
}

#[tokio::test]
async fn unchanged_or_unfinished_edits_deliver_the_researched_draft() {
    check_answer(&[None, None, None], FINAL).await;
    check_answer(&[Some(FINAL)], FINAL).await;
    check_answer(&[Some(EDITED), Some("не уверен"), Some(FINAL)], FINAL).await;
    check_answer(&[Some(EDITED), None, Some(EDITED), Some("SAME")], EDITED).await;
}

async fn check_answer(editing_replies: &[Option<&'static str>], expected: &str) {
    check_review(
        editing_replies,
        &[(expected, Some(VERIFIED))],
        &[TERRA_HIGH],
        Some(expected),
    )
    .await;
}

const TERRA_HIGH: &str = "gpt-6.1-terra@high";
const SOL_MEDIUM: &str = "gpt-6.2-sol@medium";
const SOL_HIGH: &str = "gpt-6.2-sol@high";

#[tokio::test]
async fn fact_checker_reviews_the_exact_patch_and_stops_if_a_rejected_text_returns() {
    check_review(
        &[Some(EDITED), Some("SAME")],
        &[(EDITED, Some(CORRECTION)), (CORRECTED, Some(VERIFIED))],
        &[TERRA_HIGH, TERRA_HIGH],
        Some(CORRECTED),
    )
    .await;
    let undo =
        r#"{"corrections":[{"original":"63","replacement":"42","source":"Предыдущее значение."}]}"#;
    check_review(
        &[Some(EDITED), Some("SAME")],
        &[
            (EDITED, Some(CORRECTION)),
            (CORRECTED, Some(undo)),
            (CORRECTED, Some(undo)),
            (CORRECTED, Some(undo)),
        ],
        &[TERRA_HIGH, TERRA_HIGH, SOL_MEDIUM, SOL_HIGH],
        None,
    )
    .await;
    check_review(
        &[Some(FINAL)],
        &[(FINAL, None), (FINAL, None), (FINAL, None)],
        &[TERRA_HIGH, SOL_MEDIUM, SOL_HIGH],
        None,
    )
    .await;
    check_review(
        &[Some(EDITED), Some("SAME")],
        &[
            (EDITED, Some(CORRECTION)),
            (CORRECTED, Some(undo)),
            (CORRECTED, Some(VERIFIED)),
        ],
        &[TERRA_HIGH, TERRA_HIGH, SOL_MEDIUM],
        Some(CORRECTED),
    )
    .await;
    let partial_undo = r#"{"corrections":[{"original":"63","replacement":"42","source":"Прежняя оценка."},{"original":"вторая комната","replacement":"первая комната","source":"Другая комната."}]}"#;
    check_review(
        &[Some(EDITED), Some("SAME")],
        &[
            (EDITED, Some(CORRECTION)),
            (CORRECTED, Some(partial_undo)),
            (CORRECTED, Some(VERIFIED)),
        ],
        &[TERRA_HIGH, TERRA_HIGH, SOL_MEDIUM],
        Some(CORRECTED),
    )
    .await;
    let next =
        r#"{"corrections":[{"original":"63","replacement":"64","source":"Новое вычисление."}]}"#;
    let updated = CORRECTED.replace("63", "64");
    check_review(
        &[Some(EDITED), Some("SAME")],
        &[
            (EDITED, Some(CORRECTION)),
            (CORRECTED, Some(next)),
            (&updated, Some(VERIFIED)),
        ],
        &[TERRA_HIGH, TERRA_HIGH, SOL_MEDIUM],
        Some(&updated),
    )
    .await;
}

async fn check_review(
    editing_replies: &[Option<&'static str>],
    fact_replies: &[(&str, Option<&'static str>)],
    fact_routes: &[&'static str],
    expected: Option<&str>,
) {
    check_run(
        QUESTION,
        None,
        editing_replies,
        fact_replies,
        fact_routes,
        expected,
    )
    .await;
}

#[tokio::test]
async fn auto_routes_research_and_recovers_from_an_invalid_router_reply() {
    for (question, reply, selected) in [
        ("Опиши майндскейпы Санбрингер", "low", TERRA_HIGH),
        ("Подбери команду для этой комнаты", "medium", SOL_MEDIUM),
        ("Вот мой билд, что заменить на скриншоте?", "high", SOL_HIGH),
        ("А если заменить сигну?", "неизвестно", SOL_MEDIUM),
        ("А в этой комнате?", "refusal", SOL_MEDIUM),
    ] {
        check_run(
            question,
            Some((reply, selected)),
            &[Some(FINAL)],
            &[(FINAL, Some(VERIFIED))],
            &[TERRA_HIGH],
            Some(FINAL),
        )
        .await;
    }
}

#[tokio::test]
async fn decisions_trace_keeps_the_session_usage_choice_and_fallback() {
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SimpleSpanProcessor};
    use tracing::Instrument;

    let exporter = InMemorySpanExporter::default();
    let otel = SdkTracerProvider::builder()
        .with_span_processor(crate::telemetry::SessionProcessor(
            SimpleSpanProcessor::new(exporter.clone()),
        ))
        .build();
    let _guard = tracing::subscriber::set_default(crate::telemetry::subscriber(&otel));
    for (choice, selected) in [
        ("high", SOL_HIGH),
        ("refusal", SOL_MEDIUM),
        ("http_error", SOL_MEDIUM),
    ] {
        let root = tracing::info_span!(target: "rustverse::ai", "prompt", selected_model = tracing::field::Empty);
        crate::telemetry::set_prompt_session(&root, choice.into());
        check_run(
            QUESTION,
            Some((choice, selected)),
            &[Some(FINAL)],
            &[(FINAL, Some(VERIFIED))],
            &[TERRA_HIGH],
            Some(FINAL),
        )
        .instrument(root)
        .await;
        let spans = exporter.get_finished_spans().unwrap();
        let mut session = spans.iter().filter(|s| {
            s.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "langfuse.session.id" && kv.value.as_str() == choice)
        });
        let root = session.clone().find(|s| s.name == "prompt").unwrap();
        let route = session.find(|s| s.name == "route_question").unwrap();
        assert_eq!(route.span_context.trace_id(), root.span_context.trace_id());
        assert_eq!(route.parent_span_id, root.span_context.span_id());
        let attr = |key: &str| {
            route
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.to_string())
                .unwrap()
        };
        assert_eq!(attr("langfuse.observation.type"), "generation");
        assert_eq!(attr("gen_ai.request.model"), "gpt-6-luna");
        assert_eq!(attr("selected_model"), selected);
        if choice == "http_error" {
            assert_eq!(attr("langfuse.observation.metadata.fallback"), "true");
            assert_eq!(
                attr("langfuse.observation.status_message"),
                "Decisions HTTP 403"
            );
            assert!(
                !route
                    .attributes
                    .iter()
                    .any(|kv| kv.value.as_str().contains("private upstream diagnostic"))
            );
            continue;
        }
        assert_eq!(attr("gen_ai.usage.input_tokens"), "20");
        assert_eq!(attr("gen_ai.usage.output_tokens"), "0");
        assert_eq!(attr("gen_ai.usage.cache_read.input_tokens"), "5");
        assert_eq!(attr("gen_ai.usage.cache_creation.input_tokens"), "2");
        assert!((attr("gen_ai.usage.cost").parse::<f64>().unwrap() - 0.0000016).abs() < 1e-12);
        assert!(attr("langfuse.observation.input").contains("input_image"));
        assert!(attr("langfuse.observation.output").contains(choice));
        assert_eq!(
            attr("langfuse.observation.metadata.fallback"),
            (choice == "refusal").to_string()
        );
        if choice == "refusal" {
            assert!(attr("langfuse.observation.status_message").contains("refused"));
        } else {
            assert!(attr("langfuse.observation.output").contains("probabilities"));
        }
    }
}

async fn check_run(
    question: &'static str,
    route: Option<(&'static str, &'static str)>,
    editing_replies: &[Option<&'static str>],
    fact_replies: &[(&str, Option<&'static str>)],
    fact_routes: &[&'static str],
    expected: Option<&str>,
) {
    let (tx, rx) = watch::channel(PromptProgress::default());
    let fixture = Fixture {
        question,
        route,
        turns: Default::default(),
        summaries: Default::default(),
        progress: rx.clone(),
        editing_calls: Default::default(),
        edit_attempts: Default::default(),
        editing_replies: editing_replies.to_vec(),
        fact_calls: Default::default(),
        fact_routes: fact_routes.to_vec(),
        fact_replies: fact_replies
            .iter()
            .map(|(answer, reply)| (answer.to_string(), *reply))
            .collect(),
    };
    let mut tools = KitTools::for_question(Client::new(), QUESTION);
    tools.seasons = Some(SeasonTools::fixture());
    let page = Comparison::parse(
        "https://zzz.gachabase.net/diff/agents/1601/test?prev=beta_1.0.0_1&cur=beta_1.0.0_2",
    )
    .unwrap();
    let snaps = || Snapshots {
        prev: json!({"agent":{"damage":20}}),
        cur: json!({"agent":{"damage":21}}),
    };
    tools.report_input(&page, snaps(), snaps()).unwrap();
    let references = tools.references.clone();
    let answer = run_agent(
        crate::transport::Provider {
            config: OpenAIConfig::new("mock"),
            client: DynHttpClient::new(fixture.clone()),
        },
        if route.is_some() { "auto" } else { MODEL },
        tools,
        ["Answer the question", crate::ENDGAME_GUIDANCE],
        crate::PromptInput {
            text: question.into(),
            images: vec![
                crate::PromptImage::from_bytes(b"\xff\xd8\xff").unwrap(),
                crate::PromptImage::from_bytes(b"\x89PNG\r\n\x1a\n").unwrap(),
            ],
        },
        &[ImagePromptTurn {
            question: "Кого выбрать?".into(),
            answer: NOTES.into(),
            images: vec![crate::PromptImage::from_bytes(b"\xff\xd8\xff").unwrap()],
        }],
        Some(Preview::Progress(tx)),
    )
    .await;
    assert_eq!(answer.as_deref().ok(), expected, "{answer:?}");
    let summaries = fixture.summaries.load(Ordering::Relaxed);
    if route.is_none() {
        assert!(summaries >= 2);
    }
    assert_eq!(
        fixture.editing_calls.load(Ordering::Relaxed),
        editing_replies.len()
    );
    assert_eq!(
        fixture.fact_calls.load(Ordering::Relaxed),
        fact_replies.len()
    );
    let turns = usize::from(route.is_some()) + 15 + editing_replies.len() + fact_replies.len();
    assert_eq!(rx.borrow().turns, turns);
    if route.is_some_and(|(reply, _)| reply == "http_error") {
        assert!(rx.borrow().tokens.is_none() && rx.borrow().cost_usd.is_none());
    } else if editing_replies.iter().all(Option::is_some) {
        assert_eq!(
            rx.borrow().tokens,
            Some((turns as u64 + summaries as u64) * 20)
        );
    }
    let refs = references.lock().await;
    assert!(
        refs.seasons
            .iter()
            .any(|s| s.mode == "shiyu_defense" && s.season_id == 62053 && s.requested == "current")
    );
    assert!(refs.entries.contains(SOURCE));
}
