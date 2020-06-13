extern crate curl;
extern crate flate2;
extern crate pkg_config;
extern crate semver;
extern crate serde;
extern crate serde_xml_rs;
extern crate tar;

use std::env::{
    self,
    consts::{DLL_EXTENSION, DLL_PREFIX},
};
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use curl::easy::Easy;
#[cfg(not(target_env = "msvc"))]
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use serde_xml_rs::from_reader;
#[cfg(not(target_env = "msvc"))]
use tar::Archive;
#[cfg(target_env = "msvc")]
use zip::ZipArchive;

const FRAMEWORK_LIBRARY: &'static str = "tensorflow_framework";
const LIBRARY: &'static str = "tensorflow";
const REPOSITORY: &'static str = "https://github.com/tensorflow/tensorflow.git";
const FRAMEWORK_TARGET: &'static str = "tensorflow:libtensorflow_framework";
const TARGET: &'static str = "tensorflow:libtensorflow";
// `VERSION` and `TAG` are separate because the tag is not always `'v' + VERSION`.
// `VERSION` is not currently used for nightly builds
// const VERSION: &'static str = "1.15.0";
const TAG: &'static str = "v2.2.0";
const MIN_BAZEL: &'static str = "0.5.4";

macro_rules! get(($name:expr) => (ok!(env::var($name))));
macro_rules! ok(($expression:expr) => ($expression.unwrap()));
macro_rules! log {
    ($fmt:expr) => (println!(concat!("libtensorflow-sys/build.rs:{}: ", $fmt), line!()));
    ($fmt:expr, $($arg:tt)*) => (println!(concat!("libtensorflow-sys/build.rs:{}: ", $fmt),
    line!(), $($arg)*));
}
macro_rules! log_var(($var:ident) => (log!(concat!(stringify!($var), " = {:?}"), $var)));

fn main() {
    if check_windows_lib() {
        log!("Returning early because {} was already found", LIBRARY);
        return;
    }

    // Note that pkg_config will print cargo:rustc-link-lib and cargo:rustc-link-search as
    // appropriate if the library is found.
    if pkg_config::probe_library(LIBRARY).is_ok() {
        log!("Returning early because {} was already found", LIBRARY);
        return;
    }

    let force_src = match env::var("TF_RUST_BUILD_FROM_SRC") {
        Ok(s) => s == "true",
        Err(_) => false,
    };

    if !force_src
        && env::consts::ARCH == "x86_64"
        && (env::consts::OS == "linux"
            || env::consts::OS == "macos"
            || env::consts::OS == "windows")
    {
        install_prebuilt();
    } else {
        build_from_src();
    }
}

#[cfg(not(target_env = "msvc"))]
fn check_windows_lib() -> bool {
    false
}

#[cfg(target_env = "msvc")]
fn check_windows_lib() -> bool {
    let windows_lib: &str = &format!("{}.lib", LIBRARY);
    if let Ok(path) = env::var("PATH") {
        for p in path.split(";") {
            let path = Path::new(p).join(windows_lib);
            if path.exists() {
                println!("cargo:rustc-link-lib=dylib={}", LIBRARY);
                println!("cargo:rustc-link-search=native={}", p);
                return true;
            }
        }
    }
    false
}

fn remove_suffix(value: &mut String, suffix: &str) {
    if value.ends_with(suffix) {
        let n = value.len();
        value.truncate(n - suffix.len());
    }
}

#[cfg(not(target_env = "msvc"))]
fn extract<P: AsRef<Path>, P2: AsRef<Path>>(archive_path: P, extract_to: P2) {
    let file = File::open(archive_path).unwrap();
    let unzipped = GzDecoder::new(file);
    let mut a = Archive::new(unzipped);
    a.unpack(extract_to).unwrap();
}

// NOTE: It's possible for this function to extract the dll and interface
//       file directly to OUT_DIR instead of extracting then making a
//       copy of the libraries in OUT_DIR.
//       The same approach could be utilized for the other implementation
//       of `extract`.
#[cfg(target_env = "msvc")]
fn extract<P: AsRef<Path>, P2: AsRef<Path>>(archive_path: P, extract_to: P2) {
    fs::create_dir_all(&extract_to).expect("Failed to create output path for zip archive.");
    let file = File::open(archive_path).expect("Unable to open libtensorflow zip archive.");
    let mut archive = ZipArchive::new(file).unwrap();
    for i in 0..archive.len() {
        let mut zipfile = archive.by_index(i).unwrap();
        let output_path = extract_to.as_ref().join(zipfile.sanitized_name());
        if zipfile.name().starts_with("lib") {
            if zipfile.is_dir() {
                fs::create_dir_all(&output_path)
                    .expect("Failed to create output directory when unpacking archive.");
            } else {
                if let Some(parent) = output_path.parent() {
                    if !parent.exists() {
                        fs::create_dir_all(&parent)
                            .expect("Failed to create parent directory for extracted file.");
                    }
                }
                let mut outfile = File::create(&output_path).unwrap();
                io::copy(&mut zipfile, &mut outfile).unwrap();
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListBucketResult {
    contents: Vec<BucketObject>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BucketObject {
    key: String,
    generation: u64,
}

// Get the URL for the latest pre-compiled nightly C lib on this system
fn get_latest_nightly_url(os: &str, proc_type: &str, ext: &str) -> String {
    let filename = format!(
        "libtensorflow-{}-{}-{}{}",
        proc_type,
        os,
        env::consts::ARCH,
        ext
    );
    log_var!(filename);

    // Fetch available builds from storage
    let base_url = "https://storage.googleapis.com/libtensorflow-nightly";
    let mut res = Vec::new();
    let mut easy = Easy::new();
    easy.url(&base_url).unwrap();
    {
        let mut transfer = easy.transfer();
        transfer
            .write_function(|data| {
                res.extend_from_slice(data);
                Ok(data.len())
            })
            .unwrap();
        transfer.perform().unwrap();
    }
    let parsed: ListBucketResult = from_reader(&res[..]).unwrap();

    // Find latest one matching filename
    let mut objs = parsed
        .contents
        .iter()
        .filter(|obj| obj.key.ends_with(&filename))
        .collect::<Vec<_>>();
    objs.sort_by_key(|obj| obj.generation);
    format!(
        "{}/{}",
        base_url,
        objs.last()
            .unwrap_or_else(|| panic!("Unable to find nightly build for system"))
            .key
    )
}

// Downloads and unpacks a prebuilt binary. Only works for certain platforms.
fn install_prebuilt() {
    let os = match env::consts::OS {
        "macos" => "darwin",
        x => x,
    };
    let proc_type = if cfg!(feature = "tensorflow_gpu") {
        "gpu"
    } else {
        "cpu"
    };
    #[cfg(target_env = "msvc")]
    let ext = ".zip";
    #[cfg(not(target_env = "msvc"))]
    let ext = ".tar.gz";

    let binary_url = get_latest_nightly_url(os, proc_type, ext);
    log_var!(binary_url);
    let short_file_name = binary_url.split("/").last().unwrap();
    let mut base_name = short_file_name.to_string();
    remove_suffix(&mut base_name, ext);
    log_var!(base_name);
    let download_dir = match env::var("TF_RUST_DOWNLOAD_DIR") {
        Ok(s) => PathBuf::from(s),
        Err(_) => PathBuf::from(&get!("OUT_DIR")),
    };
    if !download_dir.exists() {
        fs::create_dir(&download_dir).unwrap();
    }
    let file_name = download_dir.join(short_file_name);
    log_var!(file_name);

    // Download the tarball.
    if !file_name.exists() {
        let f = File::create(&file_name).unwrap();
        let mut writer = BufWriter::new(f);
        let mut easy = Easy::new();
        easy.url(&binary_url).unwrap();
        easy.write_function(move |data| Ok(writer.write(data).unwrap()))
            .unwrap();
        easy.perform().unwrap();

        let response_code = easy.response_code().unwrap();
        if response_code != 200 {
            panic!(
                "Unexpected response code {} for {}",
                response_code, binary_url
            );
        }
    }

    // Extract the tarball.
    let unpacked_dir = download_dir.join(base_name);
    let lib_dir = unpacked_dir.join("lib");
    #[cfg(not(target_env = "msvc"))]
    let framework_library_file = format!("lib{}.{}", FRAMEWORK_LIBRARY, DLL_EXTENSION);
    let library_file = format!("{}{}.{}", DLL_PREFIX, LIBRARY, DLL_EXTENSION);

    #[cfg(not(target_env = "msvc"))]
    let framework_library_full_path = lib_dir.join(&framework_library_file);
    let library_full_path = lib_dir.join(&library_file);

    #[cfg(not(target_env = "msvc"))]
    let download_required = !framework_library_full_path.exists() || !library_full_path.exists();
    #[cfg(target_env = "msvc")]
    let download_required = !library_full_path.exists();

    if download_required {
        extract(file_name, &unpacked_dir);
    }

    #[cfg(not(target_env = "msvc"))] // There is no tensorflow_framework.dll
    println!("cargo:rustc-link-lib=dylib={}", FRAMEWORK_LIBRARY);
    println!("cargo:rustc-link-lib=dylib={}", LIBRARY);
    let output = PathBuf::from(&get!("OUT_DIR"));

    // NOTE: The following shouldn't strictly be necessary. See note above `extract`.
    let framework_files = std::fs::read_dir(lib_dir).unwrap();
    for library_entry in framework_files.filter_map(Result::ok) {
        let library_full_path = library_entry.path();
        let new_library_full_path = output.join(&library_full_path.file_name().unwrap());
        if new_library_full_path.exists() {
            log!(
                "{} already exists. Removing",
                new_library_full_path.display()
            );
            fs::remove_file(&new_library_full_path).unwrap();
        }
        log!(
            "Copying {} to {}...",
            library_full_path.display(),
            new_library_full_path.display()
        );
        fs::copy(&library_full_path, &new_library_full_path).unwrap();
    }
    println!("cargo:rustc-link-search={}", output.display());
}

fn build_from_src() {
    let framework_target = FRAMEWORK_TARGET.to_string() + std::env::consts::DLL_SUFFIX;
    let target = TARGET.to_string() + std::env::consts::DLL_SUFFIX;

    let output = PathBuf::from(&get!("OUT_DIR"));
    log_var!(output);
    let source = PathBuf::from(&get!("CARGO_MANIFEST_DIR")).join(format!("target/source-{}", TAG));
    log_var!(source);
    let lib_dir = output.join(format!("lib-{}", TAG));
    log_var!(lib_dir);
    if lib_dir.exists() {
        log!("Directory {:?} already exists", lib_dir);
    } else {
        log!("Creating directory {:?}", lib_dir);
        fs::create_dir(lib_dir.clone()).unwrap();
    }
    let framework_library_path = lib_dir.join(format!("lib{}.so", FRAMEWORK_LIBRARY));
    log_var!(framework_library_path);
    let library_path = lib_dir.join(format!("lib{}.so", LIBRARY));
    log_var!(library_path);
    if library_path.exists() && framework_library_path.exists() {
        log!(
            "{:?} and {:?} already exist, not building",
            library_path,
            framework_library_path
        );
    } else {
        if let Err(e) = check_bazel() {
            println!(
                "cargo:error=Bazel must be installed at version {} or greater. (Error: {})",
                MIN_BAZEL, e
            );
            process::exit(1);
        }
        let framework_target_path = &framework_target.replace(":", "/");
        log_var!(framework_target_path);
        let target_path = &TARGET.replace(":", "/");
        log_var!(target_path);
        if !Path::new(&source.join(".git")).exists() {
            run("git", |command| {
                command
                    .arg("clone")
                    .arg(format!("--branch={}", TAG))
                    .arg("--recursive")
                    .arg(REPOSITORY)
                    .arg(&source)
            });
        }
        // Only configure if not previously configured.  Configuring runs a
        // `bazel clean`, which we don't want, because we want to be able to
        // continue from a cancelled build.
        let configure_hint_file_pb = source.join(".rust-configured");
        let configure_hint_file = Path::new(&configure_hint_file_pb);
        if !configure_hint_file.exists() {
            run("bash", |command| {
                command
                    .current_dir(&source)
                    .env(
                        "TF_NEED_CUDA",
                        if cfg!(feature = "tensorflow_gpu") {
                            "1"
                        } else {
                            "0"
                        },
                    )
                    .arg("-c")
                    .arg("yes ''|./configure")
            });
            File::create(configure_hint_file).unwrap();
        }
        // Allows us to pass in --incompatible_load_argument_is_label=false
        // to work around https://github.com/tensorflow/tensorflow/issues/15492
        let bazel_args_string = if let Ok(args) = env::var("TF_RUST_BAZEL_OPTS") {
            args
        } else {
            "".to_string()
        };
        run("bazel", |command| {
            command
                .current_dir(&source)
                .arg("build")
                .arg(format!("--jobs={}", get!("NUM_JOBS")))
                .arg("--compilation_mode=opt")
                .arg("--copt=-march=native")
                .args(bazel_args_string.split_whitespace())
                .arg(&target)
        });
        let framework_target_bazel_bin = source.join("bazel-bin").join(framework_target_path);
        log!(
            "Copying {:?} to {:?}",
            framework_target_bazel_bin,
            framework_library_path
        );
        fs::copy(framework_target_bazel_bin, framework_library_path).unwrap();
        let target_bazel_bin = source.join("bazel-bin").join(target_path);
        log!("Copying {:?} to {:?}", target_bazel_bin, library_path);
        fs::copy(target_bazel_bin, library_path).unwrap();
    }

    println!("cargo:rustc-link-lib=dylib={}", FRAMEWORK_LIBRARY);
    println!("cargo:rustc-link-lib=dylib={}", LIBRARY);
    println!("cargo:rustc-link-search={}", lib_dir.display());
}

fn run<F>(name: &str, mut configure: F)
where
    F: FnMut(&mut Command) -> &mut Command,
{
    let mut command = Command::new(name);
    let configured = configure(&mut command);
    log!("Executing {:?}", configured);
    if !ok!(configured.status()).success() {
        panic!("failed to execute {:?}", configured);
    }
    log!("Command {:?} finished successfully", configured);
}

// Building TF 0.11.0rc1 with Bazel 0.3.0 gives this error when running `configure`:
//   expected ConfigurationTransition or NoneType for 'cfg' while calling label_list but got
// string instead:     data.
//       ERROR: com.google.devtools.build.lib.packages.BuildFileContainsErrorsException: error
// loading package '': Extension file 'tensorflow/tensorflow.bzl' has errors.
// And the simple solution is to require Bazel 0.3.1 or higher.
fn check_bazel() -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("bazel");
    command.arg("version");
    log!("Executing {:?}", command);
    let out = command.output()?;
    log!("Command {:?} finished successfully", command);
    let stdout = String::from_utf8(out.stdout)?;
    let mut found_version = false;
    for line in stdout.lines() {
        if line.starts_with("Build label:") {
            found_version = true;
            let mut version_str = line
                .split(":")
                .nth(1)
                .unwrap()
                .split(" ")
                .nth(1)
                .unwrap()
                .trim();
            if version_str.ends_with('-') {
                // hyphen is 1 byte long, so it's safe
                version_str = &version_str[..version_str.len() - 1];
            }
            let version = Version::parse(version_str)?;
            let want = Version::parse(MIN_BAZEL)?;
            if version < want {
                return Err(format!(
                    "Installed version {} is less than required version {}",
                    version_str, MIN_BAZEL
                )
                .into());
            }
        }
    }
    if !found_version {
        return Err("Did not find version number in `bazel version` output.".into());
    }
    Ok(())
}
