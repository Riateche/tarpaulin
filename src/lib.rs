extern crate cargo;
extern crate chrono;
#[macro_use]
extern crate clap;
extern crate coveralls_api;
extern crate curl;
extern crate fallible_iterator;
extern crate gimli;
#[macro_use]
extern crate lazy_static;
extern crate memmap;
extern crate nix;
extern crate object;
extern crate proc_macro2;
extern crate quick_xml;
extern crate regex;
extern crate rustc_demangle;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate syn;
extern crate syntex_syntax;
#[macro_use]
extern crate tera;
extern crate void;
extern crate walkdir;


use std::env;
use std::ffi::CString;
use std::io;
use std::path::Path;

use cargo::core::{Workspace, Package};
use cargo::ops;
use cargo::util::Config as CargoConfig;
use nix::unistd::*;


pub mod breakpoint;
pub mod config;
pub mod reporting;
pub mod test_loader;
pub mod traces;

/// Should be unnecessary with a future nix crate release.
mod personality;
mod ptrace_control;
mod source_analysis;
mod statemachine;


use config::*;
use ptrace_control::*;
use statemachine::*;
use test_loader::*;
use traces::*;


pub fn run(config: &Config) -> Result<(), i32> {
    let (result, tp) = launch_tarpaulin(config)?;
    report_coverage(config, &result);

    if tp {
        Ok(())
    } else {
        println!("Tarpaulin ran successfully. Failure in tests.");
        Err(-1)
    }
}

/// Launches tarpaulin with the given configuration.
pub fn launch_tarpaulin(config: &Config) -> Result<(TraceMap, bool), i32> {
    let mut cargo_config = CargoConfig::default().unwrap();
    let flag_quiet = if config.verbose {
        None
    } else {
        Some(true)
    };
    // This shouldn't fail so no checking the error.
    let _ = cargo_config.configure(0u32, flag_quiet, &None, false, false, &[]);

    let workspace = Workspace::new(config.manifest.as_path(), &cargo_config).map_err(|_| 1i32)?;

    setup_environment();

    let mut copt = ops::CompileOptions::default(&cargo_config, ops::CompileMode::Test);
    if let ops::CompileFilter::Default{ref mut required_features_filterable} = copt.filter {
        *required_features_filterable = true;
    }
    copt.features = config.features.as_slice();
    copt.all_features = config.all_features;
    copt.spec = match ops::Packages::from_flags(workspace.is_virtual(), config.all, &config.exclude, &config.packages) {
        Ok(spec) => spec,
        Err(e) => {
            println!("Error getting Packages from workspace {}", e);
            return Err(-1)
        }
    };
    if config.verbose {
        println!("Running Tarpaulin");
    }
    if !config.skip_clean {
        if config.verbose {
            println!("Cleaning project");
        }
        // Clean isn't expected to fail and if it does it likely won't have an effect
        let clean_opt = ops::CleanOptions {
            config: &cargo_config,
            spec: &[],
            target: None,
            release: false,
        };
        let _ = ops::clean(&workspace, &clean_opt);
    }
    let mut result = TraceMap::new();
    println!("Building project");
    let compilation = ops::compile(&workspace, &copt);
    let mut test_passed = true;
    match compilation {
        Ok(comp) => {
            for &(ref package, ref _target_kind, ref name, ref path) in &comp.tests {
                if config.verbose {
                    println!("Processing {}", name);
                }
                if let Some((res, tp)) = get_test_coverage(&workspace, package, path.as_path(), config, false) {
                    result.merge(&res);
                    test_passed &= tp;
                }
                if config.run_ignored {
                    if let Some((res, tp)) = get_test_coverage(&workspace, package, path.as_path(),
                                                         config, true) {
                        result.merge(&res);
                        test_passed &= tp;
                    }
                }
            }
            result.dedup();
            Ok((result, test_passed))
        },
        Err(e) => {
            if config.verbose{
                println!("Error: failed to compile: {}", e);
            }
            Err(-1)
        },
    }
}


fn setup_environment() {
    let rustflags = "RUSTFLAGS";
    let mut value = " -C relocation-model=dynamic-no-pic -C link-dead-code -C opt-level=0 ".to_string();
    if let Ok(vtemp) = env::var(rustflags) {
        value.push_str(vtemp.as_ref());
    }
    env::set_var(rustflags, value);
}

/// Reports the test coverage using the users preferred method. See config.rs
/// or help text for details.
///
pub fn report_coverage(config: &Config, result: &TraceMap) {
    for format in &config.generate {
        match *format {
            Format::Cobertura   => reporting::cobertura::report(config, result).unwrap(),
            Format::Coveralls   => reporting::coveralls::report(config, result).unwrap(),
            Format::Html        => reporting::html::report(config, result).unwrap(),
            Format::Stdout      => reporting::stdout::report(config, result).unwrap(),
        }
    }
}


/// Returns the coverage statistics for a test executable in the given workspace
pub fn get_test_coverage(project: &Workspace,
                         package: &Package,
                         test: &Path,
                         config: &Config,
                         ignored: bool) -> Option<(TraceMap, bool)> {
    if !test.exists() {
        return None;
    }
    match fork() {
        Ok(ForkResult::Parent{ child }) => {
            match collect_coverage(project, test, child, config) {
                Ok(t) => {
                    Some(t)
                },
                Err(e) => {
                    println!("Error occurred: {}", e);
                    None
                },
            }
        }
        Ok(ForkResult::Child) => {
            println!("Launching test");
            execute_test(test, package, ignored, config);
            None
        }
        Err(err) => {
            println!("Failed to run {}", test.display());
            println!("Error {}", err);
            None
        }
    }

}

/// Collects the coverage data from the launched test
fn collect_coverage(project: &Workspace,
                    test_path: &Path,
                    test: Pid,
                    config: &Config) -> io::Result<(TraceMap, bool)> {
    let mut test_passed = false;
    let mut traces = generate_tracemap(project, test_path, config)?;
    {
        let (mut state, mut data) = create_state_machine(test, &mut traces, config);
        loop {
            state = state.step(&mut data, config);
            if state.is_finished() {
                if let TestState::End(i) = state {
                    test_passed = i==0;
                }
                break;
            }
        }
        if let Some(m) = data.error_message {
            println!("{}", m);
        }
        if state == TestState::Abort  {
            println!("Can't collect coverage data. Exiting");
            std::process::exit(1);
        }
    }
    Ok((traces, test_passed))
}

/// Launches the test executable
fn execute_test(test: &Path, package: &Package, ignored: bool, config: &Config) {
    let exec_path = CString::new(test.to_str().unwrap()).unwrap();
    match personality::disable_aslr() {
        Ok(_) => {},
        Err(e) => println!("ASLR disable failed: {}", e),
    }
    request_trace().expect("Failed to trace");
    if config.verbose {
        println!("running {}", test.display());
    }
    if let Some(parent) = package.manifest_path().parent() {
        let _ = env::set_current_dir(parent);
    }

    let mut envars: Vec<CString> = vec![CString::new("RUST_TEST_THREADS=1").unwrap()];
    for (key, value) in env::vars() {
        let mut temp = String::new();
        temp.push_str(key.as_str());
        temp.push('=');
        temp.push_str(value.as_str());
        envars.push(CString::new(temp).unwrap());
    }
    let mut argv = if ignored {
        vec![exec_path.clone(), CString::new("--ignored").unwrap()]
    } else {
        vec![exec_path.clone()]
    };
    if config.verbose {
        envars.push(CString::new("RUST_BACKTRACE=1").unwrap());
    } else {
        argv.push(CString::new("--quiet").unwrap());
    }
    for s in &config.varargs {
        argv.push(CString::new(s.as_bytes()).unwrap_or_default());
    }
    execve(&exec_path, &argv, envars.as_slice())
        .unwrap();
}

