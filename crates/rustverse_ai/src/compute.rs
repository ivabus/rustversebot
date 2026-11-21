//! Local arithmetic over numeric fields from this run's immutable GachaBase views.
use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use rhai::{Array, Dynamic, Engine, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
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
    values: Vec<Numbers>,
}

enum Numbers {
    Scalar(f64),
    Array(Vec<f64>),
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

impl Calculation {
    pub fn prepare(self, sources: &HashMap<String, Value>) -> Result<Prepared> {
        ensure!(
            self.script.len() <= 2048 && self.bindings.len() <= 12,
            "Calculation too large"
        );
        let mut count = 0;
        let mut values = Vec::new();
        for binding in &self.bindings {
            let entry = sources
                .get(&binding.source)
                .context("Unknown source in this run")?;
            ensure!(
                binding.path.starts_with('/') && binding.path.len() <= 384,
                "Expected a JSON pointer"
            );
            let value = if let Some((prefix, suffix)) = binding.path.split_once("/*/") {
                let rows = entry
                    .pointer(prefix)
                    .and_then(Value::as_array)
                    .context("Expected an array before /*/")?;
                count += rows.len();
                ensure!(count <= MAX_VALUES, "Too many numeric inputs");
                let ptr = format!("/{suffix}");
                let numbers = rows
                    .iter()
                    .map(|r| number(r.pointer(&ptr).context("Missing field in array")?))
                    .collect::<Result<Vec<_>>>()?;
                Numbers::Array(numbers)
            } else {
                let v = entry
                    .pointer(&binding.path)
                    .context("Missing numeric input")?;
                if let Some(items) = v.as_array() {
                    count += items.len();
                    ensure!(count <= MAX_VALUES, "Too many numeric inputs");
                    Numbers::Array(items.iter().map(number).collect::<Result<Vec<_>>>()?)
                } else {
                    count += 1;
                    Numbers::Scalar(number(v)?)
                }
            };
            values.push(value);
        }
        ensure!(count <= MAX_VALUES, "Too many numeric inputs");
        Ok(Prepared {
            request: self,
            values,
        })
    }
}

impl Prepared {
    pub fn run(self) -> Result<Value> {
        let mut engine = Engine::new_raw();
        engine
            .set_max_operations(8000)
            .set_max_variables(32)
            .set_max_expr_depths(24)
            .set_max_array_size(MAX_VALUES)
            .set_max_string_size(512)
            .set_max_map_size(16);
        engine.disable_symbol("eval");
        engine.register_iterator::<Array>();
        engine.register_iterator::<std::ops::Range<i64>>();
        engine.register_fn("len", |v: &mut Array| v.len() as i64);
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
        for (i, value) in self.values.into_iter().enumerate() {
            let input = match value {
                Numbers::Array(array) => {
                    Dynamic::from_array(array.into_iter().map(Dynamic::from_float).collect())
                }
                Numbers::Scalar(n) => Dynamic::from_float(n),
            };
            scope.push_constant(format!("x{i}"), input);
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
        Ok(json!({"result":result,"inputs":self.request.bindings}))
    }
}
