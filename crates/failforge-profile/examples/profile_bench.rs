//! JSON Lines driver for preparation parity and per-process resource measurement.
use std::hint::black_box;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

use failforge_config::Context;
use failforge_profile::{GenerateOptions, preparation::prepare_profile};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source: PathBuf,
    output: PathBuf,
    cwd: PathBuf,
    installation: PathBuf,
    families: Vec<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    operators: Vec<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    dart_sdk: Option<PathBuf>,
    #[serde(default)]
    flutter_sdk: Option<PathBuf>,
    #[serde(default)]
    adapter_root: Option<PathBuf>,
    #[serde(default = "one")]
    iterations: u32,
}
fn one() -> u32 {
    1
}

fn execute(request: Request) -> Value {
    if !(1..=100_000).contains(&request.iterations) {
        return json!({"ok":false,"error":"iterations must be in 1..=100000"});
    }
    let options = GenerateOptions {
        source: request.source,
        output: request.output,
        families: request.families,
        language: request.language,
        operators: request.operators,
        force: request.force,
        dart_sdk: request.dart_sdk,
        flutter_sdk: request.flutter_sdk,
        adapter_root: request.adapter_root,
    };
    let started = Instant::now();
    let mut prepared = None;
    for _ in 0..request.iterations {
        prepared = Some(
            Context::new(&request.cwd, &request.installation)
                .map_err(failforge_profile::ProfileError::from)
                .and_then(|context| prepare_profile(&options, &context)),
        );
        black_box(&prepared);
    }
    let elapsed = started.elapsed();
    let mut result = match prepared.expect("iteration count is positive") {
        Ok(draft) => json!({"ok":true,"configuration":draft.values,
            "project":draft.project,"source":draft.source,"output":draft.output,
            "framework_flutter":draft.framework_flutter}),
        Err(error) => {
            json!({"ok":false,"error":error.to_string(),"exitCode":error.exit_code()})
        }
    };
    result["elapsedNanoseconds"] = json!(elapsed.as_nanos());
    result["iterations"] = json!(request.iterations);
    result
}

fn main() -> io::Result<()> {
    let mut output = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let response = match serde_json::from_str::<Request>(&line?) {
            Ok(request) => execute(request),
            Err(_) => json!({"ok":false,"error":"invalid benchmark request"}),
        };
        serde_json::to_writer(&mut output, &response)?;
        writeln!(output)?;
        output.flush()?;
    }
    Ok(())
}
