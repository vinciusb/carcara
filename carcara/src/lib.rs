#![deny(clippy::disallowed_methods)]
#![deny(clippy::self_named_module_files)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![warn(clippy::branches_sharing_code)]
#![warn(clippy::cloned_instead_of_copied)]
#![warn(clippy::copy_iterator)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::doc_markdown)]
#![warn(clippy::equatable_if_let)]
#![warn(clippy::explicit_into_iter_loop)]
#![warn(clippy::explicit_iter_loop)]
#![warn(clippy::from_iter_instead_of_collect)]
#![warn(clippy::get_unwrap)]
#![warn(clippy::if_not_else)]
#![warn(clippy::implicit_clone)]
#![warn(clippy::inconsistent_struct_constructor)]
#![warn(clippy::index_refutable_slice)]
#![warn(clippy::inefficient_to_string)]
#![warn(clippy::items_after_statements)]
#![warn(clippy::large_types_passed_by_value)]
#![warn(clippy::manual_assert)]
#![warn(clippy::manual_ok_or)]
#![warn(clippy::map_unwrap_or)]
#![warn(clippy::match_wildcard_for_single_variants)]
#![warn(clippy::mixed_read_write_in_expression)]
#![warn(clippy::multiple_crate_versions)]
#![warn(clippy::redundant_closure_for_method_calls)]
#![warn(clippy::redundant_pub_crate)]
#![warn(clippy::semicolon_if_nothing_returned)]
#![warn(clippy::str_to_string)]
#![warn(clippy::string_to_string)]
#![warn(clippy::trivially_copy_pass_by_ref)]
#![warn(clippy::unnecessary_wraps)]
#![warn(clippy::unnested_or_patterns)]
#![warn(clippy::unused_self)]

#[macro_use]
pub mod ast;
pub mod benchmarking;
pub mod checker;
pub mod elaborator;
pub mod parser;
mod utils;

use benchmarking::OnlineBenchmarkResults;
use checker::{error::CheckerError, CheckerStatistics};
use parser::ParserError;
use parser::Position;
use std::cell::RefCell;
use std::io;
use std::time::{Duration, Instant};
use thiserror::Error;

use crate::benchmarking::{CollectResults, RunMeasurement};

pub type CarcaraResult<T> = Result<T, Error>;

/// The options that control how Carcara parses, checks and elaborates a proof.
#[derive(Default)]
pub struct CarcaraOptions {
    /// If `true`, Carcara will automatically expand function definitions introduced by `define-fun`
    /// commands in the SMT problem. If `false`, those `define-fun`s are instead interpreted as a
    /// function declaration and an `assert` command that defines the function as equal to its body
    /// (or to a lambda term, if it contains arguments). Note that function definitions in the proof
    /// are always expanded.
    pub apply_function_defs: bool,

    /// If `true`, Carcara will eliminate `let` bindings from terms during parsing. This is done by
    /// replacing any occurence of a variable bound in the `let` binding with its corresponding
    /// value.
    pub expand_lets: bool,

    /// If `true`, this relaxes the type checking rules in Carcara to allow `Int`-`Real` subtyping.
    /// That is, terms of sort `Int` will be allowed in arithmetic operations where a `Real` term
    /// was expected. Note that this only applies to predefined operators --- passing an `Int` term
    /// to a function that expects a `Real` will still be an error.
    pub allow_int_real_subtyping: bool,

    /// Enable checking/elaboration of `lia_generic` steps using cvc5. When checking a proof, this
    /// will call cvc5 to solve the linear integer arithmetic problem, check the proof, and discard
    /// it. When elaborating, the proof will instead be inserted in the place of the `lia_generic`
    /// step.
    pub lia_via_cvc5: bool,

    /// Enables "strict" checking of some rules.
    ///
    /// Currently, if enabled, the following rules are affected:
    /// - `assume` and `refl`: implicit reordering of equalities is not allowed
    /// - `resolution` and `th_resolution`: the pivots must be provided as arguments
    ///
    /// In general, the invariant we aim for is that, if you are checking a proof that was
    /// elaborated by Carcara, you can safely enable this option (and possibly get a performance
    /// benefit).
    pub strict: bool,

    /// If `true`, Carcara will skip any rules that it does not recognize, and will consider them as
    /// holes. Normally, using an unknown rule is considered an error.
    pub skip_unknown_rules: bool,

    /// If `true`, Carcará will log the check and elaboration statistics of any
    /// `check` or `check_and_elaborate` run. If `false` no statistics are logged.
    pub stats: bool,
}

impl CarcaraOptions {
    /// Constructs a new `CarcaraOptions` with all options set to `false`.
    pub fn new() -> Self {
        Self::default()
    }
}

fn wrap_parser_error_message(e: &ParserError, pos: &Position) -> String {
    // For unclosed subproof errors, we don't print the position
    if matches!(e, ParserError::UnclosedSubproof(_)) {
        format!("parser error: {}", e)
    } else {
        format!("parser error: {} (on line {}, column {})", e, pos.0, pos.1)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("{}", wrap_parser_error_message(.0, .1))]
    Parser(ParserError, Position),

    #[error("checking failed on step '{step}' with rule '{rule}': {inner}")]
    Checker {
        inner: CheckerError,
        rule: String,
        step: String,
    },

    // While this is a kind of checking error, it does not happen in a specific step like all other
    // checker errors, so we model it as a different variant
    #[error("checker error: proof does not conclude empty clause")]
    DoesNotReachEmptyClause,
}

pub fn check<T: io::BufRead>(
    problem: T,
    proof: T,
    options: CarcaraOptions,
    num_threads: usize,
) -> Result<bool, Error> {
    let mut run_measures: RunMeasurement = RunMeasurement::default();

    // Parsing
    let total = Instant::now();
    #[cfg(feature = "thread-safety")]
    let (prelude, proof, pool) = parser::parse_instance_multithread(
        problem,
        proof,
        options.apply_function_defs,
        options.expand_lets,
        options.allow_int_real_subtyping,
    )?;

    #[cfg(not(feature = "thread-safety"))]
    let (prelude, proof, mut pool) = parser::parse_instance(
        problem,
        proof,
        options.apply_function_defs,
        options.expand_lets,
        options.allow_int_real_subtyping,
    )?;
    run_measures.parsing = total.elapsed();

    let config = checker::Config::new()
        .strict(options.strict)
        .skip_unknown_rules(options.skip_unknown_rules)
        .lia_via_cvc5(options.lia_via_cvc5);

    let checker_stats = &mut options.stats.then(|| CheckerStatistics {
        file_name: "this",
        elaboration_time: Duration::ZERO,
        polyeq_time: Duration::ZERO,
        assume_time: Duration::ZERO,
        assume_core_time: Duration::ZERO,
        results: std::rc::Rc::new(RefCell::new(OnlineBenchmarkResults::new())),
    });

    // Checking
    let checking = Instant::now();
    let res = {
        #[cfg(feature = "thread-safety")]
        {
            use crate::checker::Scheduler::Scheduler;

            println!("THREAD");
            let (scheduler, schedule_context_usage) = Scheduler::new(num_threads, &proof);
            checker::ParallelProofChecker::new(pool, config, &prelude, &schedule_context_usage)
                .check(&proof, &scheduler, checker_stats)
        }
        #[cfg(not(feature = "thread-safety"))]
        {
            println!("NOT THREAD");
            checker::ProofChecker::new(&mut pool, config, prelude).check(&proof, checker_stats)
        }
    };
    run_measures.checking = checking.elapsed();
    run_measures.total = total.elapsed();

    // If the statistics were collected and no error happend
    if let Some(c_stats) = checker_stats {
        let mut c_stats_results = c_stats.results.as_ref().borrow_mut();
        c_stats_results.add_run_measurement(
            &("this".to_string(), 0),
            RunMeasurement {
                parsing: run_measures.parsing,
                checking: run_measures.checking,
                elaboration: c_stats.elaboration_time,
                total: run_measures.total,
                polyeq: c_stats.polyeq_time,
                assume: c_stats.assume_time,
                assume_core: c_stats.assume_core_time,
            },
        );
        // Print the statistics
        c_stats_results.print(false);
    }

    res
}

pub fn check_and_elaborate<T: io::BufRead>(
    problem: T,
    proof: T,
    options: CarcaraOptions,
) -> Result<(bool, ast::Proof), Error> {
    let mut run_measures: RunMeasurement = RunMeasurement::default();

    let total = Instant::now();
    let (prelude, proof, mut pool) = parser::parse_instance(
        problem,
        proof,
        options.apply_function_defs,
        options.expand_lets,
        options.allow_int_real_subtyping,
    )?;
    run_measures.parsing = total.elapsed();

    let config = checker::Config::new()
        .strict(options.strict)
        .skip_unknown_rules(options.skip_unknown_rules)
        .lia_via_cvc5(options.lia_via_cvc5);

    let checker_stats = &mut options.stats.then(|| CheckerStatistics {
        file_name: "this",
        elaboration_time: Duration::ZERO,
        polyeq_time: Duration::ZERO,
        assume_time: Duration::ZERO,
        assume_core_time: Duration::ZERO,
        results: std::rc::Rc::new(RefCell::new(OnlineBenchmarkResults::new())),
    });

    let checking = Instant::now();
    let res = checker::ProofChecker::new(&mut pool, config, prelude)
        .check_and_elaborate(proof, checker_stats);
    run_measures.checking = checking.elapsed();
    run_measures.total = total.elapsed();

    // If the statistics were collected and no error happend
    if let Some(c_stats) = checker_stats {
        let mut c_stats_results = c_stats.results.as_ref().borrow_mut();
        c_stats_results.add_run_measurement(
            &("this".to_string(), 0),
            RunMeasurement {
                parsing: run_measures.parsing,
                checking: run_measures.checking,
                elaboration: c_stats.elaboration_time,
                total: run_measures.total,
                polyeq: c_stats.polyeq_time,
                assume: c_stats.assume_time,
                assume_core: c_stats.assume_core_time,
            },
        );
        // Print the statistics
        c_stats_results.print(false);
    }

    res
}
