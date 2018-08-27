#![deny(warnings)]

#[macro_use]
extern crate failure;
extern crate regex;
extern crate rustc_demangle;
extern crate rustc_version;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate clap;
extern crate toml;
extern crate walkdir;

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, str};

use clap::{App, AppSettings, Arg};
pub use failure::Error;
use walkdir::WalkDir;

use cargo::Config;

mod cargo;
mod llvm;
mod postprocess;
mod rustc;
mod util;

pub type Result<T> = std::result::Result<T, failure::Error>;

#[derive(Clone, Copy, PartialEq)]
pub enum Tool {
    Nm,
    Objcopy,
    Objdump,
    Profdata,
    Size,
    Strip,
}

impl Tool {
    fn name(&self) -> &'static str {
        match *self {
            Tool::Nm => "nm",
            Tool::Objcopy => "objcopy",
            Tool::Objdump => "objdump",
            Tool::Profdata => "profdata",
            Tool::Size => "size",
            Tool::Strip => "strip",
        }
    }

    // Whether this tool requires the project to be previously built
    fn needs_build(&self) -> bool {
        match *self {
            Tool::Nm | Tool::Objcopy | Tool::Objdump | Tool::Size | Tool::Strip => true,
            Tool::Profdata /* ? */ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Endian {
    Little,
    Big,
}

/// Execution context
// TODO this should be some sort of initialize once, read-only singleton
pub struct Context {
    /// Directory within the Rust sysroot where the llvm tools reside
    bindir: PathBuf,
    // `[build] target = ".."` info in `.cargo/config`
    build_target: Option<String>,
    cfg: rustc::Cfg,
    // the compilation target
    target: String,
}

impl Context {
    /* Constructors */
    fn new(target_flag: Option<&str>) -> Result<Self> {
        let cwd = env::current_dir()?;

        let config = Config::get(&cwd)?;
        let build_target = config
            .as_ref()
            .and_then(|config| config.build.as_ref().and_then(|build| build.target.clone()));

        let meta = rustc_version::version_meta()?;
        let host = meta.host;

        let sysroot = String::from_utf8(
            Command::new("rustc")
                .arg("--print")
                .arg("sysroot")
                .output()?
                .stdout,
        )?;

        let target = target_flag
            .map(|s| s.to_owned())
            .or_else(|| build_target.clone())
            .unwrap_or(host);
        let cfg = rustc::Cfg::parse(&target)?;

        for entry in WalkDir::new(sysroot.trim()).into_iter() {
            let entry = entry?;

            if entry.file_name() == &*exe("llvm-size") {
                let bindir = entry.path().parent().unwrap().to_owned();

                return Ok(Context {
                    bindir,
                    build_target,
                    cfg,
                    target,
                });
            }
        }

        bail!(
            "`llvm-tools-preview` component is missing or empty. Install it with `rustup component \
             add llvm-tools-preview`"
        );
    }

    /* Private API */
    fn bindir(&self) -> &Path {
        &self.bindir
    }

    fn build_target(&self) -> Option<&str> {
        self.build_target.as_ref().map(|s| &**s)
    }

    fn rustc_cfg(&self) -> &rustc::Cfg {
        &self.cfg
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn tool(&self, tool: Tool, target: &str) -> Command {
        let mut c = Command::new(self.bindir().join(&*exe(&format!("llvm-{}", tool.name()))));

        if tool == Tool::Objdump {
            c.args(&["-arch-name", llvm::arch_name(self.rustc_cfg(), target)]);
        }

        c
    }
}

#[cfg(target_os = "windows")]
fn exe(name: &str) -> Cow<str> {
    format!("{}.exe", name).into()
}

#[cfg(not(target_os = "windows"))]
fn exe(name: &str) -> Cow<str> {
    name.into()
}

pub fn run(tool: Tool) -> Result<i32> {
    let name = tool.name();
    let needs_build = tool.needs_build();

    let app = App::new(format!("cargo-{}", name));
    let about = format!(
        "Proxy for the `llvm-{}` tool shipped with the Rust toolchain.",
        name
    );
    let app = app
        .about(&*about)
        .version(env!("CARGO_PKG_VERSION"))
        .setting(AppSettings::TrailingVarArg)
        .setting(AppSettings::DontCollapseArgsInUsage)
        // as this is used as a Cargo subcommand the first argument will be the name of the binary
        // we ignore this argument
        .arg(Arg::with_name("binary-name").hidden(true))
        .arg(
            Arg::with_name("target")
                .long("target")
                .takes_value(true)
                .value_name("TRIPLE")
                .help("Target triple for which the code is compiled"),
        ).arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .help("Use verbose output"),
        ).arg(Arg::with_name("--").short("-").hidden_short_help(true))
        .arg(Arg::with_name("args").multiple(true))
        .after_help("The specified <args>... will all be passed to the final tool invocation.");

    let matches = if needs_build {
        app.arg(
            Arg::with_name("bin")
                .long("bin")
                .takes_value(true)
                .value_name("NAME")
                .help("Build only the specified binary"),
        ).arg(
            Arg::with_name("example")
                .long("example")
                .takes_value(true)
                .value_name("NAME")
                .help("Build only the specified example"),
        ).arg(
            Arg::with_name("lib")
                .long("lib")
                .help("Build only this package's library"),
        ).arg(
            Arg::with_name("release")
                .long("release")
                .help("Build artifacts in release mode, with optimizations"),
        )
    } else {
        app
    }.get_matches();

    let verbose = matches.is_present("verbose");
    let target_flag = matches.value_of("target");

    let (artifact, release) = if needs_build {
        fn at_least_two_are_true(a: bool, b: bool, c: bool) -> bool {
            if a {
                b || c
            } else {
                b && c
            }
        }

        let bin = matches.is_present("bin");
        let example = matches.is_present("example");
        let lib = matches.is_present("lib");
        let release = matches.is_present("release");

        if at_least_two_are_true(bin, example, lib) {
            return Err(failure::err_msg(
                "Only one of `--bin`, `--example` or `--lib` must be specified",
            ));
        }

        if bin {
            (
                Some(Artifact::Bin(matches.value_of("bin").unwrap())),
                release,
            )
        } else if example {
            (
                Some(Artifact::Example(matches.value_of("example").unwrap())),
                release,
            )
        } else if lib {
            (Some(Artifact::Lib), release)
        } else {
            (None, release)
        }
    } else {
        (None, false)
    };

    let mut cargo = Command::new("cargo");
    cargo.arg("build");

    // NOTE we do *not* use the `build_target` info here because Cargo will figure things out on
    // its own (i.e. it will search and parse .cargo/config, etc.)
    if let Some(target) = target_flag {
        cargo.args(&["--target", target]);
    }

    match artifact {
        Some(Artifact::Bin(bin)) => {
            cargo.args(&["--bin", bin]);
        }
        Some(Artifact::Example(example)) => {
            cargo.args(&["--example", example]);
        }
        Some(Artifact::Lib) => {
            cargo.arg("--lib");
        }
        None => {}
    }

    if release {
        cargo.arg("--release");
    }

    if artifact.is_some() {
        if verbose {
            eprintln!("{:?}", cargo);
        }

        let status = cargo.status()?;

        if !status.success() {
            return Ok(status.code().unwrap_or(1));
        }
    }

    let mut tool_args = vec![];
    if let Some(arg) = matches.value_of("--") {
        tool_args.push(arg);
    }

    if let Some(args) = matches.values_of("args") {
        tool_args.extend(args);
    }

    let ctxt = Context::new(target_flag)?;

    let mut lltool = ctxt.tool(tool, ctxt.target());

    if let Some(kind) = artifact {
        let artifact = cargo::artifact(kind, release, target_flag, ctxt.build_target())?;

        match tool {
            // for some tools we change the CWD (current working directory) and
            // make the artifact path relative. This makes the path that the
            // tool will print easier to read. e.g. `libfoo.rlib` instead of
            // `/home/user/rust/project/target/$T/debug/libfoo.rlib`.
            Tool::Objdump | Tool::Nm | Tool::Size => {
                lltool
                    .current_dir(artifact.parent().unwrap())
                    .arg(artifact.file_name().unwrap());
            }
            Tool::Objcopy | Tool::Profdata | Tool::Strip => {
                lltool.arg(artifact);
            }
        }
    }

    lltool.args(&tool_args);

    if verbose {
        eprintln!("{:?}", lltool);
    }

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    let output = lltool.stderr(Stdio::inherit()).output()?;

    // post process output
    let pp_output = match tool {
        Tool::Objdump | Tool::Nm => postprocess::demangle(&output.stdout),
        Tool::Size => postprocess::size(&output.stdout),
        Tool::Objcopy | Tool::Profdata | Tool::Strip => output.stdout.into(),
    };

    stdout.write_all(&*pp_output)?;

    if output.status.success() {
        Ok(0)
    } else {
        Ok(output.status.code().unwrap_or(1))
    }
}

// The artifact we are going to build and inspect
#[derive(PartialEq)]
pub enum Artifact<'a> {
    Bin(&'a str),
    Example(&'a str),
    Lib,
}
