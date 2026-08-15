use std::{
  collections::HashMap,
  ffi::OsStr,
  fmt::Debug,
  io::{self, Write},
  path::{Path, PathBuf},
  process,
};

use itertools::Itertools;
use miette::{Diagnostic, Result};
use path_absolutize::Absolutize;
use path_slash::PathExt;
#[cfg(test)]
use speculoos::assert_that;
use tap::Pipe;
#[cfg(feature = "profiling")]
use tracing::instrument;
use wax::{Any, Glob};

use crate::{FILE_EXTENSIONS, FileFormat};

#[derive(thiserror::Error, Diagnostic, Debug)]
#[error("Encountered multiple errors")]
pub struct MultipleErrors(#[related] Vec<Box<dyn miette::Diagnostic + Send + Sync>>);

impl MultipleErrors {
  pub fn from(errors: Vec<impl miette::Diagnostic + Send + Sync + 'static>) -> Self {
    Self(errors.into_iter().map(Box::<dyn miette::Diagnostic + Send + Sync>::from).collect())
  }
}

#[cfg_attr(feature = "profiling", instrument)]
pub fn join_err_result<T, E>(result: Vec<Result<T, E>>) -> Result<Vec<T>, MultipleErrors>
where
  T: Debug,
  E: miette::Diagnostic + Send + Sync + 'static,
{
  if result.iter().any(std::result::Result::is_err) {
    MultipleErrors(result.into_iter().filter_map(Result::err).map(Box::<dyn miette::Diagnostic + Send + Sync>::from).collect_vec()).pipe(Err)
  } else {
    Ok(result.into_iter().map(Result::unwrap).collect())
  }
}

#[cfg_attr(feature = "profiling", instrument)]
pub fn join_err<E>(result: Vec<E>) -> Result<(), MultipleErrors>
where
  E: miette::Diagnostic + Send + Sync + 'static,
{
  if result.is_empty() {
    return ().pipe(Ok);
  }

  MultipleErrors(result.into_iter().map(Into::into).collect_vec()).pipe(Err)
}

pub mod os {
  #[cfg(test)]
  use fake::Dummy;
  use strum::{Display, EnumIs, EnumString};

  #[derive(EnumIs, Display, Debug, EnumString, Hash, PartialEq, Eq, Clone)]
  #[cfg_attr(test, derive(Dummy))]
  #[strum(ascii_case_insensitive)]
  pub enum Os {
    Global,
    Windows,
    Linux,
    Darwin,
  }

  #[cfg(windows)]
  pub const OS: Os = Os::Windows;
  #[cfg(all(not(target_os = "macos"), unix))]
  pub const OS: Os = Os::Linux;
  #[cfg(target_os = "macos")]
  pub const OS: Os = Os::Darwin;
}

#[derive(thiserror::Error, Diagnostic, Debug)]
pub enum RunError {
  #[error("Could not spawn command")]
  #[diagnostic(code(process::command::spawn))]
  Spawn(#[source] io::Error),

  #[error("Command did not complete successfully. (Exitcode {0:?})")]
  #[diagnostic(code(process::command::execute))]
  Execute(Option<i32>),

  #[error("Could not write output")]
  #[diagnostic(code(process::command::output))]
  Write(#[from] io::Error),
}

#[cfg_attr(feature = "profiling", instrument)]
pub fn run_command(cmd: &str, args: &[impl AsRef<OsStr> + Debug], silent: bool, dry_run: bool) -> Result<String, RunError> {
  if dry_run {
    return String::new().pipe(Ok);
  }

  let output = process::Command::new(cmd).args(args).stdin(process::Stdio::null()).output().map_err(RunError::Spawn)?;

  if !silent {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stdout().write_all(&output.stderr)?;
  }

  if !output.status.success() {
    if silent {
      std::io::stdout().write_all(&output.stdout)?;
      std::io::stdout().write_all(&output.stderr)?;
    }
    RunError::Execute(output.status.code()).pipe(Err)?;
  }

  String::from_utf8_lossy(&output.stdout).to_string().pipe(Ok)
}

#[cfg_attr(feature = "profiling", instrument)]
pub fn run_command_env(cmd: &str, args: &[impl AsRef<OsStr> + Debug], env: Option<&HashMap<String, String>>, silent: bool, dry_run: bool) -> Result<String, RunError> {
  if dry_run {
    return String::new().pipe(Ok);
  }

  let mut command = process::Command::new(cmd);
  command.args(args).stdin(process::Stdio::null());

  if let Some(env) = env {
    command.env_clear().envs(env);
  }

  let output = command.output().map_err(RunError::Spawn)?;

  if !silent {
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stdout().write_all(&output.stderr)?;
  }

  if !output.status.success() {
    if silent {
      std::io::stdout().write_all(&output.stdout)?;
      std::io::stdout().write_all(&output.stderr)?;
    }
    RunError::Execute(output.status.code()).pipe(Err)?;
  }

  String::from_utf8_lossy(&output.stdout).to_string().pipe(Ok)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedShell {
  Bash,
  Zsh,
  Powershell,
}

impl SupportedShell {
  pub fn detect(shell_command: &str) -> Option<Self> {
    let first = shell_command.split_whitespace().next()?;
    let name = first.rsplit(['/', '\\']).next().unwrap_or(first).trim_end_matches(".exe").to_ascii_lowercase();

    match name.as_str() {
      "powershell" | "pwsh" => Some(Self::Powershell),
      "bash" => Some(Self::Bash),
      "zsh" => Some(Self::Zsh),
      _ => None,
    }
  }

  /// Wraps the install command so the resulting environment is dumped to `tmp`.
  pub fn wrap_command(self, cmd: &str, tmp: &Path) -> String {
    let tmp = tmp.to_string_lossy();
    match self {
      Self::Bash | Self::Zsh => format!("{cmd}; __rotz_ec=$?; env > \"{tmp}\"; exit $__rotz_ec"),
      Self::Powershell => format!("& {{ {cmd} }}; $__rotz_ec=$LASTEXITCODE; Get-ChildItem Env: | %{{ $_.Name + '=' + $_.Value }} | Out-File -LiteralPath '{tmp}' -Encoding ascii; exit $__rotz_ec"),
    }
  }
}

#[cfg_attr(feature = "profiling", instrument)]
pub fn parse_env_dump(content: &str) -> HashMap<String, String> {
  content
    .lines()
    .filter_map(|line| {
      let line = line.trim_end_matches('\r');
      let (key, value) = line.split_once('=')?;
      if key.is_empty() {
        return None;
      }
      Some((key.to_owned(), value.to_owned()))
    })
    .collect()
}

/// Shell internal variables that should not be propagated between installs.
fn is_shell_noise(key: &str) -> bool {
  matches!(key, "PWD" | "OLDPWD" | "SHLVL" | "_" | "PS1" | "PS2" | "PS3" | "PS4" | "PROMPT_COMMAND") || key.starts_with("BASH_") || key.starts_with("ZSH_")
}

/// Applies the changes observed in `captured` (the env of a just-run install)
/// onto `accumulated`, comparing against `passed` (the env the install was spawned with).
/// Only new or changed variables are propagated; variables removed by the install
/// are removed from the accumulated map.
pub fn merge_env(accumulated: &mut HashMap<String, String>, passed: &HashMap<String, String>, captured: &HashMap<String, String>) {
  for (key, value) in captured {
    if is_shell_noise(key) {
      continue;
    }
    if passed.get(key) != Some(value) {
      accumulated.insert(key.clone(), value.clone());
    }
  }

  for key in passed.keys() {
    if is_shell_noise(key) {
      continue;
    }
    if !captured.contains_key(key) {
      accumulated.remove(key);
    }
  }
}

#[derive(thiserror::Error, Diagnostic, Debug)]
pub enum GlobError {
  #[error("Could not build GlobSet")]
  #[diagnostic(code(glob::set::parse))]
  Build(#[from] wax::BuildError),
}

#[cfg_attr(feature = "profiling", instrument)]
pub fn glob_from_vec(from: &[String], postfix: Option<&str>) -> miette::Result<Any<'static>> {
  from
    .iter()
    .map(|g| postfix.map_or_else(|| g.clone(), |postfix| format!("{g}{postfix}")))
    .map(|g| Glob::new(&g).map(Glob::into_owned).map_err(GlobError::Build))
    .collect_vec()
    .pipe(join_err_result)?
    .pipe(|g| wax::any::<_>(g).unwrap().pipe(Ok))
}

#[allow(clippy::redundant_pub_crate)]
#[cfg_attr(feature = "profiling", instrument)]
pub(crate) fn get_file_with_format(path: impl AsRef<Path> + Debug, base_name: impl AsRef<Path> + Debug) -> Option<(PathBuf, FileFormat)> {
  FILE_EXTENSIONS.iter().map(|e| (path.as_ref().join(base_name.as_ref().with_extension(e.0)), e.1)).find(|e| e.0.exists())
}

#[cfg(test)]
pub trait Select<'s, O: 's, N: 's> {
  fn select<F>(self, selector: F) -> speculoos::Spec<'s, N>
  where
    F: FnOnce(&'s O) -> &'s N;

  fn select_and<S, W>(&self, selector: S, with: W) -> &Self
  where
    S: FnOnce(&'s O) -> &'s N,
    W: FnOnce(speculoos::Spec<'s, N>);
}

#[cfg(test)]
impl<'s, O: 's, N: 's> Select<'s, O, N> for speculoos::Spec<'s, O> {
  fn select<F>(self, selector: F) -> speculoos::Spec<'s, N>
  where
    F: FnOnce(&'s O) -> &'s N,
  {
    assert_that!(*selector(self.subject))
  }

  fn select_and<S, W>(&self, selector: S, with: W) -> &Self
  where
    S: FnOnce(&'s O) -> &'s N,
    W: FnOnce(speculoos::Spec<'s, N>),
  {
    with(assert_that!(*selector(self.subject)));
    self
  }
}

#[cfg_attr(feature = "profiling", instrument)]
pub fn absolutize_virtually(path: &Path) -> Result<String, std::io::Error> {
  path
    .absolutize_virtually("/")?
    .to_slash_lossy()
    .to_string()
    .pipe(|name| name.find('/').map_or(name.as_str(), |root_index| &name[root_index..]).to_owned().pipe(Ok))
}

#[derive(thiserror::Error, Diagnostic, Debug)]
pub enum ParseError {
  #[error(transparent)]
  #[diagnostic(code(parsing::toml::de))]
  #[cfg(feature = "toml")]
  TomlDe(#[from] serde_toml::de::Error),

  #[error(transparent)]
  #[diagnostic(code(parsing::toml::ser))]
  #[cfg(feature = "toml")]
  TomlSer(#[from] serde_toml::ser::Error),

  #[error(transparent)]
  #[diagnostic(code(parsing::yaml))]
  #[cfg(feature = "yaml")]
  Yaml(#[from] serde_yaml::Error),

  #[error(transparent)]
  #[diagnostic(code(parsing::json))]
  #[cfg(feature = "json")]
  Json(#[from] serde_json::Error),

  #[error("Encountered errors while parsing selectors")]
  #[diagnostic(transparent, code(parsing::selector))]
  Selector(
    #[from]
    #[diagnostic_source]
    MultipleErrors,
  ),
}

pub fn resolve_home(path: impl AsRef<Path>) -> PathBuf {
  let path = path.as_ref();

  if path.starts_with("~/") {
    let mut iter = path.iter();
    iter.next();
    crate::USER_DIRS.home_dir().iter().chain(iter).collect()
  } else {
    path.to_owned()
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use miette::Diagnostic;
  use speculoos::prelude::*;

  use crate::helpers::{SupportedShell, join_err_result, merge_env, parse_env_dump};

  #[derive(thiserror::Error, Debug, Diagnostic)]
  #[error("")]
  struct Error;

  #[test]
  fn join_err_result_none() {
    let joined = join_err_result(vec![Ok::<(), Error>(()), Ok::<(), Error>(())]);
    assert_that!(&joined).is_ok().has_length(2);
  }

  #[test]
  fn join_err_result_some() {
    let joined = join_err_result(vec![Ok::<(), Error>(()), Err::<(), Error>(Error), Err::<(), Error>(Error), Ok::<(), Error>(())]);

    assert_that!(&joined).is_err().map(|e| &e.0).has_length(2);
  }

  #[test]
  fn detect_shell_known() {
    assert_that!(SupportedShell::detect("bash -c {{ quote \"\" cmd }}")).is_equal_to(Some(SupportedShell::Bash));
    assert_that!(SupportedShell::detect("zsh -c {{ quote \"\" cmd }}")).is_equal_to(Some(SupportedShell::Zsh));
    assert_that!(SupportedShell::detect("powershell -NoProfile -C {{ quote \"\" cmd }}")).is_equal_to(Some(SupportedShell::Powershell));
    assert_that!(SupportedShell::detect("pwsh -NoProfile -C {{ quote \"\" cmd }}")).is_equal_to(Some(SupportedShell::Powershell));
  }

  #[test]
  fn detect_shell_path_and_unknown() {
    assert_that!(SupportedShell::detect("/bin/bash -c x")).is_equal_to(Some(SupportedShell::Bash));
    assert_that!(SupportedShell::detect("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -C x")).is_equal_to(Some(SupportedShell::Powershell));
    assert_that!(SupportedShell::detect("fish -c x")).is_equal_to(None);
    assert_that!(SupportedShell::detect("cmd /c x")).is_equal_to(None);
    assert_that!(SupportedShell::detect("")).is_equal_to(None);
  }

  #[test]
  fn wrap_command_preserves_exit_code() {
    let wrapped = SupportedShell::Bash.wrap_command("echo hi", std::path::Path::new("/tmp/env"));
    assert_that!(&wrapped).contains("__rotz_ec=$?");
    assert_that!(&wrapped).contains("exit $__rotz_ec");
    assert_that!(&wrapped).contains("/tmp/env");

    let wrapped = SupportedShell::Zsh.wrap_command("echo hi", std::path::Path::new("/tmp/env"));
    assert_that!(&wrapped).contains("__rotz_ec=$?");

    let wrapped = SupportedShell::Powershell.wrap_command("echo hi", std::path::Path::new("C:\\tmp\\env"));
    assert_that!(&wrapped).contains("$__rotz_ec=$LASTEXITCODE");
    assert_that!(&wrapped).contains("C:\\tmp\\env");
  }

  #[test]
  fn parse_env_dump_handles_formats() {
    let parsed = parse_env_dump("FOO=bar\nBAZ=a=b\nEMPTY=\r\nNO_EQ\n\r\n_=/usr/bin/env\r\n");
    assert_that!(parsed.get("FOO")).is_equal_to(Some(&"bar".to_owned()));
    assert_that!(parsed.get("BAZ")).is_equal_to(Some(&"a=b".to_owned()));
    assert_that!(parsed.get("EMPTY")).is_equal_to(Some(&String::new()));
    assert_that!(parsed.contains_key("NO_EQ")).is_false();
  }

  #[test]
  fn merge_env_propagates_changes() {
    let mut accumulated = HashMap::from([("KEEP".to_owned(), "same".to_owned()), ("OLD".to_owned(), "x".to_owned())]);
    let passed = HashMap::from([("KEEP".to_owned(), "same".to_owned()), ("OLD".to_owned(), "x".to_owned())]);
    let captured = HashMap::from([
      ("KEEP".to_owned(), "same".to_owned()),
      ("NEW".to_owned(), "val".to_owned()),
      ("CHANGED".to_owned(), "y".to_owned()),
      ("PWD".to_owned(), "/noise".to_owned()),
      ("SHLVL".to_owned(), "2".to_owned()),
    ]);

    merge_env(&mut accumulated, &passed, &captured);

    assert_that!(accumulated.get("KEEP")).is_equal_to(Some(&"same".to_owned()));
    assert_that!(accumulated.get("NEW")).is_equal_to(Some(&"val".to_owned()));
    assert_that!(accumulated.get("CHANGED")).is_equal_to(Some(&"y".to_owned()));
    assert_that!(accumulated.contains_key("OLD")).is_false();
    assert_that!(accumulated.contains_key("PWD")).is_false();
    assert_that!(accumulated.contains_key("SHLVL")).is_false();
  }
}
