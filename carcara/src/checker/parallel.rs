#![allow(unused_imports)]
#![allow(dead_code)]

use super::error::CheckerError;
use super::rules::{Premise, Rule, RuleArgs, RuleResult};
#[cfg(feature = "thread-safety")]
use super::scheduler::{iter::ScheduleIter, Scheduler::Scheduler};
use super::{lia_generic, CheckerStatistics, Config};
use crate::benchmarking::CollectResults;
use crate::{ast::*, CarcaraResult, Error};
use ahash::AHashSet;
use std::thread;
use std::{
    cell::RefCell,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

unsafe impl<CR: CollectResults + Send> Sync for CheckerStatistics<'_, CR> {}
unsafe impl<CR: CollectResults + Send> Send for CheckerStatistics<'_, CR> {}

pub struct ParallelProofChecker<'c> {
    pool: Arc<SingleThreadPool::TermPool>,
    config: Config,
    prelude: &'c ProblemPrelude,
    context: ContextStack,
    reached_empty_clause: bool,
    is_holey: bool,
}

#[cfg(feature = "thread-safety")]
impl<'c> ParallelProofChecker<'c> {
    pub fn new(
        pool: Arc<SingleThreadPool::TermPool>,
        config: Config,
        prelude: &'c ProblemPrelude,
        context_usage: &Vec<usize>,
    ) -> Self {
        ParallelProofChecker {
            pool,
            config,
            prelude,
            context: ContextStack::from_usage(context_usage),
            reached_empty_clause: false,
            is_holey: false,
        }
    }

    /// Copies the proof checker and instantiate parallel fields
    pub fn parallelize_self(&self) -> Self {
        ParallelProofChecker {
            pool: self.pool.clone(),
            config: self.config.clone(),
            prelude: self.prelude,
            context: ContextStack::from_previous(&self.context),
            reached_empty_clause: false,
            is_holey: false,
        }
    }

    pub fn check<'s, 'p, CR: CollectResults + Send>(
        &'s mut self,
        proof: &'p Proof,
        scheduler: &'s Scheduler,
        statistics: &mut Option<CheckerStatistics<CR>>,
    ) -> CarcaraResult<bool> {
        // Used to estimulate threads to abort prematurely (only happens when a
        // thread already found out an invalid step)
        let premature_abort = Arc::new(RwLock::new(false));
        let context_pool = Arc::new(RwLock::new(SingleThreadPool::TermPool::new()));
        //
        thread::scope(|s| {
            let threads: Vec<_> = (&scheduler.loads)
                .into_iter()
                .enumerate()
                .map(|(i, schedule)| {
                    // Creates a local statistics collector, allowing the collection
                    // of this threads statistics and then the merge
                    let mut local_stats = statistics.as_ref().and_then(|s| {
                        Some(CheckerStatistics {
                            file_name: s.file_name,
                            elaboration_time: Duration::ZERO,
                            polyeq_time: Duration::ZERO,
                            assume_time: Duration::ZERO,
                            assume_core_time: Duration::ZERO,
                            results: std::rc::Rc::new(RefCell::new(CR::new())),
                        })
                    });
                    let mut local_self = self.parallelize_self();
                    let mut merged_pool = TermPool::from_previous(&local_self.pool, &context_pool);
                    let should_abort = premature_abort.clone();

                    thread::Builder::new()
                        .name(format!("worker-{i}"))
                        .spawn_scoped(
                        s,
                        move || -> CarcaraResult<(bool, bool, Option<CheckerStatistics<CR>>)> {
                            let mut iter = schedule.iter();

                            while let Some(command) = iter.next() {
                                match command {
                                    ProofCommand::Step(step) => {
                                        // If this step ends a subproof, it might need to implicitly reference the
                                        // previous command in the subproof
                                        let previous_command = if iter.is_end_step() {
                                            let subproof = iter.current_subproof().unwrap();
                                            let index = subproof.len() - 2;
                                            subproof.get(index).map(|command| {
                                                Premise::new((iter.depth(), index), command)
                                            })
                                        } else {
                                            None
                                        };

                                        if step.id == "t45" {
                                            print!("aqui\n")
                                        }

                                        local_self
                                            .check_step(
                                                step,
                                                previous_command,
                                                &iter,
                                                &mut merged_pool,
                                                &mut local_stats,
                                            )
                                            .map_err(|e| {
                                                // Signals to other threads to stop the proof checking
                                                *should_abort.write().unwrap() = true;
                                                Error::Checker {
                                                    inner: e,
                                                    rule: step.rule.clone(),
                                                    step: step.id.clone(),
                                                }
                                            })?;

                                        if step.clause.is_empty() {
                                            local_self.reached_empty_clause = true;
                                        }
                                    }
                                    ProofCommand::Subproof(s) => {
                                        let time = Instant::now();
                                        let step_id = command.id();

                                        local_self
                                            .context
                                            .push_from_id(
                                                &mut merged_pool,
                                                &s.assignment_args,
                                                &s.variable_args,
                                                s.context_id,
                                            )
                                            .map_err(|e| {
                                                // Signals to other threads to stop the proof checking
                                                *should_abort.write().unwrap() = true;
                                                Error::Checker {
                                                    inner: e.into(),
                                                    rule: "anchor".into(),
                                                    step: step_id.to_owned(),
                                                }
                                            })?;

                                        if let Some(stats) = &mut local_stats {
                                            let rule_name = match s.commands.last() {
                                                Some(ProofCommand::Step(step)) => {
                                                    format!("anchor({})", &step.rule)
                                                }
                                                _ => "anchor".to_owned(),
                                            };
                                            stats
                                                .results
                                                .as_ref()
                                                .borrow_mut()
                                                .add_step_measurement(
                                                    stats.file_name,
                                                    step_id,
                                                    &rule_name,
                                                    time.elapsed(),
                                                );
                                        }
                                    }
                                    ProofCommand::Assume { id, term } => {
                                        if !local_self.check_assume(
                                            id,
                                            term,
                                            &proof.premises,
                                            &iter,
                                            &mut local_stats,
                                        ) {
                                            // Signals to other threads to stop the proof checking
                                            *should_abort.write().unwrap() = true;
                                            return Err(Error::Checker {
                                                inner: CheckerError::Assume(term.clone()),
                                                rule: "assume".into(),
                                                step: id.clone(),
                                            });
                                        }
                                    }
                                    ProofCommand::Closing => {
                                        // If this is the last command of a subproof, we have to pop the subproof
                                        // commands off of the stack. The parser already ensures that the last command
                                        // in a subproof is always a `step` command
                                        local_self.context.pop();
                                    }
                                }
                                // If the thread got untill here and no error
                                // happend, then carcará will assume this thread
                                // got no error (even though an invalid step
                                // could be found in the next steps).
                                if *should_abort.read().unwrap() {
                                    break;
                                }
                            }

                            // Returns Ok(reached empty clause, isHoley, current thread statistics)
                            if local_self.config.is_running_test || local_self.reached_empty_clause
                            {
                                Ok((true, local_self.is_holey, local_stats))
                            } else {
                                Ok((false, local_self.is_holey, local_stats))
                            }
                        },
                        )
                        .unwrap()
                })
                .collect();

            // Unify the results of all threads and generate the final result based on them
            let (mut reached, mut holey) = (false, false);
            let mut err: Result<_, Error> = Ok(());

            // Wait until the threads finish and merge the results and statistics
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .for_each(|opt| {
                    match opt {
                        Ok((_reached, _holey, local_stats)) => {
                            // Combine the statistics
                            if let Some(l_stats) = local_stats.as_ref() {
                                let merged = statistics.as_mut().unwrap();

                                // Since combine needs a value instead of reference,
                                // it's needed to copy both of the results because
                                // the copy trait can't be implemented directly.
                                let (mut merged_copy, mut local_stats_copy) =
                                    (CR::new(), CR::new());
                                merged_copy.copy_from(&*merged.results.as_ref().borrow_mut());
                                local_stats_copy.copy_from(&*l_stats.results.as_ref().borrow_mut());

                                //
                                *merged.results.as_ref().borrow_mut() =
                                    CR::combine(merged_copy, local_stats_copy);

                                // Make sure
                                merged.elaboration_time += l_stats.elaboration_time;
                                merged.polyeq_time += l_stats.polyeq_time;
                                merged.assume_time += l_stats.assume_time;
                                merged.assume_core_time += l_stats.assume_core_time;
                            }
                            // Mask the result booleans
                            (reached, holey) = (reached | _reached, holey | _holey);
                        }
                        Err(e) => {
                            err = Err(e);
                        }
                    }
                });

            // If an error happend
            if let Err(x) = err {
                return Err(x);
            }

            if reached {
                Ok(holey)
            } else {
                Err(Error::DoesNotReachEmptyClause)
            }
        })
    }

    fn check_assume<CR: CollectResults + Send>(
        &mut self,
        id: &str,
        term: &Rc<Term>,
        premises: &AHashSet<Rc<Term>>,
        iter: &ScheduleIter,
        statistics: &mut Option<CheckerStatistics<CR>>,
    ) -> bool {
        let time = Instant::now();

        // Some subproofs contain `assume` commands inside them. These don't refer
        // to the original problem premises, so we ignore the `assume` command if
        // it is inside a subproof. Since the unit tests for the rules don't define the
        // original problem, but sometimes use `assume` commands, we also skip the
        // command if we are in a testing context.
        if self.config.is_running_test || iter.is_in_subproof() {
            return true;
        }

        if premises.contains(term) {
            if let Some(s) = statistics {
                let time = time.elapsed();
                s.assume_time += time;
                s.results
                    .as_ref()
                    .borrow_mut()
                    .add_assume_measurement(s.file_name, id, true, time);
            }
            return true;
        }

        if self.config.strict {
            return false;
        }

        let mut found = None;
        let mut polyeq_time = Duration::ZERO;
        let mut core_time = Duration::ZERO;
        for p in premises {
            let mut this_polyeq_time = Duration::ZERO;
            let (result, depth) = tracing_polyeq(term, p, &mut this_polyeq_time);
            polyeq_time += this_polyeq_time;
            if let Some(s) = statistics {
                s.results.as_ref().borrow_mut().add_polyeq_depth(depth);
            }
            if result {
                core_time = this_polyeq_time;
                found = Some(p.clone());
                break;
            }
        }

        let Some(_) = found else { return false };

        if let Some(s) = statistics {
            let time = time.elapsed();
            s.assume_time += time;
            s.assume_core_time += core_time;
            s.polyeq_time += polyeq_time;
            s.results
                .as_ref()
                .borrow_mut()
                .add_assume_measurement(s.file_name, id, false, time);
        }

        true
    }

    fn check_step<'a, CR: CollectResults + Send>(
        &mut self,
        step: &'a ProofStep,
        previous_command: Option<Premise<'a>>,
        iter: &'a ScheduleIter<'a>,
        pool: &mut TermPool,
        statistics: &mut Option<CheckerStatistics<CR>>,
    ) -> RuleResult {
        let time = Instant::now();
        let mut polyeq_time = Duration::ZERO;

        if step.rule == "lia_generic" {
            if self.config.lia_via_cvc5 {
                let is_hole =
                    lia_generic::lia_generic(pool, &step.clause, &self.prelude, None, &step.id);
                self.is_holey = self.is_holey || is_hole;
            } else {
                log::warn!("encountered \"lia_generic\" rule, ignoring");
                self.is_holey = true;
            }
        } else {
            let rule = match Self::get_rule(&step.rule, self.config.strict) {
                Some(r) => r,
                None if self.config.skip_unknown_rules => {
                    self.is_holey = true;
                    return Ok(());
                }
                None => return Err(CheckerError::UnknownRule),
            };

            if step.rule == "hole" {
                self.is_holey = true;
            }

            let premises: Vec<_> = step
                .premises
                .iter()
                .map(|&p| {
                    let command = iter.get_premise(p);
                    Premise::new(p, command)
                })
                .collect();
            let discharge: Vec<_> = step
                .discharge
                .iter()
                .map(|&i| iter.get_premise(i))
                .collect();

            let rule_args = RuleArgs {
                conclusion: &step.clause,
                premises: &premises,
                args: &step.args,
                pool,
                context: &mut self.context,
                previous_command,
                discharge: &discharge,
                polyeq_time: &mut polyeq_time,
            };

            rule(rule_args)?;
        }

        if let Some(s) = statistics {
            let time = time.elapsed();
            s.results.as_ref().borrow_mut().add_step_measurement(
                s.file_name,
                &step.id,
                &step.rule,
                time,
            );
            s.polyeq_time += polyeq_time;
        }
        Ok(())
    }

    pub fn get_rule(rule_name: &str, strict: bool) -> Option<Rule> {
        use super::rules::*;

        Some(match rule_name {
            "true" => tautology::r#true,
            "false" => tautology::r#false,
            "not_not" => tautology::not_not,
            "and_pos" => tautology::and_pos,
            "and_neg" => tautology::and_neg,
            "or_pos" => tautology::or_pos,
            "or_neg" => tautology::or_neg,
            "xor_pos1" => tautology::xor_pos1,
            "xor_pos2" => tautology::xor_pos2,
            "xor_neg1" => tautology::xor_neg1,
            "xor_neg2" => tautology::xor_neg2,
            "implies_pos" => tautology::implies_pos,
            "implies_neg1" => tautology::implies_neg1,
            "implies_neg2" => tautology::implies_neg2,
            "equiv_pos1" => tautology::equiv_pos1,
            "equiv_pos2" => tautology::equiv_pos2,
            "equiv_neg1" => tautology::equiv_neg1,
            "equiv_neg2" => tautology::equiv_neg2,
            "ite_pos1" => tautology::ite_pos1,
            "ite_pos2" => tautology::ite_pos2,
            "ite_neg1" => tautology::ite_neg1,
            "ite_neg2" => tautology::ite_neg2,
            "eq_reflexive" => reflexivity::eq_reflexive,
            "eq_transitive" => transitivity::eq_transitive,
            "eq_congruent" => congruence::eq_congruent,
            "eq_congruent_pred" => congruence::eq_congruent_pred,
            "distinct_elim" => clausification::distinct_elim,
            "la_rw_eq" => linear_arithmetic::la_rw_eq,
            "la_generic" => linear_arithmetic::la_generic,
            "la_disequality" => linear_arithmetic::la_disequality,
            "la_totality" => linear_arithmetic::la_totality,
            "la_tautology" => linear_arithmetic::la_tautology,
            "forall_inst" => quantifier::forall_inst,
            "qnt_join" => quantifier::qnt_join,
            "qnt_rm_unused" => quantifier::qnt_rm_unused,
            "resolution" | "th_resolution" if strict => resolution::resolution_with_args,
            "resolution" | "th_resolution" => resolution::resolution,
            "refl" if strict => reflexivity::strict_refl,
            "refl" => reflexivity::refl,
            "trans" => transitivity::trans,
            "cong" => congruence::cong,
            "ho_cong" => congruence::ho_cong,
            "and" => clausification::and,
            "tautology" => resolution::tautology,
            "not_or" => clausification::not_or,
            "or" => clausification::or,
            "not_and" => clausification::not_and,
            "xor1" => clausification::xor1,
            "xor2" => clausification::xor2,
            "not_xor1" => clausification::not_xor1,
            "not_xor2" => clausification::not_xor2,
            "implies" => clausification::implies,
            "not_implies1" => clausification::not_implies1,
            "not_implies2" => clausification::not_implies2,
            "equiv1" => tautology::equiv1,
            "equiv2" => tautology::equiv2,
            "not_equiv1" => tautology::not_equiv1,
            "not_equiv2" => tautology::not_equiv2,
            "ite1" => tautology::ite1,
            "ite2" => tautology::ite2,
            "not_ite1" => tautology::not_ite1,
            "not_ite2" => tautology::not_ite2,
            "ite_intro" => tautology::ite_intro,
            "contraction" => resolution::contraction,
            "connective_def" => tautology::connective_def,
            "ite_simplify" => simplification::ite_simplify,
            "eq_simplify" => simplification::eq_simplify,
            "and_simplify" => simplification::and_simplify,
            "or_simplify" => simplification::or_simplify,
            "not_simplify" => simplification::not_simplify,
            "implies_simplify" => simplification::implies_simplify,
            "equiv_simplify" => simplification::equiv_simplify,
            "bool_simplify" => simplification::bool_simplify,
            "qnt_simplify" => simplification::qnt_simplify,
            "div_simplify" => simplification::div_simplify,
            "prod_simplify" => simplification::prod_simplify,
            // Despite being separate rules in the specification, proofs generated by veriT don't
            // differentiate between `unary_minus_simplify` and `minus_simplify`. To account for
            // that, `simplification::minus_simplify` implements both rules in the same function.
            "unary_minus_simplify" | "minus_simplify" => simplification::minus_simplify,
            "sum_simplify" => simplification::sum_simplify,
            "comp_simplify" => simplification::comp_simplify,
            "nary_elim" => clausification::nary_elim,
            "ac_simp" => simplification::ac_simp,
            "bfun_elim" => clausification::bfun_elim,
            "bind" => subproof::bind,
            "qnt_cnf" => quantifier::qnt_cnf,
            "subproof" => subproof::subproof,
            "let" => subproof::r#let,
            "onepoint" => subproof::onepoint,
            "sko_ex" => subproof::sko_ex,
            "sko_forall" => subproof::sko_forall,
            "reordering" => extras::reordering,
            "symm" => extras::symm,
            "not_symm" => extras::not_symm,
            "eq_symmetric" => extras::eq_symmetric,
            "or_intro" => extras::or_intro,
            "bind_let" => extras::bind_let,
            "la_mult_pos" => extras::la_mult_pos,
            "la_mult_neg" => extras::la_mult_neg,

            // Special rules that always check as valid, and are used to indicate holes in the
            // proof.
            "hole" => |_| Ok(()),

            // The Alethe specification does not yet describe how this more strict version of the
            // resolution rule will be called. Until that is decided and added to the specification,
            // we define a new specialized rule that calls it
            "strict_resolution" => resolution::strict_resolution,

            _ => return None,
        })
    }
}
