// MIT License
//
// Copyright (c) 2019 Alasdair Armstrong
//
// Permission is hereby granted, free of charge, to any person
// obtaining a copy of this software and associated documentation
// files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use, copy,
// modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS
// BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN
// ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! This module loads a TOML file containing configuration for a specific instruction set
//! architecture.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use toml::Value;

use crate::ir::{Symtab, Val};
use crate::lexer::Lexer;
use crate::value_parser::ValParser;
use crate::zencode;

/// We make use of various external tools like an assembler/objdump utility. We want to make sure
/// they are available.
fn find_tool_path<P>(program: P) -> Result<PathBuf, String>
where
    P: AsRef<Path>,
{
    env::var_os("PATH")
        .and_then(|paths| {
            env::split_paths(&paths)
                .filter_map(|dir| {
                    let full_path = dir.join(&program);
                    if full_path.is_file() {
                        Some(full_path)
                    } else {
                        None
                    }
                })
                .next()
        })
        .ok_or_else(|| format!("Tool {} not found in $PATH", program.as_ref().display()))
}

fn get_tool_path(config: &Value, tool: &str) -> Result<PathBuf, String> {
    match config.get(tool) {
        Some(Value::String(program)) => find_tool_path(program),
        _ => Err(format!("Configuration option {} must be specified", tool)),
    }
}

/// Get the program counter from the ISA config, and map it to the
/// correct register identifer in the symbol table.
fn get_program_counter(config: &Value, symtab: &Symtab) -> Result<u32, String> {
    match config.get("pc") {
        Some(Value::String(register)) => match symtab.get(&zencode::encode(&register)) {
            Some(symbol) => Ok(symbol),
            None => Err(format!("Register {} does not exist in supplied architecture", register)),
        },
        _ => Err("Configuration file must specify the program counter via `pc = \"REGISTER_NAME\"`".to_string()),
    }
}

fn get_threads_value(config: &Value, key: &str) -> Result<u64, String> {
    config
        .get("threads")
        .and_then(|threads| threads.get(key).and_then(|value| value.as_str()))
        .ok_or_else(|| format!("No threads.{} found in config", key))
        .and_then(|value| {
            if value.len() >= 2 && &value[0..2] == "0x" {
                u64::from_str_radix(&value[2..], 16)
            } else {
                u64::from_str_radix(value, 10)
            }
            .map_err(|e| format!("Could not parse {} as a 64-bit unsigned integer in threads.{}: {}", value, key, e))
        })
}

fn from_toml_value(value: &Value) -> Result<Val, String> {
    match value {
        Value::Boolean(b) => Ok(Val::Bool(*b)),
        Value::Integer(i) => Ok(Val::I128(*i as i128)),
        Value::String(s) => match ValParser::new().parse(Lexer::new(&s)) {
            Ok(value) => Ok(value),
            Err(e) => Err(format!("Parse error when reading register value from configuration: {}", e)),
        },
        _ => Err(format!("Could not parse TOML value {} as register value", value)),
    }
}

fn get_default_registers(config: &Value, symtab: &Symtab) -> Result<HashMap<u32, Val>, String> {
    let defaults = config
        .get("registers")
        .and_then(|registers| registers.as_table())
        .and_then(|registers| registers.get("defaults"));

    if let Some(defaults) = defaults {
        if let Some(defaults) = defaults.as_table() {
            defaults
                .into_iter()
                .map(|(register, value)| {
                    if let Some(register) = symtab.get(&zencode::encode(register)) {
                        match from_toml_value(value) {
                            Ok(value) => Ok((register, value)),
                            Err(e) => Err(e),
                        }
                    } else {
                        Err(format!(
                            "Could not find register {} when parsing register.defaults in configuration",
                            register
                        ))
                    }
                })
                .collect()
        } else {
            Err("register.defaults should be a table or <register> = <value> pairs".to_string())
        }
    } else {
        Ok(HashMap::new())
    }
}

#[derive(Debug)]
pub struct ISAConfig {
    /// The identifier for the program counter register
    pub pc: u32,
    /// A path to an assembler for the architecture
    pub assembler: PathBuf,
    /// A path to an objdump for the architecture
    pub objdump: PathBuf,
    /// A path to a linker for the architecture
    pub linker: PathBuf,
    /// The base address for the threads in a litmus test
    pub thread_base: u64,
    /// The top address for the thread memory region
    pub thread_top: u64,
    /// The number of bytes between each thread
    pub thread_stride: u64,
    /// Default values for specified registers
    pub default_registers: HashMap<u32, Val>,
    /// Trace any function calls in this set
    pub probes: HashSet<u32>,
}

impl ISAConfig {
    fn parse(contents: &str, symtab: &Symtab) -> Result<Self, String> {
        let config = match contents.parse::<Value>() {
            Ok(config) => config,
            Err(e) => return Err(format!("Error when parsing configuration: {}", e)),
        };

        Ok(ISAConfig {
            pc: get_program_counter(&config, symtab)?,
            assembler: get_tool_path(&config, "assembler")?,
            objdump: get_tool_path(&config, "objdump")?,
            linker: get_tool_path(&config, "linker")?,
            thread_base: get_threads_value(&config, "base")?,
            thread_top: get_threads_value(&config, "top")?,
            thread_stride: get_threads_value(&config, "stride")?,
            default_registers: get_default_registers(&config, symtab)?,
            probes: HashSet::new(),
        })
    }

    /// Use a default configuration when none is specified
    pub fn new(symtab: &Symtab) -> Self {
        Self::parse(include_str!("../../configs/aarch64.toml"), symtab).expect("Default configuration was malformed!")
    }

    /// Load the configuration from a TOML file.
    pub fn from_file<P>(path: P, symtab: &Symtab) -> Result<Self, String>
    where
        P: AsRef<Path>,
    {
        let mut contents = String::new();
        match File::open(&path) {
            Ok(mut handle) => match handle.read_to_string(&mut contents) {
                Ok(_) => (),
                Err(e) => return Err(format!("Unexpected failure while reading config: {}", e)),
            },
            Err(e) => return Err(format!("Error when loading config '{}': {}", path.as_ref().display(), e)),
        };

        Self::parse(&contents, symtab)
    }
}
