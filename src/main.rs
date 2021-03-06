// Copyright © 2016 Felix Obenhuber
// This program is free software. It comes without any warranty, to the extent
// permitted by applicable law. You can redistribute it and/or modify it under
// the terms of the Do What The Fuck You Want To Public License, Version 2, as
// published by Sam Hocevar. See the COPYING file for more details.

extern crate boolinator;
#[macro_use]
extern crate clap;
extern crate csv;
extern crate crc;
#[macro_use]
extern crate error_chain;
extern crate handlebars;
extern crate futures;
extern crate indicatif;
#[macro_use]
extern crate nom;
extern crate regex;
extern crate serde;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;
extern crate serial;
extern crate time;
extern crate terminal_size;
extern crate term_painter;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_process;
extern crate tokio_signal;
extern crate tempdir;
extern crate which;
extern crate zip;

use clap::{App, AppSettings, Arg, ArgMatches, Shell, SubCommand};
use error_chain::ChainedError;
use futures::future::*;
use futures::{Future, Stream};
use record::Record;
use std::env;
use std::io::BufReader;
use std::io::{stderr, stdout, Write};
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};
use std::str::FromStr;
use term_painter::{Color, ToStyle};
use tokio_core::reactor::{Core, Handle};
use tokio_io::io::lines;
use tokio_process::CommandExt;
use tokio_signal::ctrl_c;
use which::which_in;

mod bugreport;
mod errors;
mod filewriter;
mod filter;
mod parser;
mod record;
mod reader;
mod runner;
mod terminal;

use errors::*;
use filewriter::FileWriter;
use filter::Filter;
use parser::Parser;
use reader::{FileReader, SerialReader, StdinReader};
use runner::Runner;
use terminal::Terminal;


#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Done,
    Drop,
    Record(Record),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Format {
    Csv,
    Html,
    Human,
    Raw,
}

trait Output {
    fn process(&mut self, message: Message) -> Result<Message>;
}

impl FromStr for Format {
    type Err = &'static str;
    fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
        match s {
            "csv" => Ok(Format::Csv),
            "html" => Ok(Format::Html),
            "human" => Ok(Format::Human),
            "raw" => Ok(Format::Raw),
            _ => Err("Format parsing error"),
        }
    }
}

fn build_cli() -> App<'static, 'static> {
    App::new(crate_name!())
        .setting(AppSettings::ColoredHelp)
        .version(crate_version!())
        .author(crate_authors!())
        .about("A 'adb logcat' wrapper")
        .arg(Arg::from_usage("-t --tag [TAG] 'Tag filters in RE2. The prefix ! inverts the match'").multiple(true))
        .arg(Arg::from_usage("-m --message [MSG] 'Message filters in RE2. The prefix ! inverts the match'").multiple(true))
        .arg(Arg::from_usage("-h --highlight [HIGHLIGHT] 'Highlight pattern in RE2").multiple(true))
        .arg_from_usage("-o --output [OUTPUT] 'Write to file and stdout'")
        .arg(Arg::with_name("RECORDS_PER_FILE")
            .short("n")
            .long("records-per-file")
            .takes_value(true)
            .requires("output")
            .help("Write n records per file. Use k, M, G suffixes or a plain number"))
        .arg(Arg::with_name("FILE_FORMAT")
            .long("file-format")
            .short("f")
            .takes_value(true)
            .requires("output")
            .possible_values(&["csv", "html", "raw"])
            .help("Select format for output files"))
        .arg(Arg::with_name("FILENAME_FORMAT")
            .long("filename-format")
            .short("a")
            .takes_value(true)
            .requires("output")
            .possible_values(&["single", "enumerate", "date"])
            .help("Select format for output file names. By passing 'single' the filename provided with the '-o' option is used. \
                  'enumerate' will append a file sequence number after the filename passed with '-o' option whenever a new file is \
                  created (see 'records-per-file' option). 'date' will prefix the output filename with the current local date \
                  when a new file is created"))
        .arg(Arg::with_name("TERMINAL_FORMAT")
            .long("terminal-format")
            .short("e")
            .takes_value(true)
            .default_value("human")
            .possible_values(&["human", "raw", "csv"])
            .help("Select format for stdout"))
        .arg(Arg::from_usage("-i --input [INPUT] 'Read from file instead of command. Use 'serial://COM0@11520,8N1 or similiar for reading a serial ports")
            .multiple(true))
        .arg(Arg::with_name("LEVEL")
            .short("l")
            .long("level")
            .takes_value(true)
            .possible_values(&["trace", "debug", "info", "warn", "error", "fatal", "assert", "T",
                               "D", "I", "W", "E", "F", "A"])
            .help("Minimum level"))
        .arg(Arg::with_name("OVERWRITE")
            .long("overwrite")
            .requires("output")
            .help("Overwrite output file if present"))
        .arg_from_usage("-r --restart 'Restart command on exit'")
        .arg_from_usage("-c --clear 'Clear (flush) the entire log and exit'")
        .arg_from_usage("-g --get-ringbuffer-size 'Get the size of the log's ring buffer and exit'")
        .arg_from_usage("-S --output-statistics 'Output statistics'")
        .arg_from_usage("--shorten-tags 'Shorten tag by removing vovels if too long'")
        .arg_from_usage("--show-date 'Show month and day when printing on stdout'")
        .arg_from_usage("--show-time-diff 'Show time diff of tags after timestamp'")
        // .arg_from_usage("-s --skip-on-restart 'Skip messages on restart until last message from \
        //                  previous run is (re)received. Use with caution!'")
        .arg_from_usage("[COMMAND] 'Optional command to run and capture stdout from. Pass \"-\" to \
                         capture stdin'. If omitted, rogcat will run \"adb logcat -b all\"")
        .subcommand(SubCommand::with_name("completions")
            .about("Generates completion scripts for your shell")
            .arg(Arg::with_name("SHELL")
                .required(true)
                .possible_values(&["bash", "fish", "zsh"])
                .help("The shell to generate the script for")))
        .subcommand(SubCommand::with_name("devices").about("Show list of available devices"))
        .subcommand(SubCommand::with_name("bugreport")
            .about("Capture bugreport")
            .arg(Arg::with_name("ZIP")
                .short("z")
                .long("zip")
                .help("Zip report"))
            .arg(Arg::with_name("OVERWRITE")
                .long("overwrite")
                .help("Overwrite report file if present"))
            .arg(Arg::with_name("FILE")
                .help("Output file name - defaults to <date>-bugreport")))
}

fn main() {
    match run(&build_cli().get_matches()) {
        Err(e) => {
            let stderr = &mut stderr();
            let errmsg = "Error writing to stderr";
            writeln!(stderr, "{}", e.display()).expect(errmsg);
            exit(1)
        }
        Ok(r) => exit(r),
    }
}

fn adb() -> Result<PathBuf> {
    which_in("adb", env::var_os("PATH"), env::current_dir()?)
        .map_err(|e| format!("Cannot find adb: {}", e).into())
}

fn input(handle: Handle, args: &ArgMatches) -> Result<Box<Stream<Item = Message, Error = Error>>> {
    if args.is_present("input") {
        let input = args.value_of("input").ok_or("Invalid input value")?;
        if SerialReader::parse_serial_arg(input).is_ok() {
            Ok(Box::new(SerialReader::new(input)?))
        } else {
            Ok(Box::new(FileReader::new(args)?))
        }
    } else {
        match args.value_of("COMMAND") {
            Some(c) => {
                if c == "-" {
                    Ok(Box::new(StdinReader::new()))
                } else if SerialReader::parse_serial_arg(c).is_ok() {
                    Ok(Box::new(SerialReader::new(c)?))
                } else {
                    let cmd = c.to_owned();
                    let restart = args.is_present("restart");
                    let skip_on_restart = args.is_present("skip-on-restart");
                    Ok(Box::new(Runner::new(handle, cmd, restart, skip_on_restart)?))
                }
            }
            None => {
                adb()?;
                let cmd = "adb logcat -b all".to_owned();
                let restart = true;
                let skip_on_restart = args.is_present("skip-on-restart");
                Ok(Box::new(Runner::new(handle, cmd, restart, skip_on_restart)?))
            }
        }
    }
}

fn run(args: &ArgMatches) -> Result<i32> {
    let mut core = Core::new()?;

    match args.subcommand() {
        ("completions", Some(sub_matches)) => {
            let shell = sub_matches.value_of("SHELL").unwrap();
            build_cli().gen_completions_to(crate_name!(),
                                           shell.parse::<Shell>().unwrap(),
                                           &mut stdout());
            return Ok(0);
        }
        ("devices", _) => {
            let mut child = Command::new(adb()?).arg("devices")
                .stdout(Stdio::piped())
                .spawn_async(&core.handle())?;
            let stdout = child.stdout()
                .take()
                .ok_or("Failed to read stdout of adb")?;
            let reader = BufReader::new(stdout);
            let lines = lines(reader);
            let result = lines.skip(1).for_each(|l| {
                if !l.is_empty() && !l.starts_with("* daemon") {
                    let mut s = l.split_whitespace();
                    let id: &str = s.next().unwrap_or("unknown");
                    let name: &str = s.next().unwrap_or("unknown");
                    println!("{} {}", terminal::DIMM_COLOR.paint(id), match name {
                            "unauthorized" => Color::Red.paint(name),
                            _ => Color::Green.paint(name),
                        })
                }
                Ok(())
            });

            let output = core.run(result.join(child.wait_with_output()))?.1;
            exit(output.status
                     .code()
                     .ok_or("Failed to get exit code")?);
        }
        ("bugreport", Some(sub_matches)) => exit(bugreport::create(sub_matches, &mut core)?),
        (_, _) => (),
    }

    for arg in &["clear", "get-ringbuffer-size", "output-statistics"] {
        if args.is_present(arg) {
            let arg = format!("-{}", match arg {
                &"clear" => "c",
                &"get-ringbuffer-size" => "g",
                &"output-statistics" => "S",
                _ => panic!(""),
            });
            let child = Command::new(adb()?).arg("logcat")
                .arg(arg)
                .spawn_async(&core.handle())?;
            let output = core.run(child)?;
            exit(output.code().ok_or("Failed to get exit code")?);
        }
    }

    let mut output = if args.is_present("output") {
        Box::new(FileWriter::new(args)?) as Box<Output>
    } else {
        Box::new(Terminal::new(args)?) as Box<Output>
    };
    let mut parser = Parser::new();
    let mut filter = Filter::new(args)?;

    let handle = core.handle();
    let ctrlc = core.run(ctrl_c(&handle))?
        .map(|_| Message::Done)
        .map_err(|e| e.into());

    let input = input(core.handle(), args)?;
    let result = input.select(ctrlc)
        .and_then(|m| parser.process(m))
        .and_then(|m| filter.process(m))
        .and_then(|m| output.process(m))
        .take_while(|r| ok(r != &Message::Done))
        .for_each(|_| ok(()));

    core.run(result).map(|_| 0)
}
