// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{get_config, Target};
use crate::helpers::{config::get as get_tauri_config, template::JsonMap};
use crate::Result;
use cargo_mobile::{
  android::{self, env::Env as AndroidEnv, ndk, target::Target as AndroidTarget},
  bossy,
  config::Config,
  dot_cargo,
  os::code_command,
  target::TargetTrait as _,
  util::{
    self,
    cli::{Report, TextWrapper},
  },
};
use clap::Parser;
use handlebars::{Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderError};

use std::{env::current_dir, fs, io, path::PathBuf};

#[derive(Debug, Parser)]
#[clap(about = "Initializes a Tauri Android project")]
pub struct Options {
  /// Skip prompting for values
  #[clap(long)]
  ci: bool,
}

pub fn command(mut options: Options, target: Target) -> Result<()> {
  options.ci = options.ci || std::env::var("CI").is_ok();

  let wrapper = TextWrapper::with_splitter(textwrap::termwidth(), textwrap::NoHyphenation);
  exec(target, &wrapper, options.ci, true, true).map_err(|e| anyhow::anyhow!("{:#}", e))?;
  Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
  #[error("invalid tauri configuration: {0}")]
  InvalidTauriConfig(String),
  #[error("failed to create asset dir {asset_dir}: {cause}")]
  AssetDirCreation {
    asset_dir: PathBuf,
    cause: io::Error,
  },
  #[error("failed to install LLDB VS Code extension: {0}")]
  LldbExtensionInstall(bossy::Error),
  #[error(transparent)]
  DotCargoLoad(dot_cargo::LoadError),
  #[error(transparent)]
  DotCargoGenFailed(ndk::MissingToolError),
  #[error(transparent)]
  HostTargetTripleDetection(util::HostTargetTripleError),
  #[cfg(target_os = "macos")]
  #[error(transparent)]
  IosInit(super::ios::project::Error),
  #[error(transparent)]
  AndroidEnv(android::env::Error),
  #[error(transparent)]
  AndroidInit(super::android::project::Error),
  #[error(transparent)]
  DotCargoWrite(dot_cargo::WriteError),
}

pub fn init_dot_cargo(config: &Config, android_env: Option<&AndroidEnv>) -> Result<(), Error> {
  let mut dot_cargo = dot_cargo::DotCargo::load(config.app()).map_err(Error::DotCargoLoad)?;
  // Mysteriously, builds that don't specify `--target` seem to fight over
  // the build cache with builds that use `--target`! This means that
  // alternating between i.e. `cargo run` and `cargo apple run` would
  // result in clean builds being made each time you switched... which is
  // pretty nightmarish. Specifying `build.target` in `.cargo/config`
  // fortunately has the same effect as specifying `--target`, so now we can
  // `cargo run` with peace of mind!
  //
  // This behavior could be explained here:
  // https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags
  dot_cargo
    .set_default_target(util::host_target_triple().map_err(Error::HostTargetTripleDetection)?);

  if let Some(env) = android_env {
    for target in AndroidTarget::all().values() {
      dot_cargo.insert_target(
        target.triple.to_owned(),
        target
          .generate_cargo_config(config.android(), env)
          .map_err(Error::DotCargoGenFailed)?,
      );
    }
  }

  dot_cargo.write(config.app()).map_err(Error::DotCargoWrite)
}

pub fn exec(
  target: Target,
  wrapper: &TextWrapper,
  non_interactive: bool,
  skip_dev_tools: bool,
  #[allow(unused_variables)] reinstall_deps: bool,
) -> Result<Config, Error> {
  let tauri_config =
    get_tauri_config(None).map_err(|e| Error::InvalidTauriConfig(e.to_string()))?;
  let tauri_config_guard = tauri_config.lock().unwrap();
  let tauri_config_ = tauri_config_guard.as_ref().unwrap();

  let (config, metadata) = get_config(tauri_config_);

  let asset_dir = config.app().asset_dir();
  if !asset_dir.is_dir() {
    fs::create_dir_all(&asset_dir).map_err(|cause| Error::AssetDirCreation { asset_dir, cause })?;
  }
  if !skip_dev_tools && util::command_present("code").unwrap_or_default() {
    let mut command = code_command();
    command.add_args(&["--install-extension", "vadimcn.vscode-lldb"]);
    if non_interactive {
      command.add_arg("--force");
    }
    command
      .run_and_wait()
      .map_err(Error::LldbExtensionInstall)?;
  }

  let (handlebars, mut map) = handlebars(&config);

  let mut args = std::env::args_os();
  // TODO: make this a relative path
  let tauri_binary = args
    .next()
    .unwrap_or_else(|| std::ffi::OsString::from("cargo"));
  map.insert("tauri-binary", tauri_binary.to_string_lossy());
  let mut build_args = Vec::new();
  for arg in args {
    if arg == "android" {
      break;
    }
    let path = PathBuf::from(&arg);
    if path.exists() {
      if let Ok(dir) = current_dir() {
        let absolute_path = util::prefix_path(dir, path);
        build_args.push(absolute_path.to_string_lossy().into_owned());
        continue;
      }
    }
    build_args.push(arg.to_string_lossy().into_owned());
  }
  build_args.push("android".into());
  build_args.push("build".into());
  map.insert("tauri-binary-args", build_args);

  // Generate Android Studio project
  let android_env = if target == Target::Android {
    match AndroidEnv::new() {
      Ok(env) => {
        super::android::project::gen(
          config.android(),
          metadata.android(),
          (handlebars, map),
          wrapper,
        )
        .map_err(Error::AndroidInit)?;
        Some(env)
      }
      Err(err) => {
        if err.sdk_or_ndk_issue() {
          Report::action_request(
            " to initialize Android environment; Android support won't be usable until you fix the issue below and re-run `tauri android init`!",
            err,
          )
          .print(wrapper);
          None
        } else {
          return Err(Error::AndroidEnv(err));
        }
      }
    }
  } else {
    // Generate Xcode project
    #[cfg(target_os = "macos")]
    if target == Target::Ios {
      super::ios::project::gen(
        config.apple(),
        metadata.apple(),
        (handlebars, map),
        wrapper,
        non_interactive,
        skip_dev_tools,
        reinstall_deps,
      )
      .map_err(Error::IosInit)?;
    }
    None
  };

  init_dot_cargo(&config, android_env.as_ref())?;

  Report::victory(
    "Project generated successfully!",
    "Make cool apps! 🌻 🐕 🎉",
  )
  .print(wrapper);
  Ok(config)
}

fn handlebars(config: &Config) -> (Handlebars<'static>, JsonMap) {
  let mut h = Handlebars::new();
  h.register_escape_fn(handlebars::no_escape);

  h.register_helper("html-escape", Box::new(html_escape));
  h.register_helper("join", Box::new(join));
  h.register_helper("quote-and-join", Box::new(quote_and_join));
  h.register_helper(
    "quote-and-join-colon-prefix",
    Box::new(quote_and_join_colon_prefix),
  );
  h.register_helper("snake-case", Box::new(snake_case));
  h.register_helper("reverse-domain", Box::new(reverse_domain));
  h.register_helper(
    "reverse-domain-snake-case",
    Box::new(reverse_domain_snake_case),
  );
  // don't mix these up or very bad things will happen to all of us
  h.register_helper("prefix-path", Box::new(prefix_path));
  h.register_helper("unprefix-path", Box::new(unprefix_path));

  let mut map = JsonMap::default();
  map.insert("app", config.app());
  #[cfg(target_os = "macos")]
  map.insert("apple", config.apple());
  map.insert("android", config.android());

  (h, map)
}

fn get_str<'a>(helper: &'a Helper) -> &'a str {
  helper
    .param(0)
    .and_then(|v| v.value().as_str())
    .unwrap_or("")
}

fn get_str_array<'a>(
  helper: &'a Helper,
  formatter: impl Fn(&str) -> String,
) -> Option<Vec<String>> {
  helper.param(0).and_then(|v| {
    v.value().as_array().and_then(|arr| {
      arr
        .iter()
        .map(|val| {
          val.as_str().map(
            #[allow(clippy::redundant_closure)]
            |s| formatter(s),
          )
        })
        .collect()
    })
  })
}

fn html_escape(
  helper: &Helper,
  _: &Handlebars,
  _ctx: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  out
    .write(&handlebars::html_escape(get_str(helper)))
    .map_err(Into::into)
}

fn join(
  helper: &Helper,
  _: &Handlebars,
  _: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  out
    .write(
      &get_str_array(helper, |s| s.to_string())
        .ok_or_else(|| RenderError::new("`join` helper wasn't given an array"))?
        .join(", "),
    )
    .map_err(Into::into)
}

fn quote_and_join(
  helper: &Helper,
  _: &Handlebars,
  _: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  out
    .write(
      &get_str_array(helper, |s| format!("{:?}", s))
        .ok_or_else(|| RenderError::new("`quote-and-join` helper wasn't given an array"))?
        .join(", "),
    )
    .map_err(Into::into)
}

fn quote_and_join_colon_prefix(
  helper: &Helper,
  _: &Handlebars,
  _: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  out
    .write(
      &get_str_array(helper, |s| format!("{:?}", format!(":{}", s)))
        .ok_or_else(|| {
          RenderError::new("`quote-and-join-colon-prefix` helper wasn't given an array")
        })?
        .join(", "),
    )
    .map_err(Into::into)
}

fn snake_case(
  helper: &Helper,
  _: &Handlebars,
  _: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  use heck::ToSnekCase as _;
  out
    .write(&get_str(helper).to_snek_case())
    .map_err(Into::into)
}

fn reverse_domain(
  helper: &Helper,
  _: &Handlebars,
  _: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  out
    .write(&util::reverse_domain(get_str(helper)))
    .map_err(Into::into)
}

fn reverse_domain_snake_case(
  helper: &Helper,
  _: &Handlebars,
  _: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  use heck::ToSnekCase as _;
  out
    .write(&util::reverse_domain(get_str(helper)).to_snek_case())
    .map_err(Into::into)
}

fn app_root(ctx: &Context) -> Result<&str, RenderError> {
  let app_root = ctx
    .data()
    .get("app")
    .ok_or_else(|| RenderError::new("`app` missing from template data."))?
    .get("root-dir")
    .ok_or_else(|| RenderError::new("`app.root-dir` missing from template data."))?;
  app_root
    .as_str()
    .ok_or_else(|| RenderError::new("`app.root-dir` contained invalid UTF-8."))
}

fn prefix_path(
  helper: &Helper,
  _: &Handlebars,
  ctx: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  out
    .write(
      util::prefix_path(app_root(ctx)?, get_str(helper))
        .to_str()
        .ok_or_else(|| {
          RenderError::new(
            "Either the `app.root-dir` or the specified path contained invalid UTF-8.",
          )
        })?,
    )
    .map_err(Into::into)
}

fn unprefix_path(
  helper: &Helper,
  _: &Handlebars,
  ctx: &Context,
  _: &mut RenderContext,
  out: &mut dyn Output,
) -> HelperResult {
  out
    .write(
      util::unprefix_path(app_root(ctx)?, get_str(helper))
        .map_err(|_| {
          RenderError::new("Attempted to unprefix a path that wasn't in the app root dir.")
        })?
        .to_str()
        .ok_or_else(|| {
          RenderError::new(
            "Either the `app.root-dir` or the specified path contained invalid UTF-8.",
          )
        })?,
    )
    .map_err(Into::into)
}