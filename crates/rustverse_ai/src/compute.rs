//! Local arithmetic over numeric fields from this run's immutable source views.
mod history;

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use rhai::{Array, Dynamic, Engine, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    source: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Calculation {
    script: String,
    bindings: Vec<Binding>,
}

const MAX_VALUES: usize = 512;

pub(crate) struct Prepared {
    request: Calculation,
    values: Vec<(String, Numbers)>,
}

enum Numbers {
    Scalar(f64),
    Array(Vec<f64>),
    Matches(Vec<NumericMatch>),
}

#[derive(Serialize)]
struct NumericMatch {
    path: String,
    value: f64,
}

fn number(value: &Value) -> Result<f64> {
    const EXACT_INTEGER_LIMIT: u64 = 1 << 53;
    if let Some(abs) = value
        .as_u64()
        .or_else(|| value.as_i64().map(i64::unsigned_abs))
    {
        ensure!(
            abs <= EXACT_INTEGER_LIMIT,
            "Integer exceeds exact floating-point input range"
        );
    }
    value.as_f64().context("Expected a numeric field")
}

impl Binding {
    fn expand(&self, entry: &Value, remaining: usize) -> Result<Vec<NumericMatch>> {
        let mut nodes = vec![(String::new(), entry)];
        for segment in self.path[1..].split('/') {
            let pointer = format!("/{segment}");
            let mut next = Vec::new();
            for (path, node) in nodes {
                if segment != "*" {
                    let path = format!("{path}{pointer}");
                    let child = node
                        .pointer(&pointer)
                        .with_context(|| format!("Missing field at {path}"))?;
                    next.push((path, child));
                    continue;
                }
                match node {
                    Value::Array(items) => {
                        ensure!(
                            items.len() <= MAX_VALUES - next.len(),
                            "Too many wildcard matches at {path}"
                        );
                        next.extend(
                            items
                                .iter()
                                .enumerate()
                                .map(|(i, v)| (format!("{path}/{i}"), v)),
                        );
                    }
                    Value::Object(fields) => {
                        ensure!(
                            fields.len() <= MAX_VALUES - next.len(),
                            "Too many wildcard matches at {path}"
                        );
                        next.extend(fields.iter().map(|(key, v)| {
                            let key = key.replace('~', "~0").replace('/', "~1");
                            (format!("{path}/{key}"), v)
                        }));
                    }
                    _ => bail!("Wildcard expects an array or object at {path}"),
                }
            }
            nodes = next;
        }
        ensure!(nodes.len() <= remaining, "Too many numeric inputs");
        nodes
            .into_iter()
            .map(|(path, node)| {
                let value = number(node).with_context(|| format!("At {path}"))?;
                Ok(NumericMatch { path, value })
            })
            .collect()
    }

    fn resolve(&self, sources: &HashMap<String, Value>, count: &mut usize) -> Result<Numbers> {
        let entry = sources
            .get(&self.source)
            .context("Unknown source in this run")?;
        ensure!(self.path.starts_with('/'), "JSON pointer must start with /");
        ensure!(
            self.path.len() <= 384,
            "JSON pointer is {} bytes; maximum is 384",
            self.path.len()
        );
        let value = if self.path.split('/').any(|s| s == "*") {
            let matches = self.expand(entry, MAX_VALUES - *count)?;
            *count += matches.len();
            Numbers::Matches(matches)
        } else {
            let v = entry.pointer(&self.path).context("Missing numeric input")?;
            if let Some(items) = v.as_array() {
                *count += items.len();
                ensure!(*count <= MAX_VALUES, "Too many numeric inputs");
                Numbers::Array(items.iter().map(number).collect::<Result<Vec<_>>>()?)
            } else {
                *count += 1;
                ensure!(*count <= MAX_VALUES, "Too many numeric inputs");
                Numbers::Scalar(number(v)?)
            }
        };
        Ok(value)
    }
}

impl Calculation {
    pub fn prepare(self, sources: &HashMap<String, Value>) -> Result<Prepared> {
        ensure!(
            self.bindings.len() <= 12,
            "Received {} bindings; maximum is 12. Split the calculation or bind a numeric array",
            self.bindings.len()
        );
        ensure!(
            self.script.len() <= 2048,
            "Script is {} bytes; maximum is 2048. Shorten it or split the calculation",
            self.script.len()
        );
        let mut count = 0;
        let mut values = Vec::with_capacity(self.bindings.len());
        for (i, binding) in self.bindings.iter().enumerate() {
            let name = binding.name.clone().unwrap_or_else(|| format!("x{i}"));
            // Rhai applies the same identifier and reserved-word rules to function pointers.
            ensure!(
                name.len() <= 64 && rhai::FnPtr::new(name.as_str()).is_ok(),
                "Invalid variable name for bindings[{i}]: {name}"
            );
            ensure!(
                !values.iter().any(|(existing, _)| existing == &name),
                "Duplicate variable name for bindings[{i}]: {name}"
            );
            let value = binding.resolve(sources, &mut count).map_err(|err| {
                anyhow::anyhow!(
                    "bindings[{i}] {name} ({} at {}): {err:#}",
                    binding.source,
                    binding.path
                )
            })?;
            values.push((name, value));
        }
        Ok(Prepared {
            request: self,
            values,
        })
    }
}

impl Prepared {
    pub fn run(self, calls: Vec<Arc<Value>>) -> Result<Value> {
        let mut engine = Engine::new_raw();
        engine
            .set_max_operations(8000)
            .set_max_variables(32 + 1) // Keep the script's variable budget alongside calls.
            .set_max_expr_depths(24, 24)
            .set_max_call_levels(16)
            .set_max_array_size(MAX_VALUES)
            .set_max_string_size(512)
            .set_max_map_size(16);
        engine.disable_symbol("eval");
        engine.register_iterator::<Array>();
        engine.register_iterator::<std::ops::Range<i64>>();
        engine.register_fn("len", |v: &mut Array| v.len() as i64);
        engine.register_fn(
            "push",
            |items: &mut Array, item: Dynamic| -> Result<(), Box<rhai::EvalAltResult>> {
                if items.len() >= MAX_VALUES {
                    return Err("Array size limit exceeded".into());
                }
                items.push(item);
                Ok(())
            },
        );
        engine.register_fn("abs", f64::abs);
        engine.register_fn("min", f64::min);
        engine.register_fn("max", f64::max);
        engine.register_fn(
            "sum",
            |values: Array| -> Result<f64, Box<rhai::EvalAltResult>> {
                values.iter().try_fold(0.0, |acc, v| {
                    Ok(acc
                        + v.as_float()
                            .or_else(|_| v.as_int().map(|i| i as f64))
                            .map_err(|_| "sum expects numbers")?)
                })
            },
        );
        let mut scope = Scope::new();
        history::bind(&mut engine, &mut scope, calls);
        let mut inputs = Vec::with_capacity(self.values.len());
        for ((name, value), binding) in self.values.into_iter().zip(self.request.bindings) {
            let mut reference = json!(binding);
            let input = match value {
                Numbers::Array(array) => {
                    Dynamic::from_array(array.into_iter().map(Dynamic::from_float).collect())
                }
                Numbers::Scalar(n) => Dynamic::from_float(n),
                Numbers::Matches(matches) => {
                    reference["matches"] = json!(matches);
                    Dynamic::from_array(
                        matches
                            .into_iter()
                            .map(|m| Dynamic::from_float(m.value))
                            .collect(),
                    )
                }
            };
            scope.push_constant(name, input);
            inputs.push(reference);
        }
        let result = engine
            .eval_with_scope::<Dynamic>(&mut scope, &self.request.script)
            .map_err(|error| anyhow::anyhow!("Calculation: {error}"))?;
        fn finite(n: Dynamic) -> Result<Value> {
            if let Some(i) = n.clone().try_cast::<i64>() {
                return Ok(json!(i));
            }
            if let Some(f) = n.try_cast::<f64>().filter(|f| f.is_finite()) {
                return Ok(json!(f));
            }
            bail!("Expected a finite numeric result")
        }
        let result = if result.is_array() {
            let a = result.into_array().map_err(anyhow::Error::msg)?;
            ensure!(a.len() <= 64, "Return at most 64 numbers");
            Value::Array(a.into_iter().map(finite).collect::<Result<_>>()?)
        } else {
            finite(result)?
        };
        Ok(json!({"result":result,"inputs":inputs}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calculate(script: &str, bindings: Value) -> Result<Value> {
        let sources = HashMap::from([
            ("previous".into(), json!({"hit":20})),
            ("current".into(), json!({"hits":[6,19],"zero":0})),
        ]);
        serde_json::from_value::<Calculation>(json!({"script":script,"bindings":bindings}))?
            .prepare(&sources)?
            .run(Vec::new())
    }

    #[test]
    fn named_inputs_keep_their_sources_and_work_in_any_order() {
        let before = json!({"name":"before","source":"previous","path":"/hit"});
        let hits = json!({"name":"hits","source":"current","path":"/hits"});
        for bindings in [json!([before, hits]), json!([hits, before])] {
            let answer = calculate("sum(hits) - before", bindings.clone()).unwrap();
            assert_eq!(answer, json!({"result":5.0,"inputs":bindings}));
        }
        let bindings = json!([hits,{"source":"previous","path":"/hit"}]);
        assert_eq!(
            calculate("x1 - sum(hits)", bindings).unwrap()["result"],
            -5.0
        );
    }

    #[test]
    fn invalid_inputs_identify_what_to_fix() {
        let binding = |name: &str, path: &str| json!({"name":name,"source":"current","path":path});
        for name in ["", "let", "a + b"] {
            assert!(
                calculate("1", json!([binding(name, "/zero")]))
                    .unwrap_err()
                    .to_string()
                    .contains("Invalid variable name")
            );
        }
        let err = calculate(
            "x1",
            json!([binding("x1","/hits"),{"source":"current","path":"/zero"}]),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Duplicate variable name for bindings[1]: x1")
        );
        let err = calculate("before", json!([binding("before", "/absent")])).unwrap_err();
        assert!(
            err.to_string()
                .contains("before (current at /absent): Missing numeric input")
        );
    }

    #[test]
    fn size_errors_identify_the_limit_exceeded() {
        let bindings = vec![json!({"source":"previous","path":"/hit"}); 17];
        let err = calculate("1", json!(bindings)).unwrap_err().to_string();
        assert!(err.contains("Received 17 bindings; maximum is 12"), "{err}");
        let err = calculate(&"1".repeat(2049), json!([]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Script is 2049 bytes; maximum is 2048"),
            "{err}"
        );
    }

    #[test]
    fn a_single_wildcard_binds_numeric_rows_for_iteration() {
        let sources = HashMap::from([(
            "page".into(),
            json!({"rows":[{"hp":50},{"hp":200},{"hp":150}]}),
        )]);
        let request: Calculation = serde_json::from_value(json!({
            "script":"let peak = hp[0]; for value in hp { peak = max(peak, value); } 100.0 * (peak / hp[0] - 1.0)",
            "bindings":[{"name":"hp","source":"page","path":"/rows/*/hp"}]
        })).unwrap();
        let value = request.prepare(&sources).unwrap().run(Vec::new()).unwrap();
        assert_eq!(value["result"], 300.0);
    }

    #[test]
    fn script_can_walk_large_responses_and_id_keyed_rooms() {
        let calls = vec![Arc::new(
            json!({"name":"get_endgame_seasons","args":{},"result":{
                "results":[{"unrelated":"x".repeat(4096)},{"rooms":{"a~/b":{"id":7,"hp":12.5}}}]
            }}),
        )];
        let request: Calculation = serde_json::from_value(json!({
            "bindings":[],
            "script":r#"
                let rooms = calls[-1].result.results[-1].rooms;
                fn collect(rooms) {
                    let rows = [];
                    for id in rooms.keys() { rows.push(rooms[id]); }
                    let output = [];
                    for row in rows { output.push([row.id, row.hp]); }
                    return output[0];
                }
                collect(rooms)
            "#
        }))
        .unwrap();
        let value = request
            .prepare(&HashMap::new())
            .unwrap()
            .run(calls)
            .unwrap();
        assert_eq!(value["result"], json!([7, 12.5]));
    }

    #[test]
    fn nested_wildcards_keep_exact_source_paths_in_value_order() {
        let sources = HashMap::from([(
            "page".into(),
            json!({"rows":[
                {"a~/b":{"values":[1,2]}}, {}, {"z":{"values":[5]}}
            ]}),
        )]);
        let request: Calculation = serde_json::from_value(json!({
            "script":"x0", "bindings":[{"source":"page","path":"/rows/*/*/values/*"}]
        }))
        .unwrap();
        let value = request.prepare(&sources).unwrap().run(vec![]).unwrap();
        assert_eq!(value["result"], json!([1.0, 2.0, 5.0]));
        assert_eq!(
            value["inputs"][0]["matches"],
            json!([
                {"path":"/rows/0/a~0~1b/values/0","value":1.0},
                {"path":"/rows/0/a~0~1b/values/1","value":2.0},
                {"path":"/rows/2/z/values/0","value":5.0}
            ])
        );
    }

    #[test]
    fn local_functions_and_collection_operations_respect_limits() {
        for script in [
            "fn recurse() { recurse() } recurse()",
            "let a=[]; for n in 0..513 { a.push(n); } len(a)",
        ] {
            assert!(calculate(script, json!([])).is_err(), "{script}");
        }
        let sources = HashMap::from([("page".into(), json!([vec![1; 257], vec![2; 256]]))]);
        let request: Calculation = serde_json::from_value(json!({
            "script":"sum(x0)", "bindings":[{"source":"page","path":"/*/*"}]
        }))
        .unwrap();
        assert!(
            request
                .prepare(&sources)
                .err()
                .unwrap()
                .to_string()
                .contains("Too many wildcard matches")
        );
    }

    #[test]
    fn wildcard_errors_identify_the_path_to_correct() {
        let sources = HashMap::from([(
            "page".into(),
            json!({"zones":[{"rooms":{"42":{"hp":20}}},{"rooms":{"42":{"hp":"unknown"}}}]}),
        )]);
        let error_at = |path: &str| {
            let request: Calculation = serde_json::from_value(json!({
                "script":"hp", "bindings":[{"name":"hp","source":"page","path":path}]
            }))
            .unwrap();
            request.prepare(&sources).err().unwrap().to_string()
        };
        for (path, message) in [
            (
                "/zones/*/rooms/*/hp",
                "At /zones/1/rooms/42/hp: Expected a numeric field",
            ),
            (
                "/zones/0/rooms/42/hp/*",
                "Wildcard expects an array or object at /zones/0/rooms/42/hp",
            ),
            ("/zones/*/rooms/43/hp", "Missing field at /zones/0/rooms/43"),
            (
                "/zones/*/rooms/42/hp",
                "At /zones/1/rooms/42/hp: Expected a numeric field",
            ),
        ] {
            let err = error_at(path);
            assert!(
                err.contains(&format!("bindings[0] hp (page at {path})")),
                "{err}"
            );
            assert!(err.contains(message), "{err}");
        }
    }
}
