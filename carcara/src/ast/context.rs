use crate::ast::*;
use std::sync::{atomic::AtomicUsize, Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub struct Context {
    pub mappings: Vec<(Rc<Term>, Rc<Term>)>,
    pub bindings: AHashSet<SortedVar>,
    pub cumulative_substitution: Option<Substitution>,
}

/// A tuple that will represent a single `Context` and allows a `Context` to be shared between threads.
///
/// `0`: Number of threads that will use this context.
///
/// `1`: Shareable and droppable slot for the context.
type ContextInfo = (AtomicUsize, RwLock<Option<Context>>);

#[derive(Default)]
/// Struct that implements a thread-shared context stack. That way, this stack
/// tries to use an already existing global `Context` (built by another thread).
/// If it was still not built, then the current thread is going to build this
/// context so other threads can also use it.
pub struct ContextStack {
    /// The context vector that is shared globally between all the threads.
    /// The contexts storage is index based (which the index of each context is
    /// defined by the anchor/subproof id obtained in the parser).
    context_vec: Arc<Vec<ContextInfo>>,
    /// The stack of contexts id (works just like a map to `context_vec`).
    stack: Vec<usize>,
    num_cumulative_calculated: usize,
}

impl ContextStack {
    pub fn new() -> Self {
        Default::default()
    }

    /// Creates an empty stack from contexts usage info (a vector indicating how
    /// many threads are going to use each context).
    pub fn from_usage(context_usage: &Vec<usize>) -> Self {
        let mut context_vec: Arc<Vec<ContextInfo>> = Arc::new(vec![]);
        let ctx_ref = Arc::get_mut(&mut context_vec).unwrap();

        for &usage in context_usage {
            ctx_ref.push((AtomicUsize::new(usage), RwLock::new(None)));
        }

        Self {
            context_vec,
            stack: vec![],
            num_cumulative_calculated: 0,
        }
    }

    /// Creates an empty stack from a previous stack (starts with context infos
    /// already instantiated).
    pub fn from_previous(&self) -> Self {
        Self {
            context_vec: self.context_vec.clone(),
            stack: vec![],
            num_cumulative_calculated: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn last(&self) -> Option<RwLockReadGuard<Option<Context>>> {
        self.stack
            .last()
            .and_then(|id| Some(self.context_vec[*id].1.read().unwrap()))
    }

    pub fn last_mut(&mut self) -> Option<RwLockWriteGuard<Option<Context>>> {
        self.stack
            .last_mut()
            .and_then(|id| Some(self.context_vec[*id].1.write().unwrap()))
    }

    // TODO: Add pre push function for single thread tasks

    pub fn push(
        &mut self,
        pool: &mut dyn TermPool,
        assignment_args: &[(String, Rc<Term>)],
        variable_args: &[SortedVar],
        context_id: usize,
    ) -> Result<(), SubstitutionError> {
        let ctx_building_status = self.context_vec[context_id].1.try_write();
        match ctx_building_status {
            // The write guard was yielded to this thread
            Ok(mut ctx_write_guard) => {
                match ctx_write_guard.as_mut() {
                    // Since the context already exists, just use it
                    Some(_) => {
                        drop(ctx_write_guard);
                    }
                    // It's the first thread trying to build this context. It will
                    // build this context at the context vec (accessible for all threads)
                    None => {
                        // Since some rules (like `refl`) need to apply substitutions until a fixed point, we
                        // precompute these substitutions into a separate hash map. This assumes that the assignment
                        // arguments are in the correct order.
                        let mut substitution = Substitution::empty();
                        let mut substitution_until_fixed_point = Substitution::empty();

                        // We build the `substitution_until_fixed_point` hash map from the bottom up, by using the
                        // substitutions already introduced to transform the result of a new substitution before
                        // inserting it into the hash map. So for instance, if the substitutions are `(:= y z)` and
                        // `(:= x (f y))`, we insert the first substitution, and then, when introducing the second,
                        // we use the current state of the hash map to transform `(f y)` into `(f z)`. The
                        // resulting hash map will then contain `(:= y z)` and `(:= x (f z))`
                        for (var, value) in assignment_args.iter() {
                            let var_term = Term::new_var(var, pool.sort(value));
                            let var_term = pool.add(var_term);
                            substitution.insert(pool, var_term.clone(), value.clone())?;
                            let new_value = substitution_until_fixed_point.apply(pool, value);
                            substitution_until_fixed_point.insert(pool, var_term, new_value)?;
                        }

                        let mappings = assignment_args
                            .iter()
                            .map(|(var, value)| {
                                let var_term = (var.clone(), pool.sort(value)).into();
                                (pool.add(var_term), value.clone())
                            })
                            .collect();
                        let bindings = variable_args.iter().cloned().collect();
                        // Finally creates the new context under this RwLock
                        *ctx_write_guard = Some(Context {
                            mappings,
                            bindings,
                            cumulative_substitution: None,
                        });
                    }
                }
            }
            // A thread is currently building the context
            Err(_) => {}
        }
        // Adds this context in the stack
        // Notice that even though the context is not ready for use, the write
        // guard is still being held by some thread, then if this context is
        // required at any moment, then we are assured it will wait until the
        // fully context construction
        self.stack.push(context_id);
        Ok(())
    }

    pub fn pop(&mut self) {
        use std::sync::atomic::Ordering;

        if let Some(id) = self.stack.pop() {
            let this_context = &self.context_vec[id];

            let mut remaining_threads = this_context.0.load(Ordering::Acquire);
            remaining_threads = remaining_threads
                .checked_sub(1)
                .expect("A thread tried to access a context not allocated for it.");

            if remaining_threads == 0 {
                // Drop this context since the last thread stopped using it
                *this_context.1.write().unwrap() = None;
            }
            this_context.0.store(remaining_threads, Ordering::Release);
        }

        self.num_cumulative_calculated =
            std::cmp::min(self.num_cumulative_calculated, self.stack.len());
    }

    fn catch_up_cumulative(&mut self, pool: &mut dyn TermPool, up_to: usize) {
        for i in self.num_cumulative_calculated..std::cmp::max(up_to + 1, self.len()) {
            // Requires read guard. Since the i-th context will be mutated far
            // below this line, we first take the read guard here and then, when
            // necessary, we require the write guard. This tries to avoid bigger
            // overheads
            let context_guard = self.context_vec[self.stack[i]].1.read().unwrap();
            let curr_context = context_guard.as_ref().unwrap();

            let simultaneous = build_simultaneous_substitution(pool, &curr_context.mappings).map;
            let mut cumulative_substitution = simultaneous.clone();

            if i > 0 {
                // Waits until OS allows to read this previous context. The code structure
                // makes sure that this context, when released for reading, will be already
                // instantiated since there are only 2 cases:
                //  - This thread was responsible for building this previous context. Then
                //      this context has already been built.
                //  - Another thread was assigned to build this context. Then, it doesn't
                //      matter if this other thread has already finished the process, the
                //      current thread will have to wait until the guard is released.
                if let Some(previous_context) = self
                    .stack
                    .get(i - 1)
                    .and_then(|id| Some(self.context_vec[*id].1.read().unwrap()))
                {
                    let previous_context = previous_context.as_ref().unwrap();
                    let previous_substitution =
                        previous_context.cumulative_substitution.as_ref().unwrap();

                    for (k, v) in previous_substitution.map.iter() {
                        let value = match simultaneous.get(v) {
                            Some(new_value) => new_value,
                            None => v,
                        };
                        cumulative_substitution.insert(k.clone(), value.clone());
                    }
                }
            }
            // Waits until the OS allows to mutate at this context
            let mut context_guard = self.context_vec[self.stack[i]].1.write().unwrap();
            let mut curr_context = context_guard.as_mut().unwrap();
            curr_context.cumulative_substitution =
                Some(Substitution::new(pool, cumulative_substitution).unwrap());
            self.num_cumulative_calculated = i + 1;
        }
    }

    pub fn apply_previous(&mut self, pool: &mut dyn TermPool, term: &Rc<Term>) -> Rc<Term> {
        if self.len() < 2 {
            term.clone()
        } else {
            let index = self.len() - 2;
            self.catch_up_cumulative(pool, index);
            self.context_vec[self.stack[index]]
                .1
                .write()
                .unwrap()
                .as_mut()
                .unwrap()
                .cumulative_substitution
                .as_mut()
                .unwrap()
                .apply(pool, term)
        }
    }

    pub fn apply(&mut self, pool: &mut dyn TermPool, term: &Rc<Term>) -> Rc<Term> {
        if self.is_empty() {
            term.clone()
        } else {
            let index = self.len() - 1;
            self.catch_up_cumulative(pool, index);
            self.context_vec[self.stack[index]]
                .1
                .write()
                .unwrap()
                .as_mut()
                .unwrap()
                .cumulative_substitution
                .as_mut()
                .unwrap()
                .apply(pool, term)
        }
    }
}

fn build_simultaneous_substitution(
    pool: &mut dyn TermPool,
    mappings: &[(Rc<Term>, Rc<Term>)],
) -> Substitution {
    let mut result = Substitution::empty();

    // We build the simultaneous substitution from the bottom up, by using the mappings already
    // introduced to transform the result of a new mapping before inserting it into the
    // substitution. So for instance, if the mappings are `y -> z` and `x -> (f y)`, we insert the
    // first mapping, and then, when introducing the second, we use the current state of the
    // substitution to transform `(f y)` into `(f z)`. The result will then contain `y -> z` and
    // `x -> (f z)`.
    for (var, value) in mappings {
        let new_value = result.apply(pool, value);

        // We can unwrap here safely because, by construction, the sort of `var` is the
        // same as the sort of `new_value`
        result.insert(pool, var.clone(), new_value).unwrap();
    }
    result
}
