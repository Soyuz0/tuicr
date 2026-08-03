use std::env;
use std::ffi::OsStr;
use std::io::Write;

use crate::error::TuicrError;
use crate::process::{CommandOutputError, CommandOutputErrorKind, run_command_output};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AzCommandError {
    MissingExecutable { executable: String, detail: String },
    Failed { status: Option<i32>, output: String },
}

pub type AzCommandResult<T> = std::result::Result<T, AzCommandError>;

pub trait AzCommandRunner: Send + Sync {
    fn run(&self, args: &[String]) -> AzCommandResult<String>;
    fn run_with_input_file(&self, args: &[String], input: &[u8]) -> AzCommandResult<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemAzRunner;

impl AzCommandRunner for SystemAzRunner {
    fn run(&self, args: &[String]) -> AzCommandResult<String> {
        let executable = azure_executable()?;
        run_command_output(
            &executable,
            None,
            normalized_args(args)
                .iter()
                .map(|argument| OsStr::new(argument.as_str())),
        )
        .map_err(|error| map_process_error(error, executable))
    }

    fn run_with_input_file(&self, args: &[String], input: &[u8]) -> AzCommandResult<String> {
        let mut file = tempfile::Builder::new()
            .prefix("tuicr-az-")
            .tempfile()
            .map_err(io_error)?;
        file.write_all(input).map_err(io_error)?;
        file.flush().map_err(io_error)?;
        let path = file.path().to_str().ok_or_else(|| AzCommandError::Failed {
            status: None,
            output: "temporary Azure request path is not valid UTF-8".to_string(),
        })?;

        let mut arguments = without_option(args, "--in-file");
        arguments.push("--in-file".to_string());
        arguments.push(path.to_string());
        self.run(&arguments)
    }
}

/// Strip human-readable preamble lines that Azure CLI writes to **stdout**
/// ahead of the JSON document.
///
/// `az devops invoke` prints `Please wait a couple of seconds while we fetch
/// all required information.` on stdout (not stderr) while it enumerates the
/// organization's API surface, and `--only-show-errors` does not suppress it.
/// Handing that straight to `serde_json` fails with `expected value at line 1
/// column 1`, which silently broke every discovery-backed Azure call.
///
/// Only whole leading lines that cannot start a JSON document are dropped, and
/// only up to the first line that can. Output that never reaches a `{`/`[` is
/// returned untouched so error formatting still sees the original text.
pub fn json_payload(output: &str) -> &str {
    let mut offset = 0;
    for line in output.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            return &output[offset + (line.len() - trimmed.len())..];
        }
        offset += line.len();
    }
    output
}

/// Convert transport and common Azure CLI failures into setup-oriented errors.
pub fn map_az_error(error: AzCommandError) -> TuicrError {
    match error {
        AzCommandError::MissingExecutable { .. } => TuicrError::Forge(
            "Azure DevOps integration requires Azure CLI (`az`).\nInstall Azure CLI, run `az extension add --name azure-devops`, then authenticate with `az login` or `az devops login --organization <url>`."
                .to_string(),
        ),
        AzCommandError::Failed { output, .. } if looks_like_missing_extension(&output) => {
            TuicrError::Forge(
                "Azure DevOps CLI extension is not installed.\nRun `az extension add --name azure-devops`."
                    .to_string(),
            )
        }
        AzCommandError::Failed { output, .. } if looks_like_auth_failure(&output) => {
            TuicrError::Forge(
                "Azure DevOps authentication failed.\nRun `az login` or `az devops login --organization <url>`, then configure defaults with `az devops configure --defaults organization=<url> project=<project>`."
                    .to_string(),
            )
        }
        AzCommandError::Failed { output, .. } if looks_like_permission_failure(&output) => {
            TuicrError::Forge(
                "Azure DevOps denied this operation. Verify your project/repository permissions and the identity shown by `az account show`."
                    .to_string(),
            )
        }
        AzCommandError::Failed { output, .. } if looks_like_missing_pull_request(&output) => {
            TuicrError::Forge(
                "Azure DevOps pull request was not found. Verify the PR number and repository."
                    .to_string(),
            )
        }
        AzCommandError::Failed { output, .. } if looks_like_missing_repository(&output) => {
            TuicrError::Forge(
                "Azure DevOps repository was not found. Verify the organization, project, and repository name or ID."
                    .to_string(),
            )
        }
        AzCommandError::Failed { output, .. } if looks_like_missing_project(&output) => {
            TuicrError::Forge(
                "Azure DevOps project was not found. Verify the organization URL and project name."
                    .to_string(),
            )
        }
        AzCommandError::Failed { output, .. } if looks_like_missing_organization(&output) => {
            TuicrError::Forge(
                "Azure DevOps organization was not found. Verify the organization URL passed to Azure CLI."
                    .to_string(),
            )
        }
        AzCommandError::Failed { status, output } => {
            let detail = if output.trim().is_empty() {
                status.map_or_else(
                    || "az command failed".to_string(),
                    |code| format!("az exited with status {code}"),
                )
            } else {
                output
            };
            TuicrError::Forge(format!("Azure DevOps command failed: {detail}"))
        }
    }
}

fn azure_executable() -> AzCommandResult<String> {
    match env::var("TUICR_AZ_BIN") {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Ok("az".to_string()),
        Err(env::VarError::NotUnicode(_)) => Err(AzCommandError::MissingExecutable {
            executable: "TUICR_AZ_BIN".to_string(),
            detail: "TUICR_AZ_BIN is not valid UTF-8".to_string(),
        }),
    }
}

fn normalized_args(args: &[String]) -> Vec<String> {
    let mut normalized = Vec::with_capacity(args.len() + 3);
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--output" || argument == "-o" {
            index += 1;
            if index < args.len() {
                index += 1;
            }
            continue;
        }
        if argument.starts_with("--output=") || argument.starts_with("-o=") {
            index += 1;
            continue;
        }
        if argument == "--only-show-errors" || argument.starts_with("--only-show-errors=") {
            index += 1;
            continue;
        }
        normalized.push(argument.clone());
        index += 1;
    }
    normalized.extend([
        "--output".to_string(),
        "json".to_string(),
        "--only-show-errors".to_string(),
    ]);
    normalized
}

fn without_option(args: &[String], option: &str) -> Vec<String> {
    let mut filtered = Vec::with_capacity(args.len());
    let inline_option = format!("{option}=");
    let mut index = 0;
    while index < args.len() {
        if args[index] == option {
            index += 2;
        } else if args[index].starts_with(inline_option.as_str()) {
            index += 1;
        } else {
            filtered.push(args[index].clone());
            index += 1;
        }
    }
    filtered
}

fn map_process_error(error: CommandOutputError, executable: String) -> AzCommandError {
    match error.kind {
        CommandOutputErrorKind::NotFound => AzCommandError::MissingExecutable {
            executable,
            detail: error.stderr,
        },
        CommandOutputErrorKind::SpawnFailed | CommandOutputErrorKind::Unsuccessful => {
            AzCommandError::Failed {
                status: error.status,
                output: error.stderr,
            }
        }
    }
}

fn io_error(error: std::io::Error) -> AzCommandError {
    AzCommandError::Failed {
        status: None,
        output: error.to_string(),
    }
}

fn looks_like_missing_extension(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("az extension add --name azure-devops")
        || lower.contains("no module named 'azure.devops")
        || (lower.contains("'devops'") && lower.contains("not recognized"))
        || (lower.contains("'repos'") && lower.contains("not recognized"))
}

fn looks_like_auth_failure(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("az login")
        || lower.contains("please run 'az login'")
        || lower.contains("not logged in")
        || lower.contains("tf400813")
        || lower.contains("vs30063")
        || lower.contains("401 unauthorized")
        || lower.contains("authentication failed")
}

fn looks_like_permission_failure(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("tf401027")
        || lower.contains("403 forbidden")
        || lower.contains("access denied")
        || lower.contains("does not have permission")
        || lower.contains("permission to perform this action")
}

fn looks_like_missing_repository(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("tf401019")
        || lower.contains("repositorydoesnotexist")
        || (lower.contains("repository") && lower.contains("does not exist"))
        || (lower.contains("repository") && lower.contains("not found"))
}

fn looks_like_missing_pull_request(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("tf401180")
        || lower.contains("vs402337")
        || (lower.contains("pull request") && lower.contains("does not exist"))
        || (lower.contains("pull request") && lower.contains("not found"))
}

fn looks_like_missing_project(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("projectdoesnotexist")
        || (lower.contains("project") && lower.contains("does not exist"))
        || (lower.contains("project") && lower.contains("not found"))
}

fn looks_like_missing_organization(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("organization")
        && (lower.contains("does not exist") || lower.contains("not found"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_json_and_error_flags_exactly_once() {
        let arguments = vec![
            "repos".to_string(),
            "pr".to_string(),
            "list".to_string(),
            "--output".to_string(),
            "table".to_string(),
            "--only-show-errors".to_string(),
            "--only-show-errors".to_string(),
            "-o=jsonc".to_string(),
        ];
        let normalized = normalized_args(&arguments);
        assert_eq!(
            normalized,
            [
                "repos",
                "pr",
                "list",
                "--output",
                "json",
                "--only-show-errors"
            ]
        );
    }

    /// `az devops invoke` prints its progress banner on **stdout**, ahead of the
    /// JSON, and `--only-show-errors` does not suppress it.
    #[test]
    fn strips_only_leading_non_json_lines_from_stdout() {
        let banner = "Please wait a couple of seconds while we fetch all required information.\n";
        assert_eq!(json_payload(&format!("{banner}[1,2]")), "[1,2]");
        assert_eq!(json_payload(&format!("{banner}  {{\"a\":1}}")), "{\"a\":1}");
        assert_eq!(
            json_payload(&format!("{banner}\n{banner}[]")),
            "[]",
            "multiple preamble lines must all be dropped"
        );

        // Already-clean payloads are returned untouched, including content that
        // merely contains a bracket later on.
        assert_eq!(json_payload("[{\"a\":\"b [c]\"}]"), "[{\"a\":\"b [c]\"}]");
        assert_eq!(json_payload("{\"a\":1}"), "{\"a\":1}");

        // Output that never reaches JSON is preserved so error text survives.
        assert_eq!(json_payload("ERROR: not logged in"), "ERROR: not logged in");
        assert_eq!(json_payload(""), "");
    }

    #[test]
    fn maps_setup_and_resource_failures_to_actionable_messages() {
        let missing = map_az_error(AzCommandError::MissingExecutable {
            executable: "az".to_string(),
            detail: "not found".to_string(),
        })
        .to_string();
        assert!(missing.contains("Install Azure CLI"));
        assert!(missing.contains("az extension add --name azure-devops"));

        let extension = map_az_error(AzCommandError::Failed {
            status: Some(2),
            output: "'repos' is misspelled or not recognized".to_string(),
        })
        .to_string();
        assert!(extension.contains("az extension add --name azure-devops"));

        let repository = map_az_error(AzCommandError::Failed {
            status: Some(1),
            output: "TF401019: repository does not exist".to_string(),
        })
        .to_string();
        assert!(repository.contains("repository was not found"));
    }

    #[cfg(unix)]
    mod unix {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        use super::*;

        struct EnvGuard {
            key: &'static str,
            previous: Option<std::ffi::OsString>,
        }

        impl EnvGuard {
            fn set(key: &'static str, value: &Path) -> Self {
                let previous = env::var_os(key);
                // SAFETY: tests serialize all mutations of these process-wide
                // variables with ENV_LOCK and restore the prior value on drop.
                unsafe { env::set_var(key, value) };
                Self { key, previous }
            }
        }

        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: this guard is dropped while ENV_LOCK is still held.
                unsafe {
                    if let Some(value) = &self.previous {
                        env::set_var(self.key, value);
                    } else {
                        env::remove_var(self.key);
                    }
                }
            }
        }

        fn fake_az(directory: &Path) -> PathBuf {
            let executable = directory.join("fake az \u{2603}");
            fs::write(
                &executable,
                r#"#!/bin/sh
if [ "$1" = "fail" ]; then
  printf 'stdout detail\n'
  printf 'stderr detail\n' >&2
  exit 7
fi
previous=
input=
for argument in "$@"; do
  printf 'ARG=[%s]\n' "$argument"
  if [ "$previous" = "--in-file" ]; then input=$argument; fi
  previous=$argument
done
if [ -n "$input" ]; then
  printf 'INPUT_PATH=[%s]\n' "$input"
  printf 'INPUT=['
  /bin/cat "$input"
  printf ']\n'
fi
"#,
            )
            .unwrap();
            let mut permissions = fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&executable, permissions).unwrap();
            executable
        }

        #[test]
        fn invokes_tuicr_az_bin_without_losing_argument_boundaries() {
            let _lock = crate::process::TEST_ENV_LOCK.lock().unwrap();
            let directory = tempfile::tempdir().unwrap();
            let executable = fake_az(directory.path());
            let _az = EnvGuard::set("TUICR_AZ_BIN", &executable);

            let output = SystemAzRunner
                .run(&["one argument".to_string(), "caf\u{e9}".to_string()])
                .unwrap();
            assert!(output.contains("ARG=[one argument]"));
            assert!(output.contains("ARG=[caf\u{e9}]"));
            assert_eq!(output.matches("ARG=[--output]").count(), 1);
            assert_eq!(output.matches("ARG=[--only-show-errors]").count(), 1);
        }

        #[test]
        fn keeps_input_file_alive_for_command_then_deletes_it() {
            let _lock = crate::process::TEST_ENV_LOCK.lock().unwrap();
            let directory = tempfile::tempdir().unwrap();
            let executable = fake_az(directory.path());
            let input_directory = directory.path().join("input path \u{2603}");
            fs::create_dir(&input_directory).unwrap();
            let _az = EnvGuard::set("TUICR_AZ_BIN", &executable);
            let _temp = EnvGuard::set("TMPDIR", &input_directory);

            let output = SystemAzRunner
                .run_with_input_file(&["invoke".to_string()], b"body with spaces")
                .unwrap();
            assert!(output.contains("INPUT=[body with spaces]"));
            let path = output
                .lines()
                .find_map(|line| line.strip_prefix("INPUT_PATH=[")?.strip_suffix(']'))
                .expect("input path in fake az output");
            assert!(path.contains("input path \u{2603}"));
            assert!(!Path::new(path).exists());
        }

        #[test]
        fn retains_status_and_combines_stderr_with_stdout() {
            let _lock = crate::process::TEST_ENV_LOCK.lock().unwrap();
            let directory = tempfile::tempdir().unwrap();
            let executable = fake_az(directory.path());
            let _az = EnvGuard::set("TUICR_AZ_BIN", &executable);

            let error = SystemAzRunner.run(&["fail".to_string()]).unwrap_err();
            assert_eq!(
                error,
                AzCommandError::Failed {
                    status: Some(7),
                    output: "stderr detail\nstdout detail".to_string(),
                }
            );
        }
    }
}
