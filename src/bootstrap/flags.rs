// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Command-line interface of the rustbuild build system.
//!
//! This module implements the command-line parsing of the build system which
//! has various flags to configure how it's run.

use std::fs;
use std::path::PathBuf;
use std::process;
use std::slice;

use getopts::Options;

/// Deserialized version of all flags for this compile.
pub struct Flags {
    pub verbose: bool,
    pub stage: Option<u32>,
    pub build: String,
    pub host: Filter,
    pub target: Filter,
    pub step: Vec<String>,
    pub config: Option<PathBuf>,
    pub src: Option<PathBuf>,
    pub jobs: Option<u32>,
    pub args: Vec<String>,
    pub clean: bool,
}

pub struct Filter {
    values: Vec<String>,
}

impl Flags {
    fn map_path_to_suite(path: PathBuf) -> (&'static str, Option<String>) {
        if path.starts_with("src/test") {
            let p = path.strip_prefix("src/test").unwrap();
            let (fname, cmd) = if p.starts_with("compile-fail") {
                    (p.strip_prefix("compile-fail"), "check-cfail")
                }
                else if p.starts_with("ui") {
                    (p.strip_prefix("ui"), "check-ui")
                }
                else {
                    println!("failed to find test: {:?}", path);
                    process::exit(1);
                };
            let fname = p.to_str().unwrap().to_string();
            let arg = if fname != "" {
                Some(fname)
            } else {
                None
            };
            return (cmd, arg)
        }
        else if path.starts_with("src") {
            let p = path.strip_prefix("src").unwrap();
            let fname = p.to_str().unwrap();
            if fname.starts_with("lib") {
                match p.iter().next().unwrap().to_str().unwrap() {
                    "libstd" => return ("check-crate-std", None),
                    _ => {}
                }
            }
        }
        println!("failed to find test: {:?}", path);
        process::exit(1);
    }

    pub fn parse(args: &[String]) -> Flags {
        let mut opts = Options::new();
        opts.optflag("v", "verbose", "use verbose output");
        opts.optopt("", "config", "TOML configuration file for build", "FILE");
        opts.optmulti("", "host", "host targets to build", "HOST");
        opts.reqopt("", "build", "build target of the stage0 compiler", "BUILD");
        opts.optmulti("", "target", "targets to build", "TARGET");
        opts.optopt("", "stage", "stage to build", "N");
        opts.optopt("", "test", "file or directory of tests to run", "DIR");
        opts.optopt("", "src", "path to repo root", "DIR");
        opts.optopt("j", "jobs", "number of jobs to run in parallel", "JOBS");
        opts.optflag("h", "help", "print this help message");

        let usage = |n| -> ! {
            println!("Usage: compile.py <command> [options]");
            print!("blah blah");
            if args.iter().any(|s| s=="-v" || s=="--verbose") {
                print!("{}", opts.usage(&""));
            } else {
                println!("");
            }
            process::exit(n);
        };

        let mut m = opts.parse(args).unwrap_or_else(|e| {
            println!("failed to parse options: {}", e);
            usage(1);
        });
        if m.opt_present("h") {
            usage(0);
        }

        let cfg_file = m.opt_str("config").map(PathBuf::from).or_else(|| {
            if fs::metadata("config.toml").is_ok() {
                Some(PathBuf::from("config.toml"))
            } else {
                None
            }
        });

        if m.free.len() == 0 {
            usage(1);
        }

        let (step, stage, clean) = match &*m.free.remove(0) {
            "compiler" => {
                ("libtest", Some(1), false)
            }
            "clean" => {
                ("", None, true)
            }
            "std" => {
                ("libtest", Some(0), false)
            }
            "full" => {
                ("librustc", Some(2), false)
            }
            "doc" => {
                ("doc", Some(2), false)
            }
            "check" => {
                match m.opt_str("test").map(PathBuf::from) {
                    Some(x) => {
                        let (step, test) = ::Flags::map_path_to_suite(x);
                        match test {
                            Some(v) => {
                                m.free.push(v);
                            }
                            _ => ()
                        }
                        (step, Some(1), false)
                    }
                    None => {
                        println!("Running default check...");
                        ("check", Some(2), false)
                    }
                }
            }
            x => {
                println!("unrecognized command: {}", x);
                usage(1);
            }
        };

        Flags {
            verbose: m.opt_present("v"),
            clean: clean,
            stage: m.opt_str("stage").map(|j| j.parse().unwrap()).or(stage),
            build: m.opt_str("build").unwrap(),
            host: Filter { values: m.opt_strs("host") },
            target: Filter { values: m.opt_strs("target") },
            step: vec![step.to_string()],
            config: cfg_file,
            src: m.opt_str("src").map(PathBuf::from),
            jobs: m.opt_str("jobs").map(|j| j.parse().unwrap()),
            args: m.free.clone(),
        }
    }
}

impl Filter {
    pub fn contains(&self, name: &str) -> bool {
        self.values.len() == 0 || self.values.iter().any(|s| s == name)
    }

    pub fn iter(&self) -> slice::Iter<String> {
        self.values.iter()
    }
}
