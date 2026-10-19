//! JSONL resolver driver used for parity and bounded process/batch measurements.
//!
//! Every iteration creates the context, resolves the complete configuration and
//! constructs the transport value. Input parsing and final output serialization
//! are outside `elapsedNs`, while external process measurements include them.
use failforge_config::{
    Context, RunConfig, from_mapping, from_profile_mapping, load_yaml, resolve_config,
};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Instant;

const MAX_ITERATIONS: u64 = 1_000_000;

fn required_string<'a>(request: &'a Value, field: &str) -> Result<&'a str, String> {
    request
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("benchmark request requires string {field}"))
}

fn resolve(request: &Value) -> Result<Value, String> {
    let cwd = required_string(request, "cwd")?;
    let installation = required_string(request, "installation")?;
    let context =
        Context::new(Path::new(cwd), Path::new(installation)).map_err(|error| error.to_string())?;
    let operation = required_string(request, "operation")?;
    let value = request
        .get("value")
        .ok_or_else(|| "benchmark request requires value".to_owned())?;
    let base_dir = request
        .get("base_dir")
        .and_then(Value::as_str)
        .map(Path::new);
    let config = match operation {
        "cli" => resolve_config(value, &context),
        "mapping" => from_mapping(value, &context, base_dir)
            .map(|campaign| RunConfig::Campaign(Box::new(campaign))),
        "profile" => {
            let project = required_string(request, "project")?;
            from_profile_mapping(value, Path::new(project), &context, base_dir)
                .map(RunConfig::Profile)
        }
        "yaml" => {
            let path = value
                .as_str()
                .ok_or_else(|| "YAML benchmark value must be a path string".to_owned())?;
            load_yaml(Path::new(path), &context)
        }
        _ => return Err(format!("unsupported benchmark operation: {operation}")),
    }
    .map_err(|error| error.to_string())?;
    Ok(config.to_transport_value())
}

fn response(request: Value) -> Value {
    let iterations = match request.get("iterations") {
        None => 1,
        Some(value) => match value.as_u64() {
            Some(count) if (1..=MAX_ITERATIONS).contains(&count) => count,
            _ => {
                return json!({"ok": false,
                    "error": "benchmark iterations must be an integer from 1 to 1000000"});
            }
        },
    };
    let begin = Instant::now();
    let mut result = Err("benchmark did not execute".to_owned());
    for _ in 0..iterations {
        result = std::hint::black_box(resolve(&request));
    }
    let elapsed = u64::try_from(begin.elapsed().as_nanos()).unwrap_or(u64::MAX);
    match result {
        Ok(value) => json!({"ok": true, "result": value,
                            "iterations": iterations, "elapsedNs": elapsed}),
        Err(error) => json!({"ok": false, "error": error,
                             "iterations": iterations, "elapsedNs": elapsed}),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = io::stdin();
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in input.lock().lines() {
        let line = line?;
        let value = match serde_json::from_str::<Value>(&line) {
            Ok(request) => response(request),
            Err(error) => json!({"ok": false,
                "error": format!("invalid benchmark JSON: {error}")}),
        };
        serde_json::to_writer(&mut output, &value)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{required_string, response};
    use serde_json::json;

    #[test]
    fn request_strings_are_required_without_coercion() {
        assert_eq!(required_string(&json!({"cwd": "a"}), "cwd"), Ok("a"));
        for request in [json!(null), json!({}), json!({"cwd": 7})] {
            assert!(required_string(&request, "cwd").is_err());
        }
    }

    #[test]
    fn iteration_bounds_reject_non_integer_or_unbounded_work_before_resolution() {
        for count in [
            json!(0),
            json!(1_000_001),
            json!(true),
            json!(1.0),
            json!("2"),
        ] {
            let value = response(json!({"iterations": count}));
            assert_eq!(value["ok"], false);
            assert!(
                value["error"]
                    .as_str()
                    .is_some_and(|text| text.contains("iterations"))
            );
            assert!(value.get("elapsedNs").is_none());
        }
    }
}
