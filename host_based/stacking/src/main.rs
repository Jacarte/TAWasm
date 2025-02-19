#![feature(internal_output_capture)]

use anyhow::Context;
use clap::Parser;
use core::sync::atomic::Ordering::{Relaxed, SeqCst};
use pausable_clock::PausableClock;
use rand::rngs::SmallRng;
use rand::Rng;
use rand::SeedableRng;
use std::borrow::Borrow;
use std::borrow::BorrowMut;
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Display;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Duration;
use std::{collections::HashMap, sync::Arc};
use std::{panic, process};
use wasm_mutate::WasmMutate;
use wasmtime::Val;

/// # Stacking for wasm-mutate.
///
/// ## Example
/// `stacking -s 1 -c 10 i.wasm -o o.wasm`
///
#[derive(Parser, Clone)]
struct Options {
    /// The input folder that contains the Wasm binaries.
    input: PathBuf,
    /// The seed of the random mutation, 0 by default
    #[arg(short = 's', long = "seed")]
    seed: u64,
    /// Number of stacked mutations
    #[arg(short = 'c', long = "count")]
    count: usize,
    /// Save every X steps
    #[arg(short = 'v', long = "save", default_value = "10000")]
    step: usize,
    /// Cache folder name. The cache is used to void repeated transformations that are not interesting.
    #[arg(long = "cache-folder", default_value = "cache")]
    cache_folder: String,
    /// Erase cache on start
    #[arg(long = "remove-cache", default_value = "false")]
    remove_cache: bool,

    /// Do IO equivalence checking. The original and the variant will be executed and their result compared.
    #[arg(long = "check-io", default_value = "false")]
    check_io: bool,

    /// IO check arguments. It will be for the original and the variant comparison if check_io is true.
    #[arg(long = "check-args")]
    check_args: Vec<String>,

    /// IO check arguments. It will be for the original and the variant comparison if check_io is true. This options should be a string for stdout output script, each argument will be separated by a space.
    #[arg(long = "args-generator")]
    args_generator: Option<String>,

    /// Execution fuel. Somehow like a timeout
    #[arg(long = "fuel", default_value = "0")]
    fuel: u64,

    /// If true, a random parent will be selected to create the new variant out of the db. The -v parameter will be ignored.
    #[arg(long = "chaos-mode", default_value = "false")]
    chaos_mode: bool,

    /// If true, checks consistency between original and variant memories
    #[arg(long = "check-mem", default_value = "false")]
    check_mem: bool,

    /// Take X variants from parent only. Only available if chaos mode is true
    #[arg(long = "variants-per-parent", default_value = "10")]
    variants_per_parent: usize,

    /// Uses wasm-mutate preserving semantics
    #[arg(long = "no-preserve-semantics", default_value = "false", action)]
    no_preserve_semantics: bool,

    /// Saves the default compiled Wasm binary
    #[arg(long = "save-compiling", default_value = "false", action)]
    save_compiling: bool,

    /// The output Wasm binary.
    output: PathBuf,

    /// Saves the execution stderr and stdout
    #[arg(long = "save-io", default_value = "false", action)]
    save_io: bool,

    /// Just prints metadata of the input binary
    #[arg(long = "print-meta", default_value = "false", action)]
    print_meta: bool,

    /// Target specific function index to diversify
    #[arg(long = "function_index")]
    function_index: Option<usize>,

    /// Target specific function name to diversify
    #[arg(long = "function_name")]
    function_name: Option<String>,

    /// We diversify until the oracle returns exit code 1
    /// The oracle will be launched as a subprocess, e.g. `--oracle bash size_larger_1Mb.sh`
    /// The input of the oracle script is the wasm binary file
    #[arg(long = "oracle")]
    oracle: Option<String>,

    /// If true, only Wasm binaroes producing different machine codes thatn the original will be reported
    /// If true, the oracle will be called only if the machine code is different
    #[arg(long = "report-only-if-diff-mc", default_value = "false", action)]
    report_only_if_diff_mc: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Stat {
    /// To measure the time invested in mutating
    #[serde(skip)]
    pub timer: Arc<PausableClock>,
    #[serde(skip)]
    now: std::time::Instant,
    /// To number of rotations
    pub total_rotations: usize,

    elapsed_nanos: u128,

    /// Number of generated binaries
    pub generated_binaries: usize,

    /// Number of unique generated machine codes
    pub unique_machine_codes: usize,
}

impl Stat {
    pub fn new() -> Self {
        Self {
            timer: Arc::new(PausableClock::default()),
            total_rotations: 0,
            elapsed_nanos: 0,
            now: std::time::Instant::now(),
            generated_binaries: 0,
            unique_machine_codes: 0,
        }
    }

    pub fn pause(self: &mut Self) {
        self.timer.pause();
    }

    pub fn unpause(self: &mut Self) {
        self.timer.resume();
    }

    pub fn get_nanos(&self) -> u128 {
        (std::time::Instant::from(self.timer.now()) - self.now).as_nanos()
    }

    pub fn serialize(&mut self) -> String {
        self.elapsed_nanos = self.get_nanos();
        serde_json::to_string(self).unwrap()
    }
}

fn swap(current: &mut Vec<u8>, new_interesting: Vec<u8>) {
    *current = new_interesting;
}

pub struct Stacking {
    current: (Vec<u8>, usize),
    original: Vec<u8>,
    check_args: Vec<String>,
    original_state: Option<eval::ExecutionResult>,
    pub index: usize,
    pub step: usize,
    fuel: u64,
    count: usize,
    rnd: SmallRng,
    check_io: bool,
    check_mem: bool,
    // The hashes will prevent regression and non performed transformations
    hashes: sled::Db,
    // The hashes will prevent regression on already generated machine code
    mc_hashes: sled::Db,
    chaos_mode: bool,
    variants_per_parent: usize,
    no_preserve_semantics: bool,
    save_compiling: bool,
    args_generator: Option<String>,
    oracle: Option<String>,
}

impl Stacking {
    pub fn new(
        current: Vec<u8>,
        count: usize,
        step: usize,
        seed: u64,
        remove_cache: bool,
        cache_dir: String,
        check_args: Vec<String>,
        check_io: bool,
        fuel: u64,
        chaos_mode: bool,
        check_mem: bool,
        variants_per_parent: usize,
        save_compiling: bool,
        no_preserve_semantics: bool,
        args_generator: Option<String>,
        oracle: Option<String>,
    ) -> Self {
        // Remove db if exist
        if remove_cache {
            std::fs::remove_dir_all(&cache_dir.clone());
            std::fs::remove_dir_all(format!("{}.mc", cache_dir.clone().to_owned()));
        }

        let config = sled::Config::default()
            .path(cache_dir.clone().to_owned())
            .cache_capacity(/* 4Gb */ 1 * 512 * 1024 * 1024);


        let config2 = sled::Config::default()
            .path(format!("{}.mc", cache_dir.clone().to_owned()))
            .cache_capacity(/* 4Gb */ 1 * 512 * 1024 * 1024);

        let original_state = 
            match eval::execute_single(&current, check_args.clone(), fuel, true, true) {
                Some(it) => {
                    eprintln!("Original time {}ns", it.6.as_nanos());
                    Some(it)
                }
                None => {
                    eprintln!("Could not execute the original");
                    None
                } 
            };

        Self {
            original: current.clone(),
            current: (current, 0),
            check_args,
            original_state,
            index: 0,
            chaos_mode,
            fuel,
            check_mem,
            count,
            step,
            rnd: SmallRng::seed_from_u64(seed),
            variants_per_parent,
            // Set the cache size to 3GB
            hashes: config.open().expect("Could not create external cache"),
            mc_hashes: config2.open().expect("Could not create external cache for MC"),
            save_compiling,
            no_preserve_semantics,
            args_generator,
            check_io,
            oracle,
        }
    }

    pub fn next(&mut self, chaos_cb: impl Fn(&Vec<u8>, &Vec<u8>, usize)) {
        // Mutate
        let mut wasmmutate = WasmMutate::default();
        let mut wasmmutate = wasmmutate.preserve_semantics(!self.no_preserve_semantics);

        let seed = self.rnd.gen();
        eprintln!("Seed {}", seed);
        let wasmmutate = wasmmutate.seed(seed);
        let cp = self.current.clone();
        let origwasm = cp.0.clone();
        let wasm = wasmmutate.run(&origwasm);
        //

        // chaos_cb(&origwasm, &self.original);

        let hash = blake3::hash(&origwasm);
        eprintln!("Mutating {}", hash);
        match wasm {
            Ok(it) => {
                // Get the first one only
                for w in it.take(self.variants_per_parent) {
                    match w {
                        Ok(b) => {
                            // Check if the hash was not generated before
                            let hash = blake3::hash(&b);
                            let hash = hash.as_bytes().to_vec();

                            if let Ok(true) = self.hashes.contains_key(&hash) {
                                // eprintln!("Already contained");
                                // We already generated this hash, so we skip it

                                continue;
                            }
                            if self.check_io {
                                
                                if  let Some(original_state) = &self.original_state {
                                    match eval::assert_same_evaluation(
                                        &self.original,
                                        &b,
                                        self.check_args.clone(),
                                        self.fuel,
                                        self.check_mem,
                                        self.args_generator.clone()
                                    ) {
                                        Some(st) => {
                                            // The val is the value is the wasm + the hash of the previous one
                                            let val =
                                                vec![self.index.to_le_bytes().to_vec(), b.clone()]
                                                    .concat();
                                            let _ = self
                                                .hashes
                                                .insert(hash, val)
                                                .expect("Failed to insert");

                                            // Execute to see semantic equivalence

                                            if self.chaos_mode {
                                                // TODO if chaos mode...select from the DB ?
                                                let random_item =
                                                    self.rnd.gen_range(0..self.hashes.len());

                                                match self.hashes.iter().take(random_item).next() {
                                                    Some(random_item) => {
                                                        match random_item {
                                                            Ok(random_item) => {
                                                                let k = random_item.0;
                                                                // eprintln!("Random key {:?} out of {}", k, self.hashes.len());
                                                                let random_curr = random_item.1;
                                                                // The val is the value is the wasm + the index in le bytes
                                                                let index = random_curr[0..8].to_vec();
                                                                let wasm = random_curr[8..].to_vec();
                                                                self.current = (
                                                                    wasm,
                                                                    usize::from_le_bytes(
                                                                        index
                                                                            .as_slice()
                                                                            .try_into()
                                                                            .unwrap(),
                                                                    ),
                                                                );
                                                                self.index = self.current.1 + 1;

                                                                eprintln!(
                                                                    "=== CHAOS {}",
                                                                    self.index - 1
                                                                );
                                                                eprintln!(
                                                                    "=== CHAOS COUNT {}",
                                                                    self.hashes.len()
                                                                );
                                                                // Generate the file here already
                                                                chaos_cb(
                                                                    &b,
                                                                    &origwasm,
                                                                    self.hashes.len(),
                                                                );
                                                                continue;
                                                            }
                                                            Err(e) => {
                                                                eprintln!("Error {}", e);
                                                                // We could not mutate the wasm, we skip it
                                                                continue;
                                                            }
                                                        }
                                                    }
                                                    None => continue,
                                                };
                                            } else {
                                                self.current = (b.clone(), self.index + 1);
                                                self.index += 1;

                                                if self.index % 10000 == 9999 {
                                                    eprintln!("{} mutations", self.index);
                                                }

                                                eprintln!("=== TRANSFORMED {}", self.index);

                                                if self.index % self.step == 0 {
                                                    // Call the cb
                                                    chaos_cb(&b, &origwasm, self.index);
                                                }
                                                break;
                                            }
                                        }
                                        None => {}
                                    }
                                } else {
                                    unreachable!();
                                }
                            } else {
                                eprintln!("Not initial state");
                                if self.chaos_mode {
                                    // TODO if chaos mode...select from the DB ?
                                    let random_item = self.rnd.gen_range(0..self.hashes.len());

                                    match self.hashes.iter().take(random_item).next() {
                                        Some(random_item) => {
                                            match random_item {
                                                Ok(random_item) => {
                                                    let k = random_item.0;
                                                    // eprintln!("Random key {:?} out of {}", k, self.hashes.len());
                                                    let random_curr = random_item.1;
                                                    // The val is the value is the wasm + the index in le bytes
                                                    let index = random_curr[0..8].to_vec();
                                                    let wasm = random_curr[8..].to_vec();
                                                    self.current = (
                                                        wasm,
                                                        usize::from_le_bytes(
                                                            index
                                                                .as_slice()
                                                                .try_into()
                                                                .unwrap(),
                                                        ),
                                                    );
                                                    self.index = self.current.1 + 1;

                                                    eprintln!("=== CHAOS {}", self.index - 1);
                                                    eprintln!(
                                                        "=== CHAOS COUNT {}",
                                                        self.hashes.len()
                                                    );
                                                    // Generate the file here already
                                                    chaos_cb(&b, &origwasm, self.hashes.len());
                                                    continue;
                                                }
                                                Err(e) => {
                                                    eprintln!("Error {}", e);
                                                    // We could not mutate the wasm, we skip it
                                                    continue;
                                                }
                                            }
                                        }
                                        None => continue,
                                    };
                                } else {
                                    self.current = (b.clone(), self.index + 1);
                                    self.index += 1;

                                    if self.index % 10000 == 9999 {
                                        eprintln!("{} mutations", self.index);
                                    }

                                    eprintln!("=== TRANSFORMED {}", self.index);

                                    if self.index % self.step == 0 {
                                        // Call the cb
                                        chaos_cb(&b, &origwasm, self.index);
                                    }
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Error {}", e);
                            // We could not mutate the wasm, we skip it
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Error {}", e);
                // We could not mutate the wasm, we skip it
            }
        }
    }
}

/// Returns true if the oracle returns 1 in exit code
fn call_oracle(oracle: String, wasm_file: String) -> bool {
    /// Split the oracle string by space
    let command = oracle.split(" ").collect::<Vec<_>>();
    /// The arguments are the rest of the command
    let args = &command[1..];
    eprintln!("Executing oracle {} {}", oracle, wasm_file);
    /// We call a subprocess and check its exist code
    let output = std::process::Command::new(&command[0])
        .args(args)
        .arg(wasm_file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("Failed to run command");

    let cp = output.clone();
    // Decode stdout as string
    let stdout = String::from_utf8(cp.stdout).expect("Could not decode output");
    let stderr = String::from_utf8(cp.stderr).expect("Could not decode stderr");

    eprintln!("Oracle stdout {}", stdout);
    eprintln!("Oracle stderr {}", stderr);

    // check exit code
    if output.status.code().unwrap() == 1 {
        return true;
    }

    return false;
}

fn main() -> Result<(), anyhow::Error> {
    // Init logs
    env_logger::init();

    let opts = Options::parse();

    let opsclone = opts.clone();
    // load the bytes from the input file
    let bytes = std::fs::read(&opts.input).expect("Could not read the input file");

    let mut stack = Stacking::new(
        bytes,
        opsclone.count,
        opsclone.step,
        opsclone.seed,
        opsclone.remove_cache,
        opsclone.cache_folder,
        opsclone.check_args,
        opsclone.check_io,
        opsclone.fuel,
        opsclone.chaos_mode,
        opsclone.check_mem,
        opsclone.variants_per_parent,
        opsclone.save_compiling,
        opsclone.no_preserve_semantics,
        opsclone.args_generator,
        opsclone.oracle,
    );

    if opsclone.print_meta {
        // Just output the binary metadata. How many functions, number of instructions per function, etc.
        let mut wasmmutate = WasmMutate::default();
        let mut wasmmutate = wasmmutate.preserve_semantics(!stack.no_preserve_semantics);
        wasmmutate.setup(&stack.original);
        let info = wasmmutate.info();
        let code_section = info.get_code_section();
        let sectionreader = wasmparser::CodeSectionReader::new(code_section.data, 0).unwrap();
        println!("Name: {}", opts.input.display());
        println!("Function Count: {}", sectionreader.count());
        // Calculating the total number of instructions
        let readers = sectionreader
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut total_instructions = 0;
        for reader in &readers {
            let mut reader = reader.get_operators_reader().unwrap();
            while !reader.eof() {
                let _ = reader.read().unwrap();
                total_instructions += 1;
            }
        }
        println!("Instructions count: {}", total_instructions);
        return Ok(());
    }
    let mut C = 0;

    let original_hash = match &stack.original_state {
        Some(t) => {
            blake3::hash(
                &t.clone().7
            )
        }
        None => {
            println!("Original state not found, we use the hash of the empty state");
            blake3::hash(String::from("empty").as_bytes())
        }
    }; 
    // The timer starts
    let mut stat = Stat::new();
    let stat = std::cell::RefCell::new(stat);
    let mc_hashes = std::cell::RefCell::new(stack.mc_hashes.clone());
    //stat.borrow_mut().pause();
    loop {
        let opsclone = opts.clone();
        let mut stat_clone = stat.clone();
        let mut mc_hashescp = mc_hashes.clone();
        let output = &opts.output;
        if opts.chaos_mode {
            stack.next(move |new, parent, c| {
                let hash = blake3::hash(&new);
                let hash2 = blake3::hash(&parent);
                let name = format!("{}.{}.{}.chaos.wasm", output.display(), hash2, hash);
                // Write the current to fs
                std::fs::write(&name, new).expect("Could not write the output file");
                stat_clone.borrow_mut().generated_binaries += 1;

                // Also save the cwasm
                if opts.save_compiling {
                    // We pause the stat to avoid counting the compilation time
                    stat_clone.borrow_mut().pause();
                    let mut config = wasmtime::Config::default();
                    let config = config.strategy(wasmtime::Strategy::Cranelift);
                    // We need to save the generated machine code to disk

                    // Create a new store
                    let engine = wasmtime::Engine::new(&config).unwrap();

                    let module = wasmtime::Module::new(&engine, &new).unwrap();

                    // Serialize it
                    // check if it was already serialized, avoid compiling again
                    let serialized = module.serialize().unwrap();
                    // Save it to disk, get the filename from the argument path
                    std::fs::write(
                        format!("{}.{}.{}.chaos.cwasm", output.display(), hash2, hash),
                        serialized.clone(),
                    )
                    .unwrap();

                    let serialization_hash = blake3::hash(&serialized);

                    let call_oracle_if_diff = if opsclone.report_only_if_diff_mc && serialization_hash != original_hash {
                            // Check if the hash is not in the external cache
                            let hashbytes = serialization_hash.as_bytes();
                            if !mc_hashescp.borrow_mut().contains_key(hashbytes).unwrap() {
                                eprintln!("New MC hash: {}", serialization_hash);
                                eprintln!("Original MC hash: {}", original_hash);
                                eprintln!("MC hash changed, we call the oracle then");
                                stat_clone.borrow_mut().unique_machine_codes += 1;
                                let _ = mc_hashescp
                                            .borrow_mut()
                                            .insert(hashbytes, b"")
                                            .expect("Failed to insert");


                                true
                            }
                            else {
                              false
                            }

                        } else {
                            false
                        };

                    stat_clone.borrow_mut().unpause();
                    if let Some(oracle) = &opsclone.oracle {
                        if call_oracle_if_diff || !opsclone.report_only_if_diff_mc {
                            // Pause the timer to skip oracle time
                            eprintln!("Time before {}", stat_clone.borrow().get_nanos());
                            stat_clone.borrow_mut().pause();
                            if call_oracle(
                                oracle.clone(),
                                format!("{}.{}.{}.chaos.wasm", output.display(), hash2, hash),
                            ) {
                                eprintln!(
                                    "Elapsed time until oracle: {}ns",
                                    stat_clone.borrow().get_nanos()
                                );
                                stat_clone.borrow_mut().pause();
                                stat_clone.borrow_mut().generated_binaries = c;
                                eprintln!("Oracle returned 1, we stop");
                                // Save the stat struct as a json
                                let mut stat = stat_clone.borrow().clone();
                                let stat = stat.serialize();
                                // Save to file
                                let jsonname = format!("{}.chaos.json", output.display());
                                std::fs::write(jsonname, stat).unwrap();
                                std::process::exit(0);
                            }
                            // Resume the timer
                            stat_clone.borrow_mut().unpause();
                        }
                    }
                } else {
                    if let Some(oracle) = &opsclone.oracle {
                        stat_clone.borrow_mut().pause();
                        if call_oracle(
                            oracle.clone(),
                            format!("{}.{}.{}.chaos.wasm", output.display(), hash2, hash),
                        ) {
                            // The oracle returned 1, we stop
                            eprintln!(
                                "Elapsed time until oracle: {}ns",
                                stat_clone.borrow().get_nanos()
                            );
                            eprintln!("Oracle returned 1, we stop");
                            std::process::exit(0);
                        }
                        stat_clone.borrow_mut().unpause();
                    }
                }
            });
        } else {
            let index = stack.index;
            stack.next(move |new, parent, c| {
                let name = format!("{}.{}.wasm", output.display(), index);
                // Write the current to fs
                std::fs::write(&name, new.clone()).expect("Could not write the output file");

                eprintln!("=== STACKED {}", c);

                if opts.save_compiling {
                    let mut config = wasmtime::Config::default();
                    let config = config.strategy(wasmtime::Strategy::Cranelift);
                    // We need to save the generated machine code to disk

                    // Create a new store
                    let engine = wasmtime::Engine::new(&config).unwrap();

                    let module = wasmtime::Module::new(&engine, &new.clone()).unwrap();

                    // Serialize it
                    // TODO check if it was already serialized, avoid compiling again
                    let serialized = module.serialize().unwrap();
                    // Save it to disk, get the filename from the argument path
                    std::fs::write(format!("{}.{}.cwasm", output.display(), index), serialized)
                        .unwrap();

                    if let Some(oracle) = &opsclone.oracle {
                        if call_oracle(
                            oracle.clone(),
                            format!("{}.{}.cwasm", output.display(), index),
                        ) {
                            // The oracle returned 1, we stop
                            // eprintln!("Elapsed time until oracle: {}s", elapsed.as_millis());
                            eprintln!("Oracle returned 1, we stop");
                            std::process::exit(0);
                        }
                    }
                } else {
                    if let Some(oracle) = &opsclone.oracle {
                        if call_oracle(
                            oracle.clone(),
                            format!("{}.{}.wasm", output.display(), index),
                        ) {
                            // The oracle returned 1, we stop
                            // eprintln!("Elapsed time until oracle: {}s", elapsed.as_millis());
                            eprintln!("Oracle returned 1, we stop");
                            std::process::exit(0);
                        }
                    }
                }
            });

            if stack.index >= opts.count {
                break;
            }
        }
    }

    // Assert that we have X different mutations
    // assert!(stack.hashes.len() == opts.count);

    // Write the current to fs
    if !opts.chaos_mode {
        std::fs::write(&opts.output, stack.current.0).expect("Could not write the output file");
    }
    Ok(())
}

// Somesort of the same as wasm-mutate fuzz
mod eval {
    use std::arch::asm;
    #[cfg(all(target_arch = "x86_64"))]
    use std::arch::x86_64::_mm_clflush;
    #[cfg(all(target_arch = "x86_64"))]
    use std::arch::x86_64::_mm_lfence;
    #[cfg(all(target_arch = "x86_64"))]
    use std::arch::x86_64::_mm_mfence;
    #[cfg(all(target_arch = "x86_64"))]
    use std::arch::x86_64::_rdtsc as builtin_rdtsc;

    #[cfg(all(target_arch = "x86_64"))]
    pub fn _rdtsc() -> u64 {
        let eax: u32;
        let ecx: u32;
        let edx: u32;
        {
            unsafe {
                asm!(
                  "rdtscp",
                  lateout("eax") eax,
                  lateout("ecx") ecx,
                  lateout("edx") edx,
                  options(nomem, nostack)
                );
            }
        }

        let counter: u64 = (edx as u64) << 32 | eax as u64;
        counter
    }

    use std::fs;
    use std::hash::Hash;
    use std::sync::Arc;
    use stdio_override::StderrOverride;
    use stdio_override::StdoutOverride;
    // ./target/release/stacking tests/wb_challenge.wasm stacking.sym --seed 100 -c 2 -v 1000 --check-args wb_challenge.wasm --check-args 00 --check-args 01 --check-args 02 --check-args 03 --check-args 04 --check-args 05 --check-args 06 --check-args 07 --check-args 08 --check-args 09 --check-args 0a --check-args 0b --check-args 0c --check-args 0d --check-args 0e --check-args 0f
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    use wasmtime::Val;
    use wasmtime_wasi::sync::WasiCtxBuilder;
    use wasmtime_wasi::WasiCtx;

    fn get_current_working_dir() -> std::io::Result<std::path::PathBuf> {
        std::env::current_dir()
    }

    /// Creates the WASI support
    pub fn create_linker(engine: &wasmtime::Engine) -> wasmtime::Linker<wasmtime_wasi::WasiCtx> {
        let mut linker = wasmtime::Linker::new(&engine);

        wasmtime_wasi::add_to_linker(&mut linker, |s| s).unwrap();
        // These methods are not in WASI by default, yet, let us assume they are
        // It is the same assumption of Swivel
        let linker = linker
            .func_wrap(
                "env",
                "_mm_clflush",
                |mut caller: wasmtime::Caller<'_, _>, param: u32| {
                    // get the memory of the module
                    // This comes on the guest address space, we need to translate it to the host address space

                    let memory = caller.get_export("memory").unwrap().into_memory().unwrap();
                    let memory_data = memory.data(&mut caller);
                    let addr = &memory_data[param as usize] as *const u8;

                    // println!("Flush {:?}", addr);
                    // flush the real address of the memory index
                    unsafe {
                        asm! {
                           "clflush [{x}]",
                           x = in(reg) addr
                        }
                    }
                },
            )
            .unwrap();

        let linker = linker
            .func_wrap(
                "env",
                "_mm_mfence",
                |_caller: wasmtime::Caller<'_, _>| unsafe {
                    // println!("_mm_mfence");
                    _mm_mfence();
                },
            )
            .unwrap();

        let linker = linker
            .func_wrap("env", "_rdtsc", |_caller: wasmtime::Caller<'_, _>| unsafe {
                _rdtsc()
            })
            .unwrap();

        let linker = linker
            .func_wrap(
                "env",
                "_mm_lfence",
                |_caller: wasmtime::Caller<'_, _>, _param: i32| unsafe {
                    _mm_lfence();
                },
            )
            .unwrap();

        linker.clone()
    }

    pub type ExecutionResult = (
        Vec<u64>,
        Vec<Val>,
        String,
        String,
        wasmtime::Module,
        wasmtime::Instance,
        std::time::Duration,
        // The generated machine code
        Vec<u8>,
    );

    pub fn execute_single(
        wasm: &[u8],
        args: Vec<String>,
        fuel: u64,
        get_machine_code: bool,
        is_original: bool
    ) -> Option<ExecutionResult> {
        let mut config = wasmtime::Config::default();
        let config = config.strategy(wasmtime::Strategy::Cranelift);

        // Set no opt for faster execution
        let config = config.cranelift_opt_level(wasmtime::OptLevel::None);

        config.cranelift_nan_canonicalization(true);
        if fuel > 0 {
            config.consume_fuel(true);
        }

        // config.consume_fuel(true);

        let engine = wasmtime::Engine::new(&config).unwrap();

        let now = std::time::Instant::now();
        let module = match wasmtime::Module::new(&engine, &wasm) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("Error compiling the module {}", e);
                return None;
            }
        };
        let lapsed = now.elapsed();
        if is_original {
            eprintln!("Compilation took {}ns", lapsed.as_nanos());
        } else {
            eprintln!("Variant compilation took {}ns", lapsed.as_nanos());
        }
        let folder_of_bin = get_current_working_dir().unwrap().display().to_string();

        let mut wasi = WasiCtxBuilder::new()
            .inherit_stdio()
            .args(&args)
            .unwrap()
            // Preopen in the CWD
            .preopened_dir(
                wasmtime_wasi::sync::Dir::open_ambient_dir(
                    "./",
                    wasmtime_wasi::sync::ambient_authority(),
                )
                .unwrap(),
                "./",
            )
            .unwrap()
            .preopened_dir(
                wasmtime_wasi::sync::Dir::open_ambient_dir(
                    folder_of_bin.clone(),
                    wasmtime_wasi::sync::ambient_authority(),
                )
                .unwrap(),
                "./"
            )
            .unwrap()
            .build();

        let mut linker = create_linker(&engine);
        let stdout_file = "./stdout.txt";
        let stderr_file = "./stderr.txt";

        // Recreate the file
        let _ = std::fs::File::create(&stdout_file);
        let _ = std::fs::File::create(&stderr_file);

        let guardout = StdoutOverride::override_file(stdout_file).unwrap();
        let guarderr = StderrOverride::override_file(stderr_file).unwrap();

        let mut store1 = wasmtime::Store::new(&engine, wasi.clone());
        if fuel > 0 {
            store1.add_fuel(fuel).unwrap();
            store1.out_of_fuel_trap();
        }

        eprintln!("Instantiating the module");
        match linker.instantiate(&mut store1, &module){
            Ok(instance1) => {
                match instance1.get_func(&mut store1, "_start") {
                    Some(func1) => {
                        let now = std::time::Instant::now();
    
                        match func1.call(&mut store1, &mut [], &mut []) {
                            Ok(e) => {
                                let elapsed = now.elapsed();
                                if is_original {
                                    eprintln!("Execution took {}ns", lapsed.as_nanos());
                                } else {
                                    eprintln!("Variant execution took {}ns", lapsed.as_nanos());
                                }
                                let stdout = fs::read_to_string(stdout_file);
                                let stderr = fs::read_to_string(stderr_file);
        
        
                                match (stdout, stderr) {
                                    (Ok(stdout), Ok(stderr)) => {
                                        // Get mem hash
                                        let (mem_hashes, glob_vals) =
                                            assert_same_state(&module, &mut store1, instance1);
        
                                        drop(guardout);
                                        drop(guarderr);
        
                                        return Some((
                                            mem_hashes,
                                            glob_vals,
                                            stdout.into(),
                                            stderr.into(),
                                            module.clone(),
                                            instance1,
                                            elapsed,
                                            if get_machine_code {
                                                let serialized = module.serialize().unwrap();
                                                serialized
                                            } else {
                                                vec![]
                                            },
                                        ));
                                    }
                                    _ => {
                                        eprintln!("Error reading stderr/out");
                                        drop(guardout);
                                        drop(guarderr);
        
                                        return None;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Error executing the function {}", e);
                                let elapsed = now.elapsed();
        
                                let stdout = fs::read_to_string(stdout_file);
                                let stderr = fs::read_to_string(stderr_file);
        
                                match (stdout, stderr) {
                                    (Ok(stdout), Ok(stderr)) => {
                                        eprintln!("Runtime error {e} {} {}", stdout, stderr);

                                        let (mem_hashes, glob_vals) =
                                            assert_same_state(&module, &mut store1, instance1);
                                            
                                        return Some((
                                            mem_hashes,
                                            glob_vals,
                                            stdout.into(),
                                            stderr.into(),
                                            module.clone(),
                                            instance1,
                                            elapsed,
                                            if get_machine_code {
                                                let serialized = module.serialize().unwrap();
                                                serialized
                                            } else {
                                                vec![]
                                            },
                                        ));
                                    }
                                    _ => {
                                        // do nothing
                                    }
                                }
        
                                drop(guardout);
                                drop(guarderr);
                                return None;
                            }
                        }
                    }
                    None => {
                        eprintln!("Function _start not found");
        
                        // Close files
                        return None;
                    }
                }
            },
            Err(e) => {
                eprintln!("Error instantiating the module {}", e);

                // Close files
                return None;
            }
        }

        unreachable!();
        return None;
    }
    use std::env::args;
    use std::process::ExitStatus;

    fn get_args(command: String) -> Vec<Vec<String>> {
        // Write file to tmparg
        eprintln!("Getting args from {}", command);
        // Split the command by space
        let command = command.split(" ").collect::<Vec<_>>();
        let output = std::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("Could not run the command");

        let cp = output.clone();
        let mut r = vec![];
        // Decode stdout as string
        let stdout = String::from_utf8(cp.stdout).expect("Could not decode output");
        let stderr = String::from_utf8(cp.stderr).expect("Could not decode stderr");
        let stdout = stdout.trim().to_string();
        // Split first by line
        let stdout_split = stdout.split("\n");
        for l in stdout_split {
            // add a first argument
            let l = format!("first {}", l);
            let stdout_split = l.split(" ");
            let stdout_split = stdout_split.map(String::from).collect::<Vec<_>>();
            r.push(stdout_split);
        }

        // Split the output by the space

        return r;
    }

    fn assert_same_evaluation_single(
        original_wasm: &[u8],
        mutated_wasm: &[u8],
        args: Vec<String>,
        fuel: u64,
        check_mem: bool,
    ) -> Option<ExecutionResult> {
        match (
            execute_single(original_wasm, args.clone(), fuel, false, true),
            execute_single(mutated_wasm, args.clone(), fuel, false, false),
        ) {
            (
                Some((mem1, glob1, stdout1, stderr1, _mod1, instance1, time1, _)),
                Some((mem2, glob2, stdout2, stderr2, _mod2, instance2, time2, _)),
            ) => {
                if (stdout1 != stdout2) {
                    eprintln!("Std is not the same");
                    eprintln!("{:?}\n======\n{:?}", stdout1, stdout2);
                    return None;
                }
                // Now we compare the stores
                if check_mem {
                    if mem1.len() != mem2.len() {
                        eprintln!("Memories are not the same");
                        return None;
                    }


                    // Compare the memories
                    // Zip them and compare the hashes in order
                    for (m1, m2) in mem1.iter().zip(mem2.iter()) {
                        if m1 != m2 {
                            eprintln!("Memories are not the same");
                            return None;
                        }
                    }

                    // The same for globals
                    for (g1, g2) in glob1.iter().zip(glob2.iter()) {
                        if !assert_val_eq(&g1, &g2) {
                            eprintln!("Globals are not the same");
                            return None;
                        }
                    }
                };
                eprintln!("Orig Time {}ns", time1.as_nanos());
                eprintln!("Variant Time {}ns", time2.as_nanos());

                return Some((
                    mem2,
                    glob2,
                    stdout2,
                    stderr2,
                    _mod2,
                    instance2,
                    time2,
                    vec![],
                ));
            }
            _ => return None,
        }
    }

    /// Compile, instantiate, and evaluate both the original and mutated Wasm.
    ///
    /// We should get identical results because we told `wasm-mutate` to preserve
    /// semantics.
    pub fn assert_same_evaluation(
        original_wasm: &[u8],
        mutated_wasm: &[u8],
        args: Vec<String>,
        fuel: u64,
        check_mem: bool,
        args_generator: Option<String>,
    ) -> Option<ExecutionResult> {
        // Execute the first arguments first
        let mut r1 = assert_same_evaluation_single(
            original_wasm,
            mutated_wasm,
            args.clone(),
            fuel,
            check_mem,
        );

        if let Some(command) = args_generator {
            let argsall = get_args(command);

            // Iterate over the arguments
            for ar in argsall {
                let mut args = ar.clone();

                if let Some(r) = assert_same_evaluation_single(
                    original_wasm,
                    mutated_wasm,
                    args,
                    fuel,
                    check_mem,
                ) {
                    continue;
                } else {
                    // Shortcut
                    eprintln!("One input fails. {}", ar.join(" "));
                    return None;
                }
            }
        }

        return r1;
    }

    fn assert_same_state(
        orig_module: &wasmtime::Module,
        orig_store: &mut wasmtime::Store<WasiCtx>,
        orig_instance: wasmtime::Instance,
    ) -> (Vec<u64>, Vec<Val>) {
        let mut mem_hashes = vec![];
        let mut glob_vals = vec![];
        for export in orig_module.exports() {
            match export.ty() {
                wasmtime::ExternType::Global(_) => {
                    let orig = orig_instance
                        .get_export(&mut *orig_store, export.name())
                        .unwrap()
                        .into_global()
                        .unwrap()
                        .get(&mut *orig_store);

                    glob_vals.push(orig);
                }
                wasmtime::ExternType::Memory(_) => {
                    let orig = orig_instance
                        .get_export(&mut *orig_store, export.name())
                        .unwrap()
                        .into_memory()
                        .unwrap();
                    let mut h = DefaultHasher::default();
                    orig.data(&orig_store).hash(&mut h);
                    let orig = h.finish();

                    mem_hashes.push(orig);
                }
                _ => continue,
            }
        }

        return (mem_hashes, glob_vals);
    }

    fn assert_val_eq(orig_val: &wasmtime::Val, mutated_val: &wasmtime::Val) -> bool {
        match (orig_val, mutated_val) {
            (wasmtime::Val::I32(o), wasmtime::Val::I32(m)) => return o == m,
            (wasmtime::Val::I64(o), wasmtime::Val::I64(m)) => return o == m,
            (wasmtime::Val::F32(o), wasmtime::Val::F32(m)) => {
                let o = f32::from_bits(*o);
                let m = f32::from_bits(*m);
                return o == m || (o.is_nan() && m.is_nan());
            }
            (wasmtime::Val::F64(o), wasmtime::Val::F64(m)) => {
                let o = f64::from_bits(*o);
                let m = f64::from_bits(*m);
                return o == m || (o.is_nan() && m.is_nan());
            }
            (wasmtime::Val::V128(o), wasmtime::Val::V128(m)) => return o == m,
            (wasmtime::Val::ExternRef(o), wasmtime::Val::ExternRef(m)) => {
                return o.is_none() == m.is_none()
            }
            (wasmtime::Val::FuncRef(o), wasmtime::Val::FuncRef(m)) => {
                return o.is_none() == m.is_none()
            }
            (o, m) => {
                eprintln!(
                    "mutated and original Wasm diverged: orig = {:?}; mutated = {:?}",
                    o, m,
                );
                return false;
            }
        }
    }
}
