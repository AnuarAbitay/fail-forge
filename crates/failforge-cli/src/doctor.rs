//! Read-only prerequisite inspection. No SDK programs or project code are run.
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use failforge_config::{CampaignConfig, Context, RunConfig};
use failforge_workspace::{Cancellation, inspect_project};

use crate::args::DoctorArgs;

#[path = "doctor_checks.rs"]
mod checks;
use checks::{
    ACCESS_EXECUTE, ACCESS_READ, accessible, adapter_dependencies, conventional_tests, executable,
    flutter_ready, flutter_sdk, readable_regular, report_destination, supported_version,
};

#[derive(Debug, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug)]
struct Check {
    status: Status,
    label: &'static str,
    detail: String,
    remedy: Option<String>,
}

#[derive(Debug, Default)]
struct Report(Vec<Check>);

impl Report {
    fn add(&mut self, status: Status, label: &'static str, detail: impl Into<String>) {
        self.0.push(Check {
            status,
            label,
            detail: detail.into(),
            remedy: None,
        });
    }

    fn prerequisite(&mut self, ready: bool, label: &'static str, detail: &str, remedy: String) {
        self.add(
            if ready { Status::Ok } else { Status::Fail },
            label,
            if ready {
                format!("verified: {detail}")
            } else {
                format!("missing, invalid or inaccessible: {detail}")
            },
        );
        if !ready {
            self.0.last_mut().expect("a check was just inserted").remedy = Some(remedy);
        }
    }

    fn status(&self) -> u8 {
        if self
            .0
            .iter()
            .any(|check| check.status == Status::Fail && check.label == "Configuration")
        {
            2
        } else {
            u8::from(self.0.iter().any(|check| check.status == Status::Fail))
        }
    }

    fn write(&self, output: &mut impl Write) -> io::Result<()> {
        writeln!(output, "FailForge doctor")?;
        for check in &self.0 {
            let status = match check.status {
                Status::Ok => "OK",
                Status::Warn => "WARN",
                Status::Fail => "FAIL",
                Status::Skip => "SKIP",
            };
            writeln!(
                output,
                "[{status}] {}: {}",
                check.label,
                display_text(&check.detail)
            )?;
            if let Some(remedy) = &check.remedy {
                writeln!(output, "  Fix: {}", display_text(remedy))?;
            }
        }
        writeln!(
            output,
            "Static checks only: SDK programs and tests were not run. No files were changed."
        )
    }
}

/// Quoting is suitable for bash/zsh, including paths containing terminal controls.
/// Ordinary paths use portable single quotes; ANSI-C quoting keeps controls out
/// of the diagnostic stream while preserving the argument when pasted.
fn quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    let control = |character: char| {
        character.is_control()
            || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    };
    if value.chars().any(control) {
        let mut text = String::from("$'");
        for character in value.chars() {
            match character {
                '\'' => text.push_str("\\'"),
                '\\' => text.push_str("\\\\"),
                character if control(character) => {
                    for byte in character.encode_utf8(&mut [0u8; 4]).bytes() {
                        text.push_str(&format!("\\{byte:03o}"));
                    }
                }
                character => text.push(character),
            }
        }
        text.push('\'');
        text
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn display_text(value: &str) -> String {
    let mut text = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control()
            || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            text.push_str(&format!("\\u{{{:x}}}", character as u32));
        } else {
            text.push(character);
        }
    }
    text
}

fn inspect(
    config: &CampaignConfig,
    installation: &Path,
    helper: &Path,
    selections: &[&CampaignConfig],
) -> Report {
    let mut report = Report::default();
    report.add(Status::Ok, "Configuration", "campaign settings are valid");
    report.prerequisite(
        installation.is_dir() && accessible(installation, ACCESS_READ | ACCESS_EXECUTE),
        "Installation",
        &format!("installation directory {}", quote(installation)),
        "pass --installation PATH pointing to the FailForge installation".into(),
    );
    report.prerequisite(
        executable(helper),
        "Native helper",
        &format!("profile analysis helper {} beside failforge", quote(helper)),
        "restore executable failforge-adapter-campaign beside the failforge executable".into(),
    );
    let dart = config.dart_sdk.join(failforge_config::platform::DART_BIN);
    report.prerequisite(
        executable(&dart) && readable_regular(&config.dart_sdk.join("lib/core/core.dart")),
        "Dart SDK",
        &format!("SDK executable {} and readable core library", quote(&dart)),
        "install a prepared Dart SDK and add its bin directory to PATH, or pass --dart-sdk PATH"
            .into(),
    );
    let version = failforge_workspace::read_regular(&config.dart_sdk.join("version"), 4096)
        .ok()
        .and_then(|raw| supported_version(&raw));
    report.prerequisite(
        version.is_some(),
        "Dart SDK version",
        &version.as_ref().map_or_else(
            || {
                format!(
                    "stable version >=3.13.0 <4.0.0 in {}",
                    quote(&config.dart_sdk.join("version"))
                )
            },
            |version| format!("Dart {version} matches >=3.13.0 <4.0.0"),
        ),
        "select a stable Dart SDK with version >=3.13.0 <4.0.0".into(),
    );
    if let Some(version) = version.filter(|version| !version.starts_with("3.13.")) {
        report.add(
            Status::Warn,
            "Dart SDK compatibility",
            format!(
                "Dart {version} is allowed but newer than the recorded Dart 3.13.x toolchain (3.13.5); verify adapter/analyzer compatibility with analysis, tests and a mutation baseline"
            ),
        );
    }
    report.prerequisite(
        readable_regular(&config.adapter_root.join("bin/failforge_dart_adapter.dart")),
        "Adapter entry point",
        &format!(
            "readable adapter entry point at {}",
            quote(&config.adapter_root)
        ),
        "restore the adapters/dart directory or pass --adapter-root PATH".into(),
    );
    report.prerequisite(
        adapter_dependencies(&config.adapter_root).is_ok(),
        "Adapter dependencies",
        &format!(
            "prepared metadata and all declared dependencies at {}",
            quote(&config.adapter_root)
        ),
        format!(
            "{} --suppress-analytics pub get --directory {}",
            quote(&dart),
            quote(&config.adapter_root)
        ),
    );
    let project = inspect_project(&config.project, &Cancellation::default());
    let original = project.as_ref().ok();
    if original.is_some_and(|project| !project.flutter)
        && selections.iter().any(|selection| {
            selection.rules.operators.as_ref().is_some_and(|operators| {
                operators
                    .iter()
                    .any(|operator| operator == "METHOD_CHANNEL_ERROR")
            })
        })
    {
        report.0[0] = Check {
            status: Status::Fail,
            label: "Configuration",
            detail: "METHOD_CHANNEL_ERROR requires a prepared Flutter project".into(),
            remedy: Some("disable METHOD_CHANNEL_ERROR for Dart packages".into()),
        };
    }
    let project_pub =
        if config.flutter_sdk.is_some() || original.is_some_and(|project| project.flutter) {
            flutter_sdk(config)
                .map(|sdk| sdk.join(failforge_config::platform::FLUTTER_BIN))
                .unwrap_or_else(|| dart.clone())
        } else {
            dart.clone()
        };
    report.prerequisite(original.is_some(), "Project", &format!("standalone package metadata, dependencies and source files at {}", quote(&config.project)), format!("{} --suppress-analytics pub get --directory {}; for Flutter packages use flutter pub get; ensure the package is standalone with test/flutter_test and no source aliases", quote(&project_pub), quote(&config.project)));
    if let Some(project) = original {
        if project.flutter {
            report.add(
                Status::Skip,
                "Test runner",
                "Flutter provides the test runner; see Flutter SDK",
            );
            let flutter = flutter_sdk(config);
            report.prerequisite(flutter.as_ref().is_some_and(|sdk| flutter_ready(sdk, &config.dart_sdk)), "Flutter SDK", &format!("matching bundled Dart SDK, Flutter sources and prepared host test artifacts at {}", flutter.as_ref().map_or_else(|| "the selected SDK".into(), |sdk| quote(sdk))), flutter.map_or_else(|| "pass --flutter-sdk PATH to a prepared Flutter SDK".into(), |sdk| format!("{} precache; then {} --version", quote(&sdk.join(failforge_config::platform::FLUTTER_BIN)), quote(&sdk.join(failforge_config::platform::FLUTTER_BIN)))));
        } else {
            report.prerequisite(
                project
                    .packages
                    .get("test")
                    .is_some_and(|test| readable_regular(&test.join("bin/test.dart"))),
                "Test runner",
                &format!(
                    "prepared package:test entry point for {}",
                    quote(&config.project)
                ),
                format!(
                    "{} pub get --directory {}",
                    quote(&dart),
                    quote(&config.project)
                ),
            );
            report.add(
                Status::Skip,
                "Flutter SDK",
                "Dart package uses the Dart test runner",
            );
        }
        match conventional_tests(&project.root) {
            Ok(true) => report.add(Status::Ok, "Tests", "conventional test/**/*_test.dart files are readable; test discovery was not executed"),
            Ok(false) if project.root.join("dart_test.yaml").exists() => report.add(Status::Skip, "Tests", "dart_test.yaml may customize discovery; configured tests were not evaluated"),
            _ => report.prerequisite(false, "Tests", "no readable conventional test files, or test directory contains unsupported inputs", "add test/**/*_test.dart files or check the package's custom test discovery configuration".into()),
        }
    } else {
        for label in ["Test runner", "Flutter SDK", "Tests"] {
            report.add(
                Status::Skip,
                label,
                "requires valid prepared project metadata",
            );
        }
    }
    report.prerequisite(report_destination(config, original, helper), "Report destination", &format!("nonoverlapping report destination {} with an accessible writable parent", quote(&config.output.clone().unwrap_or_else(|| config.project.join(".failforge/runs")))), "select a new output directory outside protected inputs; inside the project use .failforge/".into());
    report
}

fn selected(config: &RunConfig) -> Option<&CampaignConfig> {
    match config {
        RunConfig::Campaign(config) => Some(config),
        RunConfig::Profile(profile) => profile.campaigns.first(),
    }
}

fn native_helper(executable: &Path) -> Option<std::path::PathBuf> {
    executable.parent().map(|directory| {
        directory
            .join(failforge_config::platform::binary_name("failforge-adapter-campaign").as_ref())
    })
}

pub(crate) fn execute(arguments: DoctorArgs, invocation_cwd: &Path) -> ExitCode {
    let result = (|| -> Result<(Report, u8), (u8, String)> {
        let installation = crate::installation::resolve(arguments.installation.as_deref())
            .map_err(|error| (125, error.to_string()))?;
        let context = Context::new(invocation_cwd, &installation)
            .map_err(|error| (2, error.to_string()))?
            .with_sdk_search_path(std::env::var_os("PATH"));
        let values = arguments
            .configuration_values()
            .map_err(|error| (2, error.into()))?;
        let configuration = failforge_config::resolve_config(&values, &context)
            .map_err(|error| (2, error.to_string()))?;
        crate::mutate::validate_profile_selection(&configuration)
            .map_err(|error| (2, error.to_string()))?;
        let config = selected(&configuration)
            .ok_or((2, "profile has no selected mutation families".into()))?;
        let executable = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|_| (125, "cannot locate the running executable".into()))?;
        let helper = native_helper(&executable)
            .ok_or((125, "cannot locate the native analysis helper".into()))?;
        let selections = match &configuration {
            RunConfig::Campaign(config) => vec![config.as_ref()],
            RunConfig::Profile(profile) => profile.campaigns.iter().collect(),
        };
        let report = inspect(config, &context.installation, &helper, &selections);
        let status = report.status();
        Ok((report, status))
    })();
    let (report, status) = match result {
        Ok(result) => result,
        Err((code, error)) => {
            let mut report = Report::default();
            report.prerequisite(
                false,
                "Configuration",
                &error,
                "check failforge doctor --help, configuration paths and selected operators".into(),
            );
            (report, code)
        }
    };
    if report.write(&mut io::stdout().lock()).is_err() {
        return ExitCode::from(125);
    }
    ExitCode::from(status)
}

#[cfg(all(test, unix))]
#[path = "doctor_tests.rs"]
mod tests;

#[cfg(test)]
mod native_tests {
    use super::*;
    use std::fs;

    #[test]
    fn helper_readiness_uses_native_sibling_filename_without_running_it() {
        let root = std::env::temp_dir().join(format!(
            "failforge-doctor-helper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let name = if cfg!(windows) {
            "failforge-adapter-campaign.exe"
        } else {
            "failforge-adapter-campaign"
        };
        let expected = root.join(name);
        fs::write(&expected, b"this helper must never be executed").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&expected, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let helper = native_helper(&root.join("running-cli")).unwrap();
        assert_eq!(helper, expected);
        assert!(executable(&helper));
        assert_eq!(
            fs::read(&helper).unwrap(),
            b"this helper must never be executed"
        );
        fs::remove_file(&expected).unwrap();
        assert!(!executable(&helper));
        fs::remove_dir(root).unwrap();
    }
}
