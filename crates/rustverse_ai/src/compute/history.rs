use std::sync::Arc;

use rhai::{Array, Dynamic, Engine, EvalAltResult, Scope};
use serde_json::Value;

type ReadResult = Result<Dynamic, Box<EvalAltResult>>;

// Keep the JSON shared. Indexing a field or iterating a batch should not copy an
// entire tool response into every Rhai variable that refers to it.
#[derive(Clone)]
struct JsonView {
    root: Arc<Value>,
    path: String,
}

impl JsonView {
    fn value(&self) -> &Value {
        self.root.pointer(&self.path).expect("existing JSON path")
    }

    fn child(&self, key: &str, value: &Value) -> ReadResult {
        Ok(match value {
            Value::Null => Dynamic::UNIT,
            Value::Bool(b) => (*b).into(),
            Value::String(s) => s.as_str().into(),
            Value::Number(n) if n.is_f64() => n.as_f64().unwrap().into(),
            Value::Number(n) => n
                .as_i64()
                .ok_or("JSON integer exceeds the Rhai integer range")?
                .into(),
            // A bounded numeric array can be used by sum() or returned directly.
            Value::Array(a)
                if a.len() <= super::MAX_VALUES && a.iter().all(|n| n.is_i64() || n.is_f64()) =>
            {
                let numbers = a
                    .iter()
                    .map(|n| self.child("", n))
                    .collect::<Result<Array, _>>()?;
                Dynamic::from_array(numbers)
            }
            Value::Array(_) | Value::Object(_) => Dynamic::from(Self {
                root: Arc::clone(&self.root),
                path: format!(
                    "{}/{}",
                    self.path,
                    key.replace('~', "~0").replace('/', "~1")
                ),
            }),
        })
    }

    fn field(&mut self, key: &str) -> ReadResult {
        let fields = self.value().as_object().ok_or("Expected a JSON object")?;
        fields
            .get(key)
            .map_or(Ok(Dynamic::UNIT), |v| self.child(key, v))
    }

    fn item(&mut self, index: i64) -> ReadResult {
        let items = self.value().as_array().ok_or("Expected a JSON array")?;
        let idx = if index < 0 {
            items.len() as i64 + index
        } else {
            index
        };
        let value = items
            .get(idx as usize)
            .ok_or_else(|| format!("Index {index} outside JSON array of length {}", items.len()))?;
        self.child(&idx.to_string(), value)
    }

    fn len(&mut self) -> i64 {
        match self.value() {
            Value::Array(a) => a.len() as i64,
            Value::Object(m) => m.len() as i64,
            _ => unreachable!("view of a JSON container"),
        }
    }

    fn keys(&mut self) -> Result<Array, Box<EvalAltResult>> {
        let fields = self
            .value()
            .as_object()
            .ok_or("keys expects a JSON object")?;
        if fields.len() > super::MAX_VALUES {
            return Err("Too many keys to materialize; iterate the object directly".into());
        }
        Ok(fields.keys().map(|key| key.as_str().into()).collect())
    }
}

impl IntoIterator for JsonView {
    type Item = ReadResult;
    type IntoIter = Box<dyn Iterator<Item = ReadResult>>;

    fn into_iter(mut self) -> Self::IntoIter {
        match self.value() {
            Value::Array(a) => Box::new((0..a.len() as i64).map(move |i| self.item(i))),
            Value::Object(m) => {
                let keys: Vec<_> = m.keys().cloned().collect();
                Box::new(keys.into_iter().map(|key| Ok(key.into())))
            }
            _ => unreachable!("view of a JSON container"),
        }
    }
}

pub(super) fn bind(engine: &mut Engine, scope: &mut Scope, history: Vec<Arc<Value>>) {
    engine
        .register_indexer_get(JsonView::field)
        .register_indexer_get(JsonView::item)
        .register_fn("len", JsonView::len)
        .register_fn("keys", JsonView::keys)
        .register_iterator_result::<JsonView, Dynamic>();
    let calls: Array = history
        .into_iter()
        .map(|root| {
            Dynamic::from(JsonView {
                root,
                path: String::new(),
            })
        })
        .collect();
    scope.push_constant("calls", calls);
}
