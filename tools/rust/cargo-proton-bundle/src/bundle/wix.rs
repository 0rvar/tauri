use handlebars::Handlebars;
use lazy_static::lazy_static;
use sha2::Digest;
use slog::info;
use slog::Logger;
use std::collections::BTreeMap;
use std::fs::{create_dir_all, File};
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use zip::ZipArchive;

const WIX_URL: &str =
  "https://github.com/wixtoolset/wix3/releases/download/wix3111rtm/wix311-binaries.zip";
const WIX_SHA256: &str = "37f0a533b0978a454efb5dc3bd3598becf9660aaf4287e55bf68ca6b527d051d";

const VC_REDIST_X86_URL: &str =
    "https://download.visualstudio.microsoft.com/download/pr/c8edbb87-c7ec-4500-a461-71e8912d25e9/99ba493d660597490cbb8b3211d2cae4/vc_redist.x86.exe";

const VC_REDIST_X86_SHA256: &str =
  "3a43e8a55a3f3e4b73d01872c16d47a19dd825756784f4580187309e7d1fcb74";

const VC_REDIST_X64_URL: &str =
    "https://download.visualstudio.microsoft.com/download/pr/9e04d214-5a9d-4515-9960-3d71398d98c3/1e1e62ab57bbb4bf5199e8ce88f040be/vc_redist.x64.exe";

const VC_REDIST_X64_SHA256: &str =
  "d6cd2445f68815fe02489fafe0127819e44851e26dfbe702612bc0d223cbbc2b";

lazy_static! {
  static ref HANDLEBARS: Handlebars = {
    let mut handlebars = Handlebars::new();

    handlebars
      .register_template_string("main.wxs", include_str!("templates/main.wxs"))
      .unwrap();
    handlebars
  };
}

fn download_and_verify(logger: &Logger, url: &str, hash: &str) -> Result<Vec<u8>, String> {
  info!(logger, "Downloading {}", url);

  let mut response = reqwest::get(url).or_else(|e| Err(e.to_string()))?;

  let mut data: Vec<u8> = Vec::new();

  response
    .read_to_end(&mut data)
    .or_else(|e| Err(e.to_string()))?;

  info!(logger, "validating hash...");

  let mut hasher = sha2::Sha256::new();
  hasher.input(&data);

  let url_hash = hasher.result().to_vec();
  let expected_hash = hex::decode(hash).or_else(|e| Err(e.to_string()))?;

  if expected_hash == url_hash {
    Ok(data)
  } else {
    Err("hash mismatch of downloaded file".to_string())
  }
}

fn extract_zip(data: &Vec<u8>, path: &Path) -> Result<(), String> {
  let cursor = Cursor::new(data);

  let mut zipa = ZipArchive::new(cursor).or_else(|e| Err(e.to_string()))?;

  for i in 0..zipa.len() {
    let mut file = zipa.by_index(i).or_else(|e| Err(e.to_string()))?;
    let dest_path = path.join(file.name());
    let parent = dest_path.parent().unwrap();

    if !parent.exists() {
      create_dir_all(parent).or_else(|e| Err(e.to_string()))?;
    }

    let mut buff: Vec<u8> = Vec::new();
    file
      .read_to_end(&mut buff)
      .or_else(|e| Err(e.to_string()))?;
    let mut fileout = File::create(dest_path).unwrap();

    fileout.write_all(&buff).or_else(|e| Err(e.to_string()))?;
  }

  Ok(())
}

fn get_and_extract_wix(logger: &Logger, path: &Path) -> Result<(), String> {
  info!(logger, "downloading WIX Toolkit...");

  let data = download_and_verify(logger, WIX_URL, WIX_SHA256)?;

  info!(logger, "extracting WIX");

  extract_zip(&data, path)
}

fn run_heat_exe(
  logger: &Logger,
  wix_toolset_path: &Path,
  build_path: &Path,
  harvest_dir: &Path,
  platform: &str,
) -> Result<(), String> {
  let mut args = vec!["dir"];

  let harvest_str = harvest_dir.display().to_string();

  args.push(&harvest_str);
  args.push("-platform");
  args.push(platform);
  args.push("-cg");
  args.push("AppFiles");
  args.push("-dr");
  args.push("APPLICATIONFOLDER");
  args.push("-gg");
  args.push("-srd");
  args.push("-out");
  args.push("appdir.wxs");
  args.push("-var");
  args.push("var.SourceDir");

  let heat_exe = wix_toolset_path.join("head.exe");

  let mut cmd = Command::new(&heat_exe)
    .args(&args)
    .stdout(Stdio::piped())
    .current_dir(build_path)
    .spawn()
    .expect("error running heat.exe");

  {
    let stdout = cmd.stdout.as_mut().unwrap();
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
      info!(logger, "{}", line.unwrap());
    }
  }

  let status = cmd.wait().unwrap();
  if status.success() {
    Ok(())
  } else {
    Err("error running heat.exe".to_string())
  }
}

fn run_candle(
  logger: &Logger,
  wix_toolset_path: &Path,
  build_path: &Path,
  wxs_file_name: &str,
) -> Result<(), String> {
  let arch = "x64";

  let args = vec![
    "-arch".to_string(),
    arch.to_string(),
    wxs_file_name.to_string(),
  ];

  let candle_exe = wix_toolset_path.join("candle.exe");
  info!(logger, "running candle for {}", wxs_file_name);

  let mut cmd = Command::new(&candle_exe)
    .args(&args)
    .stdout(Stdio::piped())
    .current_dir(build_path)
    .spawn()
    .expect("error running candle.exe");
  {
    let stdout = cmd.stdout.as_mut().unwrap();
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
      info!(logger, "{}", line.unwrap());
    }
  }

  let status = cmd.wait().unwrap();
  if status.success() {
    Ok(())
  } else {
    Err("error running candle.exe".to_string())
  }
}

fn run_light(
  logger: &Logger,
  wix_toolset_path: &Path,
  build_path: &Path,
  wixobjs: &[&str],
  output_path: &Path,
) -> Result<(), String> {
  let light_exe = wix_toolset_path.join("light.exe");

  let mut args: Vec<String> = vec!["-o".to_string(), output_path.display().to_string()];

  for p in wixobjs {
    args.push(p.to_string());
  }

  info!(logger, "running light to produce {}", output_path.display());

  let mut cmd = Command::new(&light_exe)
    .args(&args)
    .stdout(Stdio::piped())
    .current_dir(build_path)
    .spawn()
    .expect("error running light.exe");
  {
    let stdout = cmd.stdout.as_mut().unwrap();
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
      info!(logger, "{}", line.unwrap());
    }
  }

  let status = cmd.wait().unwrap();
  if status.success() {
    Ok(())
  } else {
    Err("error running light.exe".to_string())
  }
}
