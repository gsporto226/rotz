use std::{
  collections::{HashMap, HashSet},
  fmt::Debug,
  path::{Path, PathBuf},
  sync::atomic::{AtomicUsize, Ordering},
};

use crossterm::style::{Attribute, Stylize};
use indexmap::IndexSet;
use miette::{Diagnostic, Report, Result};
use tap::Pipe;
#[cfg(feature = "profiling")]
use tracing::instrument;
use velcro::hash_map;
use wax::{Glob, Pattern};

use super::Command;
use crate::{
  config::Config,
  dot::Installs,
  helpers::{self, SupportedShell},
  templating,
};

#[derive(thiserror::Error, Diagnostic, Debug)]
enum Error {
  #[error("{name} has a cyclic dependency")]
  #[diagnostic(code(dependency::cyclic), help("{} depends on itsself through {}", name, through))]
  CyclicDependency { name: String, through: String },

  #[error("{name} has a cyclic installation dependency")]
  #[diagnostic(code(dependency::cyclic::install), help("{} depends on itsself through {}", name, through))]
  CyclicInstallDependency { name: String, through: String },

  #[error("Dependency {1} of {0} was not found")]
  #[diagnostic(code(dependency::not_found))]
  DependencyNotFound(String, String),

  #[error("Install command for {0} did not run successfully")]
  #[diagnostic(code(install::command::run))]
  InstallExecute(
    String,
    #[source]
    #[diagnostic_source]
    helpers::RunError,
  ),

  #[error("Could not render command templeate for {0}")]
  #[diagnostic(code(install::command::render))]
  RenderingTemplate(String, #[source] Box<handlebars::RenderError>),

  #[error("Could not parse install command for {0}")]
  #[diagnostic(code(install::command::parse))]
  ParsingInstallCommand(String, #[source] shellwords::MismatchedQuotes),

  #[error("Could not spawl install command")]
  #[diagnostic(code(install::command::spawn), help("The shell_command in your config is set to \"{0}\" is that correct?"))]
  CouldNotSpawn(String),

  #[error("Could not parse dependency \"{0}\"")]
  #[diagnostic(code(glob::parse))]
  ParseGlob(String, #[source] Box<wax::BuildError>),

  #[error("{} install(s) failed", .0.len())]
  #[diagnostic(code(install::partial_failure), help("The remaining installs were still attempted. Fix the failing installs and run again."))]
  SomeInstallsFailed(Vec<String>),
}

static ENV_FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn env_file_path() -> PathBuf {
  let counter = ENV_FILE_COUNTER.fetch_add(1, Ordering::SeqCst);
  std::env::temp_dir().join(format!("rotz-env-{}-{counter}", std::process::id()))
}

pub(crate) struct Install<'a> {
  config: Config,
  engine: templating::Engine<'a>,
}

impl Debug for Install<'_> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Link").field("config", &self.config).finish()
  }
}

impl<'b> Install<'b> {
  pub const fn new(config: crate::config::Config, engine: templating::Engine<'b>) -> Self {
    Self { config, engine }
  }

  #[cfg_attr(feature = "profiling", instrument)]
  #[allow(clippy::too_many_arguments)]
  fn install<'a>(
    &self,
    dots: &'a HashMap<String, InstallsDots>,
    entry: (&'a String, &'a InstallsDots),
    installed: &mut HashSet<&'a str>,
    failed: &mut HashSet<&'a str>,
    failures: &mut Vec<String>,
    mut stack: IndexSet<String>,
    env_dots: &HashSet<String>,
    env: &mut HashMap<String, String>,
    (globals, install_command): (&crate::cli::Globals, &crate::cli::Install),
  ) -> Result<(), Error> {
    if installed.contains(entry.0.as_str()) || failed.contains(entry.0.as_str()) {
      return ().pipe(Ok);
    }

    stack.insert(entry.0.clone());

    macro_rules! recurse {
      ($depends:expr, $error:ident) => {
        for dependency in $depends {
          let dependency_glob = Glob::new(dependency).map_err(|e| Error::ParseGlob(dependency.clone(), e.into()))?;

          if stack.iter().any(|d| dependency_glob.is_match(&**d)) {
            return Error::$error {
              name: dependency.clone(),
              through: entry.0.clone(),
            }
            .pipe(Err);
          }

          self.install(
            dots,
            (
              dependency,
              dots
                .iter()
                .find(|d| dependency_glob.is_match(&**d.0))
                .map(|d| d.1)
                .ok_or_else(|| Error::DependencyNotFound(entry.0.clone(), dependency.clone()))?,
            ),
            installed,
            failed,
            failures,
            stack.clone(),
            env_dots,
            env,
            (globals, install_command),
          )?;
        }
      };
    }

    if let Some(installs) = &entry.1.0 {
      if !(install_command.skip_all_dependencies || install_command.skip_installation_dependencies) {
        recurse!(&installs.depends, CyclicInstallDependency);
      }

      if !(install_command.skip_all_dependencies || install_command.skip_installation_dependencies) {
        let dependency_failed = installs
          .depends
          .iter()
          .any(|dependency| Glob::new(dependency).is_ok_and(|glob| dots.iter().find(|d| glob.is_match(&**d.0)).is_some_and(|d| failed.contains(d.0.as_str()))));

        if dependency_failed {
          eprintln!("{}Skipping {}: install dependency failed{}\n", Attribute::Bold, entry.0.as_str().yellow(), Attribute::Reset);
          failed.insert(entry.0.as_str());
          return ().pipe(Ok);
        }
      }

      println!("{}Installing {}{}\n", Attribute::Bold, entry.0.as_str().blue(), Attribute::Reset);

      let inner_cmd = installs.cmd.clone();

      let capture = env_dots.contains(entry.0.as_str());
      let shell = self.config.shell_command.as_deref().and_then(SupportedShell::detect);
      let tmp_path = capture.then(|| shell.map(|_| env_file_path())).flatten();

      let run_result = self.run_env_command(entry.0, &inner_cmd, capture, shell, tmp_path.as_deref(), env, globals.dry_run)?;

      match run_result {
        Ok(_) => {
          installed.insert(entry.0.as_str());
        }
        Err(err) => {
          if let helpers::RunError::Spawn(spawn_err) = &err
            && spawn_err.kind() == std::io::ErrorKind::NotFound
          {
            eprintln!("\n Error: {:?}", Report::new(Error::CouldNotSpawn(format!("{:?}", self.config.shell_command))));
          }

          eprintln!("\n Error: {:?}", Report::new(Error::InstallExecute(entry.0.clone(), err)));
          failures.push(entry.0.clone());
          failed.insert(entry.0.as_str());
        }
      }
    }

    if !(install_command.skip_all_dependencies || install_command.skip_dependencies)
      && let Some(depends) = &entry.1.1
    {
      recurse!(depends, CyclicDependency);
    }

    ().pipe(Ok)
  }

  /// Renders and runs an install command, optionally capturing its resulting
  /// environment so it can be propagated to dependent dots. Returns the raw run
  /// result (the caller handles failure reporting).
  #[cfg_attr(feature = "profiling", instrument)]
  #[allow(clippy::too_many_arguments)]
  fn run_env_command(
    &self,
    name: &str,
    inner_cmd: &str,
    capture: bool,
    shell: Option<SupportedShell>,
    tmp_path: Option<&Path>,
    env: &mut HashMap<String, String>,
    dry_run: bool,
  ) -> Result<Result<String, helpers::RunError>, Error> {
    let render_cmd = match (capture, shell) {
      (true, Some(shell)) => shell.wrap_command(inner_cmd, tmp_path.expect("tmp path is set when capturing")),
      (true, None) => {
        eprintln!(
          "Warning: could not capture environment for \"{}\": shell command \"{}\" is not supported for environment propagation",
          name,
          self.config.shell_command.as_deref().unwrap_or("raw")
        );
        inner_cmd.to_owned()
      }
      (false, _) => inner_cmd.to_owned(),
    };

    let cmd = if let Some(shell_command) = self.config.shell_command.as_ref() {
      self
        .engine
        .render_template(shell_command, &hash_map! { "cmd": &render_cmd })
        .map_err(|err| Error::RenderingTemplate(name.to_owned(), err.pipe(Box::new)))?
    } else {
      render_cmd
    };

    let cmd = shellwords::split(&cmd).map_err(|err| Error::ParsingInstallCommand(name.to_owned(), err))?;

    println!("{}{inner_cmd}{}\n", Attribute::Italic, Attribute::Reset);

    if !capture {
      return Ok(helpers::run_command(&cmd[0], &cmd[1..], false, dry_run));
    }

    let passed = env.clone();
    let run_result = helpers::run_command_env(&cmd[0], &cmd[1..], Some(env), false, dry_run);

    if let Some(tmp) = tmp_path {
      if !dry_run && run_result.is_ok() {
        if let Ok(content) = std::fs::read_to_string(tmp) {
          let captured = helpers::parse_env_dump(&content);
          helpers::merge_env(env, &passed, &captured);
        } else {
          eprintln!("Warning: could not read environment dump for \"{name}\"");
        }
      }
      let _ = std::fs::remove_file(tmp);
    }

    Ok(run_result)
  }
}

type InstallsDots = (Option<Installs>, Option<HashSet<String>>);

/// Returns the names of dots involved in the dependency graph: those that declare
/// dependencies and those referenced as dependencies. Only these participate in
/// environment propagation.
fn dots_with_dependencies(dots: &HashMap<String, InstallsDots>) -> HashSet<String> {
  let mut env_dots: HashSet<String> = HashSet::new();

  for (name, (installs, depends)) in dots {
    let mut referenced: HashSet<String> = HashSet::new();

    if let Some(installs) = installs {
      referenced.extend(installs.depends.iter().cloned());
    }
    if let Some(depends) = depends {
      referenced.extend(depends.iter().cloned());
    }

    if !referenced.is_empty() {
      env_dots.insert(name.clone());
      env_dots.extend(referenced);
    }
  }

  env_dots
}

impl Command for Install<'_> {
  type Args = (crate::cli::Globals, crate::cli::Install);
  type Result = Result<()>;

  #[cfg_attr(feature = "profiling", instrument)]
  fn execute(&self, (globals, install_command): Self::Args) -> Self::Result {
    let dots = crate::dot::read_dots(&self.config.dotfiles, &["/**".to_owned()], &self.config, &self.engine)?
      .into_iter()
      .filter(|d| d.1.installs.is_some() || d.1.depends.is_some())
      .map(|d| (d.0, (d.1.installs, d.1.depends)))
      .collect::<HashMap<String, InstallsDots>>();

    let env_dots = dots_with_dependencies(&dots);

    let mut env: HashMap<String, String> = std::env::vars().collect();
    let mut installed: HashSet<&str> = HashSet::new();
    let mut failed: HashSet<&str> = HashSet::new();
    let mut failures: Vec<String> = Vec::new();
    let globs = helpers::glob_from_vec(&install_command.dots, None)?;
    for dot in &dots {
      if globs.is_match(dot.0.as_str()) {
        self.install(
          &dots,
          dot,
          &mut installed,
          &mut failed,
          &mut failures,
          IndexSet::new(),
          &env_dots,
          &mut env,
          (&globals, &install_command),
        )?;
      }
    }

    if !failures.is_empty() {
      return Err(Report::new(Error::SomeInstallsFailed(failures)));
    }

    ().pipe(Ok)
  }
}

#[cfg(all(test, unix))]
mod tests {
  use speculoos::prelude::*;

  use super::{Command, Install, InstallsDots, dots_with_dependencies};
  use crate::{
    cli::{Cli, Command as CliCommand, Globals, Install as InstallArgs, PathBuf},
    config::{Config, LinkType},
    templating::Engine,
  };

  fn fixture(dir: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!("rotz-test-{dir}-{}", std::process::id()));
    let dotfiles = base.join("dotfiles");
    std::fs::create_dir_all(dotfiles.join("a")).unwrap();
    std::fs::create_dir_all(dotfiles.join("b")).unwrap();
    std::fs::write(dotfiles.join("a/dot.yaml"), "installs: 'export ROTZ_PROPAGATE_TEST=fromA'\n").unwrap();
    std::fs::write(dotfiles.join("b/dot.yaml"), "installs:\n  cmd: 'test \"$ROTZ_PROPAGATE_TEST\" = \"fromA\"'\n  depends:\n    - ../a\n").unwrap();
    base
  }

  fn run_install(dotfiles: &std::path::Path) -> miette::Result<()> {
    let config = Config {
      dotfiles: dotfiles.to_path_buf(),
      link_type: LinkType::Symbolic,
      shell_command: Some("bash -c {{ quote \"\" cmd }}".to_owned()),
      variables: figment::value::Dict::new(),
    };

    let cli = Cli {
      dry_run: false,
      command: CliCommand::Init { repo: None },
      config: PathBuf(std::env::temp_dir().join("rotz-test-config.yaml")),
      dotfiles: Some(PathBuf(dotfiles.to_path_buf())),
    };

    let engine = Engine::new(&config, &cli);
    let install = Install::new(config, engine);

    let globals = Globals { dry_run: false };
    let install_args = InstallArgs {
      dots: vec!["**".to_owned()],
      skip_dependencies: false,
      skip_installation_dependencies: false,
      skip_all_dependencies: false,
    };

    install.execute((globals, install_args))
  }

  #[test]
  fn propagates_env_through_install_dependency() {
    let base = fixture("prop");
    let dotfiles = base.join("dotfiles");

    let result = run_install(&dotfiles);
    assert_that!(&result).is_ok();

    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn skips_install_when_dependency_fails() {
    let base = std::env::temp_dir().join(format!("rotz-test-skip-{}", std::process::id()));
    let dotfiles = base.join("dotfiles");
    let marker_b = base.join("marker_b");
    let marker_c = base.join("marker_c");
    std::fs::create_dir_all(dotfiles.join("a")).unwrap();
    std::fs::create_dir_all(dotfiles.join("b")).unwrap();
    std::fs::create_dir_all(dotfiles.join("c")).unwrap();
    std::fs::write(dotfiles.join("a/dot.yaml"), "installs: 'exit 1'\n").unwrap();
    std::fs::write(dotfiles.join("b/dot.yaml"), format!("installs:\n  cmd: 'touch {}'\n  depends:\n    - ../a\n", marker_b.display())).unwrap();
    std::fs::write(dotfiles.join("c/dot.yaml"), format!("installs: 'touch {}'\n", marker_c.display())).unwrap();

    let result = run_install(&dotfiles);
    assert_that!(&result).is_err();

    assert_that!(marker_c.exists()).is_true();
    assert_that!(marker_b.exists()).is_false();

    let _ = std::fs::remove_dir_all(&base);
  }

  #[test]
  fn env_dots_marks_related_dots() {
    use std::collections::HashSet;

    let mut dots: std::collections::HashMap<String, InstallsDots> = std::collections::HashMap::new();
    dots.insert(
      "/a".to_owned(),
      (
        Some(crate::dot::Installs {
          cmd: "".to_owned(),
          depends: HashSet::new(),
        }),
        None,
      ),
    );
    dots.insert(
      "/b".to_owned(),
      (
        Some(crate::dot::Installs {
          cmd: "".to_owned(),
          depends: ["/a".to_owned()].into(),
        }),
        None,
      ),
    );

    let env_dots = dots_with_dependencies(&dots);

    assert_that!(&env_dots).contains("/a".to_owned());
    assert_that!(&env_dots).contains("/b".to_owned());
  }
}
