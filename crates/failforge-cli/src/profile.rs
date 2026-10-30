//! Native profile generation; this CLI owns cancellation until cleanup finishes.
#[cfg(unix)]
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use crate::signals::{SignalKind, signal};
use failforge_config::Context;
use failforge_profile::ProfileError;
use failforge_profile::analysis::{Cancellation, analyze_profile};
use failforge_profile::output::write_profile;
use failforge_profile::preparation::{apply_applicability, prepare_profile};

use crate::args::ProfileGenerateArgs;

fn emit(stderr: bool, bytes: &[u8]) -> Result<(), ProfileError> {
    #[cfg(unix)]
    {
        if stderr {
            std::io::stderr().write_all(bytes)?;
        } else {
            std::io::stdout().write_all(bytes)?;
        }
    }
    #[cfg(windows)]
    {
        crate::windows_output::Writer::stdio(stderr, std::time::Duration::from_secs(2))?
            .write_all_with_timeout(bytes, std::time::Duration::from_secs(2))?;
    }
    Ok(())
}

fn context(
    arguments: &ProfileGenerateArgs,
    invocation_cwd: &Path,
) -> Result<Context, ProfileError> {
    if !arguments.has_valid_language_selection() {
        return Err(ProfileError::Config(
            "--language dart is required exactly when --family language_specific is selected"
                .into(),
        ));
    }
    if !invocation_cwd.is_absolute() {
        return Err(ProfileError::Analysis(
            "invocation directory must be absolute".into(),
        ));
    }
    let installation = crate::installation::resolve(arguments.installation.as_deref())
        .map_err(|error| ProfileError::Analysis(error.to_string()))?;
    let context =
        Context::new(invocation_cwd, &installation)?.with_sdk_search_path(std::env::var_os("PATH"));
    if !context.installation.is_dir() {
        return Err(ProfileError::Analysis(
            "profile installation is unavailable; use --installation PATH".into(),
        ));
    }
    Ok(context)
}

fn shell_quote(value: &str) -> String {
    #[cfg(windows)]
    {
        format!("'{}'", value.replace('\'', "''"))
    }
    #[cfg(unix)]
    {
        if !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
        {
            value.to_owned()
        } else {
            format!("'{}'", value.replace('\'', "'\"'\"'"))
        }
    }
}

async fn generate(arguments: &ProfileGenerateArgs, context: &Context) -> Result<(), ProfileError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancellation = Cancellation::default();
    let signal_cancellation = cancellation.clone();
    let listener = tokio::spawn(async move {
        let signum = tokio::select! {
            _ = interrupt.recv() => libc::SIGINT,
            _ = terminate.recv() => libc::SIGTERM,
        };
        signal_cancellation.request(signum);
    });
    let result = async {
        let options = arguments.generate_options();
        let mut draft = prepare_profile(&options, context)?;
        cancellation.check()?;
        let evidence = if arguments.template {
            None
        } else {
            emit(
                true,
                b"failforge: analysing applicable mutation operators\n",
            )?;
            let executable = std::env::current_exe()?.canonicalize()?;
            let helper = executable
                .parent()
                .ok_or_else(|| {
                    ProfileError::Analysis("cannot locate the native analysis helper".into())
                })?
                .join(
                    failforge_config::platform::binary_name("failforge-adapter-campaign").as_ref(),
                );
            let result = analyze_profile(&draft, context, &helper, &cancellation).await?;
            apply_applicability(&mut draft, &result, context)?;
            for row in &result.families {
                emit(
                    true,
                    format!(
                        "failforge: {}: {} applicable operators ({} statically valid mutants)\n",
                        row.family,
                        row.active_operators.len(),
                        row.statically_valid_candidates
                    )
                    .as_bytes(),
                )?;
            }
            Some(result)
        };
        if let Some(evidence) = &evidence {
            evidence.verify_unchanged_with_cancellation(&cancellation)?;
        }
        cancellation.check()?;
        if arguments.dry_run {
            emit(false, draft.text.as_bytes())?;
        } else {
            write_profile(
                &draft.project,
                &draft.output,
                &draft.text,
                options.force,
                Some(&draft.binding),
            )?;
            let output = crate::display::user_path(&draft.output);
            let output = output
                .to_str()
                .ok_or_else(|| ProfileError::Config("profile output must be Unicode".into()))?;
            let installation = crate::display::user_path(&context.installation);
            let installation = installation
                .to_str()
                .ok_or_else(|| ProfileError::Config("installation must be Unicode".into()))?;
            emit(
                false,
                format!(
                    "Profile: {output}\nRun: failforge mutate --config {} --installation {}\n",
                    shell_quote(output),
                    shell_quote(installation)
                )
                .as_bytes(),
            )?;
        }
        Ok(())
    }
    .await;
    listener.abort();
    let _ = listener.await;
    result
}

pub(crate) fn execute(arguments: ProfileGenerateArgs, invocation_cwd: &Path) -> ExitCode {
    let result = (|| {
        let context = context(&arguments, invocation_cwd)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        runtime.block_on(generate(&arguments, &context))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = emit(true, format!("failforge: {error}\n").as_bytes());
            ExitCode::from(error.exit_code())
        }
    }
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
